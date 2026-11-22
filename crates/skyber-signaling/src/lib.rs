//! Identity-bound, one-use invitation state for Gate C-1 signaling.

use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};

use ed25519_dalek::{Signature, SigningKey, Verifier, VerifyingKey};
use subtle::ConstantTimeEq;
use thiserror::Error;
use zeroize::Zeroize;

const INVITE_DOMAIN: &[u8] = b"skyber/c1/invite/v1\0";
const ACCEPT_DOMAIN: &[u8] = b"skyber/c1/accept/v1\0";
const RESUME_DOMAIN: &[u8] = b"skyber/c1/resume/v1\0";
const RELAY_DOMAIN: &[u8] = b"skyber/c1/relay/v1\0";
const DEFAULT_INVITE_TTL: Duration = Duration::from_secs(120);
const DEFAULT_REQUEST_WINDOW: Duration = Duration::from_secs(60);
const DEFAULT_MAX_INVITES_PER_WINDOW: usize = 5;
const MAX_PENDING_INVITATIONS: usize = 8192;
const MAX_USED_NONCES: usize = 65_536;

/// Stable Ed25519 public identity. Construction rejects non-canonical keys.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct PeerIdentity([u8; 32]);

impl PeerIdentity {
    pub fn from_bytes(bytes: [u8; 32]) -> Result<Self, InvitationError> {
        VerifyingKey::from_bytes(&bytes).map_err(|_| InvitationError::InvalidIdentity)?;
        Ok(Self(bytes))
    }

    pub fn from_signing_key(key: &SigningKey) -> Self {
        Self(key.verifying_key().to_bytes())
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    fn verifying_key(&self) -> Result<VerifyingKey, InvitationError> {
        VerifyingKey::from_bytes(&self.0).map_err(|_| InvitationError::InvalidIdentity)
    }
}

impl std::fmt::Debug for PeerIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "PeerIdentity({})", hex_fingerprint(&self.0))
    }
}

/// Opaque random room identifier.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RoomId([u8; 16]);

impl RoomId {
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

/// One-use bearer ticket. Debug output never reveals its contents.
pub struct InvitationToken([u8; 32]);

impl InvitationToken {
    pub const fn expose(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Drop for InvitationToken {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for InvitationToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("InvitationToken([redacted])")
    }
}

/// Invitation response sent only to its authenticated creator.
pub struct Invitation {
    pub room: RoomId,
    pub token: InvitationToken,
    pub expires_in: Duration,
}

impl std::fmt::Debug for Invitation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Invitation")
            .field("room", &self.room)
            .field("token", &"[redacted]")
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// Result of authenticating both identities for one call room.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthenticatedPeers {
    pub room: RoomId,
    pub inviter: PeerIdentity,
    pub invitee: PeerIdentity,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum InvitationError {
    #[error("identity public key is invalid")]
    InvalidIdentity,
    #[error("operating system random source failed")]
    RandomUnavailable,
    #[error("invitation signature is invalid")]
    InvalidSignature,
    #[error("invitation request was already used")]
    ReplayedRequest,
    #[error("invitation request rate limit exceeded")]
    RateLimited,
    #[error("invitation service is at capacity")]
    CapacityExceeded,
    #[error("invitation could not be found")]
    NotFound,
    #[error("invitation has expired")]
    Expired,
    #[error("invitation is bound to a different peer")]
    WrongPeer,
}

struct PendingInvitation {
    inviter: PeerIdentity,
    invitee: PeerIdentity,
    token: InvitationToken,
    expires_at: Instant,
}

/// In-memory room and invitation state. Restarting the service invalidates
/// pending invitations; persistence is deliberately deferred until its threat
/// model and recovery behavior are specified.
pub struct InvitationRegistry {
    pending: HashMap<RoomId, PendingInvitation>,
    rate_windows: HashMap<PeerIdentity, VecDeque<Instant>>,
    used_nonces: HashMap<(PeerIdentity, [u8; 16]), Instant>,
    invite_ttl: Duration,
    request_window: Duration,
    max_invites_per_window: usize,
}

impl Default for InvitationRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl InvitationRegistry {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            rate_windows: HashMap::new(),
            used_nonces: HashMap::new(),
            invite_ttl: DEFAULT_INVITE_TTL,
            request_window: DEFAULT_REQUEST_WINDOW,
            max_invites_per_window: DEFAULT_MAX_INVITES_PER_WINDOW,
        }
    }

