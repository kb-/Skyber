use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use kymux::{kynet, types};
use serde::{Deserialize, Serialize};
use skyber_core::Track;
use skyber_media_kyber::{
    native::{Decoder, Encoder, MAX_CONFIG, MAX_PACKET},
    transport::{self, Inbound, Outbound, Role},
};
use tokio::{sync::mpsc, task::JoinSet, time::timeout};

const SETUP_TIMEOUT: Duration = Duration::from_secs(4);
const MAX_RECEIVED_PACKETS: usize = 1000;
const AUDIO_RATE: u32 = 48_000;

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Profile {
    Audio,
    Av,
    AvWithScreen,
}

impl Profile {
    fn tracks(self) -> &'static [Track] {
        match self {
            Self::Audio => &[Track::Microphone],
            Self::Av => &[Track::Microphone, Track::Camera],
            Self::AvWithScreen => &Track::ALL,
        }
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Direction {
    Both,
    Listener,
    Initiator,
}

impl Direction {
    fn sends(self, role: Role) -> bool {
        matches!(
            (self, role),
            (Self::Both, _) | (Self::Listener, Role::Listener) | (Self::Initiator, Role::Initiator)
        )
    }
}

#[derive(Deserialize)]
struct Settings {
    role: Role,
    port: u16,
    fingerprint: String,
    secret: String,
    profile: Profile,
    direction: Direction,
    duration_ms: u64,
    screen_stop_ms: Option<u64>,
}

impl Settings {
    fn load(directory: &Path) -> Result<Self> {
        let config: Self = serde_json::from_slice(&std::fs::read(directory.join("peer.json"))?)
            .context("invalid A1 run configuration")?;
        ensure!(
            config.port != 0 && (500..=15_000).contains(&config.duration_ms),
            "invalid port or run duration"
        );
        ensure!(
            !config.secret.is_empty() && config.fingerprint.len() == 64,
            "invalid authentication configuration"
        );
        if let Some(stop) = config.screen_stop_ms {
            ensure!(
                matches!(config.profile, Profile::AvWithScreen)
                    && (500..config.duration_ms).contains(&stop),
                "invalid screen stop time"
            );
        }
        Ok(config)
    }
}

fn codec(track: Track) -> u32 {
    u32::from_be_bytes(match track {
        Track::Microphone => *b"opus",
        Track::Camera | Track::Screen => *b"h264",
    })
}

fn wall_micros() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_micros()
        .try_into()?)
}

fn media(pts: u64, payload: Bytes, is_config: bool, is_key: bool) -> Result<types::AVPacket> {
    Ok(types::AVPacket::Media(types::MediaPacket {
        header: types::MediaPacketHeader {
            pts,
            size: payload.len().try_into()?,
            is_config,
            is_key,
        },
        payload,
    }))
}

fn encode_track(
    tx: mpsc::Sender<types::AVPacket>,
    track: Track,
    source: u32,
    duration_ms: u64,
) -> Result<u32> {
    let mut encoder = Encoder::new(track, source)?;
    tx.blocking_send(types::AVPacket::Codec(types::CodecPacket {
        header: types::CodecPacketHeader {
            codec: codec(track),
            rotation: 0,
            frame_size: 0,
        },
    }))?;
    tx.blocking_send(media(0, Bytes::from(encoder.config()?), true, false)?)?;
    let rate = if track == Track::Microphone { 50 } else { 15 };
    let frames = duration_ms * rate / 1000;
    let start = Instant::now();
    let mut count = 0;
    let mut last_pts = 0;
    for frame in 0..frames {
        let deadline = start + Duration::from_micros(frame * 1_000_000 / rate);
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        let pts = wall_micros()?.max(last_pts + 1);
        let (payload, key) = encoder.next_packet()?;
        if !payload.is_empty() {
            tx.blocking_send(media(pts, Bytes::copy_from_slice(payload), false, key)?)?;
            last_pts = pts;
            count += 1;
        }
    }
    Ok(count)
}

async fn publish((track, endpoint): Outbound, source: u32, duration_ms: u64) -> Result<Observed> {
    let mut protocol = endpoint.ready().await?;
    let (tx, mut rx) = mpsc::channel(2);
    let worker = tokio::task::spawn_blocking(move || encode_track(tx, track, source, duration_ms));
    let network_result = async {
        while let Some(packet) = rx.recv().await {
            protocol.send(packet).await?;
        }
        Ok::<_, types::ProtocolError>(())
    }
    .await;
    drop(rx);
    let count = worker.await?;
    network_result?;
    Ok(Observed {
        track: track.label(),
        sent_packets: count?,
        ..Observed::default()
    })
}

#[derive(Default, Serialize)]
struct Observed {
    track: &'static str,
    sent_packets: u32,
    received_packets: usize,
    decoded_frames: u32,
    samples: u32,
    tone_hz: f64,
    fingerprint: u32,
    first_decoded_us: u64,
    last_decoded_us: u64,
    pipeline_p50_us: u64,
    pipeline_p95_us: u64,
}

