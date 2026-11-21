//! Authentication and track registration for the synthetic Kyber peers.

use std::{net::SocketAddr, path::Path, time::Duration};

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use kymux::{kynet, kyproto, types};
use serde::Deserialize;
use skyber_core::Track;

// The pinned Kyber SDK assigns endpoint 0 to the listener's first registration.
const CONTROL_ENDPOINT: u16 = 0;
const MAP_VERSION: u8 = 1;
const MAP_SIZE: usize = 8;
const COMPLETE: &[u8] = b"\x01done";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Listener,
    Initiator,
}

impl Role {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Listener => "listener",
            Self::Initiator => "initiator",
        }
    }
}

/// A validated address for an explicitly enabled private IPv4 test.
#[derive(Clone, Copy)]
pub struct LanAddress(SocketAddr);

impl LanAddress {
    pub fn new(ip: std::net::Ipv4Addr, port: u16) -> Result<Self> {
        ensure!(
            ip.is_private() && port != 0,
            "LAN requires private IPv4 and a nonzero port"
        );
        Ok(Self((ip, port).into()))
    }
}

pub async fn listen_lan(
    address: LanAddress,
    certificate: &Path,
    key: &Path,
) -> Result<kynet::common::CommonServer> {
    listen_at(address.0, certificate, key).await
}

pub async fn connect_lan(
    address: LanAddress,
    fingerprint: String,
    secret: &str,
) -> Result<kyproto::Connection> {
    connect_to(address.0, fingerprint, secret).await
}

pub async fn listen(
    address: SocketAddr,
    certificate: &Path,
    key: &Path,
) -> Result<kynet::common::CommonServer> {
    ensure!(address.ip().is_loopback(), "A1 listener requires loopback");
    listen_at(address, certificate, key).await
}

async fn listen_at(
    address: SocketAddr,
    certificate: &Path,
    key: &Path,
) -> Result<kynet::common::CommonServer> {
    let cert = kynet::cert::load_cert_from_pem_file(certificate).await?;
    let key = kynet::cert::load_private_key_from_pem_file(key).await?;
    let options = kynet::common::CommonServerOptions {
        max_idle_timeout: Some(Duration::from_secs(10)),
        keep_alive_interval: None,
    };
    Ok(kynet::Connection::start_server_on_addr(
        address,
        vec![cert],
        key,
        &options,
    )?)
}

pub async fn accept(server: &impl kynet::Server, secret: &str) -> Result<kyproto::Connection> {
    let raw = server.accept().await?.context("listener closed")?;
    let pending = kyproto::Connection::accept_with_auth(raw).await?;
    if pending.get_auth().token() != secret {
        pending.reject_authentication();
        anyhow::bail!("Kyber authentication rejected");
    }
    Ok(pending.accept_authentication().await?)
}

pub async fn connect(
    address: SocketAddr,
    fingerprint: String,
    secret: &str,
) -> Result<kyproto::Connection> {
    ensure!(address.ip().is_loopback(), "A1 connector requires loopback");
    connect_to(address, fingerprint, secret).await
}

async fn connect_to(
    address: SocketAddr,
    fingerprint: String,
    secret: &str,
) -> Result<kyproto::Connection> {
    let options = kynet::quinn::QuinnClientOptions {
        max_idle_timeout: Some(Duration::from_secs(10)),
        keep_alive_interval: None,
        certificate_hash: Some(fingerprint),
    };
    let raw = kynet::Connection::quinn_connect(address, "localhost", None, &options).await?;
    let auth = kyproto::ClientAuth::new(secret)?;
    Ok(kyproto::Connection::connect_with_auth(raw, &auth).await?)
}

pub type Outbound = (Track, types::VideoServerEndpoint);
pub type Inbound = (Track, types::VideoClientEndpoint);

/// Per-peer endpoint map exchanged over the authenticated A1 control endpoint.
#[derive(Debug, Default, Eq, PartialEq)]
struct TrackMap([Option<u16>; 3]);

impl TrackMap {
    fn encode(&self) -> Bytes {
        let mut data = [0; MAP_SIZE];
        data[0] = MAP_VERSION;
        for track in Track::ALL {
            let index = track as usize;
            if let Some(id) = self.0[index] {
                data[1] |= 1 << index;
                let offset = 2 + index * 2;
                data[offset..offset + 2].copy_from_slice(&id.to_be_bytes());
            }
        }
        Bytes::copy_from_slice(&data)
    }

