//! IPv4 ICE gathering and nomination for the Gate C-1 direct path.
//!
//! This module only discovers a UDP path. It does not provide media security:
//! callers must establish QUIC over the returned ICE connection and run the
//! room identity proof before initializing Kymux.

use std::{net::IpAddr, sync::Arc, time::Duration};

use quinn::AsyncUdpSocket;
use thiserror::Error;
use tokio::sync::mpsc;
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
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

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

#[cfg(test)]
mod tests {
    use std::{net::IpAddr, sync::Arc, time::Duration};

    use super::{IceCandidateEvent, IceRole, IceSession, RemoteIceCredentials};
    use quinn::AsyncUdpSocket;
    use webrtc_ice::{
        agent::agent_config::AgentConfig, candidate::CandidateType, mdns::MulticastDnsMode,
        network_type::NetworkType,
    };

    #[tokio::test]
    async fn two_agents_gather_exchange_candidates_and_nominate_ipv4() -> anyhow::Result<()> {
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
