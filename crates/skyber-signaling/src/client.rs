//! Typed client operations for the TLS WebSocket signaling protocol.
//!
//! The caller supplies a WebSocket that has already completed TLS validation.
//! Production code must use `wss://` with a trusted CA or an explicitly
//! configured certificate pin before constructing this client.

use std::{sync::Arc, time::Duration};

use ed25519_dalek::{Signer, SigningKey};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use thiserror::Error;
use tokio::time::timeout;
use tokio_tungstenite::{
    Connector, MaybeTlsStream, connect_async_tls_with_config, tungstenite::Error as WebSocketError,
    tungstenite::Message,
};
use zeroize::Zeroize;

use crate::{
    Invitation, InvitationToken, PeerIdentity, RoomId, accept_message, invite_request_message,
    relay_message, resume_message,
    server::{ClientMessage, ServerMessage, WireInvitationToken, WireSecretString},
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_WS_MESSAGE: usize = 8 * 1024;
const MAX_CANDIDATE_LENGTH: usize = 1024;
const MAX_UFRAG_LENGTH: usize = 32;
const MIN_ICE_PASSWORD_LENGTH: usize = 22;
const MAX_ICE_PASSWORD_LENGTH: usize = 256;
const MAX_RELAY_DATAGRAM: usize = 1200;

pub type TlsSignalWebSocket =
    tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Creates a signaling client over WSS using an explicit TLS trust policy.
/// The supplied rustls configuration should use the platform trust store or a
/// service pin; this connector never disables certificate validation.
pub struct TlsSignalWebSocketConnector {
    endpoint: String,
    tls_config: Option<Arc<rustls::ClientConfig>>,
}

impl TlsSignalWebSocketConnector {
    pub fn new(
        endpoint: impl Into<String>,
        tls_config: Arc<rustls::ClientConfig>,
    ) -> Result<Self, SignalClientError> {
        let endpoint = endpoint.into();
        crate::validate_invitation_endpoint(&endpoint)
            .map_err(|_| SignalClientError::InvalidEndpoint)?;
        Ok(Self {
            endpoint,
            tls_config: Some(tls_config),
        })
    }

    /// Use the platform's native CA store. This requires native-root support,
    /// enabled by the signaling crate's tokio-tungstenite feature set.
    pub fn with_platform_roots(endpoint: impl Into<String>) -> Result<Self, SignalClientError> {
        let endpoint = endpoint.into();
        crate::validate_invitation_endpoint(&endpoint)
            .map_err(|_| SignalClientError::InvalidEndpoint)?;
        Ok(Self {
            endpoint,
            tls_config: None,
        })
    }

    pub async fn connect(&self) -> Result<SignalClient<TlsSignalWebSocket>, SignalClientError> {
        let connector = self
            .tls_config
            .as_ref()
            .map(|config| Connector::Rustls(Arc::clone(config)));
        let (websocket, _) =
            connect_async_tls_with_config(self.endpoint.as_str(), None, true, connector).await?;
        Ok(SignalClient::from_authenticated_websocket(websocket))
    }
}

/// Typed signaling client over an already authenticated WebSocket connection.
pub struct SignalClient<S> {
    websocket: S,
}

/// Authenticated WSS datagram tunnel for carrying opaque encrypted QUIC packets.
pub struct RelayClient<S> {
    websocket: S,
    room: RoomId,
}

impl<S> RelayClient<S>
where
    S: Sink<Message, Error = WebSocketError>
        + Stream<Item = Result<Message, WebSocketError>>
        + Unpin,
{
    /// Join the room relay using the separate per-peer relay credential.
    pub async fn join(
        mut websocket: S,
        room: RoomId,
        relay_token: &InvitationToken,
        signing_key: &SigningKey,
    ) -> Result<Self, SignalClientError> {
        let peer = PeerIdentity::from_signing_key(signing_key);
        let signature = signing_key.sign(&relay_message(room, peer, relay_token.expose()));
        let request = ClientMessage::RelayJoin {
            room: *room.as_bytes(),
            peer: *peer.as_bytes(),
            token: WireInvitationToken(*relay_token.expose()),
            signature: signature.to_bytes().to_vec(),
        };
        let text = serde_json::to_string(&request).map_err(SignalClientError::Json)?;
        timeout(REQUEST_TIMEOUT, websocket.send(Message::Text(text.into())))
            .await
            .map_err(|_| SignalClientError::Timeout)?
            .map_err(SignalClientError::WebSocket)?;

        let ready = timeout(REQUEST_TIMEOUT, websocket.next())
            .await
            .map_err(|_| SignalClientError::Timeout)?
            .ok_or(SignalClientError::Closed)?
            .map_err(SignalClientError::WebSocket)?;
        let Message::Text(ready) = ready else {
            return Err(SignalClientError::UnexpectedFrame);
        };
        if ready.len() > MAX_WS_MESSAGE {
            return Err(SignalClientError::InvalidFieldLength);
        }
        match serde_json::from_str::<ServerMessage>(&ready).map_err(SignalClientError::Json)? {
            ServerMessage::RelayReady { room: ready_room } if ready_room == *room.as_bytes() => {
                Ok(Self { websocket, room })
            }
            ServerMessage::Error { code } => Err(SignalClientError::Rejected(code)),
            _ => Err(SignalClientError::UnexpectedResponse),
        }
    }

    /// Forward one bounded QUIC datagram while preserving its packet boundary.
    pub async fn send_datagram(&mut self, datagram: &[u8]) -> Result<(), SignalClientError> {
        if datagram.is_empty() || datagram.len() > MAX_RELAY_DATAGRAM {
            return Err(SignalClientError::InvalidFieldLength);
        }
        timeout(
            REQUEST_TIMEOUT,
            self.websocket
                .send(Message::Binary(datagram.to_vec().into())),
        )
        .await
        .map_err(|_| SignalClientError::Timeout)?
        .map_err(SignalClientError::WebSocket)
    }

    /// Receive one bounded QUIC datagram from the other authenticated peer.
    pub async fn receive_datagram(&mut self) -> Result<Vec<u8>, SignalClientError> {
        let frame = timeout(REQUEST_TIMEOUT, self.websocket.next())
            .await
            .map_err(|_| SignalClientError::Timeout)?
            .ok_or(SignalClientError::Closed)?
            .map_err(SignalClientError::WebSocket)?;
        let Message::Binary(datagram) = frame else {
            return Err(SignalClientError::UnexpectedFrame);
        };
        if datagram.is_empty() || datagram.len() > MAX_RELAY_DATAGRAM {
            return Err(SignalClientError::InvalidFieldLength);
        }
        Ok(datagram.to_vec())
    }

    /// Close the relay tunnel and release the server's peer registration.
    pub async fn close(&mut self) -> Result<(), SignalClientError> {
        self.websocket
            .close()
            .await
            .map_err(SignalClientError::WebSocket)
    }

    pub const fn room(&self) -> RoomId {
        self.room
    }

    /// Transfer the already authenticated relay WebSocket into a datagram
    /// transport adapter such as Quinn's `AsyncUdpSocket` bridge.
    pub fn into_websocket(self) -> S {
        self.websocket
    }
}

impl<S> SignalClient<S>
where
    S: Sink<Message, Error = WebSocketError>
        + Stream<Item = Result<Message, WebSocketError>>
        + Unpin,
{
    /// Wrap a WebSocket after its TLS peer certificate has been validated.
    ///
    /// This type does not perform TLS itself so callers can apply their
    /// platform's trust store or an explicit service certificate pin.
    pub fn from_authenticated_websocket(websocket: S) -> Self {
        Self { websocket }
    }

    /// Create a signed invitation for a specific peer identity.
    pub async fn create_invitation(
        &mut self,
        invitee: PeerIdentity,
        signing_key: &SigningKey,
    ) -> Result<Invitation, SignalClientError> {
        let inviter = PeerIdentity::from_signing_key(signing_key);
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce).map_err(|_| SignalClientError::RandomUnavailable)?;
        let signature = signing_key.sign(&invite_request_message(inviter, invitee, nonce));
        self.send(ClientMessage::CreateInvitation {
            inviter: *inviter.as_bytes(),
            invitee: *invitee.as_bytes(),
            nonce,
            signature: signature.to_bytes().to_vec(),
        })
        .await?;

        match self.receive_message().await? {
            ServerMessage::InvitationCreated {
                room,
                token,
                expires_in_seconds,
            } => Ok(Invitation {
                room: RoomId::from_bytes(room),
                token: InvitationToken(token.0),
                expires_in: Duration::from_secs(u64::from(expires_in_seconds)),
            }),
            ServerMessage::Error { code } => Err(SignalClientError::Rejected(code)),
            _ => Err(SignalClientError::UnexpectedResponse),
        }
    }

    /// Accept a one-use invitation and verify the signaling service reports
    /// the expected inviter identity before returning reconnect credentials.
    pub async fn accept_invitation(
        &mut self,
        room: RoomId,
        token: &InvitationToken,
        expected_inviter: PeerIdentity,
        signing_key: &SigningKey,
    ) -> Result<AcceptedRoom, SignalClientError> {
        let invitee = PeerIdentity::from_signing_key(signing_key);
        let signature = signing_key.sign(&accept_message(room, invitee, token.expose()));
        self.send(ClientMessage::AcceptInvitation {
            room: *room.as_bytes(),
            invitee: *invitee.as_bytes(),
            token: WireInvitationToken(*token.expose()),
            signature: signature.to_bytes().to_vec(),
        })
        .await?;

        match self.receive_message().await? {
            ServerMessage::InvitationAccepted {
                room: accepted_room,
                remote_peer,
                resume_token,
                relay_token,
            } => {
                let remote_peer = PeerIdentity::from_bytes(remote_peer)
                    .map_err(|_| SignalClientError::InvalidIdentity)?;
                let accepted_room = RoomId::from_bytes(accepted_room);
                if accepted_room != room || remote_peer != expected_inviter {
                    return Err(SignalClientError::PeerBindingMismatch);
                }
                Ok(AcceptedRoom {
                    room,
                    remote_peer,
                    credentials: PeerCredentials {
                        resume: InvitationToken(resume_token.0),
                        relay: InvitationToken(relay_token.0),
                    },
                })
            }
            ServerMessage::Error { code } => Err(SignalClientError::Rejected(code)),
            _ => Err(SignalClientError::UnexpectedResponse),
        }
    }

    /// Reauthenticate a disconnected peer using its separate reconnect token.
    pub async fn resume(
        &mut self,
        room: RoomId,
        credential: &InvitationToken,
        signing_key: &SigningKey,
    ) -> Result<(), SignalClientError> {
        let peer = PeerIdentity::from_signing_key(signing_key);
        let signature = signing_key.sign(&resume_message(room, peer, credential.expose()));
        self.send(ClientMessage::ResumeRoom {
            room: *room.as_bytes(),
            peer: *peer.as_bytes(),
            token: WireInvitationToken(*credential.expose()),
            signature: signature.to_bytes().to_vec(),
        })
        .await
    }

    /// Forward a gathered ICE candidate to the authenticated room peer.
    pub async fn send_candidate(
        &mut self,
        room: RoomId,
        candidate: String,
    ) -> Result<(), SignalClientError> {
        if candidate.is_empty() || candidate.len() > MAX_CANDIDATE_LENGTH {
            return Err(SignalClientError::InvalidFieldLength);
        }
        self.send(ClientMessage::Candidate {
            room: *room.as_bytes(),
            candidate,
        })
        .await
    }

    /// Tell the room peer that this ICE agent has finished gathering candidates.
    pub async fn finish_candidate_gathering(
        &mut self,
        room: RoomId,
    ) -> Result<(), SignalClientError> {
        self.send(ClientMessage::CandidatesComplete {
            room: *room.as_bytes(),
        })
        .await
    }

    /// Forward ICE credentials to the authenticated room peer.
    pub async fn send_ice_credentials(
        &mut self,
        room: RoomId,
        ufrag: String,
        password: String,
    ) -> Result<(), SignalClientError> {
        if ufrag.is_empty()
            || ufrag.len() > MAX_UFRAG_LENGTH
            || password.len() < MIN_ICE_PASSWORD_LENGTH
            || password.len() > MAX_ICE_PASSWORD_LENGTH
        {
            return Err(SignalClientError::InvalidFieldLength);
        }
        self.send(ClientMessage::IceCredentials {
            room: *room.as_bytes(),
            ufrag: WireSecretString(ufrag),
            password: WireSecretString(password),
        })
        .await
    }

    /// Notify the room peer and close this call's signaling state.
    pub async fn hangup(&mut self, room: RoomId) -> Result<(), SignalClientError> {
        self.send(ClientMessage::Hangup {
            room: *room.as_bytes(),
        })
        .await
    }

    /// Wait for the next peer event. The timeout bounds stalled setup and
    /// connection-loss handling; applications can call again while connected.
    pub async fn receive(&mut self) -> Result<SignalEvent, SignalClientError> {
        match self.receive_message().await? {
            ServerMessage::InvitationCreated { .. } => Err(SignalClientError::UnexpectedResponse),
            ServerMessage::InvitationAccepted {
                room,
                remote_peer,
                resume_token,
                relay_token,
            } => Ok(SignalEvent::InvitationAccepted(AcceptedRoom {
                room: RoomId::from_bytes(room),
                remote_peer: PeerIdentity::from_bytes(remote_peer)
                    .map_err(|_| SignalClientError::InvalidIdentity)?,
                credentials: PeerCredentials {
                    resume: InvitationToken(resume_token.0),
                    relay: InvitationToken(relay_token.0),
                },
            })),
            ServerMessage::PeerDisconnected { room } => {
                Ok(SignalEvent::PeerDisconnected(RoomId::from_bytes(room)))
            }
            ServerMessage::PeerResumed { room, remote_peer } => Ok(SignalEvent::PeerResumed {
                room: RoomId::from_bytes(room),
                remote_peer: PeerIdentity::from_bytes(remote_peer)
                    .map_err(|_| SignalClientError::InvalidIdentity)?,
            }),
            ServerMessage::RelayReady { .. } => Err(SignalClientError::UnexpectedResponse),
            ServerMessage::IceCredentials {
                room,
                from_peer,
                ufrag,
                password,
            } => {
                if ufrag.0.is_empty()
                    || ufrag.0.len() > MAX_UFRAG_LENGTH
                    || password.0.len() < MIN_ICE_PASSWORD_LENGTH
                    || password.0.len() > MAX_ICE_PASSWORD_LENGTH
                {
                    return Err(SignalClientError::InvalidFieldLength);
                }
                Ok(SignalEvent::IceCredentials {
                    room: RoomId::from_bytes(room),
                    from_peer: PeerIdentity::from_bytes(from_peer)
                        .map_err(|_| SignalClientError::InvalidIdentity)?,
                    ufrag: SecretString(ufrag.0.clone()),
                    password: SecretString(password.0.clone()),
                })
            }
            ServerMessage::Candidate {
                room,
                from_peer,
                candidate,
            } => {
                if candidate.is_empty() || candidate.len() > MAX_CANDIDATE_LENGTH {
                    return Err(SignalClientError::InvalidFieldLength);
                }
                Ok(SignalEvent::Candidate {
                    room: RoomId::from_bytes(room),
                    from_peer: PeerIdentity::from_bytes(from_peer)
                        .map_err(|_| SignalClientError::InvalidIdentity)?,
                    candidate,
                })
            }
            ServerMessage::CandidatesComplete { room, from_peer } => {
                Ok(SignalEvent::CandidatesComplete {
                    room: RoomId::from_bytes(room),
                    from_peer: PeerIdentity::from_bytes(from_peer)
                        .map_err(|_| SignalClientError::InvalidIdentity)?,
                })
            }
            ServerMessage::PeerLeft { room } => Ok(SignalEvent::PeerLeft(RoomId::from_bytes(room))),
            ServerMessage::Error { code } => Err(SignalClientError::Rejected(code)),
        }
    }

    /// Close the WebSocket cleanly during call teardown.
    pub async fn close(&mut self) -> Result<(), SignalClientError> {
        self.websocket
            .close()
            .await
            .map_err(SignalClientError::WebSocket)
    }

    async fn send(&mut self, message: ClientMessage) -> Result<(), SignalClientError> {
        let text = serde_json::to_string(&message).map_err(SignalClientError::Json)?;
        timeout(
            REQUEST_TIMEOUT,
            self.websocket.send(Message::Text(text.into())),
        )
        .await
        .map_err(|_| SignalClientError::Timeout)?
        .map_err(SignalClientError::WebSocket)
    }

    async fn receive_message(&mut self) -> Result<ServerMessage, SignalClientError> {
        let frame = timeout(REQUEST_TIMEOUT, self.websocket.next())
            .await
            .map_err(|_| SignalClientError::Timeout)?
            .ok_or(SignalClientError::Closed)?
            .map_err(SignalClientError::WebSocket)?;
        let Message::Text(text) = frame else {
            return Err(SignalClientError::UnexpectedFrame);
        };
        if text.len() > MAX_WS_MESSAGE {
            return Err(SignalClientError::InvalidFieldLength);
        }
        serde_json::from_str(&text).map_err(SignalClientError::Json)
    }
}

