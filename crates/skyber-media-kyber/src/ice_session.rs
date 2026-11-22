//! IPv4 ICE gathering and nomination for the Gate C-1 direct path.
//!
//! This module only discovers a UDP path. It does not provide media security:
//! callers must establish QUIC over the returned ICE connection and run the
//! room identity proof before initializing Kymux.

use std::{collections::HashSet, net::IpAddr, sync::Arc, time::Duration};

use futures_util::{Sink, Stream};
use quinn::AsyncUdpSocket;
use skyber_signaling::{PeerIdentity, RoomId, SignalClient, SignalClientError, SignalEvent};
use thiserror::Error;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::{tungstenite::Error as WebSocketError, tungstenite::Message};
use webrtc_ice::{
    agent::{Agent, agent_config::AgentConfig},
    candidate::{Candidate, CandidateType, candidate_base::unmarshal_candidate},
    mdns::MulticastDnsMode,
    network_type::NetworkType,
    url::Url,
};
use webrtc_util::conn::Conn;
use zeroize::Zeroize;

use crate::ice_quinn::IceQuinnSocket;

const CANDIDATE_CAPACITY: usize = 64;
const MAX_CANDIDATE_LENGTH: usize = 1024;
const MAX_UFRAG_LENGTH: usize = 32;
const MIN_PASSWORD_LENGTH: usize = 22;
const MAX_PASSWORD_LENGTH: usize = 256;
const MAX_GATHERED_CANDIDATES: usize = 64;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const DIRECT_ICE_TIMEOUT: Duration = Duration::from_secs(90);

/// Which ICE agent performs connectivity checks and nominates the candidate pair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IceRole {
    Controlling,
    Controlled,
}

/// Local ICE credentials. Formatting is redacted and owned strings are cleared
/// on drop. Signaling needs explicit copies of the values when sending them.
pub struct LocalIceCredentials {
    ufrag: String,
    password: String,
}

impl LocalIceCredentials {
    pub fn ufrag(&self) -> &str {
        &self.ufrag
    }

    pub fn password(&self) -> &str {
        &self.password
    }
}

impl Drop for LocalIceCredentials {
    fn drop(&mut self) {
        self.ufrag.zeroize();
        self.password.zeroize();
    }
}

impl std::fmt::Debug for LocalIceCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LocalIceCredentials([redacted])")
    }
}

/// Validated remote ICE credentials received from the authenticated signaling
/// room.
pub struct RemoteIceCredentials {
    ufrag: String,
    password: String,
}

impl RemoteIceCredentials {
    pub fn new(ufrag: String, password: String) -> Result<Self, IceSessionError> {
        if ufrag.is_empty()
            || ufrag.len() > MAX_UFRAG_LENGTH
            || password.len() < MIN_PASSWORD_LENGTH
            || password.len() > MAX_PASSWORD_LENGTH
        {
            return Err(IceSessionError::InvalidCredentials);
        }
        Ok(Self { ufrag, password })
    }
}

impl Drop for RemoteIceCredentials {
    fn drop(&mut self) {
        self.ufrag.zeroize();
        self.password.zeroize();
    }
}

impl std::fmt::Debug for RemoteIceCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RemoteIceCredentials([redacted])")
    }
}

/// An ICE candidate event delivered by the asynchronous gathering callback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IceCandidateEvent {
    Candidate(String),
    GatheringComplete,
}

/// A gathered ICE agent. Its candidate receiver must be drained while gathering
/// so candidate delivery remains bounded and does not stall ICE internals.
pub struct IceSession {
    agent: Arc<Agent>,
    local_credentials: LocalIceCredentials,
    candidates: mpsc::Receiver<IceCandidateEvent>,
    cancel_tx: mpsc::Sender<()>,
    cancel_rx: Option<mpsc::Receiver<()>>,
    cleanup_on_drop: bool,
}