    /// Create an invite after verifying that the inviter owns the signing key.
    /// The signature binds the inviter, intended peer, and client-generated nonce.
    pub fn create_invitation(
        &mut self,
        inviter: PeerIdentity,
        invitee: PeerIdentity,
        nonce: [u8; 16],
        signature: &Signature,
    ) -> Result<Invitation, InvitationError> {
        self.create_invitation_at(inviter, invitee, nonce, signature, Instant::now())
    }

    fn create_invitation_at(
        &mut self,
        inviter: PeerIdentity,
        invitee: PeerIdentity,
        nonce: [u8; 16],
        signature: &Signature,
        now: Instant,
    ) -> Result<Invitation, InvitationError> {
        self.prune(now);
        if self.pending.len() >= MAX_PENDING_INVITATIONS {
            return Err(InvitationError::CapacityExceeded);
        }
        inviter
            .verifying_key()?
            .verify(&invite_request_message(inviter, invitee, nonce), signature)
            .map_err(|_| InvitationError::InvalidSignature)?;
        if inviter == invitee {
            return Err(InvitationError::WrongPeer);
        }

        let seen_until = now + self.invite_ttl;
        if self.used_nonces.contains_key(&(inviter, nonce)) {
            return Err(InvitationError::ReplayedRequest);
        }
        if self.used_nonces.len() >= MAX_USED_NONCES {
            return Err(InvitationError::CapacityExceeded);
        }
        let requests = self.rate_windows.entry(inviter).or_default();
        while requests.front().is_some_and(|timestamp| {
            now.saturating_duration_since(*timestamp) >= self.request_window
        }) {
            requests.pop_front();
        }
        if requests.len() >= self.max_invites_per_window {
            return Err(InvitationError::RateLimited);
        }

        let room = random_room_id()?;
        let token = random_token()?;
        let mut stored_token = [0_u8; 32];
        stored_token.copy_from_slice(token.expose());
        requests.push_back(now);
        self.used_nonces.insert((inviter, nonce), seen_until);
        self.pending.insert(
            room,
            PendingInvitation {
                inviter,
                invitee,
                token: InvitationToken(stored_token),
                expires_at: now + self.invite_ttl,
            },
        );
        Ok(Invitation {
            room,
            token,
            expires_in: self.invite_ttl,
        })
    }

    /// Consume the one-use ticket after verifying the invited peer's signature.
    pub fn accept_invitation(
        &mut self,
        room: RoomId,
        invitee: PeerIdentity,
        token: &[u8; 32],
        signature: &Signature,
    ) -> Result<AuthenticatedPeers, InvitationError> {
        self.accept_invitation_at(room, invitee, token, signature, Instant::now())
    }

    /// Cancel an unused invite when its authenticated creator disconnects.
    pub fn cancel_invitation(&mut self, room: RoomId, inviter: PeerIdentity) -> bool {
        if self
            .pending
            .get(&room)
            .is_some_and(|invitation| invitation.inviter == inviter)
        {
            self.pending.remove(&room);
            true
        } else {
            false
        }
    }

    fn accept_invitation_at(
        &mut self,
        room: RoomId,
        invitee: PeerIdentity,
        token: &[u8; 32],
        signature: &Signature,
        now: Instant,
    ) -> Result<AuthenticatedPeers, InvitationError> {
        if self
            .pending
            .get(&room)
            .is_some_and(|invitation| invitation.expires_at <= now)
        {
            self.pending.remove(&room);
            return Err(InvitationError::Expired);
        }
        self.prune(now);
        let Some(pending) = self.pending.get(&room) else {
            return Err(InvitationError::NotFound);
        };
        if now >= pending.expires_at {
            self.pending.remove(&room);
            return Err(InvitationError::Expired);
        }
        if invitee != pending.invitee {
            return Err(InvitationError::WrongPeer);
        }
        if !bool::from(pending.token.0.ct_eq(token)) {
            return Err(InvitationError::InvalidSignature);
        }
        invitee
            .verifying_key()?
            .verify(&accept_message(room, invitee, token), signature)
            .map_err(|_| InvitationError::InvalidSignature)?;

        let Some(consumed) = self.pending.remove(&room) else {
            return Err(InvitationError::NotFound);
        };
        Ok(AuthenticatedPeers {
            room,
            inviter: consumed.inviter,
            invitee: consumed.invitee,
        })
    }