/// The peer and per-peer credentials returned after invitation acceptance.
pub struct AcceptedRoom {
    pub room: RoomId,
    pub remote_peer: PeerIdentity,
    pub credentials: PeerCredentials,
}

impl std::fmt::Debug for AcceptedRoom {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AcceptedRoom")
            .field("room", &self.room)
            .field("remote_peer", &self.remote_peer)
            .field("credentials", &"[redacted]")
            .finish()
    }
}

/// Separate reconnect and encrypted-relay credentials for one room peer.
pub struct PeerCredentials {
    resume: InvitationToken,
    relay: InvitationToken,
}

impl PeerCredentials {
    pub const fn resume_token(&self) -> &InvitationToken {
        &self.resume
    }

    pub const fn relay_token(&self) -> &InvitationToken {
        &self.relay
    }
}

impl std::fmt::Debug for PeerCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PeerCredentials([redacted])")
    }
}

/// Events routed by the signaling service for a connected room.
pub enum SignalEvent {
    InvitationAccepted(AcceptedRoom),
    PeerDisconnected(RoomId),
    PeerResumed {
        room: RoomId,
        remote_peer: PeerIdentity,
    },
    IceCredentials {
        room: RoomId,
        from_peer: PeerIdentity,
        ufrag: SecretString,
        password: SecretString,
    },
    Candidate {
        room: RoomId,
        from_peer: PeerIdentity,
        candidate: String,
    },
    CandidatesComplete {
        room: RoomId,
        from_peer: PeerIdentity,
    },
    PeerLeft(RoomId),
}

