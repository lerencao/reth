//! Support for handling peer sessions.
pub use crate::message::PeerRequestSender;
use crate::{
    fetch::StatusUpdate,
    message::PeerMessage,
    session::{
        active::ActiveSession,
        handle::{
            ActiveSessionHandle, ActiveSessionMessage, PendingSessionEvent, PendingSessionHandle,
            SessionCommand,
        },
    },
};
use fnv::FnvHashMap;
use futures::{future::Either, io, FutureExt, StreamExt};
use reth_ecies::stream::ECIESStream;
use reth_eth_wire::{
    capability::{Capabilities, CapabilityMessage},
    error::EthStreamError,
    DisconnectReason, HelloBuilder, HelloMessage, Status, StatusBuilder, UnauthedEthStream,
    UnauthedP2PStream,
};
use reth_primitives::{ForkFilter, Hardfork, PeerId};
use secp256k1::{SecretKey, SECP256K1};
use std::{
    collections::HashMap,
    future::Future,
    net::SocketAddr,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::{
    net::TcpStream,
    sync::{mpsc, oneshot},
    task::JoinSet,
};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{instrument, trace, warn};

mod active;
mod config;
mod handle;
use crate::session::config::SessionCounter;
pub use config::SessionsConfig;
use reth_ecies::util::pk2id;

/// Internal identifier for active sessions.
#[derive(Debug, Clone, Copy, PartialOrd, PartialEq, Eq, Hash)]
pub struct SessionId(usize);

/// Manages a set of sessions.
#[must_use = "Session Manager must be polled to process session events."]
pub(crate) struct SessionManager {
    /// Tracks the identifier for the next session.
    next_id: usize,
    /// Keeps track of all sessions
    counter: SessionCounter,
    /// The maximum time we wait for a response from a peer.
    request_timeout: Duration,
    /// The secret key used for authenticating sessions.
    secret_key: SecretKey,
    /// The `Status` message to send to peers.
    status: Status,
    /// THe `Hello` message to send to peers.
    hello: HelloMessage,
    /// The [`ForkFilter`] used to validate the peer's `Status` message.
    fork_filter: ForkFilter,
    /// Size of the command buffer per session.
    session_command_buffer: usize,
    /// All spawned session tasks.
    ///
    /// Note: If dropped, the session tasks are aborted.
    spawned_tasks: JoinSet<()>,
    /// All pending session that are currently handshaking, exchanging `Hello`s.
    ///
    /// Events produced during the authentication phase are reported to this manager. Once the
    /// session is authenticated, it can be moved to the `active_session` set.
    pending_sessions: FnvHashMap<SessionId, PendingSessionHandle>,
    /// All active sessions that are ready to exchange messages.
    active_sessions: HashMap<PeerId, ActiveSessionHandle>,
    /// The original Sender half of the [`PendingSessionEvent`] channel.
    ///
    /// When a new (pending) session is created, the corresponding [`PendingSessionHandle`] will
    /// get a clone of this sender half.
    pending_sessions_tx: mpsc::Sender<PendingSessionEvent>,
    /// Receiver half that listens for [`PendingSessionEvent`] produced by pending sessions.
    pending_session_rx: ReceiverStream<PendingSessionEvent>,
    /// The original Sender half of the [`ActiveSessionEvent`] channel.
    ///
    /// When active session state is reached, the corresponding [`ActiveSessionHandle`] will get a
    /// clone of this sender half.
    active_session_tx: mpsc::Sender<ActiveSessionMessage>,
    /// Receiver half that listens for [`ActiveSessionEvent`] produced by pending sessions.
    active_session_rx: ReceiverStream<ActiveSessionMessage>,
}

// === impl SessionManager ===

impl SessionManager {
    /// Creates a new empty [`SessionManager`].
    pub(crate) fn new(secret_key: SecretKey, config: SessionsConfig) -> Self {
        let (pending_sessions_tx, pending_sessions_rx) = mpsc::channel(config.session_event_buffer);
        let (active_session_tx, active_session_rx) = mpsc::channel(config.session_event_buffer);

        let pk = secret_key.public_key(SECP256K1);
        let peer_id = pk2id(&pk);

        // TODO: make sure this is the right place to put these builders - maybe per-Network rather
        // than per-Session?
        let hello = HelloBuilder::new(peer_id).build();
        let status = StatusBuilder::default().build();
        let fork_filter = Hardfork::Frontier.fork_filter();

        Self {
            next_id: 0,
            counter: SessionCounter::new(config.limits),
            request_timeout: config.request_timeout,
            secret_key,
            status,
            hello,
            fork_filter,
            session_command_buffer: config.session_command_buffer,
            spawned_tasks: Default::default(),
            pending_sessions: Default::default(),
            active_sessions: Default::default(),
            pending_sessions_tx,
            pending_session_rx: ReceiverStream::new(pending_sessions_rx),
            active_session_tx,
            active_session_rx: ReceiverStream::new(active_session_rx),
        }
    }

    /// Returns the next unique [`SessionId`].
    fn next_id(&mut self) -> SessionId {
        let id = self.next_id;
        self.next_id += 1;
        SessionId(id)
    }

    /// Spawns the given future onto a new task that is tracked in the `spawned_tasks` [`JoinSet`].
    fn spawn<F>(&mut self, f: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.spawned_tasks.spawn(async move { f.await });
    }

    /// Invoked on a received status update
    pub(crate) fn on_status_update(&mut self, status: StatusUpdate) {
        self.status.blockhash = status.hash;
        self.status.total_difficulty = status.total_difficulty;
        self.fork_filter.set_head(status.height);
    }

    /// An incoming TCP connection was received. This starts the authentication process to turn this
    /// stream into an active peer session.
    ///
    /// Returns an error if the configured limit has been reached.
    pub(crate) fn on_incoming(
        &mut self,
        stream: TcpStream,
        remote_addr: SocketAddr,
    ) -> Result<SessionId, ExceedsSessionLimit> {
        self.counter.ensure_pending_inbound()?;

        let session_id = self.next_id();
        let (disconnect_tx, disconnect_rx) = oneshot::channel();
        let pending_events = self.pending_sessions_tx.clone();
        self.spawn(start_pending_incoming_session(
            disconnect_rx,
            session_id,
            stream,
            pending_events,
            remote_addr,
            self.secret_key,
            self.hello.clone(),
            self.status,
            self.fork_filter.clone(),
        ));

        let handle =
            PendingSessionHandle { _disconnect_tx: disconnect_tx, direction: Direction::Incoming };
        self.pending_sessions.insert(session_id, handle);
        self.counter.inc_pending_inbound();
        Ok(session_id)
    }

    /// Starts a new pending session from the local node to the given remote node.
    pub(crate) fn dial_outbound(&mut self, remote_addr: SocketAddr, remote_peer_id: PeerId) {
        let session_id = self.next_id();
        let (disconnect_tx, disconnect_rx) = oneshot::channel();
        let pending_events = self.pending_sessions_tx.clone();
        self.spawn(start_pending_outbound_session(
            disconnect_rx,
            pending_events,
            session_id,
            remote_addr,
            remote_peer_id,
            self.secret_key,
            self.hello.clone(),
            self.status,
            self.fork_filter.clone(),
        ));

        let handle = PendingSessionHandle {
            _disconnect_tx: disconnect_tx,
            direction: Direction::Outgoing(remote_peer_id),
        };
        self.pending_sessions.insert(session_id, handle);
        self.counter.inc_pending_outbound();
    }

    /// Initiates a shutdown of the channel.
    ///
    /// This will trigger the disconnect on the session task to gracefully terminate. The result
    /// will be picked up by the receiver.
    pub(crate) fn disconnect(&self, node: PeerId, reason: Option<DisconnectReason>) {
        if let Some(session) = self.active_sessions.get(&node) {
            session.disconnect(reason);
        }
    }

    /// Sends a message to the peer's session
    pub(crate) fn send_message(&mut self, peer_id: &PeerId, msg: PeerMessage) {
        if let Some(session) = self.active_sessions.get_mut(peer_id) {
            let _ = session.commands_to_session.try_send(SessionCommand::Message(msg));
        }
    }

    /// Removes the [`PendingSessionHandle`] if it exists.
    fn remove_pending_session(&mut self, id: &SessionId) -> Option<PendingSessionHandle> {
        let session = self.pending_sessions.remove(id)?;
        self.counter.dec_pending(&session.direction);
        Some(session)
    }

    /// Removes the [`PendingSessionHandle`] if it exists.
    fn remove_active_session(&mut self, id: &PeerId) -> Option<ActiveSessionHandle> {
        let session = self.active_sessions.remove(id)?;
        self.counter.dec_active(&session.direction);
        Some(session)
    }

    /// This polls all the session handles and returns [`SessionEvent`].
    ///
    /// Active sessions are prioritized.
    pub(crate) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<SessionEvent> {
        // Poll events from active sessions
        match self.active_session_rx.poll_next_unpin(cx) {
            Poll::Pending => {}
            Poll::Ready(None) => {
                unreachable!("Manager holds both channel halves.")
            }
            Poll::Ready(Some(event)) => {
                return match event {
                    ActiveSessionMessage::Disconnected { peer_id, remote_addr } => {
                        trace!(
                            target : "net::session",
                            ?peer_id,
                            "gracefully disconnected active session."
                        );
                        self.remove_active_session(&peer_id);
                        Poll::Ready(SessionEvent::Disconnected { peer_id, remote_addr })
                    }
                    ActiveSessionMessage::ClosedOnConnectionError {
                        peer_id,
                        remote_addr,
                        error,
                    } => {
                        trace!(target : "net::session",  ?peer_id, ?error,"closed session.");
                        self.remove_active_session(&peer_id);
                        Poll::Ready(SessionEvent::SessionClosedOnConnectionError {
                            remote_addr,
                            peer_id,
                            error,
                        })
                    }
                    ActiveSessionMessage::ValidMessage { peer_id, message } => {
                        Poll::Ready(SessionEvent::ValidMessage { peer_id, message })
                    }
                    ActiveSessionMessage::InvalidMessage { peer_id, capabilities, message } => {
                        Poll::Ready(SessionEvent::InvalidMessage { peer_id, message, capabilities })
                    }
                    ActiveSessionMessage::BadMessage { peer_id } => {
                        Poll::Ready(SessionEvent::BadMessage { peer_id })
                    }
                }
            }
        }

        // Poll the pending session event stream
        let event = match self.pending_session_rx.poll_next_unpin(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(None) => unreachable!("Manager holds both channel halves."),
            Poll::Ready(Some(event)) => event,
        };
        return match event {
            PendingSessionEvent::Established {
                session_id,
                remote_addr,
                peer_id,
                capabilities,
                conn,
                status,
                direction,
            } => {
                // move from pending to established.
                self.remove_pending_session(&session_id);

                let (commands_to_session, commands_rx) = mpsc::channel(self.session_command_buffer);

                let (to_session_tx, messages_rx) = mpsc::channel(self.session_command_buffer);

                let messages = PeerRequestSender { peer_id, to_session_tx };

                let session = ActiveSession {
                    next_id: 0,
                    remote_peer_id: peer_id,
                    remote_addr,
                    remote_capabilities: Arc::clone(&capabilities),
                    session_id,
                    commands_rx: ReceiverStream::new(commands_rx),
                    to_session: self.active_session_tx.clone(),
                    request_tx: ReceiverStream::new(messages_rx).fuse(),
                    inflight_requests: Default::default(),
                    conn,
                    queued_outgoing: Default::default(),
                    received_requests: Default::default(),
                    timeout_interval: tokio::time::interval(self.request_timeout),
                    request_timeout: self.request_timeout,
                };

                self.spawn(session);

                let handle = ActiveSessionHandle {
                    direction,
                    session_id,
                    remote_id: peer_id,
                    established: Instant::now(),
                    capabilities: Arc::clone(&capabilities),
                    commands_to_session,
                };

                self.active_sessions.insert(peer_id, handle);
                self.counter.inc_active(&direction);

                Poll::Ready(SessionEvent::SessionEstablished {
                    peer_id,
                    remote_addr,
                    capabilities,
                    status,
                    messages,
                    direction,
                })
            }
            PendingSessionEvent::Disconnected { remote_addr, session_id, direction, error } => {
                trace!(
                    target : "net::session",
                    ?session_id,
                    ?remote_addr,
                    ?error,
                    "disconnected pending session"
                );
                self.remove_pending_session(&session_id);
                match direction {
                    Direction::Incoming => {
                        Poll::Ready(SessionEvent::IncomingPendingSessionClosed {
                            remote_addr,
                            error,
                        })
                    }
                    Direction::Outgoing(peer_id) => {
                        Poll::Ready(SessionEvent::OutgoingPendingSessionClosed {
                            remote_addr,
                            peer_id,
                            error,
                        })
                    }
                }
            }
            PendingSessionEvent::OutgoingConnectionError {
                remote_addr,
                session_id,
                peer_id,
                error,
            } => {
                trace!(
                    target : "net::session",
                    ?error,
                    ?session_id,
                    ?remote_addr,
                    ?peer_id,
                    "connection refused"
                );
                self.remove_pending_session(&session_id);
                Poll::Ready(SessionEvent::OutgoingConnectionError { remote_addr, peer_id, error })
            }
            PendingSessionEvent::EciesAuthError { remote_addr, session_id, error, direction } => {
                self.remove_pending_session(&session_id);
                warn!(
                    target : "net::session",
                    ?error,
                    ?session_id,
                    ?remote_addr,
                    "ecies auth failed"
                );
                self.remove_pending_session(&session_id);
                match direction {
                    Direction::Incoming => {
                        Poll::Ready(SessionEvent::IncomingPendingSessionClosed {
                            remote_addr,
                            error: None,
                        })
                    }
                    Direction::Outgoing(peer_id) => {
                        Poll::Ready(SessionEvent::OutgoingPendingSessionClosed {
                            remote_addr,
                            peer_id,
                            error: None,
                        })
                    }
                }
            }
        }
    }
}

/// Events produced by the [`SessionManager`]
pub(crate) enum SessionEvent {
    /// A new session was successfully authenticated.
    ///
    /// This session is now able to exchange data.
    SessionEstablished {
        peer_id: PeerId,
        remote_addr: SocketAddr,
        capabilities: Arc<Capabilities>,
        status: Status,
        messages: PeerRequestSender,
        direction: Direction,
    },
    /// A session received a valid message via RLPx.
    ValidMessage {
        peer_id: PeerId,
        /// Message received from the peer.
        message: PeerMessage,
    },
    /// Received a message that does not match the announced capabilities of the peer.
    InvalidMessage {
        peer_id: PeerId,
        /// Announced capabilities of the remote peer.
        capabilities: Arc<Capabilities>,
        /// Message received from the peer.
        message: CapabilityMessage,
    },
    /// Received a bad message from the peer.
    BadMessage {
        /// Identifier of the remote peer.
        peer_id: PeerId,
    },
    /// Closed an incoming pending session during authentication.
    IncomingPendingSessionClosed { remote_addr: SocketAddr, error: Option<EthStreamError> },
    /// Closed an outgoing pending session during authentication.
    OutgoingPendingSessionClosed {
        remote_addr: SocketAddr,
        peer_id: PeerId,
        error: Option<EthStreamError>,
    },
    /// Failed to establish a tcp stream
    OutgoingConnectionError { remote_addr: SocketAddr, peer_id: PeerId, error: io::Error },
    /// Session was closed due to an error
    SessionClosedOnConnectionError {
        /// The id of the remote peer.
        peer_id: PeerId,
        /// The socket we were connected to.
        remote_addr: SocketAddr,
        /// The error that caused the session to close
        error: EthStreamError,
    },
    /// Active session was gracefully disconnected.
    Disconnected { peer_id: PeerId, remote_addr: SocketAddr },
}

/// The direction of the connection.
#[derive(Debug, Copy, Clone)]
pub(crate) enum Direction {
    /// Incoming connection.
    Incoming,
    /// Outgoing connection to a specific node.
    Outgoing(PeerId),
}

impl Direction {
    /// Returns `true` if this an incoming connection.
    pub(crate) fn is_incoming(&self) -> bool {
        matches!(self, Direction::Incoming)
    }
}

/// The error thrown when the max configured limit has been reached and no more connections are
/// accepted.
#[derive(Debug, Clone, thiserror::Error)]
#[error("Session limit reached {0}")]
pub struct ExceedsSessionLimit(pub(crate) u32);

/// Starts the authentication process for a connection initiated by a remote peer.
///
/// This will wait for the _incoming_ handshake request and answer it.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn start_pending_incoming_session(
    disconnect_rx: oneshot::Receiver<()>,
    session_id: SessionId,
    stream: TcpStream,
    events: mpsc::Sender<PendingSessionEvent>,
    remote_addr: SocketAddr,
    secret_key: SecretKey,
    hello: HelloMessage,
    status: Status,
    fork_filter: ForkFilter,
) {
    authenticate(
        disconnect_rx,
        events,
        stream,
        session_id,
        remote_addr,
        secret_key,
        Direction::Incoming,
        hello,
        status,
        fork_filter,
    )
    .await
}

