//! Authenticated call transport lifecycle for Gate C-1.
//!
//! A path connector performs the path-specific setup (signaling and ICE for
//! direct UDP, or authenticated `RelayClient` plus `RelayQuinnSocket` for WSS)
//! and returns an established Quinn connection. This controller owns bounded
//! direct-first selection, room authentication before Kymux, one alternate-path
//! reconnect, cancellation and transport teardown.

use std::{future::Future, pin::Pin, time::Duration};

use ed25519_dalek::SigningKey;
use quinn::Connection;
use skyber_signaling::{AuthenticatedPeers, PeerIdentity};
use thiserror::Error;
use tokio::{sync::watch, time::timeout};

use crate::{
    path_policy::{CallPathPolicy, NetworkPath, PathAttempt, PathPolicyError},
    room_auth::{AuthenticatedRoomConnection, RoomAuthError, authenticate_room},
};

const PATH_CONNECT_TIMEOUT: Duration = Duration::from_secs(110);

/// Adapter that constructs a raw QUIC connection on the requested path.
///
/// Implementations must configure peer TLS validation and must not initialize
/// Kymux. The controller performs the room-identity exporter proof immediately
/// after this returns.
pub trait CallPathConnector {
    type ConnectFuture<'a>: Future<Output = Result<Connection, String>> + Send + 'a
    where
        Self: 'a;

    fn connect(&mut self, path: NetworkPath) -> Self::ConnectFuture<'_>;

    /// Release endpoint/socket resources associated with a successful path.
    fn close(&mut self) {}

    /// Receive an authenticated room lifecycle event from signaling.
    fn next_event(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<CallEvent, String>> + Send + '_>> {
        Box::pin(std::future::pending())
    }

