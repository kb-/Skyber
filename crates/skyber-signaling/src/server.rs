//! TLS WebSocket signaling endpoint for one-to-one call setup.

use std::{
    collections::HashMap,
    io,
    sync::Arc,
    time::{Duration, Instant},
};

use ed25519_dalek::{Signature, Verifier};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize, Serializer, de::Deserializer};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Mutex, Semaphore, mpsc, oneshot, watch},
    task::JoinSet,
    time::timeout,
};
use tokio_rustls::{TlsAcceptor, server::TlsStream};
use tokio_tungstenite::{WebSocketStream, accept_hdr_async_with_config, tungstenite::Message};
use zeroize::Zeroize;

use crate::{
    InvitationError, InvitationRegistry, PeerIdentity, RoomId, random_token, relay_message,
    resume_message,
};

const MAX_CONNECTIONS: usize = 512;
const MAX_ACTIVE_ROOMS: usize = 8192;
const OUTBOUND_CAPACITY: usize = 64;
const MAX_WS_MESSAGE: usize = 8 * 1024;
const MAX_CANDIDATE_LENGTH: usize = 1024;
const MAX_UFRAG_LENGTH: usize = 32;
const MAX_ICE_PASSWORD_LENGTH: usize = 256;
const MAX_RELAY_PACKET_SIZE: usize = 1200;
const MAX_RELAY_BYTES_PER_SECOND: usize = 8 * 1024 * 1024;
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const RELAY_IDLE_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const RELAY_JOIN_TIMEOUT: Duration = Duration::from_secs(15);
const RECONNECT_GRACE_PERIOD: Duration = Duration::from_secs(10 * 60);

/// Typed client commands. Signature verification happens before a connection
/// becomes associated with an identity or room.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ClientMessage {
    CreateInvitation {
        inviter: [u8; 32],
        invitee: [u8; 32],
        nonce: [u8; 16],
        signature: Vec<u8>,
    },
    AcceptInvitation {
        room: [u8; 16],
        invitee: [u8; 32],
        token: WireInvitationToken,
        signature: Vec<u8>,
    },
    ResumeRoom {
        room: [u8; 16],
        peer: [u8; 32],
        token: WireInvitationToken,
        signature: Vec<u8>,
    },
    RelayJoin {
        room: [u8; 16],
        peer: [u8; 32],
        token: WireInvitationToken,
        signature: Vec<u8>,
    },
    IceCredentials {
        room: [u8; 16],
        ufrag: WireSecretString,
        password: WireSecretString,
    },
    Candidate {
        room: [u8; 16],
        candidate: String,
    },
    CandidatesComplete {
        room: [u8; 16],
    },
    Hangup {
        room: [u8; 16],
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ServerMessage {
    InvitationCreated {
        room: [u8; 16],
        token: WireInvitationToken,
        expires_in_seconds: u16,
    },
    InvitationAccepted {
        room: [u8; 16],
        remote_peer: [u8; 32],
        resume_token: WireInvitationToken,
        relay_token: WireInvitationToken,
    },
    PeerDisconnected {
        room: [u8; 16],
    },
    PeerResumed {
        room: [u8; 16],
        remote_peer: [u8; 32],
    },
    RelayReady {
        room: [u8; 16],
    },
    IceCredentials {
        room: [u8; 16],
        from_peer: [u8; 32],
        ufrag: WireSecretString,
        password: WireSecretString,
    },
    Candidate {
        room: [u8; 16],
        from_peer: [u8; 32],
        candidate: String,
    },
    CandidatesComplete {
        room: [u8; 16],
        from_peer: [u8; 32],
    },
    PeerLeft {
        room: [u8; 16],
    },
    Error {
        code: String,
    },
}

pub(crate) struct WireInvitationToken(pub(crate) [u8; 32]);

impl std::fmt::Debug for WireInvitationToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("[redacted]")
    }
}

impl Serialize for WireInvitationToken {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for WireInvitationToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        <[u8; 32]>::deserialize(deserializer).map(Self)
    }
}

impl Drop for WireInvitationToken {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

pub(crate) struct WireSecretString(pub(crate) String);

impl Serialize for WireSecretString {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for WireSecretString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self)
    }
}

impl std::fmt::Debug for WireSecretString {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("[redacted]")
    }
}

impl Drop for WireSecretString {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[derive(Default)]
struct SignalState {
    invitations: InvitationRegistry,
    waiting: HashMap<RoomId, WaitingPeer>,
    active: HashMap<RoomId, ActiveRoom>,
}

#[derive(Clone)]
struct WaitingPeer {
    identity: PeerIdentity,
    outbound: mpsc::Sender<ServerMessage>,
}

#[derive(Clone)]
struct ActiveRoom {
    inviter: PeerIdentity,
    invitee: PeerIdentity,
    inviter_outbound: Option<mpsc::Sender<ServerMessage>>,
    invitee_outbound: Option<mpsc::Sender<ServerMessage>>,
    inviter_resume_hash: [u8; 32],
    invitee_resume_hash: [u8; 32],
    inviter_relay_hash: [u8; 32],
    invitee_relay_hash: [u8; 32],
    inviter_relay: Option<mpsc::Sender<RelayCommand>>,
    invitee_relay: Option<mpsc::Sender<RelayCommand>>,
    relay_ready: watch::Sender<bool>,
    inviter_relay_quota: RelayQuota,
    invitee_relay_quota: RelayQuota,
    disconnected_until: Option<Instant>,
}

#[derive(Clone)]
enum RelayCommand {
    Data(Vec<u8>),
    Close,
}

#[derive(Clone)]
struct RelayQuota {
    window_start: Instant,
    bytes: usize,
}

impl RelayQuota {
    fn new() -> Self {
        Self {
            window_start: Instant::now(),
            bytes: 0,
        }
    }