impl IceSession {
    /// Create an IPv4 ICE agent. Optional STUN URLs must use the pinned ICE
    /// library's supported URL syntax. A Skyber-owned TURN service is not
    /// configured here; relay fallback remains the separate WSS path.
    pub async fn new(stun_server_urls: &[String]) -> Result<Self, IceSessionError> {
        let urls = stun_server_urls
            .iter()
            .map(|url| {
                Url::parse_url(url).map_err(|error| IceSessionError::InvalidStunUrl {
                    url: url.clone(),
                    reason: error.to_string(),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Self::from_config(AgentConfig {
            urls,
            network_types: vec![NetworkType::Udp4],
            candidate_types: vec![CandidateType::Host, CandidateType::ServerReflexive],
            multicast_dns_mode: MulticastDnsMode::Disabled,
            include_loopback: cfg!(test),
            ip_filter: if cfg!(test) {
                Arc::new(Some(Box::new(|ip: IpAddr| ip.is_loopback())))
            } else {
                Arc::new(None)
            },
            ..Default::default()
        })
        .await
    }

    async fn from_config(config: AgentConfig) -> Result<Self, IceSessionError> {
        let agent = Arc::new(
            Agent::new(config)
                .await
                .map_err(|error| IceSessionError::Agent(error.to_string()))?,
        );

        let (candidate_tx, candidates) = mpsc::channel(CANDIDATE_CAPACITY);
        agent.on_candidate(Box::new(move |candidate| {
            let candidate_tx = candidate_tx.clone();
            Box::pin(async move {
                let event = candidate.map_or(IceCandidateEvent::GatheringComplete, |candidate| {
                    IceCandidateEvent::Candidate(candidate.marshal())
                });
                let _ = candidate_tx.send(event).await;
            })
        }));

        let (ufrag, password) = agent.get_local_user_credentials().await;
        validate_credentials(&ufrag, &password)?;
        let (cancel_tx, cancel_rx) = mpsc::channel::<()>(1);
        Ok(Self {
            agent,
            local_credentials: LocalIceCredentials { ufrag, password },
            candidates,
            cancel_tx,
            cancel_rx: Some(cancel_rx),
            cleanup_on_drop: true,
        })
    }

    /// Start asynchronous candidate gathering. Candidate events can be relayed
    /// to the other authenticated peer immediately as they arrive.
    pub fn gather(&self) -> Result<(), IceSessionError> {
        self.agent
            .gather_candidates()
            .map_err(|error| IceSessionError::Agent(error.to_string()))
    }

    pub fn local_credentials(&self) -> &LocalIceCredentials {
        &self.local_credentials
    }

    pub async fn next_candidate(&mut self) -> Result<IceCandidateEvent, IceSessionError> {
        self.candidates
            .recv()
            .await
            .ok_or(IceSessionError::CandidateStreamClosed)
    }

    /// Parse and register one candidate delivered by the authenticated peer.
    pub fn add_remote_candidate(&self, candidate: &str) -> Result<(), IceSessionError> {
        if candidate.is_empty() || candidate.len() > MAX_CANDIDATE_LENGTH {
            return Err(IceSessionError::InvalidCandidate);
        }
        let candidate =
            unmarshal_candidate(candidate).map_err(|_| IceSessionError::InvalidCandidate)?;
        let candidate: Arc<dyn Candidate + Send + Sync> = Arc::new(candidate);
        self.agent
            .add_remote_candidate(&candidate)
            .map_err(|error| IceSessionError::Agent(error.to_string()))
    }

    /// Expose cancellation to the call controller while nomination is pending.
    pub fn cancellation_handle(&self) -> Result<IceCancellationHandle, IceSessionError> {
        if self.cancel_rx.is_none() {
            return Err(IceSessionError::AlreadyConnecting);
        }
        Ok(IceCancellationHandle(self.cancel_tx.clone()))
    }

    /// Run ICE connectivity checks and return the nominated connection.
    pub async fn connect(
        &mut self,
        role: IceRole,
        remote: RemoteIceCredentials,
    ) -> Result<NominatedIceConnection, IceSessionError> {
        let cancel_rx = self
            .cancel_rx
            .take()
            .ok_or(IceSessionError::AlreadyConnecting)?;

        let attempt = tokio::time::timeout(CONNECT_TIMEOUT, async {
            match role {
                IceRole::Controlling => self
                    .agent
                    .dial(cancel_rx, remote.ufrag.clone(), remote.password.clone())
                    .await
                    .map(|connection| connection as Arc<dyn Conn + Send + Sync>),
                IceRole::Controlled => self
                    .agent
                    .accept(cancel_rx, remote.ufrag.clone(), remote.password.clone())
                    .await
                    .map(|connection| connection as Arc<dyn Conn + Send + Sync>),
            }
        })
        .await;
        match attempt {
            Err(_) => Err(IceSessionError::Timeout),
            Ok(Err(error)) => Err(IceSessionError::Connectivity(error.to_string())),
            Ok(Ok(connection)) => {
                self.cleanup_on_drop = false;
                let remote_addr = connection.remote_addr();
                let socket = IceQuinnSocket::new(Arc::clone(&connection))
                    .map_err(|error| IceSessionError::Socket(error.to_string()))?;
                Ok(NominatedIceConnection {
                    _agent: Arc::clone(&self.agent),
                    _ice_connection: connection,
                    socket,
                    remote_addr,
                })
            }
        }
    }

    pub fn cancel(&self) {
        let _ = self.cancel_tx.try_send(());
    }

    /// Stop ICE gathering/checks and release the underlying network sockets.
    pub async fn close(&self) -> Result<(), IceSessionError> {
        self.cancel();
        self.agent
            .close()
            .await
            .map_err(|error| IceSessionError::Agent(error.to_string()))
    }
}

impl Drop for IceSession {
    fn drop(&mut self) {
        if !self.cleanup_on_drop {
            return;
        }
        self.cancel();
        let agent = Arc::clone(&self.agent);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = agent.close().await;
            });
        }
    }
}

/// Handle that can cancel ICE nomination from a competing controller task.
#[derive(Clone)]
pub struct IceCancellationHandle(mpsc::Sender<()>);

impl IceCancellationHandle {
    pub fn cancel(&self) {
        let _ = self.0.try_send(());
    }
}

/// The ICE agent and selected socket must remain alive for the QUIC endpoint.
pub struct NominatedIceConnection {
    _agent: Arc<Agent>,
    _ice_connection: Arc<dyn Conn + Send + Sync>,
    socket: Arc<IceQuinnSocket>,
    remote_addr: Option<std::net::SocketAddr>,
}

impl NominatedIceConnection {
    pub fn quinn_socket(&self) -> Arc<IceQuinnSocket> {
        Arc::clone(&self.socket)
    }

