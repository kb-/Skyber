//! Bounded initial path selection and one-shot recovery policy for Gate C-1.

use thiserror::Error;

/// Candidate transport path for a new authenticated Kyber/QUIC connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkPath {
    /// A nominated ICE candidate pair backed by UDP.
    DirectUdp,
    /// A WebSocket tunnel that preserves QUIC datagram boundaries.
    WebSocketRelay,
}

impl NetworkPath {
    const fn alternate(self) -> Self {
        match self {
            Self::DirectUdp => Self::WebSocketRelay,
            Self::WebSocketRelay => Self::DirectUdp,
        }
    }
}

/// An attempt to establish a fresh authenticated QUIC connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathAttempt {
    /// Initial call setup tries direct UDP before the relay.
    Initial(NetworkPath),
    /// One fresh-connection recovery attempt after an established path fails.
    Recovery(NetworkPath),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Ready,
    Attempting(PathAttempt),
    Connected(NetworkPath),
    Failed,
    Closed,
}

/// Pure path policy. It does not perform ICE, create sockets, migrate QUIC, or
/// restore media tracks; the caller carries out each returned attempt.
#[derive(Debug)]
pub struct CallPathPolicy {
    phase: Phase,
    recovery_used: bool,
}

impl Default for CallPathPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl CallPathPolicy {
    pub const fn new() -> Self {
        Self {
            phase: Phase::Ready,
            recovery_used: false,
        }
    }

    /// Begin setup with the direct UDP path.
    pub fn start(&mut self) -> Result<PathAttempt, PathPolicyError> {
        if self.phase != Phase::Ready {
            return Err(PathPolicyError::InvalidTransition);
        }
        let attempt = PathAttempt::Initial(NetworkPath::DirectUdp);
        self.phase = Phase::Attempting(attempt);
        Ok(attempt)
    }

    /// Record that an attempt established a new authenticated connection.
    pub fn connected(&mut self, attempt: PathAttempt) -> Result<NetworkPath, PathPolicyError> {
        self.require_attempt(attempt)?;
        let path = path_of(attempt);
        self.phase = Phase::Connected(path);
        Ok(path)
    }

    /// Record a failed attempt. Initial direct failure advances to the relay;
    /// every other failure exhausts the bounded policy.
    pub fn failed(&mut self, attempt: PathAttempt) -> Result<Option<PathAttempt>, PathPolicyError> {
        self.require_attempt(attempt)?;
        let next = match attempt {
            PathAttempt::Initial(NetworkPath::DirectUdp) => {
                Some(PathAttempt::Initial(NetworkPath::WebSocketRelay))
            }
            PathAttempt::Initial(NetworkPath::WebSocketRelay) | PathAttempt::Recovery(_) => None,
        };
        self.phase = next.map_or(Phase::Failed, Phase::Attempting);
        Ok(next)
    }

    /// On active-connection loss, request one fresh authenticated connection
    /// on the alternate path. The policy never assumes QUIC migration succeeds.
    pub fn connection_lost(&mut self) -> Result<Option<PathAttempt>, PathPolicyError> {
        let Phase::Connected(path) = self.phase else {
            return Err(PathPolicyError::InvalidTransition);
        };
        if self.recovery_used {
            self.phase = Phase::Failed;
            return Ok(None);
        }
        self.recovery_used = true;
        let attempt = PathAttempt::Recovery(path.alternate());
        self.phase = Phase::Attempting(attempt);
        Ok(Some(attempt))
    }

    /// Close the call and reject all late attempt results.
    pub fn close(&mut self) {
        self.phase = Phase::Closed;
    }

    fn require_attempt(&self, attempt: PathAttempt) -> Result<(), PathPolicyError> {
        if self.phase == Phase::Attempting(attempt) {
            Ok(())
        } else {
            Err(PathPolicyError::InvalidTransition)
        }
    }
}

const fn path_of(attempt: PathAttempt) -> NetworkPath {
    match attempt {
        PathAttempt::Initial(path) | PathAttempt::Recovery(path) => path,
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum PathPolicyError {
    #[error("path policy received an out-of-order or stale connection result")]
    InvalidTransition,
}

#[cfg(test)]
mod tests {
    use super::{CallPathPolicy, NetworkPath, PathAttempt, PathPolicyError};

    #[test]
    fn setup_tries_direct_then_relay_and_stops_after_both_fail() {
        let mut policy = CallPathPolicy::new();
        let direct = policy.start().expect("setup should start");
        assert_eq!(direct, PathAttempt::Initial(NetworkPath::DirectUdp));
        let relay = policy.failed(direct).expect("direct failure is current");
        assert_eq!(
            relay,
            Some(PathAttempt::Initial(NetworkPath::WebSocketRelay))
        );
        assert_eq!(
            policy
                .failed(relay.expect("relay attempt should be returned"))
                .expect("relay failure is current"),
            None
        );
        assert!(matches!(
            policy.start(),
            Err(PathPolicyError::InvalidTransition)
        ));
    }

    #[test]
    fn established_path_gets_one_fresh_connection_on_the_alternate() {
        let mut policy = CallPathPolicy::new();
        let initial = policy.start().expect("setup should start");
        assert_eq!(
            policy.connected(initial).expect("direct should connect"),
            NetworkPath::DirectUdp
        );
        let recovery = policy
            .connection_lost()
            .expect("connection loss is valid")
            .expect("one recovery should be allowed");
        assert_eq!(recovery, PathAttempt::Recovery(NetworkPath::WebSocketRelay));
        assert_eq!(
            policy.connected(recovery).expect("relay reconnects"),
            NetworkPath::WebSocketRelay
        );
        assert_eq!(policy.connection_lost(), Ok(None));
    }

    #[test]
    fn stale_results_and_closed_calls_are_rejected() {
        let mut policy = CallPathPolicy::new();
        let direct = policy.start().expect("setup should start");
        assert_eq!(
            policy.connected(PathAttempt::Initial(NetworkPath::WebSocketRelay)),
            Err(PathPolicyError::InvalidTransition)
        );
        policy.close();
        assert_eq!(
            policy.failed(direct),
            Err(PathPolicyError::InvalidTransition)
        );
    }
}