impl std::fmt::Debug for SignalEvent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvitationAccepted(accepted) => formatter
                .debug_tuple("InvitationAccepted")
                .field(accepted)
                .finish(),
            Self::PeerDisconnected(room) => formatter
                .debug_tuple("PeerDisconnected")
                .field(room)
                .finish(),
            Self::PeerResumed { room, remote_peer } => formatter
                .debug_struct("PeerResumed")
                .field("room", room)
                .field("remote_peer", remote_peer)
                .finish(),
            Self::IceCredentials {
                room, from_peer, ..
            } => formatter
                .debug_struct("IceCredentials")
                .field("room", room)
                .field("from_peer", from_peer)
                .field("credentials", &"[redacted]")
                .finish(),
            Self::Candidate {
                room,
                from_peer,
                candidate,
            } => formatter
                .debug_struct("Candidate")
                .field("room", room)
                .field("from_peer", from_peer)
                .field("candidate", candidate)
                .finish(),
            Self::CandidatesComplete { room, from_peer } => formatter
                .debug_struct("CandidatesComplete")
                .field("room", room)
                .field("from_peer", from_peer)
                .finish(),
            Self::PeerLeft(room) => formatter.debug_tuple("PeerLeft").field(room).finish(),
        }
    }
}

/// ICE credentials with redacted formatting and zeroizing drop behavior.
pub struct SecretString(String);

