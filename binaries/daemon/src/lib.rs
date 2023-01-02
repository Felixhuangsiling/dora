use coordinator::CoordinatorEvent;
use dora_core::{
    config::{DataId, InputMapping, NodeId},
    coordinator_messages::DaemonEvent,
    daemon_messages::{
        self, ControlReply, DaemonCoordinatorEvent, DaemonCoordinatorReply, DataflowId, DropEvent,
        DropToken, SpawnDataflowNodes, SpawnNodeParams,
    },
    descriptor::{CoreNodeKind, Descriptor},
};
use dora_message::uhlc::HLC;
use eyre::{bail, eyre, Context, ContextCompat};
use futures::{future, stream, FutureExt, TryFutureExt};
use futures_concurrency::stream::Merge;
use shared_memory::{Shmem, ShmemConf};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    net::SocketAddr,
    path::Path,
    rc::Rc,
    time::Duration,
};
use tcp_utils::tcp_receive;
use tokio::{
    fs,
    net::TcpStream,
    sync::{mpsc, oneshot},
    time::timeout,
};
use tokio_stream::{
    wrappers::{ReceiverStream, TcpListenerStream},
    Stream, StreamExt,
};
use uuid::Uuid;

mod coordinator;
mod listener;
mod spawn;
mod tcp_utils;

pub struct Daemon {
    port: u16,
    prepared_messages: HashMap<String, PreparedMessage>,
    sent_out_shared_memory: HashMap<DropToken, Rc<Shmem>>,

    running: HashMap<DataflowId, RunningDataflow>,

    dora_events_tx: mpsc::Sender<DoraEvent>,

    coordinator_addr: Option<SocketAddr>,
    machine_id: String,

    /// used for testing and examples
    exit_when_done: Option<BTreeSet<(Uuid, NodeId)>>,
}

impl Daemon {
    pub async fn run(coordinator_addr: SocketAddr, machine_id: String) -> eyre::Result<()> {
        // connect to the coordinator
        let coordinator_events = coordinator::register(coordinator_addr, machine_id.clone())
            .await
            .wrap_err("failed to connect to dora-coordinator")?
            .map(Event::Coordinator);
        Self::run_general(coordinator_events, Some(coordinator_addr), machine_id, None).await
    }

    pub async fn run_dataflow(dataflow_path: &Path) -> eyre::Result<()> {
        let working_dir = dataflow_path
            .canonicalize()
            .context("failed to canoncialize dataflow path")?
            .parent()
            .ok_or_else(|| eyre::eyre!("canonicalized dataflow path has no parent"))?
            .to_owned();

        let nodes = read_descriptor(dataflow_path).await?.resolve_aliases();
        let mut custom_nodes = BTreeMap::new();
        for node in nodes {
            match node.kind {
                CoreNodeKind::Runtime(_) => todo!(),
                CoreNodeKind::Custom(n) => {
                    custom_nodes.insert(
                        node.id.clone(),
                        SpawnNodeParams {
                            node_id: node.id,
                            node: n,
                            working_dir: working_dir.clone(),
                        },
                    );
                }
            }
        }

        let spawn_command = SpawnDataflowNodes {
            dataflow_id: Uuid::new_v4(),
            nodes: custom_nodes,
        };

        let exit_when_done = spawn_command
            .nodes
            .iter()
            .map(|(id, _)| (spawn_command.dataflow_id, id.clone()))
            .collect();
        let (reply_tx, reply_rx) = oneshot::channel();
        let coordinator_events = stream::once(async move {
            Event::Coordinator(CoordinatorEvent {
                event: DaemonCoordinatorEvent::Spawn(spawn_command),
                reply_tx,
            })
        });
        let run_result = Self::run_general(
            Box::pin(coordinator_events),
            None,
            "".into(),
            Some(exit_when_done),
        );

        let spawn_result = reply_rx
            .map_err(|err| eyre!("failed to receive spawn result: {err}"))
            .and_then(|r| async {
                match r {
                    DaemonCoordinatorReply::SpawnResult(result) => result.map_err(|err| eyre!(err)),
                    _ => Err(eyre!("unexpected spawn reply")),
                }
            });

        future::try_join(run_result, spawn_result).await?;
        Ok(())
    }