    pub fn local_addr(&self) -> Result<std::net::SocketAddr, IceSessionError> {
        self.socket
            .local_addr()
            .map_err(|error| IceSessionError::Socket(error.to_string()))
    }

    pub fn remote_addr(&self) -> Option<std::net::SocketAddr> {
        self.remote_addr
    }

    /// Tear down the Quinn socket adapter and close ICE-owned sockets.
    pub async fn close(&self) -> Result<(), IceSessionError> {
        self.socket.close();
        self._agent
            .close()
            .await
            .map_err(|error| IceSessionError::Agent(error.to_string()))
    }
}

impl Drop for NominatedIceConnection {
    fn drop(&mut self) {
        self.socket.close();
        let agent = Arc::clone(&self._agent);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = agent.close().await;
            });
        }
    }
}

/// Gather IPv4 candidates, exchange them through the authenticated signaling
/// room, and nominate the direct UDP pair. The caller supplies a cancellation
/// watch channel and must continue with QUIC room authentication before Kymux.
pub async fn negotiate_direct_ice<S>(
    signaling: &mut SignalClient<S>,
    room: RoomId,
    expected_peer: PeerIdentity,
    role: IceRole,
    stun_server_urls: &[String],
    cancellation: &mut watch::Receiver<bool>,
) -> Result<NominatedIceConnection, SignaledIceError>
where
    S: Sink<Message, Error = WebSocketError>
        + Stream<Item = Result<Message, WebSocketError>>
        + Unpin,
{
    if *cancellation.borrow() {
        return Err(SignaledIceError::Cancelled);
    }
    tokio::time::timeout(
        DIRECT_ICE_TIMEOUT,
        negotiate_direct_ice_inner(
            signaling,
            room,
            expected_peer,
            role,
            stun_server_urls,
            cancellation,
        ),
    )
    .await
    .map_err(|_| SignaledIceError::Timeout)?
}