impl SecretString {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("[redacted]")
    }
}

#[derive(Debug, Error)]
pub enum SignalClientError {
    #[error("operating system random source failed")]
    RandomUnavailable,
    #[error("signaling service closed the WebSocket")]
    Closed,
    #[error(
        "signaling endpoint must be a valid wss:// URL without credentials, query, or fragment"
    )]
    InvalidEndpoint,
    #[error("signaling service returned an unexpected WebSocket frame")]
    UnexpectedFrame,
    #[error("signaling service returned an unexpected response")]
    UnexpectedResponse,
    #[error("signaling operation timed out")]
    Timeout,
    #[error("signaling request was rejected: {0}")]
    Rejected(String),
    #[error("signaling response contains an invalid peer identity")]
    InvalidIdentity,
    #[error("accepted invitation does not match the expected peer or room")]
    PeerBindingMismatch,
    #[error("ICE signaling field has an invalid length")]
    InvalidFieldLength,
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    WebSocket(#[from] WebSocketError),
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, sync::Arc, time::Duration};

    use ed25519_dalek::SigningKey;
    use tokio::{net::TcpListener, time::timeout};
    use tokio_rustls::TlsConnector;
    use tokio_tungstenite::client_async;

    use super::{RelayClient, SignalClient, SignalEvent};
    use crate::{PeerIdentity, TlsSignalWebSocketConnector, server};

    async fn websocket(
        address: SocketAddr,
        certificate: rustls::pki_types::CertificateDer<'static>,
        path: &str,
    ) -> anyhow::Result<
        tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>,
    > {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(certificate)?;
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(config));
        let tcp = tokio::net::TcpStream::connect(address).await?;
        let server_name = rustls::pki_types::ServerName::try_from("localhost".to_owned())?;
        let tls = connector.connect(server_name, tcp).await?;
        let (websocket, _) = client_async(format!("wss://localhost/{path}"), tls).await?;
        Ok(websocket)
    }

    async fn client(
        address: SocketAddr,
        certificate: rustls::pki_types::CertificateDer<'static>,
    ) -> anyhow::Result<SignalClient<super::TlsSignalWebSocket>> {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(certificate)?;
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = TlsSignalWebSocketConnector::new(
            format!("wss://localhost:{}/signal", address.port()),
            Arc::new(config),
        )?;
        Ok(connector.connect().await?)
    }

    #[tokio::test]
    async fn validated_wss_clients_create_and_accept_identity_bound_invitation()
    -> anyhow::Result<()> {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])?;
        let certificate = certified.cert.der().clone();
        let acceptor = server::tls_acceptor(
            certified.cert.pem().as_bytes(),
            certified.signing_key.serialize_pem().as_bytes(),
        )?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server_task = tokio::spawn(server::serve(listener, acceptor));

        let wrong_ca = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])?;
        let mut wrong_roots = rustls::RootCertStore::empty();
        wrong_roots.add(wrong_ca.cert.der().clone())?;
        let wrong_tls_config = Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(wrong_roots)
                .with_no_client_auth(),
        );
        let untrusted_connector = TlsSignalWebSocketConnector::new(
            format!("wss://localhost:{}/signal", address.port()),
            wrong_tls_config,
        )?;
        assert!(untrusted_connector.connect().await.is_err());

        let inviter_key = SigningKey::from_bytes(&[71; 32]);
        let invitee_key = SigningKey::from_bytes(&[72; 32]);
        let inviter_identity = PeerIdentity::from_signing_key(&inviter_key);
        let invitee_identity = PeerIdentity::from_signing_key(&invitee_key);
        let mut inviter = client(address, certificate.clone()).await?;
        let invitation = inviter
            .create_invitation(invitee_identity, &inviter_key)
            .await?;
        let mut invitee = client(address, certificate.clone()).await?;
        let accepted = invitee
            .accept_invitation(
                invitation.room,
                &invitation.token,
                inviter_identity,
                &invitee_key,
            )
            .await?;
        assert_eq!(accepted.room, invitation.room);
        assert_eq!(accepted.remote_peer, inviter_identity);
        let resume_token_json =
            serde_json::to_string(accepted.credentials.resume_token().expose())?;
        assert!(!format!("{:?}", accepted.credentials).contains(&resume_token_json));

        let SignalEvent::InvitationAccepted(inviter_accepted) =
            timeout(Duration::from_secs(2), inviter.receive()).await??
        else {
            anyhow::bail!("expected inviter acceptance event");
        };
        assert_eq!(inviter_accepted.remote_peer, invitee_identity);
        invitee
            .send_candidate(invitation.room, "candidate:synthetic-udp".to_owned())
            .await?;
        assert!(matches!(
            timeout(Duration::from_secs(2), inviter.receive()).await??,
            SignalEvent::Candidate { room, from_peer, candidate }
                if room == invitation.room && from_peer == invitee_identity && candidate == "candidate:synthetic-udp"
        ));
        invitee
            .send_ice_credentials(
                invitation.room,
                "local-ufrag".to_owned(),
                "local-ice-password-with-22-chars".to_owned(),
            )
            .await?;
        invitee.finish_candidate_gathering(invitation.room).await?;
        let SignalEvent::IceCredentials {
            room,
            from_peer,
            ufrag,
            password,
        } = timeout(Duration::from_secs(2), inviter.receive()).await??
        else {
            anyhow::bail!("expected forwarded ICE credentials");
        };
        assert_eq!(room, invitation.room);
        assert_eq!(from_peer, invitee_identity);
        assert_eq!(ufrag.expose(), "local-ufrag");
        assert_eq!(password.expose(), "local-ice-password-with-22-chars");
        assert_eq!(format!("{ufrag:?}"), "[redacted]");
        assert!(matches!(
            timeout(Duration::from_secs(2), inviter.receive()).await??,
            SignalEvent::CandidatesComplete { room, from_peer }
                if room == invitation.room && from_peer == invitee_identity
        ));

        let (mut inviter_relay, mut invitee_relay) = tokio::try_join!(
            RelayClient::join(
                websocket(address, certificate.clone(), "relay").await?,
                invitation.room,
                inviter_accepted.credentials.relay_token(),
                &inviter_key,
            ),
            RelayClient::join(
                websocket(address, certificate, "relay").await?,
                invitation.room,
                accepted.credentials.relay_token(),
                &invitee_key,
            ),
        )?;
        inviter_relay.send_datagram(&[1, 3, 5, 7]).await?;
        assert_eq!(invitee_relay.receive_datagram().await?, [1, 3, 5, 7]);
        invitee_relay.send_datagram(&[2, 4, 6]).await?;
        assert_eq!(inviter_relay.receive_datagram().await?, [2, 4, 6]);
        inviter_relay.close().await?;
        invitee_relay.close().await?;

        invitee.hangup(invitation.room).await?;
        assert!(matches!(
            timeout(Duration::from_secs(2), inviter.receive()).await??,
            SignalEvent::PeerLeft(room) if room == invitation.room
        ));
        inviter.close().await?;
        invitee.close().await?;
        server_task.abort();
        Ok(())
    }

    #[test]
    fn tls_signaling_connector_rejects_cleartext_and_secret_bearing_urls() {
        let config = Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(rustls::RootCertStore::empty())
                .with_no_client_auth(),
        );
        for endpoint in [
            "ws://calls.example.test/signal",
            "wss://user@calls.example.test/signal",
            "wss://calls.example.test/signal?token=secret",
            "wss://calls.example.test/signal#fragment",
        ] {
            assert!(matches!(
                TlsSignalWebSocketConnector::new(endpoint, Arc::clone(&config)),
                Err(super::SignalClientError::InvalidEndpoint),
            ));
        }
        assert!(
            TlsSignalWebSocketConnector::with_platform_roots("wss://calls.example.test/signal")
                .is_ok()
        );
    }
}
