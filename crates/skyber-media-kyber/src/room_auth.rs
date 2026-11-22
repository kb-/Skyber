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
    pub fn as_inner(&self) -> &Connection {
        &self.0
    }

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
    use std::{net::SocketAddr, sync::Arc, time::Duration};

    use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
    use quinn::Endpoint;
    use skyber_signaling::{AuthenticatedPeers, PeerIdentity, RoomId};
    use tokio::time::timeout;

    use super::{RoomAuthError, authenticate_room, proof_message};

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

    async fn quic_pair() -> anyhow::Result<(quinn::Connection, quinn::Connection)> {
        let certificate = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])?;
        let certificate_der = certificate.cert.der().clone();
        let private_key = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(certificate.signing_key.serialize_der()),
        );
        let server_tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate_der.clone()], private_key)?;
        let server_config = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(server_tls)?,
        ));
        let mut roots = rustls::RootCertStore::empty();
        roots.add(certificate_der)?;
        let client_tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let client_config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(client_tls)?,
        ));
        let server_endpoint =
            Endpoint::server(server_config, SocketAddr::from(([127, 0, 0, 1], 0)))?;
        let server_address = server_endpoint.local_addr()?;
        let mut client_endpoint = Endpoint::client(SocketAddr::from(([127, 0, 0, 1], 0)))?;
        client_endpoint.set_default_client_config(client_config);
        let client_connecting = client_endpoint.connect(server_address, "localhost")?;
        let server_connecting = async {
            server_endpoint
                .accept()
                .await
                .ok_or_else(|| anyhow::anyhow!("server endpoint closed"))?
                .await
                .map_err(anyhow::Error::from)
        };
        let (client, server) = timeout(Duration::from_secs(5), async {
            tokio::try_join!(
                async { client_connecting.await.map_err(anyhow::Error::from) },
                server_connecting,
            )
        })
        .await??;
        Ok((client, server))
    }

    #[tokio::test]
    async fn quic_room_mismatch_rejects_both_peers() -> anyhow::Result<()> {
        let _network_test = crate::NETWORK_INTEGRATION_TEST_LOCK.lock().await;
        let (client, server) = quic_pair().await?;
        let (client_peers, inviter, invitee, _) = fixture();
        let server_peers = AuthenticatedPeers {
            room: RoomId::from_bytes([8; 16]),
            ..client_peers
        };
        let (client_result, server_result) = tokio::join!(
            authenticate_room(client, client_peers, &inviter),
            authenticate_room(server, server_peers, &invitee),
        );
        assert!(client_result.is_err());
        assert!(matches!(server_result, Err(RoomAuthError::InvalidProof)));
        Ok(())
    }

    #[tokio::test]
    async fn quic_wrong_local_identity_is_rejected_and_closes_peer() -> anyhow::Result<()> {
        let _network_test = crate::NETWORK_INTEGRATION_TEST_LOCK.lock().await;
        let (client, server) = quic_pair().await?;
        let (peers, inviter, _, _) = fixture();
        let impersonator = SigningKey::from_bytes(&[44; 32]);
        let (client_result, server_result) = tokio::join!(
            authenticate_room(client, peers, &inviter),
            authenticate_room(server, peers, &impersonator),
        );
        assert!(matches!(
            server_result,
            Err(RoomAuthError::LocalIdentityMismatch)
        ));
        assert!(client_result.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn quic_room_proof_from_previous_connection_cannot_be_replayed() -> anyhow::Result<()> {
        let _network_test = crate::NETWORK_INTEGRATION_TEST_LOCK.lock().await;
        let (peers, inviter, invitee, _) = fixture();

        // A valid proof is bound to this connection's exporter. Retain a
        // signature from one QUIC session and try to present it in another.
        let (old_client, old_server) = quic_pair().await?;
        let mut old_exporter = [0u8; 32];
        old_client
            .export_keying_material(&mut old_exporter, super::EXPORTER_LABEL, &[])
            .map_err(|_| anyhow::anyhow!("could not export old QUIC keying material"))?;
        let replayed_signature = inviter
            .sign(&proof_message(0, peers, &old_exporter))
            .to_bytes();
        old_client.close(quinn::VarInt::from_u32(0), b"end replay fixture");
        old_server.close(quinn::VarInt::from_u32(0), b"end replay fixture");

        let (client, server) = quic_pair().await?;
        let server_auth =
            tokio::spawn(async move { authenticate_room(server, peers, &invitee).await });
        let mut replay_stream = client.open_bi().await?;
        replay_stream.0.write_all(&[super::VERSION, 0]).await?;
        replay_stream.0.write_all(&replayed_signature).await?;
        replay_stream.0.finish()?;

        assert!(matches!(
            server_auth.await?,
            Err(RoomAuthError::InvalidProof)
        ));
        client.close(quinn::VarInt::from_u32(0), b"replay rejected");
        Ok(())
    }

    #[tokio::test]
    async fn each_quic_reconnect_authenticates_a_new_connection() -> anyhow::Result<()> {
        let _network_test = crate::NETWORK_INTEGRATION_TEST_LOCK.lock().await;
        let (peers, inviter, invitee, _) = fixture();
        for _ in 0..2 {
            let (client, server) = quic_pair().await?;
            let (client_result, server_result) = tokio::join!(
                authenticate_room(client, peers, &inviter),
                authenticate_room(server, peers, &invitee),
            );
            client_result?;
            server_result?;
        }
        Ok(())
    }
}