async fn negotiate_direct_ice_inner<S>(
    signaling: &mut SignalClient<S>,
    room: RoomId,
    expected_peer: PeerIdentity,
    role: IceRole,
    stun_server_urls: &[String],
    cancellation: &mut watch::Receiver<bool>,
) -> Result<NominatedIceConnection, SignaledIceError>
where
    S: Sink<Message, Error = WebSocketError>
        + Stream<Item = Result<Message, WebSocketError>>
        + Unpin,
{
    let mut ice = IceSession::new(stun_server_urls).await?;
    ice.gather()?;

    let mut local_candidates = Vec::with_capacity(8);
    loop {
        let event = tokio::select! {
            event = ice.next_candidate() => event?,
            _ = wait_for_cancellation(cancellation) => return Err(SignaledIceError::Cancelled),
        };
        match event {
            IceCandidateEvent::Candidate(candidate) => {
                if local_candidates.len() >= MAX_GATHERED_CANDIDATES {
                    return Err(SignaledIceError::TooManyCandidates);
                }
                local_candidates.push(candidate);
            }
            IceCandidateEvent::GatheringComplete => break,
        }
    }

    let local_credentials = ice.local_credentials();
    tokio::select! {
        result = signaling.send_ice_credentials(
            room,
            local_credentials.ufrag().to_owned(),
            local_credentials.password().to_owned(),
        ) => result?,
        _ = wait_for_cancellation(cancellation) => return Err(SignaledIceError::Cancelled),
    }
    for candidate in local_candidates {
        tokio::select! {
            result = signaling.send_candidate(room, candidate) => result?,
            _ = wait_for_cancellation(cancellation) => return Err(SignaledIceError::Cancelled),
        }
    }
    tokio::select! {
        result = signaling.finish_candidate_gathering(room) => result?,
        _ = wait_for_cancellation(cancellation) => return Err(SignaledIceError::Cancelled),
    }

    let mut remote_credentials = None;
    let mut remote_candidates = HashSet::new();
    let mut remote_complete = false;
    while remote_credentials.is_none() || !remote_complete {
        let event = tokio::select! {
            event = signaling.receive() => event?,
            _ = wait_for_cancellation(cancellation) => return Err(SignaledIceError::Cancelled),
        };
        match event {
            SignalEvent::IceCredentials {
                room: event_room,
                from_peer,
                ufrag,
                password,
            } if event_room == room && from_peer == expected_peer => {
                if remote_credentials.is_some() {
                    return Err(SignaledIceError::DuplicateCredentials);
                }
                remote_credentials = Some(RemoteIceCredentials::new(
                    ufrag.expose().to_owned(),
                    password.expose().to_owned(),
                )?);
            }
            SignalEvent::Candidate {
                room: event_room,
                from_peer,
                candidate,
            } if event_room == room && from_peer == expected_peer => {
                if remote_complete || remote_candidates.len() >= MAX_GATHERED_CANDIDATES {
                    return Err(SignaledIceError::InvalidCandidateOrder);
                }
                if remote_candidates.insert(candidate.clone()) {
                    ice.add_remote_candidate(&candidate)?;
                }
            }
            SignalEvent::CandidatesComplete {
                room: event_room,
                from_peer,
            } if event_room == room && from_peer == expected_peer => {
                if remote_complete {
                    return Err(SignaledIceError::DuplicateCandidateCompletion);
                }
                remote_complete = true;
            }
            SignalEvent::PeerDisconnected(event_room) | SignalEvent::PeerLeft(event_room)
                if event_room == room =>
            {
                return Err(SignaledIceError::PeerDisconnected);
            }
            _ => return Err(SignaledIceError::UnexpectedPeerEvent),
        }
    }

    let remote_credentials = remote_credentials.ok_or(SignaledIceError::MissingCredentials)?;
    let cancellation_handle = ice.cancellation_handle()?;
    tokio::select! {
        connection = ice.connect(role, remote_credentials) => Ok(connection?),
        _ = wait_for_cancellation(cancellation) => {
            cancellation_handle.cancel();
            Err(SignaledIceError::Cancelled)
        }
    }
}

async fn wait_for_cancellation(cancellation: &mut watch::Receiver<bool>) {
    while !*cancellation.borrow() {
        if cancellation.changed().await.is_err() {
            return;
        }
    }
}