/// Starts the authentication process for a connection initiated by a remote peer.
#[instrument(skip_all, fields(%remote_addr, peer_id), target = "net")]
#[allow(clippy::too_many_arguments)]
async fn start_pending_outbound_session(
    disconnect_rx: oneshot::Receiver<()>,
    events: mpsc::Sender<PendingSessionEvent>,
    session_id: SessionId,
    remote_addr: SocketAddr,
    remote_peer_id: PeerId,
    secret_key: SecretKey,
    hello: HelloMessage,
    status: Status,
    fork_filter: ForkFilter,
) {
    let stream = match TcpStream::connect(remote_addr).await {
        Ok(stream) => stream,
        Err(error) => {
            let _ = events
                .send(PendingSessionEvent::OutgoingConnectionError {
                    remote_addr,
                    session_id,
                    peer_id: remote_peer_id,
                    error,
                })
                .await;
            return
        }
    };
    authenticate(
        disconnect_rx,
        events,
        stream,
        session_id,
        remote_addr,
        secret_key,
        Direction::Outgoing(remote_peer_id),
        hello,
        status,
        fork_filter,
    )
    .await
}

/// Authenticates a session
#[allow(clippy::too_many_arguments)]
async fn authenticate(
    disconnect_rx: oneshot::Receiver<()>,
    events: mpsc::Sender<PendingSessionEvent>,
    stream: TcpStream,
    session_id: SessionId,
    remote_addr: SocketAddr,
    secret_key: SecretKey,
    direction: Direction,
    hello: HelloMessage,
    status: Status,
    fork_filter: ForkFilter,
) {
    let stream = match direction {
        Direction::Incoming => match ECIESStream::incoming(stream, secret_key).await {
            Ok(stream) => stream,
            Err(error) => {
                let _ = events
                    .send(PendingSessionEvent::EciesAuthError {
                        remote_addr,
                        session_id,
                        error,
                        direction,
                    })
                    .await;
                return
            }
        },
        Direction::Outgoing(remote_peer_id) => {
            match ECIESStream::connect(stream, secret_key, remote_peer_id).await {
                Ok(stream) => stream,
                Err(error) => {
                    let _ = events
                        .send(PendingSessionEvent::EciesAuthError {
                            remote_addr,
                            session_id,
                            error,
                            direction,
                        })
                        .await;
                    return
                }
            }
        }
    };

    let unauthed = UnauthedP2PStream::new(stream);
    let auth = authenticate_stream(
        unauthed,
        session_id,
        remote_addr,
        direction,
        hello,
        status,
        fork_filter,
    )
    .boxed();

    match futures::future::select(disconnect_rx, auth).await {
        Either::Left((_, _)) => {
            let _ = events
                .send(PendingSessionEvent::Disconnected {
                    remote_addr,
                    session_id,
                    direction,
                    error: None,
                })
                .await;
        }
        Either::Right((res, _)) => {
            let _ = events.send(res).await;
        }
    }
}