    async fn run_general(
        external_events: impl Stream<Item = Event> + Unpin,
        coordinator_addr: Option<SocketAddr>,
        machine_id: String,
        exit_when_done: Option<BTreeSet<(Uuid, NodeId)>>,
    ) -> eyre::Result<()> {
        // create listener for node connection
        let listener = listener::create_listener().await?;
        let port = listener
            .local_addr()
            .wrap_err("failed to get local addr of listener")?
            .port();
        let new_connections = TcpListenerStream::new(listener).map(|c| {
            c.map(Event::NewConnection)
                .wrap_err("failed to open connection")
                .unwrap_or_else(Event::ConnectError)
        });
        tracing::info!("Listening for node connections on 127.0.0.1:{port}");

        let (dora_events_tx, dora_events_rx) = mpsc::channel(5);
        let daemon = Self {
            port,
            prepared_messages: Default::default(),
            sent_out_shared_memory: Default::default(),
            running: HashMap::new(),
            dora_events_tx,
            coordinator_addr,
            machine_id,
            exit_when_done,
        };
        let dora_events = ReceiverStream::new(dora_events_rx).map(Event::Dora);
        let watchdog_interval = tokio_stream::wrappers::IntervalStream::new(tokio::time::interval(
            Duration::from_secs(5),
        ))
        .map(|_| Event::WatchdogInterval);
        let events = (
            external_events,
            new_connections,
            dora_events,
            watchdog_interval,
        )
            .merge();
        daemon.run_inner(events).await
    }

    async fn run_inner(
        mut self,
        incoming_events: impl Stream<Item = Event> + Unpin,
    ) -> eyre::Result<()> {
        let (node_events_tx, node_events_rx) = mpsc::channel(10);
        let node_events = ReceiverStream::new(node_events_rx);

        let mut events = (incoming_events, node_events).merge();

        while let Some(event) = events.next().await {
            match event {
                Event::NewConnection(connection) => {
                    connection.set_nodelay(true)?;
                    let events_tx = node_events_tx.clone();
                    tokio::spawn(listener::handle_connection(connection, events_tx));
                }
                Event::ConnectError(err) => {
                    tracing::warn!("{:?}", err.wrap_err("failed to connect"));
                }
                Event::Coordinator(CoordinatorEvent { event, reply_tx }) => {
                    let (reply, status) = self.handle_coordinator_event(event).await;
                    let _ = reply_tx.send(reply);
                    match status {
                        RunStatus::Continue => {}
                        RunStatus::Exit => break,
                    }
                }
                Event::Node {
                    dataflow_id: dataflow,
                    node_id,
                    event,
                    reply_sender,
                } => {
                    self.handle_node_event(event, dataflow, node_id, reply_sender)
                        .await?
                }
                Event::Dora(event) => match self.handle_dora_event(event).await? {
                    RunStatus::Continue => {}
                    RunStatus::Exit => break,
                },
                Event::Drop(DropEvent { token }) => {
                    match self.sent_out_shared_memory.remove(&token) {
                        Some(rc) => {
                            if let Ok(_shmem) = Rc::try_unwrap(rc) {
                                tracing::trace!(
                                    "freeing shared memory after receiving last drop token"
                                )
                            }
                        }
                        None => tracing::warn!("received unknown drop token {token:?}"),
                    }
                }
                Event::WatchdogInterval => {
                    if let Some(addr) = self.coordinator_addr {
                        let mut connection = coordinator::send_event(
                            addr,
                            self.machine_id.clone(),
                            DaemonEvent::Watchdog,
                        )
                        .await
                        .wrap_err("lost connection to coordinator")?;
                        let reply_raw = tcp_receive(&mut connection)
                            .await
                            .wrap_err("lost connection to coordinator")?;
                        let _: dora_core::coordinator_messages::WatchdogAck =
                            serde_json::from_slice(&reply_raw)
                                .wrap_err("received unexpected watchdog reply from coordinator")?;
                    }
                }
            }
        }

        Ok(())
    }

    async fn handle_coordinator_event(
        &mut self,
        event: DaemonCoordinatorEvent,
    ) -> (DaemonCoordinatorReply, RunStatus) {
        match event {
            DaemonCoordinatorEvent::Spawn(SpawnDataflowNodes { dataflow_id, nodes }) => {
                let result = self.spawn_dataflow(dataflow_id, nodes).await;
                let reply =
                    DaemonCoordinatorReply::SpawnResult(result.map_err(|err| format!("{err:?}")));
                (reply, RunStatus::Continue)
            }
            DaemonCoordinatorEvent::StopDataflow { dataflow_id } => {
                let stop = async {
                    let dataflow = self
                        .running
                        .get_mut(&dataflow_id)
                        .wrap_err_with(|| format!("no running dataflow with ID `{dataflow_id}`"))?;

                    for (_node_id, channel) in dataflow.subscribe_channels.drain() {
                        let _ = channel.send_async(daemon_messages::NodeEvent::Stop).await;
                    }
                    Result::<(), eyre::Report>::Ok(())
                };
                let reply = DaemonCoordinatorReply::SpawnResult(
                    stop.await.map_err(|err| format!("{err:?}")),
                );
                (reply, RunStatus::Continue)
            }
            DaemonCoordinatorEvent::Destroy => {
                tracing::info!("received destroy command -> exiting");
                let reply = DaemonCoordinatorReply::DestroyResult(Ok(()));
                (reply, RunStatus::Exit)
            }
            DaemonCoordinatorEvent::Watchdog => {
                (DaemonCoordinatorReply::WatchdogAck, RunStatus::Continue)
            }
        }
    }