    fn prune(&mut self, now: Instant) {
        self.pending
            .retain(|_, invitation| invitation.expires_at > now);
        self.used_nonces.retain(|_, expiry| *expiry > now);
        self.rate_windows.retain(|_, requests| {
            while requests.front().is_some_and(|timestamp| {
                now.saturating_duration_since(*timestamp) >= self.request_window
            }) {
                requests.pop_front();
            }
            !requests.is_empty()
        });
    }
}

pub mod server;

/// Bytes an inviter signs before requesting an invitation.
pub fn invite_request_message(
    inviter: PeerIdentity,
    invitee: PeerIdentity,
    nonce: [u8; 16],
) -> Vec<u8> {
    let mut message = Vec::with_capacity(INVITE_DOMAIN.len() + 80);
    message.extend_from_slice(INVITE_DOMAIN);
    message.extend_from_slice(inviter.as_bytes());
    message.extend_from_slice(invitee.as_bytes());
    message.extend_from_slice(&nonce);
    message
}

/// Bytes an invitee signs to accept a particular room ticket.
pub fn accept_message(room: RoomId, invitee: PeerIdentity, token: &[u8; 32]) -> Vec<u8> {
    let mut message = Vec::with_capacity(ACCEPT_DOMAIN.len() + 80);
    message.extend_from_slice(ACCEPT_DOMAIN);
    message.extend_from_slice(room.as_bytes());
    message.extend_from_slice(invitee.as_bytes());
    message.extend_from_slice(token);
    message
}

fn random_room_id() -> Result<RoomId, InvitationError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| InvitationError::RandomUnavailable)?;
    Ok(RoomId(bytes))
}

pub(crate) fn random_token() -> Result<InvitationToken, InvitationError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| InvitationError::RandomUnavailable)?;
    Ok(InvitationToken(bytes))
}

/// Bytes a peer signs when presenting its separate reconnect credential.
pub fn resume_message(room: RoomId, peer: PeerIdentity, token: &[u8; 32]) -> Vec<u8> {
    let mut message = Vec::with_capacity(RESUME_DOMAIN.len() + 80);
    message.extend_from_slice(RESUME_DOMAIN);
    message.extend_from_slice(room.as_bytes());
    message.extend_from_slice(peer.as_bytes());
    message.extend_from_slice(token);
    message
}

/// Bytes a peer signs to authorize its separate QUIC relay tunnel.
pub fn relay_message(room: RoomId, peer: PeerIdentity, token: &[u8; 32]) -> Vec<u8> {
    let mut message = Vec::with_capacity(RELAY_DOMAIN.len() + 80);
    message.extend_from_slice(RELAY_DOMAIN);
    message.extend_from_slice(room.as_bytes());
    message.extend_from_slice(peer.as_bytes());
    message.extend_from_slice(token);
    message
}

