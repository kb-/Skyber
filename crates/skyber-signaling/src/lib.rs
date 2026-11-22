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
const INVITATION_CODE_PREFIX: &str = "skyber-invite-v1|";
const MAX_INVITATION_ENDPOINT_LENGTH: usize = 1024;
const MAX_INVITATION_CODE_LENGTH: usize = 1280;
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

    /// Stable hexadecimal representation for explicit peer identity checks.
    pub fn fingerprint(&self) -> String {
        hex_fingerprint(&self.0)
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

/// Explicitly shareable invitation data. Its text form contains the one-use
/// bearer ticket, so callers should reveal it only through a deliberate share
/// action and should never log or persist it by default.
pub struct InvitationShareCode {
    endpoint: String,
    room: RoomId,
    token: InvitationToken,
    inviter: PeerIdentity,
    invitee: PeerIdentity,
}

impl InvitationShareCode {
    /// Build a share code for a validated WSS signaling endpoint.
    pub fn new(
        endpoint: impl Into<String>,
        invitation: &Invitation,
        inviter: PeerIdentity,
        invitee: PeerIdentity,
    ) -> Result<Self, InvitationCodeError> {
        let endpoint = endpoint.into();
        validate_invitation_endpoint(&endpoint)?;
        if inviter == invitee {
            return Err(InvitationCodeError::InvalidIdentity);
        }
        let mut token = [0_u8; 32];
        token.copy_from_slice(invitation.token.expose());
        Ok(Self {
            endpoint,
            room: invitation.room,
            token: InvitationToken(token),
            inviter,
            invitee,
        })
    }

    /// Parse a bounded versioned code pasted by the invited peer.
    pub fn parse(text: &str) -> Result<Self, InvitationCodeError> {
        if text.len() > MAX_INVITATION_CODE_LENGTH {
            return Err(InvitationCodeError::TooLong);
        }
        let body = text
            .strip_prefix(INVITATION_CODE_PREFIX)
            .ok_or(InvitationCodeError::UnsupportedVersion)?;
        let mut fields = body.split('|');
        let endpoint = fields.next().ok_or(InvitationCodeError::Malformed)?;
        let room = fields.next().ok_or(InvitationCodeError::Malformed)?;
        let token = fields.next().ok_or(InvitationCodeError::Malformed)?;
        let inviter = fields.next().ok_or(InvitationCodeError::Malformed)?;
        let invitee = fields.next().ok_or(InvitationCodeError::Malformed)?;
        if fields.next().is_some() {
            return Err(InvitationCodeError::Malformed);
        }
        validate_invitation_endpoint(endpoint)?;

        let room = RoomId::from_bytes(decode_hex::<16>(room)?);
        let token = InvitationToken(decode_hex::<32>(token)?);
        let inviter = PeerIdentity::from_bytes(decode_hex::<32>(inviter)?)
            .map_err(|_| InvitationCodeError::InvalidIdentity)?;
        let invitee = PeerIdentity::from_bytes(decode_hex::<32>(invitee)?)
            .map_err(|_| InvitationCodeError::InvalidIdentity)?;
        if inviter == invitee {
            return Err(InvitationCodeError::InvalidIdentity);
        }
        Ok(Self {
            endpoint: endpoint.to_owned(),
            room,
            token,
            inviter,
            invitee,
        })
    }

    /// Return the bearer code for explicit display or clipboard sharing.
    pub fn expose_text(&self) -> String {
        format!(
            "{INVITATION_CODE_PREFIX}{}|{}|{}|{}|{}",
            self.endpoint,
            encode_hex(self.room.as_bytes()),
            encode_hex(self.token.expose()),
            encode_hex(self.inviter.as_bytes()),
            encode_hex(self.invitee.as_bytes()),
        )
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub const fn room(&self) -> RoomId {
        self.room
    }

    pub const fn token(&self) -> &InvitationToken {
        &self.token
    }

    pub const fn inviter(&self) -> PeerIdentity {
        self.inviter
    }

    pub const fn invitee(&self) -> PeerIdentity {
        self.invitee
    }

    /// Refuse acceptance unless the code names this local identity.
    pub fn validate_recipient(&self, local_peer: PeerIdentity) -> Result<(), InvitationCodeError> {
        if self.invitee == local_peer {
            Ok(())
        } else {
            Err(InvitationCodeError::WrongRecipient)
        }
    }
}

impl std::fmt::Debug for InvitationShareCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InvitationShareCode")
            .field("endpoint", &self.endpoint)
            .field("room", &self.room)
            .field("token", &"[redacted]")
            .field("inviter", &self.inviter)
            .field("invitee", &self.invitee)
            .finish()
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum InvitationCodeError {
    #[error("invitation code exceeds its maximum length")]
    TooLong,
    #[error("invitation code version is not supported")]
    UnsupportedVersion,
    #[error("invitation code is malformed")]
    Malformed,
    #[error(
        "invitation endpoint must be a bounded wss:// URL without credentials, query, or fragment"
    )]
    InvalidEndpoint,
    #[error("invitation contains an invalid peer identity")]
    InvalidIdentity,
    #[error("invitation is addressed to a different local identity")]
    WrongRecipient,
}