    async fn spawn_dataflow(
        &mut self,
        dataflow_id: uuid::Uuid,
        nodes: BTreeMap<NodeId, daemon_messages::SpawnNodeParams>,
    ) -> eyre::Result<()> {
        let dataflow = match self.running.entry(dataflow_id) {
            std::collections::hash_map::Entry::Vacant(entry) => entry.insert(Default::default()),
            std::collections::hash_map::Entry::Occupied(_) => {
                bail!("there is already a running dataflow with ID `{dataflow_id}`")
            }
        };
        for (node_id, params) in nodes {
            dataflow.running_nodes.insert(node_id.clone());
            for (input_id, mapping) in params.node.run_config.inputs.clone() {
                dataflow
                    .open_inputs
                    .entry(node_id.clone())
                    .or_default()
                    .insert(input_id.clone());
                match mapping {
                    InputMapping::User(mapping) => {
                        if mapping.operator.is_some() {
                            bail!("operators are not supported");
                        }
                        dataflow
                            .mappings
                            .entry((mapping.source, mapping.output))
                            .or_default()
                            .insert((node_id.clone(), input_id));
                    }
                    InputMapping::Timer { interval } => {
                        dataflow
                            .timers
                            .entry(interval)
                            .or_default()
                            .insert((node_id.clone(), input_id));
                    }
                }
            }

            spawn::spawn_node(dataflow_id, params, self.port, self.dora_events_tx.clone())
                .await
                .wrap_err_with(|| format!("failed to spawn node `{node_id}`"))?;
        }
        for interval in dataflow.timers.keys().copied() {
            let events_tx = self.dora_events_tx.clone();
            let task = async move {
                let mut interval_stream = tokio::time::interval(interval);
                let hlc = HLC::default();
                loop {
                    interval_stream.tick().await;

                    let event = DoraEvent::Timer {
                        dataflow_id,
                        interval,
                        metadata: dora_message::Metadata::from_parameters(
                            hlc.new_timestamp(),
                            Default::default(),
                        ),
                    };
                    if events_tx.send(event).await.is_err() {
                        break;
                    }
                }
            };
            let (task, handle) = task.remote_handle();
            tokio::spawn(task);
            dataflow._timer_handles.push(handle);
        }
        Ok(())
    }