fn hex_fingerprint(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use ed25519_dalek::{Signature, Signer, SigningKey};

    use super::{
        AuthenticatedPeers, InvitationError, InvitationRegistry, PeerIdentity, accept_message,
        invite_request_message,
    };

    fn key(byte: u8) -> SigningKey {
        SigningKey::from_bytes(&[byte; 32])
    }

    fn identities() -> (SigningKey, SigningKey, PeerIdentity, PeerIdentity) {
        let inviter_key = key(11);
        let invitee_key = key(22);
        let inviter = PeerIdentity::from_signing_key(&inviter_key);
        let invitee = PeerIdentity::from_signing_key(&invitee_key);
        (inviter_key, invitee_key, inviter, invitee)
    }

    fn request_signature(
        inviter_key: &SigningKey,
        inviter: PeerIdentity,
        invitee: PeerIdentity,
        nonce: [u8; 16],
    ) -> Signature {
        inviter_key.sign(&invite_request_message(inviter, invitee, nonce))
    }

    #[test]
    fn invite_is_bound_to_two_identities_and_single_use() -> Result<(), InvitationError> {
        let (inviter_key, invitee_key, inviter, invitee) = identities();
        let mut registry = InvitationRegistry::new();
        let invitation = registry.create_invitation_at(
            inviter,
            invitee,
            [1; 16],
            &request_signature(&inviter_key, inviter, invitee, [1; 16]),
            Instant::now(),
        )?;
        let signature = invitee_key.sign(&accept_message(
            invitation.room,
            invitee,
            invitation.token.expose(),
        ));
        assert_eq!(
            registry.accept_invitation(
                invitation.room,
                invitee,
                invitation.token.expose(),
                &signature
            )?,
            AuthenticatedPeers {
                room: invitation.room,
                inviter,
                invitee
            },
        );
        assert_eq!(
            registry.accept_invitation(
                invitation.room,
                invitee,
                invitation.token.expose(),
                &signature
            ),
            Err(InvitationError::NotFound),
        );
        Ok(())
    }

    #[test]
    fn wrong_peer_cannot_consume_invite_and_nonce_cannot_be_replayed() -> Result<(), InvitationError>
    {
        let (inviter_key, invitee_key, inviter, invitee) = identities();
        let wrong_key = key(33);
        let wrong_peer = PeerIdentity::from_signing_key(&wrong_key);
        let mut registry = InvitationRegistry::new();
        let signature = request_signature(&inviter_key, inviter, invitee, [2; 16]);
        let invitation =
            registry.create_invitation_at(inviter, invitee, [2; 16], &signature, Instant::now())?;
        let wrong_signature = wrong_key.sign(&accept_message(
            invitation.room,
            wrong_peer,
            invitation.token.expose(),
        ));
        assert_eq!(
            registry.accept_invitation(
                invitation.room,
                wrong_peer,
                invitation.token.expose(),
                &wrong_signature
            ),
            Err(InvitationError::WrongPeer),
        );
        assert_eq!(
            registry
                .create_invitation_at(inviter, invitee, [2; 16], &signature, Instant::now())
                .err(),
            Some(InvitationError::ReplayedRequest),
        );
        let valid_signature = invitee_key.sign(&accept_message(
            invitation.room,
            invitee,
            invitation.token.expose(),
        ));
        registry.accept_invitation(
            invitation.room,
            invitee,
            invitation.token.expose(),
            &valid_signature,
        )?;
        Ok(())
    }

    #[test]
    fn expired_invite_is_rejected() -> Result<(), InvitationError> {
        let (inviter_key, _, inviter, invitee) = identities();
        let mut registry = InvitationRegistry::new();
        let now = Instant::now();
        let signature = request_signature(&inviter_key, inviter, invitee, [3; 16]);
        let invitation =
            registry.create_invitation_at(inviter, invitee, [3; 16], &signature, now)?;
        let result = registry.accept_invitation_at(
            invitation.room,
            invitee,
            invitation.token.expose(),
            &Signature::from_bytes(&[0; 64]),
            now + Duration::from_secs(121),
        );
        assert_eq!(result, Err(InvitationError::Expired));
        Ok(())
    }

    #[test]
    fn invite_creation_is_rate_limited() -> Result<(), InvitationError> {
        let (inviter_key, _, inviter, invitee) = identities();
        let mut registry = InvitationRegistry::new();
        for value in 0..5_u8 {
            let mut nonce = [0; 16];
            nonce[0] = value;
            let signature = request_signature(&inviter_key, inviter, invitee, nonce);
            registry.create_invitation_at(inviter, invitee, nonce, &signature, Instant::now())?;
        }
        let mut nonce = [0; 16];
        nonce[0] = 6;
        let signature = request_signature(&inviter_key, inviter, invitee, nonce);
        assert_eq!(
            registry
                .create_invitation_at(inviter, invitee, nonce, &signature, Instant::now())
                .err(),
            Some(InvitationError::RateLimited),
        );
        Ok(())
    }

    #[test]
    fn identity_cannot_invite_itself() {
        let (signing_key, _, identity, _) = identities();
        let nonce = [7; 16];
        let signature = request_signature(&signing_key, identity, identity, nonce);
        let error = InvitationRegistry::new()
            .create_invitation(identity, identity, nonce, &signature)
            .err();
        assert_eq!(error, Some(InvitationError::WrongPeer));
    }
}