    fn charge(&mut self, size: usize, now: Instant) -> Result<(), InvitationError> {
        if now.saturating_duration_since(self.window_start) >= Duration::from_secs(1) {
            self.window_start = now;
            self.bytes = 0;
        }
        let Some(total) = self.bytes.checked_add(size) else {
            return Err(InvitationError::CapacityExceeded);
        };
        if total > MAX_RELAY_BYTES_PER_SECOND {
            return Err(InvitationError::CapacityExceeded);
        }
        self.bytes = total;
        Ok(())
    }
}

impl Drop for ActiveRoom {
    fn drop(&mut self) {
        self.inviter_resume_hash.zeroize();
        self.invitee_resume_hash.zeroize();
        self.inviter_relay_hash.zeroize();
        self.invitee_relay_hash.zeroize();
    }
}

#[derive(Default)]
struct ConnectionState {
    identity: Option<PeerIdentity>,
    room: Option<RoomId>,
}

/// Run a TLS WebSocket signaling service. Each connection and room queue is
/// bounded; TLS is terminated here so invitation and ICE credentials are
/// protected in transit.
pub async fn serve(listener: TcpListener, tls: TlsAcceptor) -> io::Result<()> {
    let shared = Arc::new(Mutex::new(SignalState::default()));
    let active_connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut tasks = JoinSet::new();

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (tcp, _) = accepted?;
                let Ok(permit) = Arc::clone(&active_connections).try_acquire_owned() else {
                    continue;
                };
                let shared = Arc::clone(&shared);
                let tls = tls.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    if let Ok(Ok(stream)) = timeout(TLS_HANDSHAKE_TIMEOUT, tls.accept(tcp)).await {
                        handle_tls_stream(stream, shared).await;
                    }
                });
            }
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
        }
    }
}

/// Build a TLS acceptor from a PEM certificate chain and private key.
pub fn tls_acceptor(cert_pem: &[u8], key_pem: &[u8]) -> io::Result<TlsAcceptor> {
    let mut cert_reader = io::BufReader::new(cert_pem);
    let certificates = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(io::Error::other)?;
    if certificates.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "certificate chain is empty",
        ));
    }
    let mut key_reader = io::BufReader::new(key_pem);
    let private_key = rustls_pemfile::private_key(&mut key_reader)
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "private key is missing"))?;
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, private_key)
        .map_err(io::Error::other)?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

async fn handle_tls_stream(stream: TlsStream<TcpStream>, shared: Arc<Mutex<SignalState>>) {
    let mut config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
    config.max_message_size = Some(MAX_WS_MESSAGE);
    config.max_frame_size = Some(MAX_WS_MESSAGE);
    let (route_tx, route_rx) = oneshot::channel();
    #[allow(clippy::result_large_err)]
    // Callback signature is fixed by tungstenite's handshake API.
    let callback =
        move |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
              response: tokio_tungstenite::tungstenite::handshake::server::Response| {
            let _ = route_tx.send(request.uri().path().to_owned());
            Ok(response)
        };
    let Ok(websocket) = accept_hdr_async_with_config(stream, callback, Some(config)).await else {
        return;
    };
    match route_rx.await.as_deref() {
        Ok("/signal") => serve_connection(websocket, shared).await,
        Ok("/relay") => serve_relay_connection(websocket, shared).await,
        _ => {
            let (mut sink, _) = websocket.split();
            let _ = sink.close().await;
        }
    }
}

async fn serve_relay_connection(
    websocket: WebSocketStream<TlsStream<TcpStream>>,
    shared: Arc<Mutex<SignalState>>,
) {
    let (mut sink, mut stream) = websocket.split();
    let join_frame = match timeout(Duration::from_secs(5), stream.next()).await {
        Ok(Some(Ok(Message::Text(frame)))) if frame.len() <= MAX_WS_MESSAGE => frame,
        _ => return,
    };
    let join = match serde_json::from_str::<ClientMessage>(&join_frame) {
        Ok(ClientMessage::RelayJoin {
            room,
            peer,
            token,
            signature,
        }) => {
            let room = RoomId::from_bytes(room);
            let peer = match PeerIdentity::from_bytes(peer) {
                Ok(peer) => peer,
                Err(_) => return,
            };
            let signature = match Signature::from_slice(&signature) {
                Ok(signature) => signature,
                Err(_) => return,
            };
            (room, peer, token, signature)
        }
        _ => return,
    };
    let (room, peer, token, signature) = join;
    let (relay_tx, mut relay_rx) = mpsc::channel::<RelayCommand>(OUTBOUND_CAPACITY);
    if register_relay(room, peer, &token.0, &signature, relay_tx, &shared)
        .await
        .is_err()
    {
        return;
    }
    if wait_for_relay_peer(room, &shared).await.is_err() {
        cleanup_relay(room, peer, &shared).await;
        return;
    }
    let ready = match serde_json::to_string(&ServerMessage::RelayReady {
        room: *room.as_bytes(),
    }) {
        Ok(ready) => ready,
        Err(_) => return,
    };
    if sink.send(Message::Text(ready.into())).await.is_err() {
        cleanup_relay(room, peer, &shared).await;
        return;
    }

    loop {
        tokio::select! {
            command = relay_rx.recv() => match command {
                Some(RelayCommand::Data(packet)) => {
                    if sink.send(Message::Binary(packet.into())).await.is_err() { break; }
                }
                Some(RelayCommand::Close) | None => break,
            },
            frame = timeout(RELAY_IDLE_TIMEOUT, stream.next()) => match frame {
                Err(_) => break,
                Ok(Some(Ok(Message::Binary(packet)))) if !packet.is_empty() && packet.len() <= MAX_RELAY_PACKET_SIZE => {
                    if route_relay_packet(room, peer, packet.to_vec(), &shared).await.is_err() { break; }
                }
                Ok(Some(Ok(Message::Ping(_)))) | Ok(Some(Ok(Message::Pong(_)))) => {}
                _ => break,
            }
        }
    }
    cleanup_relay(room, peer, &shared).await;
}

