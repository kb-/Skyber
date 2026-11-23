use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use kymux::{kynet, types};
use serde::{Deserialize, Serialize};
use skyber_core::Track;
use skyber_media_kyber::{
    native::{CaptureFrame, Decoder, Encoder, MAX_CONFIG, MAX_PACKET},
    transport::{self, Inbound, LanAddress, Outbound, Role},
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
    time::timeout,
};

const SETUP_TIMEOUT: Duration = Duration::from_secs(4);
const PLAYBACK_DRAIN_TIMEOUT: Duration = Duration::from_secs(12);
const MAX_RECEIVED_PACKETS: usize = 1000;
const SUSTAINED_DURATION_MS: u64 = 900_000;
// 15 minutes of 50 audio packets per second plus bounded headroom.
const MAX_SUSTAINED_PACKETS: usize = 60_000;
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
    bind_address: Option<Ipv4Addr>,
    remote_address: Option<Ipv4Addr>,
    secret: String,
    profile: Profile,
    direction: Direction,
    duration_ms: u64,
    #[serde(default)]
    sustained: bool,
    #[serde(default)]
    wait_for_remote_screen: bool,
    screen_stop_ms: Option<u64>,
    capture: Option<DeviceCapture>,
    #[serde(default)]
    capture_wait_ms: u64,
    #[serde(default)]
    playback_drain_timeout_ms: u64,
    #[serde(default)]
    upstream_avconv_frames: Option<usize>,
    #[serde(default)]
    upstream_avconv_decode: bool,
    audio_output: Option<DeviceSelector>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeviceCapture {
    microphone: DeviceSelector,
    camera: DeviceSelector,
    screen: bool,
}

#[derive(Clone, Deserialize)]
#[serde(untagged)]
enum DeviceSelector {
    Windows(u32),
    Ubuntu(String),
}

impl DeviceSelector {
    fn matches_platform(&self) -> bool {
        matches!(self, Self::Windows(_) if cfg!(windows))
            || matches!(self, Self::Ubuntu(_) if cfg!(target_os = "linux"))
    }
}

#[derive(Clone)]
enum Input {
    Synthetic,
    UpstreamAvconvSynthetic(usize),
    Device(DeviceSelector),
    ScreenPicker,
}

impl Settings {
    fn input(&self, track: Track) -> Input {
        if track == Track::Camera
            && let Some(frames) = self.upstream_avconv_frames
        {
            return Input::UpstreamAvconvSynthetic(frames);
        }
        match (&self.capture, track) {
            (Some(capture), Track::Microphone) => Input::Device(capture.microphone.clone()),
            (Some(capture), Track::Camera) => Input::Device(capture.camera.clone()),
            (Some(_), Track::Screen) => Input::ScreenPicker,
            (None, _) => Input::Synthetic,
        }
    }
}

impl Settings {
    fn load(directory: &Path) -> Result<Self> {
        let config: Self = serde_json::from_slice(&std::fs::read(directory.join("peer.json"))?)
            .context("invalid A1 run configuration")?;
        let max_duration_ms = if config.sustained {
            SUSTAINED_DURATION_MS
        } else {
            15_000
        };
        ensure!(
            config.port != 0 && (500..=max_duration_ms).contains(&config.duration_ms),
            "invalid port or run duration"
        );
        ensure!(
            (1..=256).contains(&config.secret.len())
                && config.fingerprint.len() == 64
                && config
                    .fingerprint
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit()),
            "invalid authentication configuration"
        );
        if let Some(stop) = config.screen_stop_ms {
            ensure!(
                matches!(config.profile, Profile::AvWithScreen)
                    && (500..config.duration_ms).contains(&stop),
                "invalid screen stop time"
            );
        }
        ensure!(
            config.capture_wait_ms <= 65_000,
            "capture setup allowance exceeds 65 seconds"
        );
        if let Some(frames) = config.upstream_avconv_frames {
            ensure!(
                (1..=MAX_RECEIVED_PACKETS).contains(&frames)
                    && !matches!(config.profile, Profile::Audio)
                    && config.direction.sends(config.role)
                    && config.capture.is_none(),
                "upstream avconv input requires a sending video peer, 1..=1000 frames and no live capture"
            );
        }
        ensure!(
            !config.upstream_avconv_decode
                || (cfg!(windows)
                    && !matches!(config.profile, Profile::Audio)
                    && config.direction.sends(match config.role {
                        Role::Listener => Role::Initiator,
                        Role::Initiator => Role::Listener,
                    })),
            "upstream avconv decoding requires a Windows peer receiving video"
        );
        if let Some(capture) = &config.capture {
            ensure!(
                capture.microphone.matches_platform()
                    && capture.camera.matches_platform()
                    && config.direction.sends(config.role)
                    && !matches!(config.profile, Profile::Audio)
                    && capture.screen == matches!(config.profile, Profile::AvWithScreen),
                "invalid capture profile or device selector for this platform"
            );
        }
        if let Some(output) = &config.audio_output {
            ensure!(
                output.matches_platform(),
                "invalid audio output selector for this platform"
            );
        }
        ensure!(
            !config.wait_for_remote_screen
                || (matches!(config.profile, Profile::AvWithScreen)
                    && matches!(config.direction, Direction::Both)
                    && config.role == Role::Listener
                    && config.capture.is_some()
                    && config.capture_wait_ms > 0),
            "waiting for remote screen requires a bidirectional live screen listener"
        );
        Ok(config)
    }
}

fn codec(track: Track) -> u32 {
    u32::from_be_bytes(match track {
        Track::Microphone => *b"opus",
        Track::Camera | Track::Screen => *b"h264",
    })
}

