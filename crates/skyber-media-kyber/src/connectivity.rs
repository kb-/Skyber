//! Bounded Gate C-0 connectivity proof over Kyber's Quinn transport.
//!
//! This module deliberately uses raw Kyber bidirectional streams and never
//! creates a `kyproto::Connection` or accepts media/data packets.

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::SocketAddr,
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use kymux::kynet;
use kymux::kynet::Server;
use subtle::ConstantTimeEq;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use zeroize::Zeroize;

const AUTH_MAGIC: &[u8; 8] = b"SKYBC0A1";
const PROBE_MAGIC: &[u8; 8] = b"SKYBC0P1";
const VERSION: u8 = 1;
const AUTH_SIZE: usize = 44;
const PROBE_HEADER_SIZE: usize = 16;
const PROBE_PAYLOAD_SIZE: usize = 48;
const PROBE_SIZE: usize = PROBE_HEADER_SIZE + PROBE_PAYLOAD_SIZE;
const AUTH_OK: u8 = 0xA1;
const AUTH_REJECTED: u8 = 0x00;
const EXCHANGE_ACK: u8 = 0xA2;
const SHUTDOWN_ACK: u8 = 0xA3;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const ACCEPT_WAIT: Duration = Duration::from_secs(15 * 60);
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Short-lived token storage that clears its bytes when dropped.
pub struct SecretToken([u8; 32]);