fn decode_track(
    track: Track,
    config: Bytes,
    mut rx: mpsc::Receiver<types::MediaPacket>,
) -> Result<Observed> {
    let mut decoder = Decoder::new(track, &config)?;
    let mut observed = Observed {
        track: track.label(),
        ..Observed::default()
    };
    let mut last_pts = 0;
    let mut crossings = 0;
    let mut timings = Vec::new();
    while let Some(packet) = rx.blocking_recv() {
        ensure!(
            packet.header.pts > last_pts,
            "non-increasing media timestamp"
        );
        last_pts = packet.header.pts;
        let decoded = decoder.decode(&packet.payload)?;
        observed.received_packets += 1;
        if decoded.frames != 0 {
            let now = wall_micros()?;
            if observed.first_decoded_us == 0 {
                observed.first_decoded_us = now;
            }
            observed.last_decoded_us = now;
            if let Some(elapsed) = now.checked_sub(last_pts) {
                timings.push(elapsed);
            }
        }
        observed.decoded_frames += decoded.frames;
        observed.samples += decoded.samples;
        crossings += decoded.crossings;
        observed.fingerprint = observed.fingerprint.rotate_left(5) ^ decoded.fingerprint;
    }
    ensure!(observed.decoded_frames != 0, "empty decoded track");
    if observed.samples != 0 {
        observed.tone_hz =
            f64::from(crossings) * f64::from(AUDIO_RATE) / f64::from(observed.samples);
    }
    timings.sort_unstable();
    if !timings.is_empty() {
        observed.pipeline_p50_us = timings[(timings.len() - 1) * 50 / 100];
        observed.pipeline_p95_us = timings[(timings.len() - 1) * 95 / 100];
    }
    Ok(observed)
}

async fn subscribe((track, endpoint): Inbound, progress: PathBuf) -> Result<Observed> {
    let mut protocol = endpoint.ready().await?;
    let Some(types::AVPacket::Codec(params)) = protocol.recv().await? else {
        anyhow::bail!("missing codec parameters");
    };
    ensure!(
        params.header.codec == codec(track) && params.header.rotation == 0,
        "unexpected codec parameters"
    );
    let Some(types::AVPacket::Media(config)) = protocol.recv().await? else {
        anyhow::bail!("missing codec configuration");
    };
    ensure!(
        config.header.is_config && config.payload.len() <= MAX_CONFIG,
        "invalid codec configuration"
    );
    let (tx, rx) = mpsc::channel(2);
    let worker = tokio::task::spawn_blocking(move || decode_track(track, config.payload, rx));
    let network_result = async {
        let mut received = 0;
        while let Some(packet) = protocol.recv().await? {
            let types::AVPacket::Media(packet) = packet else {
                anyhow::bail!("unexpected media packet");
            };
            ensure!(
                !packet.header.is_config
                    && !packet.payload.is_empty()
                    && packet.payload.len() <= MAX_PACKET
                    && received < MAX_RECEIVED_PACKETS,
                "invalid or excess media data"
            );
            tx.send(packet).await?;
            if received == 0 {
                tokio::fs::write(&progress, b"receiving").await?;
            }
            received += 1;
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    drop(tx);
    let decoded = worker.await?;
    network_result?;
    decoded
}

async fn run(directory: &Path) -> Result<()> {
    let config = Settings::load(directory)?;
    let role = config.role;
    let address: SocketAddr = (Ipv4Addr::LOCALHOST, config.port).into();
    kynet::init_crypto();
    let server = if role == Role::Listener {
        let server = transport::listen(
            address,
            &directory.join("cert.pem"),
            &directory.join("key.pem"),
        )
        .await?;
        std::fs::write(directory.join("ready"), b"listening")?;
        Some(server)
    } else {
        None
    };
    let connection = timeout(SETUP_TIMEOUT, async {
        if let Some(server) = &server {
            transport::accept(server, &config.secret).await
        } else {
            transport::connect(address, config.fingerprint, &config.secret).await
        }
    })
    .await
    .context("Kyber connection timed out")??;
    let remote_role = match role {
        Role::Listener => Role::Initiator,
        Role::Initiator => Role::Listener,
    };
    let selected = |role| {
        if config.direction.sends(role) {
            config.profile.tracks()
        } else {
            &[]
        }
    };
    let mut endpoints = timeout(
        SETUP_TIMEOUT,
        transport::register_tracks(&connection, role, selected(role), selected(remote_role)),
    )
    .await
    .context("Kyber track registration timed out")??;

    let duration = Duration::from_millis(config.duration_ms) + SETUP_TIMEOUT;
    let result = timeout(duration, async {
        let mut tasks = JoinSet::new();
        let source = u32::from(role == Role::Initiator);
        for endpoint in endpoints.outbound.drain(..) {
            let duration_ms = if endpoint.0 == Track::Screen {
                config.screen_stop_ms.unwrap_or(config.duration_ms)
            } else {
                config.duration_ms
            };
            let track = endpoint.0;
            tasks.spawn(async move {
                publish(endpoint, source, duration_ms)
                    .await
                    .with_context(|| format!("publishing {}", track.label()))
            });
        }
        for endpoint in endpoints.inbound.drain(..) {
            let track = endpoint.0;
            let progress = directory.join(format!("{}.receiving", track.label()));
            tasks.spawn(async move {
                subscribe(endpoint, progress)
                    .await
                    .with_context(|| format!("subscribing to {}", track.label()))
            });
        }
        let mut observations = Vec::new();
        while let Some(result) = tasks.join_next().await {
            observations.push(result??);
        }
        endpoints.finish(role).await?;
        Ok::<_, anyhow::Error>(observations)
    })
    .await
    .context("Kyber media run timed out")?;

    if result.is_ok() && role == Role::Listener {
        let _ = timeout(SETUP_TIMEOUT, connection.closed()).await;
    }
    connection.close();
    println!(
        "{}",
        serde_json::json!({ "role": role.label(), "tracks": result? })
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let directory = std::env::args_os()
        .nth(1)
        .context("usage: skyber-a1-peer RUN_DIRECTORY")?;
    run(&PathBuf::from(directory)).await
}