async fn register_relay(
    room: RoomId,
    peer: PeerIdentity,
    token: &[u8; 32],
    signature: &Signature,
    relay: mpsc::Sender<RelayCommand>,
    shared: &Arc<Mutex<SignalState>>,
) -> Result<(), InvitationError> {
    peer.verifying_key()?
        .verify(&relay_message(room, peer, token), signature)
        .map_err(|_| InvitationError::InvalidSignature)?;
    let mut state = shared.lock().await;
    let Some(active) = state.active.get_mut(&room) else {
        return Err(InvitationError::NotFound);
    };
    if active
        .disconnected_until
        .is_some_and(|until| until <= Instant::now())
    {
        state.active.remove(&room);
        return Err(InvitationError::Expired);
    }
    let (expected_hash, slot) = if active.inviter == peer {
        (&active.inviter_relay_hash, &mut active.inviter_relay)
    } else if active.invitee == peer {
        (&active.invitee_relay_hash, &mut active.invitee_relay)
    } else {
        return Err(InvitationError::WrongPeer);
    };
    if !bool::from(expected_hash.ct_eq(&hash_token(token))) {
        return Err(InvitationError::InvalidSignature);
    }
    if slot.is_some() {
        return Err(InvitationError::WrongPeer);
    }
    *slot = Some(relay);
    if active.inviter_relay.is_some() && active.invitee_relay.is_some() {
        active.relay_ready.send_replace(true);
    }
    Ok(())
}

async fn wait_for_relay_peer(
    room: RoomId,
    shared: &Arc<Mutex<SignalState>>,
) -> Result<(), InvitationError> {
    let mut ready = {
        let state = shared.lock().await;
        state
            .active
            .get(&room)
            .ok_or(InvitationError::NotFound)?
            .relay_ready
            .subscribe()
    };
    timeout(RELAY_JOIN_TIMEOUT, async {
        loop {
            if *ready.borrow() {
                return Ok(());
            }
            ready
                .changed()
                .await
                .map_err(|_| InvitationError::NotFound)?;
        }
    })
    .await
    .map_err(|_| InvitationError::Expired)?
}

async fn route_relay_packet(
    room: RoomId,
    peer: PeerIdentity,
    packet: Vec<u8>,
    shared: &Arc<Mutex<SignalState>>,
) -> Result<(), InvitationError> {
    if packet.is_empty() || packet.len() > MAX_RELAY_PACKET_SIZE {
        return Err(InvitationError::CapacityExceeded);
    }
    let target = {
        let mut state = shared.lock().await;
        let Some(active) = state.active.get_mut(&room) else {
            return Err(InvitationError::NotFound);
        };
        let (target, quota) = if active.inviter == peer {
            (&active.invitee_relay, &mut active.inviter_relay_quota)
        } else if active.invitee == peer {
            (&active.inviter_relay, &mut active.invitee_relay_quota)
        } else {
            return Err(InvitationError::WrongPeer);
        };
        quota.charge(packet.len(), Instant::now())?;
        target.clone().ok_or(InvitationError::NotFound)?
    };
    if matches!(
        timeout(
            Duration::from_secs(5),
            target.send(RelayCommand::Data(packet))
        )
        .await,
        Ok(Ok(()))
    ) {
        Ok(())
    } else {
        Err(InvitationError::CapacityExceeded)
    }
}

async fn cleanup_relay(room: RoomId, peer: PeerIdentity, shared: &Arc<Mutex<SignalState>>) {
    let remote = {
        let mut state = shared.lock().await;
        let Some(active) = state.active.get_mut(&room) else {
            return;
        };
        if active.inviter == peer {
            active.inviter_relay = None;
            active.relay_ready.send_replace(false);
            active.invitee_relay.clone()
        } else if active.invitee == peer {
            active.invitee_relay = None;
            active.relay_ready.send_replace(false);
            active.inviter_relay.clone()
        } else {
            None
        }
    };
    if let Some(remote) = remote {
        let _ = remote.try_send(RelayCommand::Close);
    }
}