fn validate_credentials(ufrag: &str, password: &str) -> Result<(), IceSessionError> {
    if ufrag.is_empty()
        || ufrag.len() > MAX_UFRAG_LENGTH
        || password.len() < MIN_PASSWORD_LENGTH
        || password.len() > MAX_PASSWORD_LENGTH
    {
        return Err(IceSessionError::InvalidCredentials);
    }
    Ok(())
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum IceSessionError {
    #[error("invalid STUN server URL `{url}`: {reason}")]
    InvalidStunUrl { url: String, reason: String },
    #[error("ICE agent setup failed: {0}")]
    Agent(String),
    #[error("ICE local or remote credentials have invalid lengths")]
    InvalidCredentials,
    #[error("ICE candidate is malformed or exceeds the size limit")]
    InvalidCandidate,
    #[error("ICE candidate stream closed")]
    CandidateStreamClosed,
    #[error("ICE connection setup timed out")]
    Timeout,
    #[error("ICE connectivity checks failed: {0}")]
    Connectivity(String),
    #[error("ICE session is already connecting or no longer reusable")]
    AlreadyConnecting,
    #[error("ICE socket setup failed: {0}")]
    Socket(String),
}

#[derive(Debug, Error)]
pub enum SignaledIceError {
    #[error("ICE negotiation was cancelled")]
    Cancelled,
    #[error("signaling and direct ICE negotiation timed out")]
    Timeout,
    #[error("peer disconnected during ICE negotiation")]
    PeerDisconnected,
    #[error("signaling sent an event from an unexpected peer, room, or phase")]
    UnexpectedPeerEvent,
    #[error("peer sent duplicate ICE credentials")]
    DuplicateCredentials,
    #[error("peer sent duplicate candidate-completion events")]
    DuplicateCandidateCompletion,
    #[error("peer sent a candidate after candidate completion or exceeded candidate limit")]
    InvalidCandidateOrder,
    #[error("peer did not send ICE credentials")]
    MissingCredentials,
    #[error("local ICE agent gathered too many candidates")]
    TooManyCandidates,
    #[error(transparent)]
    Ice(#[from] IceSessionError),
    #[error(transparent)]
    Signaling(#[from] SignalClientError),
}

#[cfg(test)]
mod tests {
    use std::{net::IpAddr, sync::Arc, time::Duration};

    use super::{IceCandidateEvent, IceRole, IceSession, RemoteIceCredentials};
    use crate::room_auth::authenticate_room;
    use anyhow::Context;
    use ed25519_dalek::SigningKey;
    use quinn::{AsyncUdpSocket, EndpointConfig, TokioRuntime};
    use skyber_signaling::{AuthenticatedPeers, PeerIdentity, SignalClient, SignalEvent, server};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::watch,
    };
    use tokio_rustls::TlsConnector;
    use tokio_tungstenite::client_async;
    use webrtc_ice::{
        agent::agent_config::AgentConfig, candidate::CandidateType, mdns::MulticastDnsMode,
        network_type::NetworkType,
    };

    #[tokio::test]
    async fn two_agents_gather_exchange_candidates_and_nominate_ipv4() -> anyhow::Result<()> {
        let _network_test = crate::NETWORK_INTEGRATION_TEST_LOCK.lock().await;
        let mut controlling = test_session().await?;
        let mut controlled = test_session().await?;
        controlling.gather()?;
        controlled.gather()?;
        let controlling_candidate = gather_candidate(&mut controlling).await?;
        let controlled_candidate = gather_candidate(&mut controlled).await?;
        controlling.add_remote_candidate(&controlled_candidate)?;
        controlled.add_remote_candidate(&controlling_candidate)?;

        let controlling_credentials = RemoteIceCredentials::new(
            controlled.local_credentials().ufrag().to_owned(),
            controlled.local_credentials().password().to_owned(),
        )?;
        let controlled_credentials = RemoteIceCredentials::new(
            controlling.local_credentials().ufrag().to_owned(),
            controlling.local_credentials().password().to_owned(),
        )?;
        let (controlling_connection, controlled_connection) = tokio::try_join!(
            controlling.connect(IceRole::Controlling, controlling_credentials),
            controlled.connect(IceRole::Controlled, controlled_credentials),
        )?;
        assert!(controlling_connection.remote_addr().is_some());
        assert!(controlled_connection.remote_addr().is_some());
        assert_eq!(
            controlling_connection.quinn_socket().local_addr()?,
            controlling_connection.local_addr()?
        );
        Ok(())
    }

    #[tokio::test]
    async fn invalid_stun_url_credentials_and_candidate_are_rejected() -> anyhow::Result<()> {
        assert!(
            IceSession::new(&["not-a-stun-url".to_owned()])
                .await
                .is_err()
        );
        assert!(RemoteIceCredentials::new("x".to_owned(), "short".to_owned()).is_err());
        let session = test_session().await?;
        assert!(session.add_remote_candidate(&"a".repeat(1025)).is_err());
        assert!(session.add_remote_candidate("invalid candidate").is_err());
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_handle_stops_pending_connectivity_checks() -> anyhow::Result<()> {
        let _network_test = crate::NETWORK_INTEGRATION_TEST_LOCK.lock().await;
        let mut session = test_session().await?;
        session.gather()?;
        let candidate = gather_candidate(&mut session).await?;
        session.add_remote_candidate(&candidate)?;
        let cancel = session.cancellation_handle()?;
        let remote = RemoteIceCredentials::new(
            "remote-ufrag".to_owned(),
            "remote-password-with-more-than-22-characters".to_owned(),
        )?;
        let attempt =
            tokio::spawn(async move { session.connect(IceRole::Controlling, remote).await });
        cancel.cancel();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), attempt)
                .await??
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn authenticated_signaling_drives_direct_ice_for_both_peer_roles() -> anyhow::Result<()> {
        let _network_test = crate::NETWORK_INTEGRATION_TEST_LOCK.lock().await;
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])?;
        let certificate = certified.cert.der().clone();
        let acceptor = server::tls_acceptor(
            certified.cert.pem().as_bytes(),
            certified.signing_key.serialize_pem().as_bytes(),
        )?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server_task = tokio::spawn(server::serve(listener, acceptor));

        let inviter_key = SigningKey::from_bytes(&[31; 32]);
        let invitee_key = SigningKey::from_bytes(&[32; 32]);
        let inviter_identity = PeerIdentity::from_signing_key(&inviter_key);
        let invitee_identity = PeerIdentity::from_signing_key(&invitee_key);
        let mut inviter = signaling_client(address, certificate.clone()).await?;
        let invitation = inviter
            .create_invitation(invitee_identity, &inviter_key)
            .await?;
        let mut invitee = signaling_client(address, certificate.clone()).await?;
        let accepted = invitee
            .accept_invitation(
                invitation.room,
                &invitation.token,
                inviter_identity,
                &invitee_key,
            )
            .await?;
        assert!(matches!(
            inviter.receive().await?,
            SignalEvent::InvitationAccepted(accepted)
                if accepted.remote_peer == invitee_identity
        ));

        let (_cancel_sender_a, mut cancel_a) = watch::channel(false);
        let (_cancel_sender_b, mut cancel_b) = watch::channel(false);
        let (inviter_connection, invitee_connection) = tokio::try_join!(
            super::negotiate_direct_ice(
                &mut inviter,
                invitation.room,
                invitee_identity,
                IceRole::Controlling,
                &[],
                &mut cancel_a,
            ),
            super::negotiate_direct_ice(
                &mut invitee,
                invitation.room,
                inviter_identity,
                IceRole::Controlled,
                &[],
                &mut cancel_b,
            ),
        )?;
        assert!(inviter_connection.remote_addr().is_some());
        assert!(invitee_connection.remote_addr().is_some());
        assert_eq!(accepted.room, invitation.room);

        let cert_der = certified.cert.der().clone();
        let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()),
        );
        let mut server_tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der)?;
        server_tls.alpn_protocols = vec![b"kymux".to_vec()];
        let server_config = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(server_tls)?,
        ));
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert_der)?;
        let mut client_tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        client_tls.alpn_protocols = vec![b"kymux".to_vec()];
        let client_config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(client_tls)?,
        ));

        let runtime = Arc::new(TokioRuntime);
        let mut inviter_endpoint = quinn::Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            None,
            inviter_connection.quinn_socket(),
            runtime.clone(),
        )?;
        inviter_endpoint.set_default_client_config(client_config);
        let invitee_endpoint = quinn::Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            Some(server_config),
            invitee_connection.quinn_socket(),
            runtime,
        )?;
        let quic_connecting = inviter_endpoint.connect(
            inviter_connection
                .remote_addr()
                .ok_or_else(|| anyhow::anyhow!("nominated ICE peer address is missing"))?,
            "localhost",
        )?;
        let (inviter_quic, invitee_quic) = tokio::time::timeout(Duration::from_secs(10), async {
            let inviter_connect = async { quic_connecting.await.map_err(anyhow::Error::from) };
            let invitee_accept = async {
                invitee_endpoint
                    .accept()
                    .await
                    .ok_or_else(|| anyhow::anyhow!("QUIC endpoint closed before handshake"))?
                    .await
                    .map_err(anyhow::Error::from)
            };
            tokio::try_join!(inviter_connect, invitee_accept)
        })
        .await
        .context("signaled ICE/Quinn handshake timed out")??;

        let peers = AuthenticatedPeers {
            room: invitation.room,
            inviter: inviter_identity,
            invitee: invitee_identity,
        };
        let (inviter_quic, invitee_quic) = tokio::try_join!(
            authenticate_room(inviter_quic, peers, &inviter_key),
            authenticate_room(invitee_quic, peers, &invitee_key),
        )?;
        let inviter_kymux = kymux::kynet::Connection::from(inviter_quic.into_inner());
        let invitee_kymux = kymux::kynet::Connection::from(invitee_quic.into_inner());
        let (mut stream_send, mut stream_receive) = inviter_kymux.open_bi().await?;
        stream_send.write_all(b"signaling-ice-auth-kyber").await?;
        stream_send.finish().await?;
        let (mut response_send, mut response_receive) =
            tokio::time::timeout(Duration::from_secs(5), invitee_kymux.accept_bi())
                .await
                .context("Kymux stream acceptance timed out")??;
        let mut request = [0_u8; 24];
        tokio::time::timeout(
            Duration::from_secs(5),
            response_receive.read_exact(&mut request),
        )
        .await
        .context("Kymux payload receive timed out")??;
        assert_eq!(&request, b"signaling-ice-auth-kyber");
        response_send.write_all(b"ack").await?;
        response_send.finish().await?;
        let mut acknowledgement = [0_u8; 3];
        tokio::time::timeout(
            Duration::from_secs(5),
            stream_receive.read_exact(&mut acknowledgement),
        )
        .await
        .context("Kymux response receive timed out")??;
        assert_eq!(&acknowledgement, b"ack");
        inviter_kymux.close(0, "integration proof complete");
        invitee_kymux.close(0, "integration proof complete");
        inviter_endpoint.wait_idle().await;
        invitee_endpoint.wait_idle().await;

        invitee.hangup(invitation.room).await?;
        inviter.close().await?;
        invitee.close().await?;
        server_task.abort();
        Ok(())
    }

    async fn signaling_client(
        address: std::net::SocketAddr,
        certificate: rustls::pki_types::CertificateDer<'static>,
    ) -> anyhow::Result<
        SignalClient<
            tokio_tungstenite::WebSocketStream<
                tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
            >,
        >,
    > {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(certificate)?;
        let tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(tls));
        let tcp = tokio::net::TcpStream::connect(address).await?;
        let server_name = rustls::pki_types::ServerName::try_from("localhost".to_owned())?;
        let tls = connector.connect(server_name, tcp).await?;
        let (websocket, _) = client_async("wss://localhost/signal", tls).await?;
        Ok(SignalClient::from_authenticated_websocket(websocket))
    }

    async fn test_session() -> anyhow::Result<IceSession> {
        let config = AgentConfig {
            network_types: vec![NetworkType::Udp4],
            candidate_types: vec![CandidateType::Host],
            multicast_dns_mode: MulticastDnsMode::Disabled,
            include_loopback: true,
            ip_filter: Arc::new(Some(Box::new(|ip: IpAddr| ip.is_loopback()))),
            ..Default::default()
        };
        Ok(IceSession::from_config(config).await?)
    }

    async fn gather_candidate(session: &mut IceSession) -> anyhow::Result<String> {
        match tokio::time::timeout(Duration::from_secs(5), session.next_candidate()).await?? {
            IceCandidateEvent::Candidate(candidate) => Ok(candidate),
            IceCandidateEvent::GatheringComplete => anyhow::bail!("no local ICE candidate"),
        }
    }
}
