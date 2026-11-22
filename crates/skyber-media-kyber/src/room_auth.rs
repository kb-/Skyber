//! Bind accepted invitation identities to an established QUIC connection.
//!
//! Call this after TLS certificate verification and before constructing a
//! Kymux connection. Both peers must use the same invited room and identities.

use std::time::Duration;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use quinn::{Connection, Side};
use skyber_signaling::{AuthenticatedPeers, PeerIdentity};
use thiserror::Error;
use tokio::time::timeout;

const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const EXPORTER_LABEL: &[u8] = b"EXPORTER-Skyber-C1-Room-Identity-v1";
const PROOF_DOMAIN: &[u8] = b"skyber/c1/quic-room-identity/v1\0";
const VERSION: u8 = 1;
const ACK: u8 = 1;
const SIGNATURE_SIZE: usize = 64;

#[derive(Debug, Error)]
pub enum RoomAuthError {
    #[error("room identity proof timed out")]
    Timeout,
    #[error("local signing key does not match the invited peer")]
    LocalIdentityMismatch,
    #[error("invited peer identity is invalid")]
    InvalidIdentity,
    #[error("QUIC room identity stream failed: {0}")]
    Stream(#[from] quinn::WriteError),
    #[error("QUIC room identity connection failed: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("QUIC room identity stream closed before completion")]
    ClosedStream(#[from] quinn::ClosedStream),
    #[error("QUIC room identity stream read failed: {0}")]
    Read(#[from] quinn::ReadExactError),
    #[error("QUIC room identity stream read failed: {0}")]
    StreamRead(#[from] quinn::ReadError),
    #[error("peer sent an invalid room identity proof")]
    InvalidProof,
    #[error("peer did not acknowledge room identity authentication")]
    MissingAcknowledgement,
}

/// A QUIC connection whose authenticated TLS session is bound to the invited
/// room and both Ed25519 peer identities.
#[derive(Debug)]
pub struct AuthenticatedRoomConnection(Connection);

impl AuthenticatedRoomConnection {
    pub fn into_inner(self) -> Connection {
        self.0
    }
}

/// Authenticate both invited identities using a fresh TLS exporter value.
///
/// The inviter is always the QUIC client and initiates the proof stream. The
/// complete exchange is bounded by one timeout. Failure closes the connection.
pub async fn authenticate_room(
    connection: Connection,
    peers: AuthenticatedPeers,
    local_key: &SigningKey,
) -> Result<AuthenticatedRoomConnection, RoomAuthError> {
    match timeout(
        AUTH_TIMEOUT,
        authenticate_inner_result(&connection, peers, local_key),
    )
    .await
    {
        Ok(Ok(())) => Ok(AuthenticatedRoomConnection(connection)),
        Ok(Err(error)) => {
            connection.close(
                quinn::VarInt::from_u32(0x101),
                b"room identity authentication failed",
            );
            Err(error)
        }
        Err(_) => {
            connection.close(
                quinn::VarInt::from_u32(0x100),
                b"room authentication timeout",
            );
            Err(RoomAuthError::Timeout)
        }
    }
}

async fn authenticate_inner_result(
    connection: &Connection,
    peers: AuthenticatedPeers,
    local_key: &SigningKey,
) -> Result<(), RoomAuthError> {
    let (role, expected_peer) = match connection.side() {
        Side::Client if peers.inviter == PeerIdentity::from_signing_key(local_key) => {
            (0, peers.invitee)
        }
        Side::Server if peers.invitee == PeerIdentity::from_signing_key(local_key) => {
            (1, peers.inviter)
        }
        _ => return Err(RoomAuthError::LocalIdentityMismatch),
    };
    let verifying_key = VerifyingKey::from_bytes(expected_peer.as_bytes())
        .map_err(|_| RoomAuthError::InvalidIdentity)?;
    let mut exporter = [0u8; 32];
    connection
        .export_keying_material(&mut exporter, EXPORTER_LABEL, &[])
        .map_err(|_| RoomAuthError::InvalidProof)?;
    let proof = proof_message(role, peers, &exporter);
    let local_signature = local_key.sign(&proof).to_bytes();

    if role == 0 {
        let (mut send, mut receive) = connection.open_bi().await?;
        send.write_all(&[VERSION, role]).await?;
        send.write_all(&local_signature).await?;
        send.finish()?;
        let mut remote_signature = [0u8; SIGNATURE_SIZE];
        receive.read_exact(&mut remote_signature).await?;
        verifying_key
            .verify(
                &proof_message(1, peers, &exporter),
                &Signature::from_bytes(&remote_signature),
            )
            .map_err(|_| RoomAuthError::InvalidProof)?;
        let mut acknowledgement = [0u8; 1];
        receive.read_exact(&mut acknowledgement).await?;
        if acknowledgement[0] != ACK {
            return Err(RoomAuthError::MissingAcknowledgement);
        }
        let mut trailing = [0u8; 1];
        if !matches!(receive.read(&mut trailing).await?, None | Some(0)) {
            return Err(RoomAuthError::InvalidProof);
        }
    } else {
        let (mut send, mut receive) = connection.accept_bi().await?;
        let mut header = [0u8; 2];
        receive.read_exact(&mut header).await?;
        if header != [VERSION, 0] {
            return Err(RoomAuthError::InvalidProof);
        }
        let mut remote_signature = [0u8; SIGNATURE_SIZE];
        receive.read_exact(&mut remote_signature).await?;
        let mut trailing = [0u8; 1];
        if !matches!(receive.read(&mut trailing).await?, None | Some(0)) {
            return Err(RoomAuthError::InvalidProof);
        }
        verifying_key
            .verify(
                &proof_message(0, peers, &exporter),
                &Signature::from_bytes(&remote_signature),
            )
            .map_err(|_| RoomAuthError::InvalidProof)?;
        send.write_all(&local_signature).await?;
        send.write_all(&[ACK]).await?;
        send.finish()?;
    }
    Ok(())
}

fn proof_message(role: u8, peers: AuthenticatedPeers, exporter: &[u8; 32]) -> Vec<u8> {
    let mut message = Vec::with_capacity(PROOF_DOMAIN.len() + 1 + 16 + 32 + 32 + 32);
    message.extend_from_slice(PROOF_DOMAIN);
    message.push(role);
    message.extend_from_slice(peers.room.as_bytes());
    message.extend_from_slice(peers.inviter.as_bytes());
    message.extend_from_slice(peers.invitee.as_bytes());
    message.extend_from_slice(exporter);
    message
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
    use skyber_signaling::{AuthenticatedPeers, PeerIdentity, RoomId};

    use super::proof_message;

    fn fixture() -> (AuthenticatedPeers, SigningKey, SigningKey, [u8; 32]) {
        let inviter = SigningKey::from_bytes(&[11; 32]);
        let invitee = SigningKey::from_bytes(&[22; 32]);
        let peers = AuthenticatedPeers {
            room: RoomId::from_bytes([7; 16]),
            inviter: PeerIdentity::from_signing_key(&inviter),
            invitee: PeerIdentity::from_signing_key(&invitee),
        };
        (peers, inviter, invitee, [33; 32])
    }

    #[test]
    fn peer_proofs_bind_room_role_identities_and_tls_exporter() {
        let (peers, inviter, invitee, exporter) = fixture();
        let inviter_proof = proof_message(0, peers, &exporter);
        let invitee_proof = proof_message(1, peers, &exporter);
        let inviter_signature = inviter.sign(&inviter_proof);
        let invitee_signature = invitee.sign(&invitee_proof);

        VerifyingKey::from_bytes(peers.inviter.as_bytes())
            .expect("fixture inviter key is valid")
            .verify(&inviter_proof, &inviter_signature)
            .expect("inviter proof verifies");
        VerifyingKey::from_bytes(peers.invitee.as_bytes())
            .expect("fixture invitee key is valid")
            .verify(&invitee_proof, &invitee_signature)
            .expect("invitee proof verifies");

        let mut other_room = peers;
        other_room.room = RoomId::from_bytes([8; 16]);
        assert!(
            VerifyingKey::from_bytes(peers.inviter.as_bytes())
                .expect("fixture inviter key is valid")
                .verify(&proof_message(0, other_room, &exporter), &inviter_signature)
                .is_err()
        );

        assert!(
            VerifyingKey::from_bytes(peers.invitee.as_bytes())
                .expect("fixture invitee key is valid")
                .verify(&proof_message(0, peers, &exporter), &invitee_signature)
                .is_err()
        );

        let other_exporter = [34; 32];
        assert!(
            VerifyingKey::from_bytes(peers.inviter.as_bytes())
                .expect("fixture inviter key is valid")
                .verify(
                    &proof_message(0, peers, &other_exporter),
                    &inviter_signature
                )
                .is_err()
        );

        let impersonator = SigningKey::from_bytes(&[44; 32]);
        let impersonated_proof = proof_message(0, peers, &exporter);
        let impersonated_signature = impersonator.sign(&impersonated_proof);
        assert!(
            VerifyingKey::from_bytes(peers.inviter.as_bytes())
                .expect("fixture inviter key is valid")
                .verify(&impersonated_proof, &impersonated_signature)
                .is_err()
        );
    }
}