/// Authenticate the stream via handshake
///
/// On Success return the authenticated stream as [`PendingSessionEvent`]
#[allow(clippy::too_many_arguments)]
async fn authenticate_stream(
    stream: UnauthedP2PStream<ECIESStream<TcpStream>>,
    session_id: SessionId,
    remote_addr: SocketAddr,
    direction: Direction,
    hello: HelloMessage,
    status: Status,
    fork_filter: ForkFilter,
) -> PendingSessionEvent {
    // conduct the p2p handshake and return the authenticated stream
    let (p2p_stream, their_hello) = match stream.handshake(hello).await {
        Ok(stream_res) => stream_res,
        Err(err) => {
            return PendingSessionEvent::Disconnected {
                remote_addr,
                session_id,
                direction,
                error: Some(err.into()),
            }
        }
    };

    // if the hello handshake was successful we can try status handshake
    let eth_unauthed = UnauthedEthStream::new(p2p_stream);
    let (eth_stream, their_status) = match eth_unauthed.handshake(status, fork_filter).await {
        Ok(stream_res) => stream_res,
        Err(err) => {
            return PendingSessionEvent::Disconnected {
                remote_addr,
                session_id,
                direction,
                error: Some(err),
            }
        }
    };

    PendingSessionEvent::Established {
        session_id,
        remote_addr,
        peer_id: their_hello.id,
        capabilities: Arc::new(Capabilities::from(their_hello.capabilities)),
        status: their_status,
        conn: eth_stream,
        direction,
    }
}