async fn serve_connection(
    websocket: WebSocketStream<TlsStream<TcpStream>>,
    shared: Arc<Mutex<SignalState>>,
) {
    let (mut sink, mut stream) = websocket.split();
    let (outbound, mut outbound_rx) = mpsc::channel::<ServerMessage>(OUTBOUND_CAPACITY);
    let writer = tokio::spawn(async move {
        while let Some(message) = outbound_rx.recv().await {
            let Ok(text) = serde_json::to_string(&message) else {
                break;
            };
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let mut connection = ConnectionState::default();
    while let Some(frame) = stream.next().await {
        match frame {
            Ok(Message::Text(text)) if text.len() <= MAX_WS_MESSAGE => {
                let request = match serde_json::from_str::<ClientMessage>(&text) {
                    Ok(request) => request,
                    Err(_) => {
                        let _ = outbound.try_send(ServerMessage::Error {
                            code: "invalid_message".to_owned(),
                        });
                        break;
                    }
                };
                if handle_message(request, &mut connection, &outbound, &shared)
                    .await
                    .is_err()
                {
                    let _ = outbound.try_send(ServerMessage::Error {
                        code: "request_rejected".to_owned(),
                    });
                    break;
                }
            }
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
            Ok(Message::Close(_)) | Err(_) | Ok(_) => break,
        }
    }

    cleanup_connection(&connection, &shared).await;
    drop(outbound);
    let _ = writer.await;
}

async fn handle_message(
    message: ClientMessage,
    connection: &mut ConnectionState,
    outbound: &mpsc::Sender<ServerMessage>,
    shared: &Arc<Mutex<SignalState>>,
) -> Result<(), InvitationError> {
    match message {
        ClientMessage::CreateInvitation {
            inviter,
            invitee,
            nonce,
            signature,
        } => {
            if connection.room.is_some() || connection.identity.is_some() {
                return Err(InvitationError::NotFound);
            }
            let response = outbound
                .try_reserve()
                .map_err(|_| InvitationError::CapacityExceeded)?;
            let inviter = PeerIdentity::from_bytes(inviter)?;
            let invitee = PeerIdentity::from_bytes(invitee)?;
            let signature =
                Signature::from_slice(&signature).map_err(|_| InvitationError::InvalidSignature)?;
            let mut state = shared.lock().await;
            let invitation = state
                .invitations
                .create_invitation(inviter, invitee, nonce, &signature)?;
            let room = invitation.room;
            state.waiting.insert(
                room,
                WaitingPeer {
                    identity: inviter,
                    outbound: outbound.clone(),
                },
            );
            let expiration_state = Arc::clone(shared);
            let expiration_notice = outbound.clone();
            let expiration_delay = invitation.expires_in;
            tokio::spawn(async move {
                tokio::time::sleep(expiration_delay).await;
                let mut state = expiration_state.lock().await;
                if state.waiting.remove(&room).is_some() {
                    state.invitations.cancel_invitation(room, inviter);
                    let _ = expiration_notice.try_send(ServerMessage::Error {
                        code: "invitation_expired".to_owned(),
                    });
                }
            });
            connection.identity = Some(inviter);
            connection.room = Some(room);
            response.send(ServerMessage::InvitationCreated {
                room: *room.as_bytes(),
                token: WireInvitationToken(*invitation.token.expose()),
                expires_in_seconds: u16::try_from(invitation.expires_in.as_secs())
                    .unwrap_or(u16::MAX),
            });
            Ok(())
        }
        ClientMessage::AcceptInvitation {
            room,
            invitee,
            token,
            signature,
        } => {
            if connection.room.is_some() || connection.identity.is_some() {
                return Err(InvitationError::NotFound);
            }
            let room = RoomId::from_bytes(room);
            let invitee = PeerIdentity::from_bytes(invitee)?;
            let signature =
                Signature::from_slice(&signature).map_err(|_| InvitationError::InvalidSignature)?;
            let mut state = shared.lock().await;
            let Some(waiting) = state.waiting.get(&room).cloned() else {
                return Err(InvitationError::NotFound);
            };
            if state.active.len() >= MAX_ACTIVE_ROOMS {
                return Err(InvitationError::CapacityExceeded);
            }
            let inviter_response = waiting
                .outbound
                .try_reserve()
                .map_err(|_| InvitationError::CapacityExceeded)?;
            let invitee_response = outbound
                .try_reserve()
                .map_err(|_| InvitationError::CapacityExceeded)?;
            let authenticated = state
                .invitations
                .accept_invitation(room, invitee, &token.0, &signature)?;
            state.waiting.remove(&room);
            let inviter_resume = random_token()?;
            let invitee_resume = random_token()?;
            let inviter_relay = random_token()?;
            let invitee_relay = random_token()?;
            let inviter_resume_hash = hash_token(inviter_resume.expose());
            let invitee_resume_hash = hash_token(invitee_resume.expose());
            let inviter_relay_hash = hash_token(inviter_relay.expose());
            let invitee_relay_hash = hash_token(invitee_relay.expose());
            state.active.insert(
                room,
                ActiveRoom {
                    inviter: authenticated.inviter,
                    invitee: authenticated.invitee,
                    inviter_outbound: Some(waiting.outbound.clone()),
                    invitee_outbound: Some(outbound.clone()),
                    inviter_resume_hash,
                    invitee_resume_hash,
                    inviter_relay_hash,
                    invitee_relay_hash,
                    inviter_relay: None,
                    invitee_relay: None,
                    relay_ready: watch::channel(false).0,
                    inviter_relay_quota: RelayQuota::new(),
                    invitee_relay_quota: RelayQuota::new(),
                    disconnected_until: None,
                },
            );
            inviter_response.send(ServerMessage::InvitationAccepted {
                room: *room.as_bytes(),
                remote_peer: *invitee.as_bytes(),
                resume_token: WireInvitationToken(*inviter_resume.expose()),
                relay_token: WireInvitationToken(*inviter_relay.expose()),
            });
            invitee_response.send(ServerMessage::InvitationAccepted {
                room: *room.as_bytes(),
                remote_peer: *waiting.identity.as_bytes(),
                resume_token: WireInvitationToken(*invitee_resume.expose()),
                relay_token: WireInvitationToken(*invitee_relay.expose()),
            });
            connection.identity = Some(invitee);
            connection.room = Some(room);
            Ok(())
        }
        ClientMessage::ResumeRoom {
            room,
            peer,
            token,
            signature,
        } => {
            if connection.room.is_some() || connection.identity.is_some() {
                return Err(InvitationError::NotFound);
            }
            let room = RoomId::from_bytes(room);
            let peer = PeerIdentity::from_bytes(peer)?;
            let signature =
                Signature::from_slice(&signature).map_err(|_| InvitationError::InvalidSignature)?;
            peer.verifying_key()?
                .verify(&resume_message(room, peer, &token.0), &signature)
                .map_err(|_| InvitationError::InvalidSignature)?;

            let mut state = shared.lock().await;
            let expired = state.active.get(&room).is_some_and(|active| {
                active
                    .disconnected_until
                    .is_some_and(|until| until <= Instant::now())
            });
            if expired {
                state.active.remove(&room);
                return Err(InvitationError::Expired);
            }
            let Some(active) = state.active.get_mut(&room) else {
                return Err(InvitationError::NotFound);
            };
            let (resume_hash, slot, remote_peer, remote_outbound) = if active.inviter == peer {
                (
                    &active.inviter_resume_hash,
                    &mut active.inviter_outbound,
                    active.invitee,
                    active.invitee_outbound.clone(),
                )
            } else if active.invitee == peer {
                (
                    &active.invitee_resume_hash,
                    &mut active.invitee_outbound,
                    active.inviter,
                    active.inviter_outbound.clone(),
                )
            } else {
                return Err(InvitationError::WrongPeer);
            };
            if !bool::from(resume_hash.ct_eq(&hash_token(&token.0))) {
                return Err(InvitationError::InvalidSignature);
            }
            if slot.is_some() {
                return Err(InvitationError::WrongPeer);
            }
            *slot = Some(outbound.clone());
            if remote_outbound.is_some() {
                active.disconnected_until = None;
            }
            outbound
                .try_send(ServerMessage::PeerResumed {
                    room: *room.as_bytes(),
                    remote_peer: *remote_peer.as_bytes(),
                })
                .map_err(|_| InvitationError::CapacityExceeded)?;
            if let Some(remote_outbound) = remote_outbound {
                remote_outbound
                    .try_send(ServerMessage::PeerResumed {
                        room: *room.as_bytes(),
                        remote_peer: *peer.as_bytes(),
                    })
                    .map_err(|_| InvitationError::CapacityExceeded)?;
            }
            connection.identity = Some(peer);
            connection.room = Some(room);
            Ok(())
        }
        ClientMessage::RelayJoin { .. } => Err(InvitationError::WrongPeer),
        ClientMessage::IceCredentials {
            room,
            ufrag,
            password,
        } => {
            if ufrag.0.is_empty()
                || ufrag.0.len() > MAX_UFRAG_LENGTH
                || password.0.len() < 22
                || password.0.len() > MAX_ICE_PASSWORD_LENGTH
            {
                return Err(InvitationError::InvalidSignature);
            }
            let room = RoomId::from_bytes(room);
            forward_to_peer(
                room,
                connection,
                shared,
                ServerMessage::IceCredentials {
                    room: *room.as_bytes(),
                    from_peer: *connection
                        .identity
                        .ok_or(InvitationError::NotFound)?
                        .as_bytes(),
                    ufrag,
                    password,
                },
            )
            .await
        }
        ClientMessage::Candidate { room, candidate } => {
            if candidate.is_empty() || candidate.len() > MAX_CANDIDATE_LENGTH {
                return Err(InvitationError::InvalidSignature);
            }
            let room = RoomId::from_bytes(room);
            forward_to_peer(
                room,
                connection,
                shared,
                ServerMessage::Candidate {
                    room: *room.as_bytes(),
                    from_peer: *connection
                        .identity
                        .ok_or(InvitationError::NotFound)?
                        .as_bytes(),
                    candidate,
                },
            )
            .await
        }
        ClientMessage::CandidatesComplete { room } => {
            let room = RoomId::from_bytes(room);
            forward_to_peer(
                room,
                connection,
                shared,
                ServerMessage::CandidatesComplete {
                    room: *room.as_bytes(),
                    from_peer: *connection
                        .identity
                        .ok_or(InvitationError::NotFound)?
                        .as_bytes(),
                },
            )
            .await
        }
        ClientMessage::Hangup { room } => {
            let room = RoomId::from_bytes(room);
            hangup(room, connection.identity, shared).await;
            connection.room = None;
            connection.identity = None;
            Ok(())
        }
    }
}

async fn forward_to_peer(
    room: RoomId,
    connection: &ConnectionState,
    shared: &Arc<Mutex<SignalState>>,
    message: ServerMessage,
) -> Result<(), InvitationError> {
    let identity = connection.identity.ok_or(InvitationError::NotFound)?;
    if connection.room != Some(room) {
        return Err(InvitationError::NotFound);
    }
    let target = {
        let state = shared.lock().await;
        let Some(active) = state.active.get(&room) else {
            return Err(InvitationError::NotFound);
        };
        if active.inviter == identity {
            active.invitee_outbound.clone()
        } else if active.invitee == identity {
            active.inviter_outbound.clone()
        } else {
            return Err(InvitationError::WrongPeer);
        }
    };
    let Some(target) = target else {
        return Err(InvitationError::NotFound);
    };
    if matches!(
        timeout(Duration::from_secs(5), target.send(message)).await,
        Ok(Ok(()))
    ) {
        return Ok(());
    }
    let mut state = shared.lock().await;
    state.active.remove(&room);
    let _ = target.try_send(ServerMessage::PeerLeft {
        room: *room.as_bytes(),
    });
    Err(InvitationError::CapacityExceeded)
}

async fn hangup(room: RoomId, identity: Option<PeerIdentity>, shared: &Arc<Mutex<SignalState>>) {
    let Some(identity) = identity else { return };
    let (target, relays) = {
        let mut state = shared.lock().await;
        if let Some(waiting) = state.waiting.get(&room) {
            if waiting.identity == identity {
                state.waiting.remove(&room);
                state.invitations.cancel_invitation(room, identity);
            }
            return;
        }
        let Some(active) = state.active.remove(&room) else {
            return;
        };
        let target = if active.inviter == identity {
            active.invitee_outbound.clone()
        } else if active.invitee == identity {
            active.inviter_outbound.clone()
        } else {
            None
        };
        let relays = [active.inviter_relay.clone(), active.invitee_relay.clone()];
        (target, relays)
    };
    if let Some(target) = target {
        let _ = target.try_send(ServerMessage::PeerLeft {
            room: *room.as_bytes(),
        });
    }
    for relay in relays.into_iter().flatten() {
        let _ = relay.try_send(RelayCommand::Close);
    }
}

async fn cleanup_connection(connection: &ConnectionState, shared: &Arc<Mutex<SignalState>>) {
    if let Some(room) = connection.room {
        disconnect_peer(room, connection.identity, shared).await;
    }
}

async fn disconnect_peer(
    room: RoomId,
    identity: Option<PeerIdentity>,
    shared: &Arc<Mutex<SignalState>>,
) {
    let Some(identity) = identity else { return };
    let (remote, relays, expires_at) = {
        let mut state = shared.lock().await;
        if state
            .waiting
            .get(&room)
            .is_some_and(|waiting| waiting.identity == identity)
        {
            state.waiting.remove(&room);
            state.invitations.cancel_invitation(room, identity);
            return;
        }
        let Some(active) = state.active.get_mut(&room) else {
            return;
        };
        let (slot, remote) = if active.inviter == identity {
            (
                &mut active.inviter_outbound,
                active.invitee_outbound.clone(),
            )
        } else if active.invitee == identity {
            (
                &mut active.invitee_outbound,
                active.inviter_outbound.clone(),
            )
        } else {
            return;
        };
        *slot = None;
        let relays = [active.inviter_relay.take(), active.invitee_relay.take()];
        let expires_at = *active
            .disconnected_until
            .get_or_insert_with(|| Instant::now() + RECONNECT_GRACE_PERIOD);
        (remote, relays, expires_at)
    };
    if let Some(remote) = remote {
        let _ = remote.try_send(ServerMessage::PeerDisconnected {
            room: *room.as_bytes(),
        });
    }
    for relay in relays.into_iter().flatten() {
        let _ = relay.try_send(RelayCommand::Close);
    }
    let shared = Arc::clone(shared);
    tokio::spawn(async move {
        tokio::time::sleep(expires_at.saturating_duration_since(Instant::now())).await;
        let (remote, relays) = {
            let mut state = shared.lock().await;
            if state.active.get(&room).is_some_and(|active| {
                active.disconnected_until == Some(expires_at)
                    && (active.inviter_outbound.is_none() || active.invitee_outbound.is_none())
            }) {
                let Some(active) = state.active.remove(&room) else {
                    return;
                };
                let remote = active
                    .inviter_outbound
                    .clone()
                    .or(active.invitee_outbound.clone());
                let relays = [active.inviter_relay.clone(), active.invitee_relay.clone()];
                (remote, relays)
            } else {
                (None, [None, None])
            }
        };
        if let Some(remote) = remote {
            let _ = remote.try_send(ServerMessage::PeerLeft {
                room: *room.as_bytes(),
            });
        }
        for relay in relays.into_iter().flatten() {
            let _ = relay.try_send(RelayCommand::Close);
        }
    });
}

fn hash_token(token: &[u8; 32]) -> [u8; 32] {
    Sha256::digest(token).into()
}

#[cfg(test)]
mod tests {
    use std::{io, sync::Arc, time::Duration};

    use ed25519_dalek::{Signer, SigningKey};
    use futures_util::{SinkExt, StreamExt};
    use tokio::{
        net::{TcpListener, TcpStream},
        sync::{Mutex, mpsc},
        time::timeout,
    };
    use tokio_tungstenite::tungstenite::Message;

    use super::{
        ActiveRoom, ClientMessage, ConnectionState, RelayCommand, RelayQuota, ServerMessage,
        SignalState, WireInvitationToken, cleanup_connection, cleanup_relay, handle_message,
        handle_tls_stream, hash_token, register_relay, route_relay_packet, tls_acceptor,
    };
    use crate::{
        PeerIdentity, RoomId, accept_message, invite_request_message, relay_message, resume_message,
    };

    #[tokio::test]
    async fn signed_invitation_binds_signaling_and_teardown_to_both_peers() {
        let inviter_key = SigningKey::from_bytes(&[41; 32]);
        let invitee_key = SigningKey::from_bytes(&[52; 32]);
        let inviter = PeerIdentity::from_signing_key(&inviter_key);
        let invitee = PeerIdentity::from_signing_key(&invitee_key);
        let state = Arc::new(Mutex::new(SignalState::default()));
        let (inviter_tx, mut inviter_rx) = mpsc::channel(8);
        let mut inviter_connection = ConnectionState::default();
        let nonce = [7; 16];
        let signature = inviter_key.sign(&invite_request_message(inviter, invitee, nonce));

        handle_message(
            ClientMessage::CreateInvitation {
                inviter: *inviter.as_bytes(),
                invitee: *invitee.as_bytes(),
                nonce,
                signature: signature.to_bytes().to_vec(),
            },
            &mut inviter_connection,
            &inviter_tx,
            &state,
        )
        .await
        .expect("signed room invitation should be accepted");
        let ServerMessage::InvitationCreated { room, token, .. } = inviter_rx
            .recv()
            .await
            .expect("inviter should receive a ticket")
        else {
            panic!("unexpected signaling response");
        };

        let (invitee_tx, mut invitee_rx) = mpsc::channel(8);
        let mut invitee_connection = ConnectionState::default();
        let room_id = crate::RoomId::from_bytes(room);
        let acceptance = invitee_key.sign(&accept_message(room_id, invitee, &token.0));
        handle_message(
            ClientMessage::AcceptInvitation {
                room,
                invitee: *invitee.as_bytes(),
                token,
                signature: acceptance.to_bytes().to_vec(),
            },
            &mut invitee_connection,
            &invitee_tx,
            &state,
        )
        .await
        .expect("bound invitee should be accepted");
        let Some(ServerMessage::InvitationAccepted {
            resume_token: inviter_resume,
            ..
        }) = inviter_rx.recv().await
        else {
            panic!("inviter should receive its reconnect credential");
        };
        let Some(ServerMessage::InvitationAccepted {
            resume_token: invitee_resume,
            ..
        }) = invitee_rx.recv().await
        else {
            panic!("invitee should receive its reconnect credential");
        };

        handle_message(
            ClientMessage::Candidate {
                room,
                candidate: "candidate:host-loopback".to_owned(),
            },
            &mut inviter_connection,
            &inviter_tx,
            &state,
        )
        .await
        .expect("authenticated peer candidate should be forwarded");
        assert!(matches!(
            invitee_rx.recv().await,
            Some(ServerMessage::Candidate { from_peer, .. }) if from_peer == *inviter.as_bytes()
        ));

        cleanup_connection(&inviter_connection, &state).await;
        assert!(matches!(
            invitee_rx.recv().await,
            Some(ServerMessage::PeerDisconnected { .. })
        ));
        let (resumed_tx, mut resumed_rx) = mpsc::channel(8);
        let mut resumed_connection = ConnectionState::default();
        let resume_signature =
            inviter_key.sign(&resume_message(room_id, inviter, &inviter_resume.0));
        handle_message(
            ClientMessage::ResumeRoom {
                room,
                peer: *inviter.as_bytes(),
                token: WireInvitationToken(inviter_resume.0),
                signature: resume_signature.to_bytes().to_vec(),
            },
            &mut resumed_connection,
            &resumed_tx,
            &state,
        )
        .await
        .expect("peer should reconnect with the separate credential");
        assert!(matches!(
            resumed_rx.recv().await,
            Some(ServerMessage::PeerResumed { remote_peer, .. }) if remote_peer == *invitee.as_bytes()
        ));
        assert!(matches!(
            invitee_rx.recv().await,
            Some(ServerMessage::PeerResumed { remote_peer, .. }) if remote_peer == *inviter.as_bytes()
        ));

        handle_message(
            ClientMessage::Hangup { room },
            &mut invitee_connection,
            &invitee_tx,
            &state,
        )
        .await
        .expect("participant hangup should clean up the room");
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), resumed_rx.recv()).await,
            Ok(Some(ServerMessage::PeerLeft { .. }))
        ));
        drop(invitee_resume);
    }

    #[tokio::test]
    async fn tls_websocket_endpoint_accepts_a_connection_and_rejects_bad_json() -> anyhow::Result<()>
    {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])?;
        let certificate = certified.cert.der().clone();
        let cert_pem = certified.cert.pem();
        let key_pem = certified.signing_key.serialize_pem();
        let acceptor = super::tls_acceptor(cert_pem.as_bytes(), key_pem.as_bytes())?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(super::serve(listener, acceptor));

        let mut roots = rustls::RootCertStore::empty();
        roots.add(certificate)?;
        let tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(tls));
        let tcp = TcpStream::connect(address).await?;
        let server_name = rustls::pki_types::ServerName::try_from("localhost".to_owned())?;
        let tls_stream = connector.connect(server_name, tcp).await?;
        let (mut websocket, _) =
            tokio_tungstenite::client_async("wss://localhost/signal", tls_stream).await?;
        websocket.send(Message::Text("{".into())).await?;
        let response = timeout(Duration::from_secs(2), websocket.next())
            .await?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "signaling server closed")
            })??;
        let Message::Text(response) = response else {
            return Err(
                io::Error::new(io::ErrorKind::InvalidData, "expected JSON error frame").into(),
            );
        };
        let parsed: serde_json::Value = serde_json::from_str(&response)?;
        assert_eq!(parsed["code"], "invalid_message");
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn tls_websocket_relay_forwards_binary_datagrams_both_ways() -> anyhow::Result<()> {
        let inviter_key = SigningKey::from_bytes(&[81; 32]);
        let invitee_key = SigningKey::from_bytes(&[82; 32]);
        let inviter = PeerIdentity::from_signing_key(&inviter_key);
        let invitee = PeerIdentity::from_signing_key(&invitee_key);
        let room = RoomId::from_bytes([8; 16]);
        let inviter_token = [83; 32];
        let invitee_token = [84; 32];
        let shared = Arc::new(Mutex::new(SignalState::default()));
        shared.lock().await.active.insert(
            room,
            ActiveRoom {
                inviter,
                invitee,
                inviter_outbound: None,
                invitee_outbound: None,
                inviter_resume_hash: [0; 32],
                invitee_resume_hash: [0; 32],
                inviter_relay_hash: hash_token(&inviter_token),
                invitee_relay_hash: hash_token(&invitee_token),
                inviter_relay: None,
                invitee_relay: None,
                relay_ready: tokio::sync::watch::channel(false).0,
                inviter_relay_quota: RelayQuota::new(),
                invitee_relay_quota: RelayQuota::new(),
                disconnected_until: None,
            },
        );

        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])?;
        let certificate = certified.cert.der().clone();
        let acceptor = tls_acceptor(
            certified.cert.pem().as_bytes(),
            certified.signing_key.serialize_pem().as_bytes(),
        )?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let shared = Arc::clone(&shared);
                tokio::spawn(handle_tls_stream(tls, shared));
            }
        });
        let mut roots = rustls::RootCertStore::empty();
        roots.add(certificate)?;
        let client_tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_tls));

        let inviter_tcp = TcpStream::connect(address).await?;
        let inviter_name = rustls::pki_types::ServerName::try_from("localhost".to_owned())?;
        let inviter_tls = connector.connect(inviter_name, inviter_tcp).await?;
        let (mut inviter_ws, _) =
            tokio_tungstenite::client_async("wss://localhost/relay", inviter_tls).await?;
        let invitee_tcp = TcpStream::connect(address).await?;
        let invitee_name = rustls::pki_types::ServerName::try_from("localhost".to_owned())?;
        let invitee_tls = connector.connect(invitee_name, invitee_tcp).await?;
        let (mut invitee_ws, _) =
            tokio_tungstenite::client_async("wss://localhost/relay", invitee_tls).await?;

        let inviter_signature = inviter_key.sign(&relay_message(room, inviter, &inviter_token));
        let invitee_signature = invitee_key.sign(&relay_message(room, invitee, &invitee_token));
        let inviter_signature_bytes = inviter_signature.to_bytes().to_vec();
        let invitee_signature_bytes = invitee_signature.to_bytes().to_vec();
        let inviter_join = serde_json::json!({
            "type": "relay_join", "room": room.as_bytes(), "peer": inviter.as_bytes(),
            "token": inviter_token, "signature": inviter_signature_bytes,
        });
        let invitee_join = serde_json::json!({
            "type": "relay_join", "room": room.as_bytes(), "peer": invitee.as_bytes(),
            "token": invitee_token, "signature": invitee_signature_bytes,
        });
        inviter_ws
            .send(Message::Text(inviter_join.to_string().into()))
            .await?;
        invitee_ws
            .send(Message::Text(invitee_join.to_string().into()))
            .await?;
        for websocket in [&mut inviter_ws, &mut invitee_ws] {
            let response = timeout(Duration::from_secs(2), websocket.next())
                .await?
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::UnexpectedEof, "relay closed before ready")
                })??;
            let Message::Text(response) = response else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "expected relay ready response",
                )
                .into());
            };
            let parsed: serde_json::Value = serde_json::from_str(&response)?;
            assert_eq!(parsed["type"], "relay_ready");
        }

        inviter_ws
            .send(Message::Binary(vec![1, 4, 9].into()))
            .await?;
        let received = timeout(Duration::from_secs(2), invitee_ws.next())
            .await?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "relay peer closed"))??;
        assert!(matches!(received, Message::Binary(packet) if packet.as_ref() == [1, 4, 9]));
        invitee_ws
            .send(Message::Binary(vec![2, 5, 8].into()))
            .await?;
        let received = timeout(Duration::from_secs(2), inviter_ws.next())
            .await?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "relay peer closed"))??;
        assert!(matches!(received, Message::Binary(packet) if packet.as_ref() == [2, 5, 8]));

        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn authenticated_relay_preserves_datagrams_and_closes_both_ends() {
        let inviter_key = SigningKey::from_bytes(&[61; 32]);
        let invitee_key = SigningKey::from_bytes(&[72; 32]);
        let inviter = PeerIdentity::from_signing_key(&inviter_key);
        let invitee = PeerIdentity::from_signing_key(&invitee_key);
        let room = RoomId::from_bytes([9; 16]);
        let inviter_token = [33; 32];
        let invitee_token = [44; 32];
        let state = Arc::new(Mutex::new(SignalState::default()));
        let (inviter_tx, mut inviter_rx) = mpsc::channel(8);
        let (invitee_tx, mut invitee_rx) = mpsc::channel(8);
        state.lock().await.active.insert(
            room,
            ActiveRoom {
                inviter,
                invitee,
                inviter_outbound: None,
                invitee_outbound: None,
                inviter_resume_hash: [0; 32],
                invitee_resume_hash: [0; 32],
                inviter_relay_hash: hash_token(&inviter_token),
                invitee_relay_hash: hash_token(&invitee_token),
                inviter_relay: None,
                invitee_relay: None,
                relay_ready: tokio::sync::watch::channel(false).0,
                inviter_relay_quota: RelayQuota::new(),
                invitee_relay_quota: RelayQuota::new(),
                disconnected_until: None,
            },
        );
        let inviter_signature = inviter_key.sign(&relay_message(room, inviter, &inviter_token));
        let invitee_signature = invitee_key.sign(&relay_message(room, invitee, &invitee_token));
        register_relay(
            room,
            inviter,
            &inviter_token,
            &inviter_signature,
            inviter_tx,
            &state,
        )
        .await
        .expect("inviter relay credential should validate");
        register_relay(
            room,
            invitee,
            &invitee_token,
            &invitee_signature,
            invitee_tx,
            &state,
        )
        .await
        .expect("invitee relay credential should validate");

        route_relay_packet(room, inviter, vec![1, 2, 3, 4], &state)
            .await
            .expect("relay should forward one datagram intact");
        assert!(
            matches!(invitee_rx.recv().await, Some(RelayCommand::Data(packet)) if packet == [1, 2, 3, 4])
        );
        route_relay_packet(room, invitee, vec![5, 6], &state)
            .await
            .expect("relay should forward in both directions");
        assert!(
            matches!(inviter_rx.recv().await, Some(RelayCommand::Data(packet)) if packet == [5, 6])
        );
        assert!(
            route_relay_packet(room, inviter, vec![0; 1201], &state)
                .await
                .is_err()
        );

        cleanup_relay(room, inviter, &state).await;
        assert!(matches!(invitee_rx.recv().await, Some(RelayCommand::Close)));
    }
}