    async fn handle_node_event(
        &mut self,
        event: DaemonNodeEvent,
        dataflow_id: DataflowId,
        node_id: NodeId,
        reply_sender: oneshot::Sender<ControlReply>,
    ) -> eyre::Result<()> {
        match event {
            DaemonNodeEvent::Subscribe { event_sender } => {
                let result = match self.running.get_mut(&dataflow_id) {
                    Some(dataflow) => {
                        dataflow.subscribe_channels.insert(node_id, event_sender);
                        Ok(())
                    }
                    None => Err(format!(
                        "subscribe failed: no running dataflow with ID `{dataflow_id}`"
                    )),
                };
                let _ = reply_sender.send(ControlReply::Result(result));
            }
            DaemonNodeEvent::PrepareOutputMessage {
                output_id,
                metadata,
                data_len,
            } => {
                let memory = if data_len > 0 {
                    Some(
                        ShmemConf::new()
                            .size(data_len)
                            .create()
                            .wrap_err("failed to allocate shared memory")?,
                    )
                } else {
                    None
                };
                let id = memory
                    .as_ref()
                    .map(|m| m.get_os_id().to_owned())
                    .unwrap_or_else(|| Uuid::new_v4().to_string());
                let message = PreparedMessage {
                    output_id,
                    metadata,
                    data: memory.map(|m| (m, data_len)),
                };
                self.prepared_messages.insert(id.clone(), message);

                let reply = ControlReply::PreparedMessage {
                    shared_memory_id: id.clone(),
                };
                if reply_sender.send(reply).is_err() {
                    // free shared memory slice again
                    self.prepared_messages.remove(&id);
                }
            }
            DaemonNodeEvent::SendOutMessage { id } => {
                let message = self
                    .prepared_messages
                    .remove(&id)
                    .ok_or_else(|| eyre!("invalid shared memory id"))?;
                let PreparedMessage {
                    output_id,
                    metadata,
                    data,
                } = message;
                let data = data.map(|(m, len)| (Rc::new(m), len));

                let dataflow = self.running.get_mut(&dataflow_id).wrap_err_with(|| {
                    format!("send out failed: no running dataflow with ID `{dataflow_id}`")
                })?;

                // figure out receivers from dataflow graph
                let empty_set = BTreeSet::new();
                let local_receivers = dataflow
                    .mappings
                    .get(&(node_id, output_id))
                    .unwrap_or(&empty_set);

                // send shared memory ID to all local receivers
                let mut closed = Vec::new();
                for (receiver_id, input_id) in local_receivers {
                    if let Some(channel) = dataflow.subscribe_channels.get(receiver_id) {
                        let drop_token = DropToken::generate();
                        let send_result = channel.send_async(daemon_messages::NodeEvent::Input {
                            id: input_id.clone(),
                            metadata: metadata.clone(),
                            data: data.as_ref().map(|(m, len)| daemon_messages::InputData {
                                shared_memory_id: m.get_os_id().to_owned(),
                                len: *len,
                                drop_token: drop_token.clone(),
                            }),
                        });

                        match timeout(Duration::from_millis(10), send_result).await {
                            Ok(Ok(())) => {
                                // keep shared memory ptr in order to free it once all subscribers are done
                                if let Some((memory, _)) = &data {
                                    self.sent_out_shared_memory
                                        .insert(drop_token, memory.clone());
                                }
                            }
                            Ok(Err(_)) => {
                                closed.push(receiver_id);
                            }
                            Err(_) => {
                                tracing::warn!(
                                    "dropping input event `{receiver_id}/{input_id}` (send timeout)"
                                );
                            }
                        }
                    }
                }
                for id in closed {
                    dataflow.subscribe_channels.remove(id);
                }

                // TODO send `data` via network to all remove receivers
                if let Some((memory, len)) = &data {
                    let data = std::ptr::slice_from_raw_parts(memory.as_ptr(), *len);
                }

                let _ = reply_sender.send(ControlReply::Result(Ok(())));
            }
            DaemonNodeEvent::Stopped => {
                tracing::info!("Stopped: {dataflow_id}/{node_id}");

                let _ = reply_sender.send(ControlReply::Result(Ok(())));

                // notify downstream nodes
                let dataflow = self
                    .running
                    .get_mut(&dataflow_id)
                    .wrap_err_with(|| format!("failed to get downstream nodes: no running dataflow with ID `{dataflow_id}`"))?;
                let downstream_nodes: BTreeSet<_> = dataflow
                    .mappings
                    .iter()
                    .filter(|((source_id, _), _)| source_id == &node_id)
                    .flat_map(|(_, v)| v)
                    .collect();
                for (receiver_id, input_id) in downstream_nodes {
                    let Some(channel) = dataflow.subscribe_channels.get(receiver_id) else {
                        continue;
                    };

                    let _ = channel
                        .send_async(daemon_messages::NodeEvent::InputClosed {
                            id: input_id.clone(),
                        })
                        .await;

                    if let Some(open_inputs) = dataflow.open_inputs.get_mut(receiver_id) {
                        open_inputs.remove(input_id);
                        if open_inputs.is_empty() {
                            // close the subscriber channel
                            dataflow.subscribe_channels.remove(receiver_id);
                        }
                    }
                }

                // TODO: notify remote nodes

                dataflow.running_nodes.remove(&node_id);
                if dataflow.running_nodes.is_empty() {
                    tracing::info!(
                        "Dataflow `{dataflow_id}` finished on machine `{}`",
                        self.machine_id
                    );
                    if let Some(addr) = self.coordinator_addr {
                        if coordinator::send_event(
                            addr,
                            self.machine_id.clone(),
                            DaemonEvent::AllNodesFinished {
                                dataflow_id,
                                result: Ok(()),
                            },
                        )
                        .await
                        .is_err()
                        {
                            tracing::warn!("failed to report dataflow finish to coordinator");
                        }
                    }
                    self.running.remove(&dataflow_id);
                }
            }
        }
        Ok(())
    }