impl SecretToken {
    pub fn expose(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Drop for SecretToken {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Load a raw 32-byte token without permitting an unbounded local file read.
pub fn read_token(path: &Path) -> Result<SecretToken> {
    let file = File::open(path).context("could not open token file")?;
    let mut bytes = Vec::with_capacity(33);
    let result = file
        .take(33)
        .read_to_end(&mut bytes)
        .context("could not read token file")
        .and_then(|_| {
            ensure!(
                bytes.len() == 32,
                "token file must contain exactly 32 bytes"
            );
            Ok(())
        });
    if let Err(error) = result {
        bytes.zeroize();
        return Err(error);
    }
    let mut token = [0; 32];
    token.copy_from_slice(&bytes);
    bytes.zeroize();
    Ok(SecretToken(token))
}

/// Create a fresh token without overwriting an existing path or displaying it.
pub fn create_token(path: &Path) -> Result<()> {
    let mut token = [0u8; 32];
    if let Err(error) = getrandom::fill(&mut token) {
        token.zeroize();
        return Err(error).context("operating system random source failed");
    }

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) => {
            token.zeroize();
            return Err(error).context("could not create token file");
        }
    };
    let result = file
        .write_all(&token)
        .and_then(|()| file.sync_all())
        .context("could not persist token file");
    token.zeroize();
    if result.is_err() {
        drop(file);
        let _ = fs::remove_file(path);
    }
    result
}

/// Validate the explicitly selected bind/connect mode.
pub fn validate_address_mode(address: SocketAddr, internet: bool) -> Result<()> {
    ensure!(
        address.ip().is_loopback() || internet,
        "non-loopback addresses require explicit --internet mode"
    );
    Ok(())
}

/// Validate the hex SHA-256 certificate pin accepted by Kyber's Quinn driver.
pub fn validate_certificate_pin(pin: &str) -> Result<()> {
    ensure!(
        pin.len() == 64,
        "certificate pin must be 64 hexadecimal characters"
    );
    ensure!(
        pin.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "certificate pin must be 64 hexadecimal characters"
    );
    Ok(())
}

/// Perform one authenticated, fixed-size synthetic request/response.
pub async fn connect_and_probe(
    address: SocketAddr,
    internet: bool,
    pin: &str,
    token: &[u8; 32],
    payload: &[u8],
) -> Result<()> {
    validate_address_mode(address, internet)?;
    validate_certificate_pin(pin)?;
    ensure!(
        payload.len() <= PROBE_PAYLOAD_SIZE,
        "probe message is too long"
    );

    let options = kynet::quinn::QuinnClientOptions {
        max_idle_timeout: Some(Duration::from_secs(10)),
        keep_alive_interval: None,
        certificate_hash: Some(pin.to_ascii_lowercase()),
    };
    let connection = match tokio::time::timeout(
        CONNECT_TIMEOUT,
        kynet::Connection::quinn_connect(address, "skyber-gate-c0", None, &options),
    )
    .await
    {
        Ok(Ok(connection)) => connection,
        Ok(Err(_)) => bail!("Gate C-0 certificate pin or QUIC connection failed"),
        Err(_) => bail!("Gate C-0 connection timed out"),
    };

    let (mut send, mut receive) = tokio::time::timeout(AUTH_TIMEOUT, async {
        let (mut send, mut receive) = connection.open_bi().await?;
        send.write_all(&encode_auth(token)).await?;
        send.flush().await?;

        let auth_response =
            read_fixed::<1, _>(&mut receive, AUTH_TIMEOUT, "auth response timed out").await?;
        ensure!(
            auth_response[0] == AUTH_OK,
            "Gate C-0 authentication rejected"
        );
        Ok::<_, anyhow::Error>((send, receive))
    })
    .await
    .context("Gate C-0 authentication timed out")??;

    let exchange_result = tokio::time::timeout(EXCHANGE_TIMEOUT, async {
        let request = encode_probe(payload)?;
        send.write_all(&request).await?;
        send.flush().await?;
        let response =
            read_fixed::<PROBE_SIZE, _>(&mut receive, EXCHANGE_TIMEOUT, "probe response timed out")
                .await?;
        validate_probe_response(&response, payload)?;
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("Gate C-0 exchange timed out");

    let shutdown_result = tokio::time::timeout(SHUTDOWN_TIMEOUT, async {
        send.write_all(&[EXCHANGE_ACK]).await?;
        send.flush().await?;
        let ack = read_fixed::<1, _>(
            &mut receive,
            SHUTDOWN_TIMEOUT,
            "shutdown acknowledgment timed out",
        )
        .await?;
        ensure!(
            ack[0] == SHUTDOWN_ACK,
            "invalid Gate C-0 shutdown acknowledgment"
        );
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("Gate C-0 shutdown timed out");
    if exchange_result.is_err() || shutdown_result.is_err() {
        connection.close(1, "Gate C-0 probe failed");
    } else {
        connection.close(0, "Gate C-0 probe complete");
    }
    exchange_result??;
    shutdown_result??;
    Ok(())
}

/// Bind the Gate C-0 listener. Its address must have been explicitly enabled.
pub async fn start_listener(
    address: SocketAddr,
    internet: bool,
    certificate: &Path,
    private_key: &Path,
) -> Result<kynet::common::CommonServer> {
    validate_address_mode(address, internet)?;
    let cert = kynet::cert::load_cert_from_pem_file(certificate)
        .await
        .context("could not load server certificate")?;
    let key = kynet::cert::load_private_key_from_pem_file(private_key)
        .await
        .context("could not load server key")?;
    let options = kynet::common::CommonServerOptions {
        max_idle_timeout: Some(Duration::from_secs(10)),
        keep_alive_interval: None,
    };
    kynet::Connection::start_server_on_addr(address, vec![cert], key, &options)
        .context("could not start Gate C-0 listener")
}

/// Accept one authenticated connection, exchange one bounded message, then stop.
pub async fn accept_probe_once(server: &impl Server, token: &[u8; 32]) -> Result<()> {
    let result = tokio::time::timeout(ACCEPT_WAIT, async {
        let raw = server
            .accept()
            .await
            .map_err(|_| anyhow::anyhow!("incoming Gate C-0 QUIC handshake failed"))?
            .context("Gate C-0 listener closed")?;
        let (mut send, mut receive) = tokio::time::timeout(AUTH_TIMEOUT, async {
            let (mut send, mut receive) = raw.accept_bi().await?;
            let auth_frame =
                read_fixed::<AUTH_SIZE, _>(&mut receive, AUTH_TIMEOUT, "auth frame timed out")
                    .await?;
            let valid = validate_auth(&auth_frame, token);
            send.write_all(&[if valid { AUTH_OK } else { AUTH_REJECTED }])
                .await?;
            send.flush().await?;
            ensure!(valid, "Gate C-0 authentication rejected");
            Ok::<_, anyhow::Error>((send, receive))
        })
        .await
        .context("Gate C-0 authentication timed out")??;

        let exchange_result = tokio::time::timeout(EXCHANGE_TIMEOUT, async {
            let request = read_fixed::<PROBE_SIZE, _>(
                &mut receive,
                EXCHANGE_TIMEOUT,
                "probe request timed out",
            )
            .await?;
            let response = encode_probe_response(&request)?;
            send.write_all(&response).await?;
            send.flush().await?;
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("Gate C-0 exchange timed out");

        let shutdown_result = tokio::time::timeout(SHUTDOWN_TIMEOUT, async {
            let ack =
                read_fixed::<1, _>(&mut receive, SHUTDOWN_TIMEOUT, "probe ack timed out").await?;
            ensure!(
                ack[0] == EXCHANGE_ACK,
                "invalid Gate C-0 exchange acknowledgment"
            );
            send.write_all(&[SHUTDOWN_ACK]).await?;
            send.flush().await?;
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("Gate C-0 shutdown timed out");
        let close_result = tokio::time::timeout(SHUTDOWN_TIMEOUT, raw.closed())
            .await
            .context("Gate C-0 shutdown timed out")
            .map_err(|_| anyhow::anyhow!("Gate C-0 shutdown timed out"))
            .and_then(|result| {
                result.map_err(|_| anyhow::anyhow!("Gate C-0 peer shutdown failed"))
            });
        exchange_result??;
        shutdown_result??;
        close_result?;
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("Gate C-0 listener timed out waiting for one probe");

    server.close(0, "Gate C-0 probe complete");
    result??;
    Ok(())
}

async fn read_fixed<const N: usize, R: AsyncRead + Unpin>(
    stream: &mut R,
    deadline: Duration,
    timeout_message: &'static str,
) -> Result<[u8; N]> {
    tokio::time::timeout(deadline, async {
        let mut frame = [0u8; N];
        stream.read_exact(&mut frame).await?;
        Ok::<_, std::io::Error>(frame)
    })
    .await
    .context(timeout_message)?
    .context("truncated Gate C-0 frame")
}

fn encode_auth(token: &[u8; 32]) -> [u8; AUTH_SIZE] {
    let mut frame = [0u8; AUTH_SIZE];
    frame[..8].copy_from_slice(AUTH_MAGIC);
    frame[8] = VERSION;
    frame[12..].copy_from_slice(token);
    frame
}

fn validate_auth(frame: &[u8; AUTH_SIZE], expected_token: &[u8; 32]) -> bool {
    let token_matches: bool = frame[12..].ct_eq(expected_token).into();
    let header_matches =
        frame[..8] == *AUTH_MAGIC && frame[8] == VERSION && frame[9..12] == [0, 0, 0];
    header_matches & token_matches
}

fn encode_probe(payload: &[u8]) -> Result<[u8; PROBE_SIZE]> {
    ensure!(
        payload.len() <= PROBE_PAYLOAD_SIZE,
        "probe message is too long"
    );
    let mut frame = [0u8; PROBE_SIZE];
    frame[..8].copy_from_slice(PROBE_MAGIC);
    frame[8] = VERSION;
    frame[9] = 1;
    frame[10..14].copy_from_slice(&1u32.to_be_bytes());
    frame[14..16].copy_from_slice(&(payload.len() as u16).to_be_bytes());
    frame[PROBE_HEADER_SIZE..PROBE_HEADER_SIZE + payload.len()].copy_from_slice(payload);
    Ok(frame)
}

fn encode_probe_response(request: &[u8; PROBE_SIZE]) -> Result<[u8; PROBE_SIZE]> {
    validate_probe_request(request)?;
    let mut response = *request;
    response[9] = 2;
    Ok(response)
}

fn validate_probe_request(frame: &[u8; PROBE_SIZE]) -> Result<&[u8]> {
    ensure!(&frame[..8] == PROBE_MAGIC, "invalid Gate C-0 probe magic");
    ensure!(frame[8] == VERSION, "unsupported Gate C-0 probe version");
    ensure!(frame[9] == 1, "invalid Gate C-0 probe type");
    ensure!(
        frame[10..14] == 1u32.to_be_bytes(),
        "invalid Gate C-0 sequence"
    );
    let payload_len = u16::from_be_bytes([frame[14], frame[15]]) as usize;
    ensure!(
        payload_len <= PROBE_PAYLOAD_SIZE,
        "invalid Gate C-0 payload length"
    );
    ensure!(
        frame[PROBE_HEADER_SIZE + payload_len..]
            .iter()
            .all(|byte| *byte == 0),
        "invalid Gate C-0 padding"
    );
    Ok(&frame[PROBE_HEADER_SIZE..PROBE_HEADER_SIZE + payload_len])
}

fn validate_probe_response(frame: &[u8; PROBE_SIZE], expected_payload: &[u8]) -> Result<()> {
    ensure!(
        &frame[..8] == PROBE_MAGIC,
        "invalid Gate C-0 response magic"
    );
    ensure!(frame[8] == VERSION, "unsupported Gate C-0 response version");
    ensure!(frame[9] == 2, "invalid Gate C-0 response type");
    ensure!(
        frame[10..14] == 1u32.to_be_bytes(),
        "invalid Gate C-0 response sequence"
    );
    let payload_len = u16::from_be_bytes([frame[14], frame[15]]) as usize;
    ensure!(
        payload_len == expected_payload.len(),
        "invalid Gate C-0 response length"
    );
    ensure!(
        &frame[PROBE_HEADER_SIZE..PROBE_HEADER_SIZE + payload_len] == expected_payload,
        "Gate C-0 response payload did not match"
    );
    ensure!(
        frame[PROBE_HEADER_SIZE + payload_len..]
            .iter()
            .all(|byte| *byte == 0),
        "invalid Gate C-0 response padding"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TOKEN_TEST: AtomicU64 = AtomicU64::new(0);

    fn token_test_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "skyber-gate-c0-token-{}-{}.bin",
            std::process::id(),
            NEXT_TOKEN_TEST.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn token_authentication_checks_header_and_constant_time_token_result() {
        let token = [0x5Au8; 32];
        let frame = encode_auth(&token);
        assert!(validate_auth(&frame, &token));
        assert!(!validate_auth(&frame, &[0xA5; 32]));
        let mut malformed = frame;
        malformed[9] = 1;
        assert!(!validate_auth(&malformed, &token));
    }

    #[test]
    fn probe_frame_round_trips_and_rejects_bad_fields() -> Result<()> {
        let request = encode_probe(b"synthetic gate c0 payload")?;
        let response = encode_probe_response(&request)?;
        validate_probe_response(&response, b"synthetic gate c0 payload")?;

        let mut malformed = request;
        malformed[14..16].copy_from_slice(&49u16.to_be_bytes());
        assert!(encode_probe_response(&malformed).is_err());
        let mut malformed = request;
        malformed[63] = 1;
        assert!(encode_probe_response(&malformed).is_err());
        Ok(())
    }

    #[test]
    fn certificate_pin_and_address_mode_are_explicit() -> Result<()> {
        assert!(validate_certificate_pin(&"ab".repeat(32)).is_ok());
        assert!(validate_certificate_pin(&"xy".repeat(32)).is_err());
        assert!(validate_certificate_pin(&"ab".repeat(31)).is_err());

        let loopback: SocketAddr = "127.0.0.1:34780".parse()?;
        let public_or_lan: SocketAddr = "203.0.113.5:34780".parse()?;
        assert!(validate_address_mode(loopback, false).is_ok());
        assert!(validate_address_mode(public_or_lan, false).is_err());
        assert!(validate_address_mode(public_or_lan, true).is_ok());
        Ok(())
    }

    #[test]
    fn socket_address_parser_supports_bracketed_ipv6() -> Result<()> {
        let ipv6: SocketAddr = "[::1]:34780".parse()?;
        assert!(ipv6.ip().is_loopback());
        Ok(())
    }

    #[tokio::test]
    async fn fixed_frame_reader_rejects_truncation_and_times_out_stalled_peers() {
        let (mut writer, mut truncated_reader) = tokio::io::duplex(8);
        writer
            .write_all(b"short")
            .await
            .expect("duplex write succeeds");
        drop(writer);
        let truncated = read_fixed::<AUTH_SIZE, _>(
            &mut truncated_reader,
            Duration::from_secs(1),
            "test read timed out",
        )
        .await;
        assert!(truncated.is_err());

        let (_writer, mut stalled_reader) = tokio::io::duplex(8);
        let stalled = read_fixed::<AUTH_SIZE, _>(
            &mut stalled_reader,
            Duration::from_millis(1),
            "test read timed out",
        )
        .await;
        assert!(stalled.is_err());
    }

    #[test]
    fn token_file_is_exact_size_and_never_overwritten() -> Result<()> {
        let path = token_test_path();
        create_token(&path)?;
        let token = read_token(&path)?;
        assert_eq!(token.expose().len(), 32);
        assert_eq!(fs::metadata(&path)?.len(), 32);
        assert!(create_token(&path).is_err());
        fs::remove_file(&path)?;

        let short_path = token_test_path();
        fs::write(&short_path, [0u8; 31])?;
        assert!(read_token(&short_path).is_err());
        fs::remove_file(short_path)?;
        Ok(())
    }
}