    /// Notify the signaling peer that this call has ended.
    fn notify_hangup(&mut self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallEvent {
    PeerDisconnected,
    PeerResumed,
    PeerHungUp,
}

/// Owns one call's direct-first authenticated QUIC path and bounded recovery.
pub struct CallController<C> {
    connector: C,
    peers: AuthenticatedPeers,
    local_key: SigningKey,
    local_peer: PeerIdentity,
    policy: CallPathPolicy,
    active: Option<ConnectedCall>,
}

impl<C: CallPathConnector> CallController<C> {
    pub fn new(
        connector: C,
        peers: AuthenticatedPeers,
        local_key: SigningKey,
    ) -> Result<Self, CallControllerError> {
        let local_peer = PeerIdentity::from_signing_key(&local_key);
        if peers.inviter != local_peer && peers.invitee != local_peer {
            return Err(CallControllerError::LocalIdentityMismatch);
        }
        Ok(Self {
            connector,
            peers,
            local_key,
            local_peer,
            policy: CallPathPolicy::new(),
            active: None,
        })
    }

    /// Establish a room-authenticated connection, trying direct UDP and then
    /// the WSS relay. Authentication failure is terminal and never triggers a
    /// path fallback.
    pub async fn connect(
        &mut self,
        cancellation: &mut watch::Receiver<bool>,
    ) -> Result<&ConnectedCall, CallControllerError> {
        if self.active.is_some() {
            return Err(CallControllerError::AlreadyConnected);
        }
        let mut attempt = self.policy.start()?;
        loop {
            match self.connect_attempt(attempt, cancellation).await {
                Ok(call) => {
                    self.policy.connected(attempt)?;
                    self.active = Some(call);
                    return self.active.as_ref().ok_or(CallControllerError::Closed);
                }
                Err(CallControllerError::PathFailed { path, reason }) => {
                    let next = self.policy.failed(attempt)?;
                    if let Some(next_attempt) = next {
                        attempt = next_attempt;
                    } else {
                        self.connector.close();
                        return Err(CallControllerError::AttemptsExhausted { path, reason });
                    }
                }
                Err(error) => {
                    self.policy.close();
                    self.connector.close();
                    return Err(error);
                }
            }
        }
    }

    /// Replace a failed connection with one new authenticated connection on
    /// the alternate path. The call is closed if bounded recovery fails.
    pub async fn reconnect(
        &mut self,
        cancellation: &mut watch::Receiver<bool>,
    ) -> Result<&ConnectedCall, CallControllerError> {
        let current = self.active.take().ok_or(CallControllerError::Closed)?;
        current.close("call path lost");
        self.connector.close();
        let Some(attempt) = self.policy.connection_lost()? else {
            return Err(CallControllerError::RecoveryExhausted);
        };
        match self.connect_attempt(attempt, cancellation).await {
            Ok(call) => {
                self.policy.connected(attempt)?;
                self.active = Some(call);
                self.active.as_ref().ok_or(CallControllerError::Closed)
            }
            Err(error) => {
                self.policy.failed(attempt)?;
                self.policy.close();
                self.connector.close();
                Err(error)
            }
        }
    }

    /// Close the active QUIC connection and reject any late policy results.
    pub fn hangup(&mut self) {
        if let Some(call) = self.active.take() {
            call.close("call ended");
        }
        self.connector.close();
        self.policy.close();
    }

    pub fn active_call(&self) -> Option<&ConnectedCall> {
        self.active.as_ref()
    }

    pub async fn next_event(&mut self) -> Result<CallEvent, CallControllerError> {
        if self.active.is_none() {
            return Err(CallControllerError::Closed);
        }
        self.connector
            .next_event()
            .await
            .map_err(CallControllerError::Signaling)
    }

    /// Close the active QUIC path and send the room hang-up event.
    pub async fn hangup_and_notify(&mut self) -> Result<(), CallControllerError> {
        self.hangup();
        self.connector
            .notify_hangup()
            .await
            .map_err(CallControllerError::Signaling)
    }

    /// Access adapter configuration for the next path attempt or recovery.
    pub fn connector_mut(&mut self) -> &mut C {
        &mut self.connector
    }

    async fn connect_attempt(
        &mut self,
        attempt: PathAttempt,
        cancellation: &mut watch::Receiver<bool>,
    ) -> Result<ConnectedCall, CallControllerError> {
        if *cancellation.borrow() {
            return Err(CallControllerError::Cancelled);
        }
        let path = path_of(attempt);
        let raw_connection = tokio::select! {
            result = timeout(PATH_CONNECT_TIMEOUT, self.connector.connect(path)) => {
                match result {
                    Err(_) => return Err(CallControllerError::PathFailed {
                        path,
                        reason: "path setup timed out".to_owned(),
                    }),
                    Ok(Err(reason)) => return Err(CallControllerError::PathFailed { path, reason }),
                    Ok(Ok(connection)) => connection,
                }
            }
            _ = wait_for_cancellation(cancellation) => return Err(CallControllerError::Cancelled),
        };

        let connection_for_cancel = raw_connection.clone();
        let authenticated = tokio::select! {
            result = authenticate_room(raw_connection, self.peers, &self.local_key) => {
                result.map_err(CallControllerError::Authentication)?
            }
            _ = wait_for_cancellation(cancellation) => {
                connection_for_cancel.close(
                    quinn::VarInt::from_u32(0xC1),
                    b"call cancelled during room authentication",
                );
                return Err(CallControllerError::Cancelled);
            }
        };
        Ok(ConnectedCall {
            path,
            local_peer: self.local_peer,
            authenticated,
        })
    }
}

/// A QUIC session whose room identity exchange completed before Kymux setup.
pub struct ConnectedCall {
    path: NetworkPath,
    local_peer: PeerIdentity,
    authenticated: AuthenticatedRoomConnection,
}

impl ConnectedCall {
    pub const fn path(&self) -> NetworkPath {
        self.path
    }

    pub const fn local_peer(&self) -> PeerIdentity {
        self.local_peer
    }

    pub fn quic_connection(&self) -> &Connection {
        self.authenticated.as_inner()
    }

    fn close(&self, reason: &'static str) {
        self.quic_connection()
            .close(quinn::VarInt::from_u32(0), reason.as_bytes());
    }
}

impl std::fmt::Debug for ConnectedCall {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConnectedCall")
            .field("path", &self.path)
            .field("local_peer", &self.local_peer)
            .field("remote_address", &self.quic_connection().remote_address())
            .finish()
    }
}

fn path_of(attempt: PathAttempt) -> NetworkPath {
    match attempt {
        PathAttempt::Initial(path) | PathAttempt::Recovery(path) => path,
    }
}

async fn wait_for_cancellation(cancellation: &mut watch::Receiver<bool>) {
    while !*cancellation.borrow() {
        if cancellation.changed().await.is_err() {
            return;
        }
    }
}

#[derive(Debug, Error)]
pub enum CallControllerError {
    #[error("local signing key is not one of the accepted room identities")]
    LocalIdentityMismatch,
    #[error("a call is already active")]
    AlreadyConnected,
    #[error("call is closed or has no active path")]
    Closed,
    #[error("call setup was cancelled")]
    Cancelled,
    #[error("call signaling event failed: {0}")]
    Signaling(String),
    #[error("both direct and relay path attempts failed at {path:?}: {reason}")]
    AttemptsExhausted { path: NetworkPath, reason: String },
    #[error("the bounded reconnect attempt was exhausted")]
    RecoveryExhausted,
    #[error("path policy transition failed: {0}")]
    Policy(#[from] PathPolicyError),
    #[error("room identity authentication failed: {0}")]
    Authentication(#[source] RoomAuthError),
    #[error("path {path:?} failed: {reason}")]
    PathFailed { path: NetworkPath, reason: String },
}

#[cfg(test)]
mod tests {
    use std::{future::Ready, future::ready};

    use ed25519_dalek::SigningKey;
    use skyber_signaling::{AuthenticatedPeers, PeerIdentity, RoomId};
    use tokio::sync::watch;

    use super::{CallController, CallControllerError, CallPathConnector, NetworkPath};

    #[derive(Default)]
    struct FailingConnector {
        attempts: Vec<NetworkPath>,
    }

    impl CallPathConnector for FailingConnector {
        type ConnectFuture<'a> = Ready<Result<quinn::Connection, String>>;

        fn connect(&mut self, path: NetworkPath) -> Self::ConnectFuture<'_> {
            self.attempts.push(path);
            ready(Err("test path failure".to_owned()))
        }
    }

    fn peers(inviter: &SigningKey, invitee: &SigningKey) -> AuthenticatedPeers {
        AuthenticatedPeers {
            room: RoomId::from_bytes([7; 16]),
            inviter: PeerIdentity::from_signing_key(inviter),
            invitee: PeerIdentity::from_signing_key(invitee),
        }
    }

    #[tokio::test]
    async fn failed_direct_path_falls_back_to_relay_then_stops() {
        let inviter = SigningKey::from_bytes(&[11; 32]);
        let invitee = SigningKey::from_bytes(&[22; 32]);
        let mut controller = CallController::new(
            FailingConnector::default(),
            peers(&inviter, &invitee),
            inviter,
        )
        .expect("inviter should be an accepted room peer");
        let (_cancel, mut cancellation) = watch::channel(false);

        let result = controller.connect(&mut cancellation).await;

        assert!(matches!(
            result,
            Err(CallControllerError::AttemptsExhausted {
                path: NetworkPath::WebSocketRelay,
                ..
            })
        ));
        assert_eq!(
            controller.connector.attempts,
            [NetworkPath::DirectUdp, NetworkPath::WebSocketRelay]
        );
    }

    #[tokio::test]
    async fn cancelled_setup_does_not_start_a_path_attempt() {
        let inviter = SigningKey::from_bytes(&[11; 32]);
        let invitee = SigningKey::from_bytes(&[22; 32]);
        let mut controller = CallController::new(
            FailingConnector::default(),
            peers(&inviter, &invitee),
            inviter,
        )
        .expect("inviter should be an accepted room peer");
        let (_cancel, mut cancellation) = watch::channel(true);

        assert!(matches!(
            controller.connect(&mut cancellation).await,
            Err(CallControllerError::Cancelled)
        ));
        assert!(controller.connector.attempts.is_empty());
    }

    #[test]
    fn rejects_local_identity_outside_the_room() {
        let inviter = SigningKey::from_bytes(&[11; 32]);
        let invitee = SigningKey::from_bytes(&[22; 32]);
        let stranger = SigningKey::from_bytes(&[33; 32]);

        assert!(matches!(
            CallController::new(
                FailingConnector::default(),
                peers(&inviter, &invitee),
                stranger,
            ),
            Err(CallControllerError::LocalIdentityMismatch)
        ));
    }
}