    fn decode(data: &[u8], remote_role: Role) -> Result<Self> {
        ensure!(data.len() == MAP_SIZE, "invalid A1 track map length");
        ensure!(data[0] == MAP_VERSION, "unsupported A1 track map version");
        ensure!(data[1] & !0b111 == 0, "unknown A1 track flags");
        let parity = u16::from(remote_role == Role::Initiator);
        let mut map = Self::default();
        for track in Track::ALL {
            let index = track as usize;
            let offset = 2 + index * 2;
            let id = u16::from_be_bytes([data[offset], data[offset + 1]]);
            if data[1] & (1 << index) == 0 {
                ensure!(id == 0, "unexpected endpoint for disabled track");
                continue;
            }
            ensure!(
                id != CONTROL_ENDPOINT && id % 2 == parity,
                "invalid endpoint ID"
            );
            ensure!(!map.0.contains(&Some(id)), "duplicate endpoint ID");
            map.0[index] = Some(id);
        }
        Ok(map)
    }
}

pub struct TrackEndpoints {
    pub outbound: Vec<Outbound>,
    pub inbound: Vec<Inbound>,
    control: types::DataProtocol,
}

impl TrackEndpoints {
    pub async fn finish(&mut self, role: Role) -> Result<()> {
        let (tx, rx) = &mut self.control;
        let complete = types::DataPacket {
            payload: Bytes::from_static(COMPLETE),
        };
        // The listener acknowledges only after receiving the initiator's completion.
        if role == Role::Initiator {
            tx.send(complete.clone()).await?;
        }
        let packet = rx.recv().await?.context("missing peer completion")?;
        ensure!(packet.payload == COMPLETE, "invalid peer completion");
        if role == Role::Listener {
            tx.send(complete).await?;
        }
        Ok(())
    }
}

pub async fn register_tracks(
    connection: &kyproto::Connection,
    role: Role,
    send: &[Track],
    receive: &[Track],
) -> Result<TrackEndpoints> {
    let endpoint = if role == Role::Listener {
        let (id, endpoint) = connection.register_data_endpoint().await?;
        ensure!(id == CONTROL_ENDPOINT, "Kyber bootstrap endpoint changed");
        endpoint
    } else {
        connection.connect_data_endpoint(CONTROL_ENDPOINT)?
    };
    let (mut tx, mut rx) = endpoint.ready().await?;

    let mut outbound = Vec::new();
    let mut map = TrackMap::default();
    for &track in send {
        let (id, endpoint) = match track {
            Track::Microphone => {
                connection
                    .register_audio_endpoint(kyproto::AudioProtocol::Reliable)
                    .await?
            }
            Track::Camera | Track::Screen => {
                connection
                    .register_video_endpoint(kyproto::VideoProtocol::Reliable)
                    .await?
            }
        };
        map.0[track as usize] = Some(id);
        outbound.push((track, endpoint));
    }
    tx.send(types::DataPacket {
        payload: map.encode(),
    })
    .await?;
    let remote_role = match role {
        Role::Listener => Role::Initiator,
        Role::Initiator => Role::Listener,
    };
    let packet = rx.recv().await?.context("missing peer track map")?;
    let remote = TrackMap::decode(&packet.payload, remote_role)?;
    let mut inbound = Vec::new();
    for track in Track::ALL {
        let endpoint = remote.0[track as usize];
        ensure!(
            endpoint.is_some() == receive.contains(&track),
            "unexpected remote {} track",
            track.label()
        );
        if let Some(id) = endpoint {
            let endpoint = match track {
                Track::Microphone => {
                    connection.connect_audio_endpoint(id, kyproto::AudioProtocol::Reliable)?
                }
                Track::Camera | Track::Screen => {
                    connection.connect_video_endpoint(id, kyproto::VideoProtocol::Reliable)?
                }
            };
            inbound.push((track, endpoint));
        }
    }
    Ok(TrackEndpoints {
        outbound,
        inbound,
        control: (tx, rx),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lan_address_requires_specific_private_ipv4() {
        for ip in ["0.0.0.0", "127.0.0.1", "192.0.2.1", "224.0.0.1"] {
            assert!(LanAddress::new(ip.parse().unwrap(), 34829).is_err());
        }
        let private = "192.168.1.2".parse().unwrap();
        assert!(LanAddress::new(private, 0).is_err());
        assert!(LanAddress::new(private, 34829).is_ok());
    }

    #[test]
    fn map_preserves_independent_tracks() -> Result<()> {
        let map = TrackMap([Some(3), None, Some(5)]);
        assert_eq!(TrackMap::decode(&map.encode(), Role::Initiator)?, map);
        assert_eq!(
            TrackMap::decode(&TrackMap::default().encode(), Role::Listener)?,
            TrackMap::default()
        );
        Ok(())
    }

    #[test]
    fn map_rejects_malformed_or_conflicting_ids() {
        let samples = [
            [2, 1, 0, 3, 0, 0, 0, 0],
            [1, 8, 0, 0, 0, 0, 0, 0],
            [1, 1, 0, 2, 0, 0, 0, 0],
            [1, 3, 0, 3, 0, 3, 0, 0],
            [1, 0, 0, 3, 0, 0, 0, 0],
        ];
        for sample in samples {
            assert!(TrackMap::decode(&sample, Role::Initiator).is_err());
        }
        assert!(TrackMap::decode(&[1, 0], Role::Initiator).is_err());
    }
}