fn elapsed_micros(origin: Instant) -> Result<u64> {
    Ok(origin.elapsed().as_micros().try_into()?)
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

struct SourceFrame {
    bytes: Vec<u8>,
    source_ready_us: u64,
    capture_helper_timestamp_us: u64,
    capture_queue_depth: usize,
    capture_queue_wait_us: u64,
}

struct EncodedPacket {
    packet: types::AVPacket,
    timing: Option<TimingSample>,
}

struct ReceivedPacket {
    packet: types::MediaPacket,
    timing: TimingSample,
}

struct PresentedPacket {
    packet_id_us: u64,
    playback_queue_depth: usize,
    bytes: Vec<u8>,
}

#[derive(Clone, Serialize)]
struct PresentationEvent {
    packet_id_us: u64,
    helper_timestamp_us: u64,
    helper_clock_domain: &'static str,
}

#[derive(Clone, Serialize)]
struct AudioDrainEvent {
    sample_frames: u64,
    helper_timestamp_us: u64,
    helper_clock_domain: &'static str,
}

#[derive(Default, Serialize)]
struct PresentationSummary {
    clock_domain: &'static str,
    presented_frames: usize,
    decoded_frames_without_paint_event: usize,
    frames_per_second: Option<f64>,
    inter_event_interval_us: Percentiles,
}

#[derive(Serialize)]
struct TransportSample {
    peer_timestamp_us: u64,
    rtt_us: Option<u64>,
    lost_packets_total: Option<u64>,
}

#[derive(Serialize)]
struct TransportReport {
    clock_domain: &'static str,
    rtt_us: Percentiles,
    lost_packets_total_start: Option<u64>,
    lost_packets_total_end: Option<u64>,
    lost_packets_delta: Option<u64>,
    protocol_dropped_packets: Option<u64>,
    samples: Vec<TransportSample>,
}

#[derive(Clone, Default, Serialize)]
struct TimingSample {
    packet_id_us: u64,
    source_ready_us: Option<u64>,
    capture_helper_timestamp_us: Option<u64>,
    capture_queue_depth: Option<usize>,
    capture_queue_wait_us: Option<u64>,
    encode_start_us: Option<u64>,
    encode_end_us: Option<u64>,
    encode_queue_depth: Option<usize>,
    transport_send_start_us: Option<u64>,
    transport_send_end_us: Option<u64>,
    transport_receive_us: Option<u64>,
    decode_start_us: Option<u64>,
    decode_end_us: Option<u64>,
    playback_queue_depth: Option<usize>,
    playback_enqueue_us: Option<u64>,
    playback_submit_start_us: Option<u64>,
    playback_submit_end_us: Option<u64>,
}

#[derive(Default, Serialize)]
struct Percentiles {
    samples: usize,
    p50_us: Option<u64>,
    p95_us: Option<u64>,
}

#[derive(Default, Serialize)]
struct TimingSummary {
    clock_domain: &'static str,
    capture_pipe_packets_per_second: Option<f64>,
    received_packets_per_second: Option<f64>,
    source_to_encode_us: Percentiles,
    capture_queue_wait_us: Percentiles,
    encode_us: Percentiles,
    encode_to_transport_us: Percentiles,
    transport_send_us: Percentiles,
    receive_to_decode_us: Percentiles,
    decode_us: Percentiles,
    decode_to_playback_queue_us: Percentiles,
    playback_submit_us: Percentiles,
    capture_queue_depth_max: usize,
    encode_queue_depth_max: usize,
    playback_queue_depth_max: usize,
}

fn percentiles(mut samples: Vec<u64>) -> Percentiles {
    samples.sort_unstable();
    let percentile = |percent| {
        let rank = samples.len().checked_mul(percent)?.div_ceil(100);
        rank.checked_sub(1)
            .and_then(|index| samples.get(index).copied())
    };
    Percentiles {
        samples: samples.len(),
        p50_us: percentile(50),
        p95_us: percentile(95),
    }
}

fn packet_rate(
    samples: &[TimingSample],
    timestamp: impl Fn(&TimingSample) -> Option<u64>,
) -> Option<f64> {
    let times: Vec<_> = samples.iter().filter_map(timestamp).collect();
    let first = *times.first()?;
    let last = *times.last()?;
    let span = last.checked_sub(first)?;
    if times.len() < 2 || span == 0 {
        return None;
    }
    Some((times.len() - 1) as f64 * 1_000_000.0 / span as f64)
}

fn timing_summary(samples: &[TimingSample]) -> TimingSummary {
    let duration = |start: Option<u64>, end: Option<u64>| {
        start
            .zip(end)
            .and_then(|(start, end)| end.checked_sub(start))
    };
    TimingSummary {
        clock_domain: "peer_monotonic_us_since_media_start",
        capture_pipe_packets_per_second: packet_rate(samples, |sample| sample.source_ready_us),
        received_packets_per_second: packet_rate(samples, |sample| sample.transport_receive_us),
        source_to_encode_us: percentiles(
            samples
                .iter()
                .filter_map(|sample| duration(sample.source_ready_us, sample.encode_start_us))
                .collect(),
        ),
        capture_queue_wait_us: percentiles(
            samples
                .iter()
                .filter_map(|sample| sample.capture_queue_wait_us)
                .collect(),
        ),
        encode_us: percentiles(
            samples
                .iter()
                .filter_map(|sample| duration(sample.encode_start_us, sample.encode_end_us))
                .collect(),
        ),
        encode_to_transport_us: percentiles(
            samples
                .iter()
                .filter_map(|sample| duration(sample.encode_end_us, sample.transport_send_start_us))
                .collect(),
        ),
        transport_send_us: percentiles(
            samples
                .iter()
                .filter_map(|sample| {
                    duration(sample.transport_send_start_us, sample.transport_send_end_us)
                })
                .collect(),
        ),
        receive_to_decode_us: percentiles(
            samples
                .iter()
                .filter_map(|sample| duration(sample.transport_receive_us, sample.decode_start_us))
                .collect(),
        ),
        decode_us: percentiles(
            samples
                .iter()
                .filter_map(|sample| duration(sample.decode_start_us, sample.decode_end_us))
                .collect(),
        ),
        decode_to_playback_queue_us: percentiles(
            samples
                .iter()
                .filter_map(|sample| duration(sample.decode_end_us, sample.playback_enqueue_us))
                .collect(),
        ),
        playback_submit_us: percentiles(
            samples
                .iter()
                .filter_map(|sample| {
                    duration(
                        sample.playback_submit_start_us,
                        sample.playback_submit_end_us,
                    )
                })
                .collect(),
        ),
        capture_queue_depth_max: samples
            .iter()
            .filter_map(|sample| sample.capture_queue_depth)
            .max()
            .unwrap_or(0),
        encode_queue_depth_max: samples
            .iter()
            .filter_map(|sample| sample.encode_queue_depth)
            .max()
            .unwrap_or(0),
        playback_queue_depth_max: samples
            .iter()
            .filter_map(|sample| sample.playback_queue_depth)
            .max()
            .unwrap_or(0),
    }
}

fn parse_video_presentation_event(line: &str) -> Result<Option<PresentationEvent>> {
    let Some(rest) = line.strip_prefix("SKYBER_PRESENT_VIDEO ") else {
        return Ok(None);
    };
    let mut fields = rest.split_ascii_whitespace();
    let packet_id_us = fields
        .next()
        .context("missing presentation packet ID")?
        .parse()?;
    let helper_timestamp_us = fields
        .next()
        .context("missing presentation helper timestamp")?
        .parse()?;
    ensure!(
        fields.next().is_none(),
        "unexpected presentation event fields"
    );
    Ok(Some(PresentationEvent {
        packet_id_us,
        helper_timestamp_us,
        helper_clock_domain: "playback_helper_monotonic_us_since_start",
    }))
}

fn parse_audio_drain_event(line: &str) -> Result<Option<AudioDrainEvent>> {
    let Some(rest) = line.strip_prefix("SKYBER_AUDIO_DRAINED ") else {
        return Ok(None);
    };
    let mut fields = rest.split_ascii_whitespace();
    let sample_frames = fields
        .next()
        .context("missing audio drain sample-frame count")?
        .parse()?;
    let helper_timestamp_us = fields
        .next()
        .context("missing audio drain helper timestamp")?
        .parse()?;
    ensure!(
        fields.next().is_none(),
        "unexpected audio drain event fields"
    );
    Ok(Some(AudioDrainEvent {
        sample_frames,
        helper_timestamp_us,
        helper_clock_domain: "playback_helper_monotonic_us_since_start",
    }))
}

fn parse_video_superseded_event(line: &str, expected_track: &str) -> Result<Option<usize>> {
    let Some(rest) = line.strip_prefix("SKYBER_VIDEO_SUPERSEDED ") else {
        return Ok(None);
    };
    let mut fields = rest.split_ascii_whitespace();
    let track = fields.next().context("missing video superseded track")?;
    ensure!(track == expected_track, "video superseded track changed");
    let count = fields
        .next()
        .context("missing video superseded frame count")?
        .parse()?;
    ensure!(
        fields.next().is_none(),
        "unexpected video superseded fields"
    );
    Ok(Some(count))
}

fn presentation_summary(
    events: &[PresentationEvent],
    decoded_frames: usize,
) -> PresentationSummary {
    let times: Vec<_> = events
        .iter()
        .map(|event| event.helper_timestamp_us)
        .collect();
    let intervals: Vec<_> = times
        .windows(2)
        .filter_map(|pair| pair[1].checked_sub(pair[0]))
        .collect();
    let frames_per_second = times
        .first()
        .zip(times.last())
        .filter(|(first, last)| last > first)
        .map(|(first, last)| (times.len() - 1) as f64 * 1_000_000.0 / (last - first) as f64);
    PresentationSummary {
        clock_domain: "playback_helper_monotonic_us_since_start",
        presented_frames: events.len(),
        decoded_frames_without_paint_event: decoded_frames.saturating_sub(events.len()),
        frames_per_second,
        inter_event_interval_us: percentiles(intervals),
    }
}

async fn transport_sample(
    provider: &kymux::kyproto::KyProtoStatsProvider,
    origin: Instant,
) -> Result<TransportSample> {
    let stats = provider.connection_stats().await;
    Ok(TransportSample {
        peer_timestamp_us: elapsed_micros(origin)?,
        rtt_us: stats.rtt.and_then(|rtt| rtt.as_micros().try_into().ok()),
        lost_packets_total: stats.packets_lost,
    })
}

fn transport_report(
    samples: Vec<TransportSample>,
    protocol_dropped_packets: Option<u64>,
) -> TransportReport {
    let lost_packets_total_start = samples.first().and_then(|sample| sample.lost_packets_total);
    let lost_packets_total_end = samples.last().and_then(|sample| sample.lost_packets_total);
    TransportReport {
        clock_domain: "peer_monotonic_us_since_media_start",
        rtt_us: percentiles(samples.iter().filter_map(|sample| sample.rtt_us).collect()),
        lost_packets_total_start,
        lost_packets_total_end,
        lost_packets_delta: lost_packets_total_start
            .zip(lost_packets_total_end)
            .and_then(|(start, end)| end.checked_sub(start)),
        protocol_dropped_packets,
        samples,
    }
}

fn encode_track(
    tx: mpsc::Sender<EncodedPacket>,
    track: Track,
    source: u32,
    duration_ms: u64,
    mut capture: Option<mpsc::Receiver<SourceFrame>>,
    origin: Instant,
) -> Result<u32> {
    let mut encoder = Encoder::new(track, source)?;
    tx.blocking_send(EncodedPacket {
        packet: types::AVPacket::Codec(types::CodecPacket {
            header: types::CodecPacketHeader {
                codec: codec(track),
                rotation: 0,
                frame_size: 0,
            },
        }),
        timing: None,
    })?;
    tx.blocking_send(EncodedPacket {
        packet: media(0, Bytes::from(encoder.config()?), true, false)?,
        timing: None,
    })?;
    let rate = if track == Track::Microphone { 50 } else { 15 };
    let frames = duration_ms * rate / 1000;
    let start = Instant::now();
    let mut count = 0;
    let mut last_pts = 0;
    for frame in 0.. {
        let captured = if let Some(capture) = &mut capture {
            let Some(frame) = capture.blocking_recv() else {
                break;
            };
            Some(frame)
        } else {
            if frame >= frames {
                break;
            }
            let deadline = start + Duration::from_micros(frame * 1_000_000 / rate);
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
            None
        };
        let source_ready_us = captured
            .as_ref()
            .map_or_else(|| elapsed_micros(origin), |frame| Ok(frame.source_ready_us))?;
        let pts = source_ready_us.max(last_pts + 1);
        let encode_start_us = elapsed_micros(origin)?;
        let (payload, key) = match captured.as_ref().map(|frame| frame.bytes.as_slice()) {
            Some(bytes) if track == Track::Microphone => {
                let mut samples = [0.0; 1920];
                for (sample, bytes) in samples.iter_mut().zip(bytes.chunks_exact(4)) {
                    let raw: [u8; 4] = bytes.try_into()?;
                    *sample = f32::from_le_bytes(raw);
                }
                encoder.encode_capture(CaptureFrame::PcmF32(&samples))?
            }
            Some(bytes) => encoder.encode_capture(CaptureFrame::I420(bytes))?,
            None => encoder.next_packet()?,
        };
        let encode_end_us = elapsed_micros(origin)?;
        if !payload.is_empty() {
            let encode_queue_depth = tx.max_capacity().saturating_sub(tx.capacity());
            let mut timing = TimingSample {
                packet_id_us: pts,
                source_ready_us: Some(source_ready_us),
                capture_helper_timestamp_us: captured
                    .as_ref()
                    .map(|frame| frame.capture_helper_timestamp_us),
                encode_start_us: Some(encode_start_us),
                encode_end_us: Some(encode_end_us),
                encode_queue_depth: Some(encode_queue_depth),
                ..TimingSample::default()
            };
            if let Some(frame) = &captured {
                timing.capture_queue_depth = Some(frame.capture_queue_depth);
                timing.capture_queue_wait_us = Some(frame.capture_queue_wait_us);
            }
            let packet = media(pts, Bytes::copy_from_slice(payload), false, key)?;
            tx.blocking_send(EncodedPacket {
                packet,
                timing: Some(timing),
            })?;
            last_pts = pts;
            count += 1;
        }
    }
    Ok(count)
}

async fn capture_frames(
    track: Track,
    input: Input,
    duration_ms: u64,
    channel: mpsc::Sender<SourceFrame>,
    ready: Option<watch::Sender<bool>>,
    origin: Instant,
) -> Result<()> {
    use std::process::Stdio;
    use tokio::{io::AsyncReadExt, process::Command};

    let name = if cfg!(windows) {
        "skyber-windows-capture.exe"
    } else {
        "skyber-ubuntu-capture"
    };
    let helper = std::env::current_exe()?.with_file_name(name);
    let mut command = Command::new(helper);
    let duration = duration_ms.to_string();
    match input {
        Input::Device(DeviceSelector::Windows(index)) => {
            command.args(["--stream", track.label(), &index.to_string(), &duration]);
        }
        Input::Device(DeviceSelector::Ubuntu(device)) => {
            command.args([track.label(), &duration, "--device", &device]);
        }
        Input::ScreenPicker => {
            command.args([if cfg!(windows) { "--screen" } else { "screen" }, &duration]);
        }
        Input::Synthetic => anyhow::bail!("synthetic input does not start a capture helper"),
        Input::UpstreamAvconvSynthetic(_) => {
            anyhow::bail!("upstream avconv input uses its dedicated publisher")
        }
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("start platform capture helper")?;
    let mut stdout = child.stdout.take().context("missing capture pipe")?;
    let size = if track == Track::Microphone {
        1920 * 4
    } else {
        320 * 180 * 3 / 2
    };
    let mut ready = ready;
    loop {
        let mut timestamp = [0; 8];
        if stdout.read(&mut timestamp[..1]).await? == 0 {
            break;
        }
        stdout
            .read_exact(&mut timestamp[1..])
            .await
            .context("truncated capture timestamp")?;
        let mut frame = vec![0; size];
        stdout.read_exact(&mut frame[..1]).await?;
        stdout
            .read_exact(&mut frame[1..])
            .await
            .context("truncated capture frame")?;
        if let Some(signal) = ready.take() {
            signal.send_replace(true);
        }
        let queue_depth = channel.max_capacity().saturating_sub(channel.capacity());
        let queued = SourceFrame {
            bytes: frame,
            source_ready_us: elapsed_micros(origin)?,
            capture_helper_timestamp_us: u64::from_le_bytes(timestamp),
            capture_queue_depth: queue_depth,
            capture_queue_wait_us: 0,
        };
        let queue_wait_start = Instant::now();
        let permit = channel.reserve().await?;
        let mut queued = queued;
        queued.capture_queue_wait_us = queue_wait_start.elapsed().as_micros().try_into()?;
        permit.send(queued);
    }
    ensure!(child.wait().await?.success(), "capture helper failed");
    Ok(())
}

async fn publish(
    (track, endpoint): Outbound,
    source: u32,
    duration_ms: u64,
    input: Input,
    screen_ready: Option<watch::Sender<bool>>,
    remote_screen_ready: Option<watch::Receiver<bool>>,
    origin: Instant,
) -> Result<Observed> {
    let input = match input {
        Input::UpstreamAvconvSynthetic(frames) => {
            return publish_upstream_avconv((track, endpoint), frames, origin).await;
        }
        input => input,
    };
    let mut protocol = endpoint.ready().await?;
    let (tx, mut rx) = mpsc::channel(2);
    let (raw_tx, raw_rx) = mpsc::channel(2);
    let capture = (!matches!(input, Input::Synthetic)).then_some(raw_rx);
    let worker = tokio::task::spawn_blocking(move || {
        encode_track(tx, track, source, duration_ms, capture, origin)
    });
    let send = async {
        let mut timings = Vec::new();
        while let Some(packet) = rx.recv().await {
            let mut packet = packet;
            if let Some(mut timing) = packet.timing.take() {
                timing.transport_send_start_us = Some(elapsed_micros(origin)?);
                protocol.send(packet.packet).await?;
                timing.transport_send_end_us = Some(elapsed_micros(origin)?);
                ensure!(
                    timings.len() < MAX_SUSTAINED_PACKETS,
                    "excess published media data"
                );
                timings.push(timing);
            } else {
                protocol.send(packet.packet).await?;
            }
        }
        Ok::<_, anyhow::Error>(timings)
    };
    let acquire = async {
        if matches!(input, Input::Synthetic) {
            Ok(())
        } else {
            if let (Input::ScreenPicker, Some(mut ready)) = (&input, remote_screen_ready) {
                ready.wait_for(|ready| *ready).await?;
            }
            if let (Input::Device(_), Some(ready)) = (&input, &screen_ready) {
                let mut signal = ready.subscribe();
                signal.wait_for(|ready| *ready).await?;
            }
            let signal = matches!(input, Input::ScreenPicker)
                .then_some(screen_ready)
                .flatten();
            capture_frames(track, input, duration_ms, raw_tx, signal, origin).await
        }
    };
    let encode = async { worker.await? };
    let (mut timings, (), sent_packets) = tokio::try_join!(send, acquire, encode)?;
    ensure!(sent_packets > 0, "empty published track");
    timings.sort_unstable_by_key(|timing| timing.packet_id_us);
    let summary = timing_summary(&timings);
    Ok(Observed {
        track: track.label(),
        sent_packets,
        timings,
        timing_summary: summary,
        ..Observed::default()
    })
}

async fn publish_upstream_avconv(
    (track, endpoint): Outbound,
    frames: usize,
    origin: Instant,
) -> Result<Observed> {
    use std::process::Stdio;
    use tokio::{io::AsyncReadExt, process::Command};

    ensure!(
        track == Track::Camera,
        "upstream avconv spike supports webcam video only"
    );
    ensure!(
        cfg!(windows),
        "upstream avconv probe is currently built for Windows only"
    );
    let executable = std::env::current_exe()?.with_file_name("skyber-avconv-frame-probe.exe");
    let frame_count = frames.to_string();
    let mut child = Command::new(executable)
        .args(["--emit", "--synthetic", frame_count.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("start pinned Kyber avconv video probe")?;
    let mut stdout = child.stdout.take().context("missing avconv output pipe")?;
    let mut stderr = child.stderr.take().context("missing avconv log pipe")?;
    let stderr_task = tokio::spawn(async move {
        let mut output = Vec::new();
        stderr.read_to_end(&mut output).await?;
        Ok::<_, std::io::Error>(output)
    });

    let mut magic = [0; 8];
    stdout
        .read_exact(&mut magic)
        .await
        .context("read avconv stream signature")?;
    ensure!(&magic == b"SKYAVC01", "invalid avconv stream signature");
    let mut protocol = endpoint.ready().await?;
    let mut codec_config_received = false;
    let mut sent_packets = 0_u32;
    let mut last_pts_us = 0_u64;
    let mut timings = Vec::with_capacity(frames);

    loop {
        let record_type = match stdout.read_u8().await {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error).context("read avconv record type"),
        };
        let mut rest = [0; 13];
        stdout
            .read_exact(&mut rest)
            .await
            .context("read avconv record header")?;
        let pts_us = u64::from_le_bytes(rest[..8].try_into()?);
        ensure!(rest[8] <= 1, "invalid avconv keyframe flag");
        let is_key = rest[8] != 0;
        ensure!(
            record_type == 1 || record_type == 2,
            "unknown avconv record type"
        );
        let size = u32::from_le_bytes(rest[9..13].try_into()?) as usize;
        let limit = if record_type == 1 {
            MAX_CONFIG
        } else {
            MAX_PACKET
        };
        ensure!(
            size > 0 && size <= limit,
            "avconv record size exceeds its bound"
        );
        let mut payload = vec![0; size];
        stdout
            .read_exact(&mut payload)
            .await
            .context("read avconv packet payload")?;

        match record_type {
            1 => {
                ensure!(
                    !codec_config_received && sent_packets == 0 && pts_us == 0 && !is_key,
                    "invalid or repeated avconv codec configuration"
                );
                protocol
                    .send(types::AVPacket::Codec(types::CodecPacket {
                        header: types::CodecPacketHeader {
                            codec: codec(Track::Camera),
                            rotation: 0,
                            frame_size: 0,
                        },
                    }))
                    .await?;
                protocol
                    .send(media(0, Bytes::from(payload), true, false)?)
                    .await?;
                codec_config_received = true;
            }
            2 => {
                ensure!(
                    codec_config_received,
                    "avconv media preceded codec configuration"
                );
                ensure!(
                    sent_packets < frames as u32 && pts_us <= (frames as u64 * 1_000_000 / 15) + 1,
                    "avconv emitted more frames or a timestamp beyond the requested source"
                );
                ensure!(pts_us > last_pts_us, "avconv timestamps are not increasing");
                tokio::time::sleep(Duration::from_micros(pts_us).saturating_sub(origin.elapsed()))
                    .await;
                let mut timing = TimingSample {
                    packet_id_us: pts_us,
                    ..TimingSample::default()
                };
                timing.transport_send_start_us = Some(elapsed_micros(origin)?);
                protocol
                    .send(media(pts_us, Bytes::from(payload), false, is_key)?)
                    .await?;
                timing.transport_send_end_us = Some(elapsed_micros(origin)?);
                timings.push(timing);
                last_pts_us = pts_us;
                sent_packets += 1;
            }
            _ => anyhow::bail!("unsupported avconv record type {record_type}"),
        }
    }

    ensure!(
        codec_config_received && sent_packets == frames as u32,
        "avconv did not emit the requested configured video frame count"
    );
    let exit = child.wait().await.context("wait for avconv probe")?;
    let diagnostic = stderr_task.await.context("join avconv log reader")??;
    eprint!("{}", String::from_utf8_lossy(&diagnostic));
    ensure!(exit.success(), "Kyber avconv probe failed with {exit}");
    let summary = timing_summary(&timings);
    Ok(Observed {
        track: track.label(),
        sent_packets,
        timings,
        timing_summary: summary,
        encoder_backend: Some("kyber-avconv-libx264"),
        ..Observed::default()
    })
}

#[derive(Default, Serialize)]
struct Observed {
    track: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    encoder_backend: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    decoder_backend: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    decoded_frame_checksum: Option<u64>,
    sent_packets: u32,
    received_packets: usize,
    decoded_frames: u32,
    samples: u32,
    tone_hz: f64,
    fingerprint: u32,
    first_decoded_us: u64,
    last_decoded_us: u64,
    timings: Vec<TimingSample>,
    presentation_events: Vec<PresentationEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    presentation_superseded_frames: Option<usize>,
    audio_drain_events: Vec<AudioDrainEvent>,
    #[serde(skip_serializing_if = "presentation_summary_is_empty")]
    presentation_summary: PresentationSummary,
    timing_summary: TimingSummary,
    decoded_frames_per_second: f64,
    #[serde(skip_serializing_if = "is_zero")]
    playback_bytes: usize,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

fn presentation_summary_is_empty(summary: &PresentationSummary) -> bool {
    summary.presented_frames == 0
}

async fn present(
    track: Track,
    mut rx: mpsc::Receiver<PresentedPacket>,
    audio_output: Option<DeviceSelector>,
    origin: Instant,
) -> Result<(
    usize,
    Vec<TimingSample>,
    Vec<PresentationEvent>,
    Vec<AudioDrainEvent>,
    Option<usize>,
)> {
    use std::process::Stdio;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        process::Command,
    };

    let mut first_packet = Some(rx.recv().await.context("playback had no decoded output")?);
    let name = if cfg!(windows) {
        "skyber-windows-playback.exe"
    } else {
        "skyber-ubuntu-playback"
    };
    let helper = std::env::current_exe()?.with_file_name(name);
    let mut command = Command::new(helper);
    command.arg(track.label());
    if track == Track::Microphone {
        match audio_output {
            Some(DeviceSelector::Windows(index)) => {
                command.arg(index.to_string());
            }
            Some(DeviceSelector::Ubuntu(name)) => {
                command.arg(name);
            }
            None => {}
        }
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("start platform playback helper")?;
    let mut stdin = child.stdin.take().context("missing playback pipe")?;
    let stderr = child
        .stderr
        .take()
        .context("missing playback helper log pipe")?;
    let event_reader = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut video_events = Vec::new();
        let mut audio_events = Vec::new();
        let mut superseded_frames = None;
        let expected_track = track.label();
        while let Some(line) = lines.next_line().await? {
            if let Some(event) = parse_video_presentation_event(&line)? {
                ensure!(
                    video_events.len() < MAX_SUSTAINED_PACKETS,
                    "excess video presentation events"
                );
                video_events.push(event);
            } else if let Some(event) = parse_audio_drain_event(&line)? {
                ensure!(audio_events.is_empty(), "duplicate audio drain event");
                audio_events.push(event);
            } else if let Some(count) = parse_video_superseded_event(&line, expected_track)? {
                ensure!(
                    superseded_frames.replace(count).is_none(),
                    "duplicate video superseded event"
                );
            } else {
                eprintln!("{line}");
            }
        }
        Ok::<_, anyhow::Error>((video_events, audio_events, superseded_frames))
    });
    let mut delivered = 0;
    let mut dismissed = false;
    let mut timings = Vec::new();
    while let Some(packet) = match first_packet.take() {
        Some(packet) => Some(packet),
        None => rx.recv().await,
    } {
        if !dismissed {
            let mut timing = TimingSample {
                packet_id_us: packet.packet_id_us,
                playback_submit_start_us: Some(elapsed_micros(origin)?),
                ..TimingSample::default()
            };
            let write_result = if track == Track::Microphone {
                stdin.write_all(&packet.bytes).await
            } else {
                let frame_size = 320 * 180 * 3 / 2;
                ensure!(
                    packet.bytes.len() % frame_size == 0,
                    "decoded video is not whole I420 frames"
                );
                let packet_id = packet.packet_id_us.to_le_bytes();
                let mut result = Ok(());
                for frame in packet.bytes.chunks_exact(frame_size) {
                    if let Err(error) = stdin.write_all(&packet_id).await {
                        result = Err(error);
                        break;
                    }
                    if let Err(error) = stdin.write_all(frame).await {
                        result = Err(error);
                        break;
                    }
                }
                result
            };
            match write_result {
                Ok(()) => {
                    delivered += packet.bytes.len();
                    timing.playback_submit_end_us = Some(elapsed_micros(origin)?);
                    timing.playback_queue_depth = Some(packet.playback_queue_depth);
                    timings.push(timing);
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::BrokenPipe
                        && track != Track::Microphone =>
                {
                    // A user may close one video viewer while the other tracks continue.
                    ensure!(child.wait().await?.success(), "video viewer failed");
                    dismissed = true;
                }
                Err(error) => return Err(error).context("write frames to playback helper"),
            }
        }
    }
    drop(stdin);
    if !dismissed {
        ensure!(child.wait().await?.success(), "playback helper failed");
    }
    ensure!(delivered > 0 || dismissed, "playback had no decoded output");
    let (presentation_events, audio_drain_events, superseded_frames) = event_reader
        .await
        .context("read presentation helper events")??;
    Ok((
        delivered,
        timings,
        presentation_events,
        audio_drain_events,
        superseded_frames,
    ))
}

fn decode_track(
    track: Track,
    config: Bytes,
    mut rx: mpsc::Receiver<ReceivedPacket>,
    origin: Instant,
    playback: Option<mpsc::Sender<PresentedPacket>>,
) -> Result<Observed> {
    let mut decoder = Decoder::new(track, &config)?;
    let mut observed = Observed {
        track: track.label(),
        ..Observed::default()
    };
    let mut last_pts = 0;
    let mut crossings = 0;
    let mut output = playback.as_ref().map(|_| vec![0; MAX_PACKET]);
    while let Some(ReceivedPacket { packet, mut timing }) = rx.blocking_recv() {
        ensure!(
            packet.header.pts > last_pts,
            "non-increasing media timestamp"
        );
        ensure!(
            timing.packet_id_us == packet.header.pts,
            "packet timing identity mismatch"
        );
        last_pts = packet.header.pts;
        timing.decode_start_us = Some(elapsed_micros(origin)?);
        let decoded = if let (Some(tx), Some(buffer)) = (&playback, &mut output) {
            let (decoded, frames) = decoder.decode_output(&packet.payload, buffer)?;
            timing.decode_end_us = Some(elapsed_micros(origin)?);
            if !frames.is_empty() {
                timing.playback_queue_depth = Some(tx.max_capacity().saturating_sub(tx.capacity()));
                tx.blocking_send(PresentedPacket {
                    packet_id_us: timing.packet_id_us,
                    playback_queue_depth: timing.playback_queue_depth.unwrap_or(0),
                    bytes: frames.to_vec(),
                })?;
                timing.playback_enqueue_us = Some(elapsed_micros(origin)?);
            }
            decoded
        } else {
            let decoded = decoder.decode(&packet.payload)?;
            timing.decode_end_us = Some(elapsed_micros(origin)?);
            decoded
        };
        observed.received_packets += 1;
        if decoded.frames != 0 {
            let now = elapsed_micros(origin)? + 1;
            if observed.first_decoded_us == 0 {
                observed.first_decoded_us = now;
            }
            observed.last_decoded_us = now;
        }
        observed.decoded_frames += decoded.frames;
        observed.samples += decoded.samples;
        crossings += decoded.crossings;
        observed.fingerprint = observed.fingerprint.rotate_left(5) ^ decoded.fingerprint;
        observed.timings.push(timing);
    }
    ensure!(observed.decoded_frames != 0, "empty decoded track");
    if observed.samples != 0 {
        observed.tone_hz =
            f64::from(crossings) * f64::from(AUDIO_RATE) / f64::from(observed.samples);
    }
    observed.timing_summary = timing_summary(&observed.timings);
    let decoded_span = observed
        .last_decoded_us
        .saturating_sub(observed.first_decoded_us);
    if decoded_span > 0 {
        observed.decoded_frames_per_second =
            f64::from(observed.decoded_frames) * 1_000_000.0 / decoded_span as f64;
    }
    Ok(observed)
}

#[derive(Deserialize)]
struct UpstreamDecodeSummary {
    decoder_backend: String,
    packets: usize,
    decoded_frames: u32,
    last_pts: u64,
    frame_checksum: u64,
}

fn write_avconv_record_header(
    output: &mut impl std::io::Write,
    record_type: u8,
    pts_us: u64,
    is_key: bool,
    size: usize,
) -> Result<()> {
    ensure!(
        size > 0 && size <= u32::MAX as usize,
        "invalid avconv record size"
    );
    output.write_all(&[record_type])?;
    output.write_all(&pts_us.to_le_bytes())?;
    output.write_all(&[u8::from(is_key)])?;
    output.write_all(&(size as u32).to_le_bytes())?;
    Ok(())
}

fn decode_track_upstream_avconv(
    track: Track,
    config: Bytes,
    mut rx: mpsc::Receiver<ReceivedPacket>,
) -> Result<Observed> {
    use std::{io::Write, process::Stdio};

    ensure!(
        track == Track::Camera,
        "upstream avconv proof only supports video"
    );
    ensure!(
        !config.is_empty() && config.len() <= MAX_CONFIG,
        "invalid H.264 config size"
    );
    let executable =
        std::env::current_exe()?.with_file_name("skyber-avconv-packet-decode-probe.exe");
    let mut child = std::process::Command::new(executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("start pinned Kyber avconv decoder probe")?;
    let mut stdin = child
        .stdin
        .take()
        .context("missing avconv decoder input pipe")?;
    stdin.write_all(b"SKYAVC01")?;
    write_avconv_record_header(&mut stdin, 1, 0, false, config.len())?;
    stdin.write_all(&config)?;

    let mut observed = Observed {
        track: track.label(),
        decoder_backend: Some("kyber-avconv-h264"),
        ..Observed::default()
    };
    let mut last_pts = 0_u64;
    while let Some(ReceivedPacket { packet, timing }) = rx.blocking_recv() {
        ensure!(
            !packet.header.is_config
                && !packet.payload.is_empty()
                && packet.payload.len() <= MAX_PACKET
                && packet.header.pts > last_pts,
            "invalid upstream avconv video packet"
        );
        ensure!(
            timing.packet_id_us == packet.header.pts,
            "packet timing identity mismatch"
        );
        write_avconv_record_header(
            &mut stdin,
            2,
            packet.header.pts,
            packet.header.is_key,
            packet.payload.len(),
        )?;
        stdin.write_all(&packet.payload)?;
        observed.timings.push(timing);
        observed.received_packets += 1;
        last_pts = packet.header.pts;
    }
    drop(stdin);
    let output = child
        .wait_with_output()
        .context("wait for Kyber avconv decoder probe")?;
    ensure!(
        output.status.success(),
        "Kyber avconv decoder failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary: UpstreamDecodeSummary =
        serde_json::from_slice(&output.stdout).context("parse Kyber avconv decoder summary")?;
    ensure!(
        summary.decoder_backend == "kyber-avconv-h264"
            && summary.packets == observed.received_packets
            && summary.decoded_frames > 0
            && summary.last_pts < u64::from(summary.decoded_frames) + 30
            && summary.frame_checksum != 0,
        "Kyber avconv decoder did not return valid frames"
    );
    observed.decoded_frames = summary.decoded_frames;
    observed.decoded_frame_checksum = Some(summary.frame_checksum);
    observed.timing_summary = timing_summary(&observed.timings);
    Ok(observed)
}

struct SubscribeOptions {
    progress: PathBuf,
    origin: Instant,
    playback: bool,
    audio_output: Option<DeviceSelector>,
    max_received_packets: usize,
    remote_screen_ready: Option<watch::Sender<bool>>,
    upstream_avconv_decode: bool,
}

async fn subscribe((track, endpoint): Inbound, options: SubscribeOptions) -> Result<Observed> {
    let SubscribeOptions {
        progress,
        origin,
        playback,
        audio_output,
        max_received_packets,
        remote_screen_ready,
        upstream_avconv_decode,
    } = options;
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
    let (frames, output) = mpsc::channel(2);
    let frames = playback.then_some(frames);
    let worker = tokio::task::spawn_blocking(move || {
        if upstream_avconv_decode {
            decode_track_upstream_avconv(track, config.payload, rx)
        } else {
            decode_track(track, config.payload, rx, origin, frames)
        }
    });
    let receive = async move {
        let mut received = 0;
        while let Some(packet) = protocol.recv().await? {
            let types::AVPacket::Media(packet) = packet else {
                anyhow::bail!("unexpected media packet");
            };
            ensure!(
                !packet.header.is_config
                    && !packet.payload.is_empty()
                    && packet.payload.len() <= MAX_PACKET
                    && received < max_received_packets,
                "invalid or excess media data"
            );
            let timing = TimingSample {
                packet_id_us: packet.header.pts,
                transport_receive_us: Some(elapsed_micros(origin)?),
                ..TimingSample::default()
            };
            tx.send(ReceivedPacket { packet, timing }).await?;
            if received == 0 {
                tokio::fs::write(&progress, b"receiving").await?;
                if let Some(ready) = &remote_screen_ready {
                    ready.send_replace(true);
                }
            }
            received += 1;
        }
        Ok::<_, anyhow::Error>(())
    };
    let render = async {
        if playback {
            present(track, output, audio_output, origin).await
        } else {
            Ok((0, Vec::new(), Vec::new(), Vec::new(), None))
        }
    };
    let decode = async { worker.await? };
    let (
        _,
        (
            delivered,
            playback_timings,
            presentation_events,
            audio_drain_events,
            presentation_superseded_frames,
        ),
        mut observed,
    ) = tokio::try_join!(receive, render, decode)?;
    observed.playback_bytes = delivered;
    observed.presentation_events = presentation_events;
    observed.audio_drain_events = audio_drain_events;
    observed.presentation_superseded_frames = presentation_superseded_frames;
    observed.presentation_summary = presentation_summary(
        &observed.presentation_events,
        observed.decoded_frames as usize,
    );
    let playback: std::collections::HashMap<_, _> = playback_timings
        .into_iter()
        .map(|timing| (timing.packet_id_us, timing))
        .collect();
    for timing in &mut observed.timings {
        if let Some(presented) = playback.get(&timing.packet_id_us) {
            timing.playback_queue_depth = presented.playback_queue_depth;
            timing.playback_enqueue_us = presented.playback_enqueue_us;
            timing.playback_submit_start_us = presented.playback_submit_start_us;
            timing.playback_submit_end_us = presented.playback_submit_end_us;
        }
    }
    observed.timing_summary = timing_summary(&observed.timings);
    Ok(observed)
}

async fn run(directory: &Path, lan: bool, live: bool, playback: bool) -> Result<()> {
    let config = Settings::load(directory)?;
    ensure!(
        live == config.capture.is_some(),
        "live device input requires --capture and device settings"
    );
    let role = config.role;
    let address: SocketAddr = (Ipv4Addr::LOCALHOST, config.port).into();
    let lan_address = match (lan, role, config.bind_address, config.remote_address) {
        (false, _, None, None) => None,
        (true, Role::Listener, Some(ip), None) | (true, Role::Initiator, None, Some(ip)) => {
            Some(LanAddress::new(ip, config.port)?)
        }
        _ => anyhow::bail!("invalid role-specific LAN addresses"),
    };
    kynet::init_crypto();
    // The authenticated call may be quiet while the operator is in the screen picker.
    let keep_alive = config.capture_wait_ms > 0;
    let server = if role == Role::Listener {
        let cert = directory.join("cert.pem");
        let key = directory.join("key.pem");
        let server = match lan_address {
            Some(address) => transport::listen_lan(address, &cert, &key, keep_alive).await?,
            None => transport::listen(address, &cert, &key, keep_alive).await?,
        };
        std::fs::write(directory.join("ready"), b"listening")?;
        Some(server)
    } else {
        None
    };
    let connect_timeout = if lan && server.is_some() {
        Duration::from_secs(60)
    } else {
        SETUP_TIMEOUT
    };
    let connection = timeout(connect_timeout, async {
        if let Some(server) = &server {
            transport::accept(server, &config.secret).await
        } else if let Some(address) = lan_address {
            transport::connect_lan(
                address,
                config.fingerprint.clone(),
                &config.secret,
                keep_alive,
            )
            .await
        } else {
            transport::connect(
                address,
                config.fingerprint.clone(),
                &config.secret,
                keep_alive,
            )
            .await
        }
    })
    .await
    .context("Kyber connection timed out")??;
    let result = async {
        if lan {
            tokio::fs::write(directory.join("authenticated"), b"authenticated").await?;
        }
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

        let drain_timeout = if playback || config.playback_drain_timeout_ms > 0 {
            Duration::from_millis(config.playback_drain_timeout_ms).max(PLAYBACK_DRAIN_TIMEOUT)
        } else {
            SETUP_TIMEOUT
        };
        let duration =
            Duration::from_millis(config.duration_ms + config.capture_wait_ms) + drain_timeout;
        let origin = Instant::now();
        let stats_provider = connection.stats_provider();
        let mut transport_samples = vec![transport_sample(&stats_provider, origin).await?];
        let result = timeout(duration, async {
            let mut tasks = JoinSet::new();
            let source = u32::from(role == Role::Initiator);
            let screen_ready = config
                .capture
                .as_ref()
                .filter(|cap| cap.screen)
                .map(|_| watch::channel(false).0);
            let remote_screen_ready = config
                .wait_for_remote_screen
                .then(|| watch::channel(false).0);
            for endpoint in endpoints.outbound.drain(..) {
                let duration_ms = if endpoint.0 == Track::Screen {
                    config.screen_stop_ms.unwrap_or(config.duration_ms)
                } else {
                    config.duration_ms
                };
                let track = endpoint.0;
                let input = config.input(track);
                let ready = screen_ready.clone();
                let remote_ready = remote_screen_ready.as_ref().map(watch::Sender::subscribe);
                tasks.spawn(async move {
                    publish(
                        endpoint,
                        source,
                        duration_ms,
                        input,
                        ready,
                        remote_ready,
                        origin,
                    )
                    .await
                    .with_context(|| format!("publishing {}", track.label()))
                });
            }
            for endpoint in endpoints.inbound.drain(..) {
                let track = endpoint.0;
                let progress = directory.join(format!("{}.receiving", track.label()));
                let audio_output = config.audio_output.clone();
                let remote_ready = (track == Track::Screen)
                    .then(|| remote_screen_ready.clone())
                    .flatten();
                let max_received_packets = if config.sustained {
                    MAX_SUSTAINED_PACKETS
                } else {
                    MAX_RECEIVED_PACKETS
                };
                let upstream_avconv_decode =
                    config.upstream_avconv_decode && track == Track::Camera;
                tasks.spawn(async move {
                    subscribe(
                        endpoint,
                        SubscribeOptions {
                            progress,
                            origin,
                            playback,
                            audio_output,
                            max_received_packets,
                            remote_screen_ready: remote_ready,
                            upstream_avconv_decode,
                        },
                    )
                    .await
                    .with_context(|| format!("subscribing to {}", track.label()))
                });
            }
            let mut observations = Vec::new();
            let mut stats_interval = tokio::time::interval(Duration::from_secs(1));
            stats_interval.tick().await;
            while !tasks.is_empty() {
                tokio::select! {
                    _ = stats_interval.tick() => {
                        transport_samples.push(transport_sample(&stats_provider, origin).await?);
                    }
                    result = tasks.join_next() => {
                        let Some(result) = result else { break; };
                        observations.push(result??);
                    }
                }
            }
            endpoints.finish(role).await?;
            transport_samples.push(transport_sample(&stats_provider, origin).await?);
            let transport = transport_report(
                transport_samples,
                connection.protocol_stats().dropped_packets,
            );
            Ok::<_, anyhow::Error>((observations, transport))
        })
        .await
        .context("Kyber media run timed out")?;

        if result.is_ok() && role == Role::Listener {
            let _ = timeout(SETUP_TIMEOUT, connection.closed()).await;
        }
        result
    }
    .await;
    connection.close();
    let (tracks, transport) = result?;
    println!(
        "{}",
        serde_json::json!({
            "role": role.label(),
            "tracks": tracks,
            "transport": transport,
        })
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1).peekable();
    let lan = args.next_if(|arg| arg == "--lan").is_some();
    let live = args.next_if(|arg| arg == "--capture").is_some();
    let playback = args.next_if(|arg| arg == "--playback").is_some();
    let directory = args
        .next()
        .context("usage: skyber-a1-peer [--lan] [--capture] [--playback] RUN_DIRECTORY")?;
    ensure!(args.next().is_none(), "unexpected peer arguments");
    run(&PathBuf::from(directory), lan, live, playback).await
}

#[cfg(test)]
mod timing_tests {
    use super::{
        PresentationEvent, TimingSample, TransportSample, packet_rate, parse_audio_drain_event,
        parse_video_presentation_event, parse_video_superseded_event, percentiles,
        presentation_summary, timing_summary, transport_report,
    };

    #[test]
    fn parse_correlated_video_presentation_event() {
        let event = parse_video_presentation_event("SKYBER_PRESENT_VIDEO 123 456")
            .expect("valid event")
            .expect("recognized event");
        assert_eq!(event.packet_id_us, 123);
        assert_eq!(event.helper_timestamp_us, 456);
        assert!(
            parse_video_presentation_event("ordinary helper log")
                .expect("non-event line")
                .is_none()
        );
        assert!(parse_video_presentation_event("SKYBER_PRESENT_VIDEO nope 456").is_err());
    }

    #[test]
    fn parse_audio_output_drain_event() {
        let event = parse_audio_drain_event("SKYBER_AUDIO_DRAINED 9600 123456")
            .expect("valid event")
            .expect("recognized event");
        assert_eq!(event.sample_frames, 9600);
        assert_eq!(event.helper_timestamp_us, 123456);
        assert_eq!(
            event.helper_clock_domain,
            "playback_helper_monotonic_us_since_start"
        );
        assert!(parse_audio_drain_event("SKYBER_AUDIO_DRAINED bad 10").is_err());
    }

    #[test]
    fn parse_video_superseded_event_checks_track_identity() {
        assert_eq!(
            parse_video_superseded_event("SKYBER_VIDEO_SUPERSEDED camera 3", "camera")
                .expect("valid event"),
            Some(3)
        );
        assert!(
            parse_video_superseded_event("SKYBER_VIDEO_SUPERSEDED screen 3", "camera").is_err()
        );
    }

    #[test]
    fn presentation_summary_uses_only_helper_local_time() {
        let events = [100, 66_766, 133_433].map(|helper_timestamp_us| PresentationEvent {
            packet_id_us: helper_timestamp_us,
            helper_timestamp_us,
            helper_clock_domain: "playback_helper_monotonic_us_since_start",
        });
        let summary = presentation_summary(&events, 4);
        assert_eq!(summary.presented_frames, 3);
        assert_eq!(summary.decoded_frames_without_paint_event, 1);
        assert!((summary.frames_per_second.unwrap_or_default() - 15.0).abs() < 0.01);
        assert_eq!(summary.inter_event_interval_us.samples, 2);
        assert_eq!(summary.inter_event_interval_us.p50_us, Some(66_666));
    }

    #[test]
    fn empty_percentiles_have_no_duration_claim() {
        let stats = percentiles(Vec::new());
        assert_eq!(stats.samples, 0);
        assert_eq!(stats.p50_us, None);
        assert_eq!(stats.p95_us, None);
    }

    #[test]
    fn percentiles_use_sorted_microsecond_samples() {
        let stats = percentiles(vec![40, 10, 30, 20]);
        assert_eq!(stats.samples, 4);
        assert_eq!(stats.p50_us, Some(20));
        assert_eq!(stats.p95_us, Some(40));
    }

    #[test]
    fn transport_summary_reports_local_rtt_and_loss_counter_delta() {
        let samples = vec![
            TransportSample {
                peer_timestamp_us: 100,
                rtt_us: Some(20_000),
                lost_packets_total: Some(3),
            },
            TransportSample {
                peer_timestamp_us: 1_000_100,
                rtt_us: Some(30_000),
                lost_packets_total: Some(5),
            },
        ];
        let report = transport_report(samples, None);
        assert_eq!(report.rtt_us.samples, 2);
        assert_eq!(report.rtt_us.p95_us, Some(30_000));
        assert_eq!(report.lost_packets_total_start, Some(3));
        assert_eq!(report.lost_packets_total_end, Some(5));
        assert_eq!(report.lost_packets_delta, Some(2));
        assert_eq!(report.protocol_dropped_packets, None);
    }

    #[test]
    fn packet_rate_requires_distinct_local_timestamps() {
        let samples = [
            TimingSample {
                source_ready_us: Some(100),
                ..TimingSample::default()
            },
            TimingSample {
                source_ready_us: Some(500_100),
                ..TimingSample::default()
            },
        ];
        let rate = packet_rate(&samples, |sample| sample.source_ready_us);
        assert_eq!(rate, Some(2.0));
        assert_eq!(
            packet_rate(&samples[..1], |sample| sample.source_ready_us),
            None
        );
    }

    #[test]
    fn summary_only_uses_durations_from_one_peer_clock() {
        let sample = TimingSample {
            packet_id_us: 700,
            source_ready_us: Some(100),
            encode_start_us: Some(110),
            encode_end_us: Some(130),
            transport_send_start_us: Some(150),
            transport_send_end_us: Some(160),
            transport_receive_us: Some(900),
            decode_start_us: Some(920),
            decode_end_us: Some(950),
            playback_enqueue_us: Some(960),
            playback_submit_start_us: Some(970),
            playback_submit_end_us: Some(980),
            ..TimingSample::default()
        };
        let summary = timing_summary(&[sample]);
        assert_eq!(summary.clock_domain, "peer_monotonic_us_since_media_start");
        assert_eq!(summary.source_to_encode_us.p50_us, Some(10));
        assert_eq!(summary.encode_us.p50_us, Some(20));
        assert_eq!(summary.encode_to_transport_us.p50_us, Some(20));
        assert_eq!(summary.transport_send_us.p50_us, Some(10));
        assert_eq!(summary.receive_to_decode_us.p50_us, Some(20));
        assert_eq!(summary.decode_us.p50_us, Some(30));
        assert_eq!(summary.decode_to_playback_queue_us.p50_us, Some(10));
        assert_eq!(summary.playback_submit_us.p50_us, Some(10));
    }
}