fn validate_invitation_endpoint(endpoint: &str) -> Result<(), InvitationCodeError> {
    let authority_and_path = endpoint
        .strip_prefix("wss://")
        .ok_or(InvitationCodeError::InvalidEndpoint)?;
    let authority = authority_and_path.split('/').next().unwrap_or_default();
    if endpoint.len() > MAX_INVITATION_ENDPOINT_LENGTH
        || endpoint.chars().any(|character| {
            character.is_control()
                || character.is_whitespace()
                || matches!(character, '?' | '#' | '|' | '\\')
        })
        || !valid_authority(authority)
    {
        return Err(InvitationCodeError::InvalidEndpoint);
    }
    Ok(())
}

fn valid_authority(authority: &str) -> bool {
    if authority.is_empty() || authority.contains('@') {
        return false;
    }
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let Some((address, suffix)) = bracketed.split_once(']') else {
            return false;
        };
        if address.parse::<std::net::Ipv6Addr>().is_err() {
            return false;
        }
        let port = if suffix.is_empty() {
            None
        } else if let Some(port) = suffix.strip_prefix(':') {
            Some(port)
        } else {
            return false;
        };
        (address, port)
    } else {
        let mut parts = authority.split(':');
        let host = parts.next().unwrap_or_default();
        let port = parts.next();
        if parts.next().is_some() {
            return false;
        }
        (host, port)
    };
    let valid_host = if host.parse::<std::net::Ipv4Addr>().is_ok()
        || host.parse::<std::net::Ipv6Addr>().is_ok()
    {
        true
    } else {
        !host.is_empty()
            && host.len() <= 253
            && host.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && label.as_bytes()[0].is_ascii_alphanumeric()
                    && label
                        .as_bytes()
                        .last()
                        .is_some_and(u8::is_ascii_alphanumeric)
                    && label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            })
    };
    valid_host
        && port.is_none_or(|value| {
            !value.is_empty()
                && value.bytes().all(|byte| byte.is_ascii_digit())
                && value.parse::<u16>().is_ok_and(|port| port != 0)
        })
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn decode_hex<const N: usize>(text: &str) -> Result<[u8; N], InvitationCodeError> {
    if text.len() != N * 2 {
        return Err(InvitationCodeError::Malformed);
    }
    let mut output = [0_u8; N];
    for (index, chunk) in text.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_nibble(chunk[0]).ok_or(InvitationCodeError::Malformed)?;
        let low = hex_nibble(chunk[1]).ok_or(InvitationCodeError::Malformed)?;
        output[index] = (high << 4) | low;
    }
    Ok(output)
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
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

pub mod client;
pub mod server;