    async fn handle_dora_event(&mut self, event: DoraEvent) -> eyre::Result<RunStatus> {
        match event {
            DoraEvent::Timer {
                dataflow_id,
                interval,
                metadata,
            } => {
                let Some(dataflow) = self.running.get_mut(&dataflow_id) else {
                    tracing::warn!("Timer event for unknown dataflow `{dataflow_id}`");
                    return Ok(RunStatus::Continue);
                };

                let Some(subscribers) = dataflow.timers.get(&interval) else {
                    return Ok(RunStatus::Continue);
                };

                let mut closed = Vec::new();
                for (receiver_id, input_id) in subscribers {
                    let Some(channel) = dataflow.subscribe_channels.get(receiver_id) else {
                        continue;
                    };

                    let send_result = channel.send_async(daemon_messages::NodeEvent::Input {
                        id: input_id.clone(),
                        metadata: metadata.clone(),
                        data: None,
                    });
                    match timeout(Duration::from_millis(1), send_result).await {
                        Ok(Ok(())) => {}
                        Ok(Err(_)) => {
                            closed.push(receiver_id);
                        }
                        Err(_) => {
                            tracing::info!(
                                "dropping timer tick event for `{receiver_id}` (send timeout)"
                            );
                        }
                    }
                }
                for id in closed {
                    dataflow.subscribe_channels.remove(id);
                }
            }
            DoraEvent::SpawnedNodeResult {
                dataflow_id,
                node_id,
                result,
            } => {
                if self
                    .running
                    .get(&dataflow_id)
                    .and_then(|d| d.subscribe_channels.get(&node_id))
                    .is_some()
                {
                    tracing::warn!(
                        "node `{dataflow_id}/{node_id}` finished without sending `Stopped` message"
                    );
                }
                match result {
                    Ok(()) => {
                        tracing::info!("node {dataflow_id}/{node_id} finished successfully");
                    }
                    Err(err) => {
                        let err = err.wrap_err(format!("error in node `{dataflow_id}/{node_id}`"));
                        if self.exit_when_done.is_some() {
                            bail!(err);
                        } else {
                            tracing::error!("{err:?}",);
                        }
                    }
                }

                if let Some(exit_when_done) = &mut self.exit_when_done {
                    exit_when_done.remove(&(dataflow_id, node_id));
                    if exit_when_done.is_empty() {
                        tracing::info!(
                            "exiting daemon because all required dataflows are finished"
                        );
                        return Ok(RunStatus::Exit);
                    }
                }
            }
        }
        Ok(RunStatus::Continue)
    }
}

struct PreparedMessage {
    output_id: DataId,
    metadata: dora_message::Metadata<'static>,
    data: Option<(Shmem, usize)>,
}

#[derive(Default)]
pub struct RunningDataflow {
    subscribe_channels: HashMap<NodeId, flume::Sender<daemon_messages::NodeEvent>>,
    mappings: HashMap<OutputId, BTreeSet<InputId>>,
    timers: BTreeMap<Duration, BTreeSet<InputId>>,
    open_inputs: BTreeMap<NodeId, BTreeSet<DataId>>,
    running_nodes: BTreeSet<NodeId>,
    /// Keep handles to all timer tasks of this dataflow to cancel them on drop.
    _timer_handles: Vec<futures::future::RemoteHandle<()>>,
}

type OutputId = (NodeId, DataId);
type InputId = (NodeId, DataId);

#[derive(Debug)]
pub enum Event {
    NewConnection(TcpStream),
    ConnectError(eyre::Report),
    Node {
        dataflow_id: DataflowId,
        node_id: NodeId,
        event: DaemonNodeEvent,
        reply_sender: oneshot::Sender<ControlReply>,
    },
    Coordinator(CoordinatorEvent),
    Dora(DoraEvent),
    Drop(DropEvent),
    WatchdogInterval,
}

#[derive(Debug)]
pub enum DaemonNodeEvent {
    PrepareOutputMessage {
        output_id: DataId,
        metadata: dora_message::Metadata<'static>,
        data_len: usize,
    },
    SendOutMessage {
        id: MessageId,
    },
    Stopped,
    Subscribe {
        event_sender: flume::Sender<daemon_messages::NodeEvent>,
    },
}

#[derive(Debug)]
pub enum DoraEvent {
    Timer {
        dataflow_id: DataflowId,
        interval: Duration,
        metadata: dora_message::Metadata<'static>,
    },
    SpawnedNodeResult {
        dataflow_id: DataflowId,
        node_id: NodeId,
        result: eyre::Result<()>,
    },
}

type MessageId = String;

#[must_use]
enum RunStatus {
    Continue,
    Exit,
}

pub async fn read_descriptor(file: &Path) -> eyre::Result<Descriptor> {
    let descriptor_file = fs::read(file).await.wrap_err("failed to open given file")?;
    let descriptor: Descriptor =
        serde_yaml::from_slice(&descriptor_file).context("failed to parse given descriptor")?;
    Ok(descriptor)
}