pub use client::{
    AcceptedRoom, PeerCredentials, RelayClient, SecretString, SignalClient, SignalClientError,
    SignalEvent, TlsSignalWebSocket, TlsSignalWebSocketConnector,
};

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
        AuthenticatedPeers, Invitation, InvitationCodeError, InvitationError, InvitationRegistry,
        InvitationShareCode, InvitationToken, PeerIdentity, RoomId, accept_message,
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

    #[test]
    fn invitation_share_code_round_trips_and_binds_recipient() {
        let (_, _, inviter, invitee) = identities();
        let invitation = Invitation {
            room: RoomId::from_bytes([4; 16]),
            token: InvitationToken([5; 32]),
            expires_in: Duration::from_secs(120),
        };
        let code = InvitationShareCode::new(
            "wss://calls.example.test/signal",
            &invitation,
            inviter,
            invitee,
        )
        .expect("valid invite should create a share code");
        let text = code.expose_text();
        let parsed = InvitationShareCode::parse(&text).expect("share code should parse");

        assert_eq!(parsed.endpoint(), "wss://calls.example.test/signal");
        assert_eq!(parsed.room(), invitation.room);
        assert_eq!(parsed.token().expose(), invitation.token.expose());
        assert_eq!(parsed.inviter(), inviter);
        assert_eq!(parsed.invitee(), invitee);
        assert_eq!(parsed.inviter().fingerprint().len(), 64);
        assert_eq!(parsed.validate_recipient(invitee), Ok(()));
        assert_eq!(
            parsed.validate_recipient(PeerIdentity::from_signing_key(&key(33))),
            Err(InvitationCodeError::WrongRecipient),
        );
        assert!(!format!("{parsed:?}").contains(&text));
        assert!(!format!("{parsed:?}").contains(&"05".repeat(32)));
    }

    #[test]
    fn invitation_share_code_rejects_unsafe_or_malformed_input() {
        let (_, _, inviter, invitee) = identities();
        for endpoint in [
            "ws://calls.example.test",
            "wss://user@calls.example.test",
            "wss://calls.example.test/?secret=value",
            "wss://calls.example.test/#fragment",
            "wss://calls.example.test:99999",
            "wss://bad_host.example.test",
            "wss://calls.example.test\\other",
        ] {
            let invitation = Invitation {
                room: RoomId::from_bytes([4; 16]),
                token: InvitationToken([5; 32]),
                expires_in: Duration::from_secs(120),
            };
            assert!(matches!(
                InvitationShareCode::new(endpoint, &invitation, inviter, invitee),
                Err(InvitationCodeError::InvalidEndpoint),
            ));
        }
        assert!(matches!(
            InvitationShareCode::parse("skyber-invite-v2|wss://calls.example.test"),
            Err(InvitationCodeError::UnsupportedVersion),
        ));
        assert!(matches!(
            InvitationShareCode::parse("skyber-invite-v1|wss://calls.example.test|bad"),
            Err(InvitationCodeError::Malformed),
        ));
        assert!(matches!(
            InvitationShareCode::parse(&"x".repeat(1281)),
            Err(InvitationCodeError::TooLong),
        ));
    }

    #[test]
    fn invitation_share_code_accepts_ipv6_and_rejects_self_invitation() {
        let (inviter_key, _, inviter, invitee) = identities();
        let invitation = Invitation {
            room: RoomId::from_bytes([4; 16]),
            token: InvitationToken([5; 32]),
            expires_in: Duration::from_secs(120),
        };
        assert!(
            InvitationShareCode::new(
                "wss://[2001:db8::1]:443/signal",
                &invitation,
                inviter,
                invitee,
            )
            .is_ok()
        );
        assert!(matches!(
            InvitationShareCode::new(
                "wss://calls.example.test",
                &invitation,
                PeerIdentity::from_signing_key(&inviter_key),
                PeerIdentity::from_signing_key(&inviter_key),
            ),
            Err(InvitationCodeError::InvalidIdentity),
        ));
    }
}
