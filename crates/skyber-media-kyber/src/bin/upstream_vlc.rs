use std::{
    path::Path,
    sync::mpsc,
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use kymux::{kycom::KyCom, types::VideoClientEndpoint};
use tokio::runtime::Builder;

use crate::UpstreamVlcMetrics;

struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            eprintln!("[{}] {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

static VLC_LOGGER: StderrLogger = StderrLogger;

pub(super) async fn present(
    endpoint: VideoClientEndpoint,
    progress: &Path,
    duration_ms: u64,
) -> Result<UpstreamVlcMetrics> {
    let progress = progress.to_owned();
    tokio::task::spawn_blocking(move || {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .context("create Kyber VLC player runtime")?;
        runtime.block_on(present_on_worker(endpoint, &progress, duration_ms))
    })
    .await
    .context("join Kyber VLC player worker")?
}

async fn present_on_worker(
    endpoint: VideoClientEndpoint,
    progress: &Path,
    duration_ms: u64,
) -> Result<UpstreamVlcMetrics> {
    use kyvlcplayer::{Player, PlayerEvent, PlayerMetric, VideoConfig, WindowHandle};

    let window = NativeVideoWindow::create()?;
    let kycom = KyCom::start_on_any_port(50_500..50_600)
        .await
        .context("start Kyber KyCom video forwarder")?;
    let uri = kycom
        .register_and_forward(endpoint)
        .context("register Kyber video endpoint with KyCom")?
        .url();
    eprintln!("SKYBER_KYBER_VLC_URI {uri}");
    let _ = log::set_logger(&VLC_LOGGER);
    log::set_max_level(log::LevelFilter::Debug);
    let (metric_tx, mut metric_rx) = tokio::sync::mpsc::unbounded_channel();
    let listener = Box::new(move |event| match event {
        PlayerEvent::Metric { data, .. } => {
            let kind = match data {
                PlayerMetric::FrameReceived => Some(0),
                PlayerMetric::FrameDecoded => Some(1),
                PlayerMetric::FrameDisplayed => Some(2),
                _ => None,
            };
            if let Some(kind) = kind {
                let _ = metric_tx.send(kind);
            }
        }
        other => eprintln!("SKYBER_KYBER_VLC_EVENT {other:?}"),
    });
    let mut player =
        Player::new(listener, Some(0), None, true).context("create pinned Kyber VLC player")?;
    player
        .play(
            Some(vec![VideoConfig {
                source_id: 1,
                software_decode: false,
                window_handle: WindowHandle::Win32(window.handle() as u64),
                uri,
                video_buffer: 0,
            }]),
            None,
        )
        .context("start pinned Kyber VLC video playback")?;

    let mut metrics = UpstreamVlcMetrics::default();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(duration_ms + 3_000);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(kind)) = tokio::time::timeout_at(deadline, metric_rx.recv()).await {
            match kind {
                0 => metrics.received += 1,
                1 => metrics.decoded += 1,
                2 => {
                    metrics.displayed += 1;
                    if metrics.displayed == 1 {
                        std::fs::write(progress, b"displaying")
                            .context("write VLC presentation progress marker")?;
                    }
                }
                _ => unreachable!("metric channel contains only known frame metric ids"),
            }
        }
    }
    eprintln!(
        "SKYBER_KYBER_VLC_METRICS received={} decoded={} displayed={}",
        metrics.received, metrics.decoded, metrics.displayed
    );
    metrics.stop_completed = tokio::time::timeout(Duration::from_secs(3), player.stop_async())
        .await
        .context("timed out stopping pinned Kyber VLC player")?
        .is_ok();
    drop(kycom);
    drop(window);
    ensure!(
        metrics.displayed > 0,
        "Kyber VLC displayed no synthetic frames"
    );
    Ok(metrics)
}

struct NativeVideoWindow {
    handle: usize,
    thread: Option<JoinHandle<()>>,
}

impl NativeVideoWindow {
    fn create() -> Result<Self> {
        use std::sync::mpsc;

        let (tx, rx) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("skyber-kyber-vlc-window".into())
            .spawn(move || window_thread(tx))
            .context("start native video window thread")?;
        let handle = rx
            .recv_timeout(Duration::from_secs(5))
            .context("wait for native video window creation")??;
        Ok(Self {
            handle,
            thread: Some(thread),
        })
    }

    fn handle(&self) -> usize {
        self.handle
    }
}

impl Drop for NativeVideoWindow {
    fn drop(&mut self) {
        unsafe {
            // SAFETY: handle was created on the window thread and remains owned until this drop.
            windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(
                self.handle as _,
                windows_sys::Win32::UI::WindowsAndMessaging::WM_CLOSE,
                0,
                0,
            );
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn window_thread(ready: mpsc::SyncSender<Result<usize>>) {
    use std::ptr;
    use windows_sys::Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
        Graphics::Gdi::UpdateWindow,
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{
            CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW,
            RegisterClassW, SW_SHOW, ShowWindow, TranslateMessage, WM_DESTROY, WNDCLASSW,
            WS_OVERLAPPEDWINDOW,
        },
    };

    const CLASS_NAME: &[u16] = &[83, 107, 121, 98, 101, 114, 86, 108, 99, 87, 105, 110, 0];
    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_DESTROY {
            unsafe {
                // SAFETY: called by Windows for this thread's top-level window.
                windows_sys::Win32::UI::WindowsAndMessaging::PostQuitMessage(0);
            }
            return 0;
        }
        unsafe {
            // SAFETY: all other messages are delegated to the system default procedure.
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
    }

    unsafe {
        // SAFETY: this worker owns the Win32 window lifecycle and message loop.
        let instance: HINSTANCE = GetModuleHandleW(ptr::null());
        if instance.is_null() {
            let _ = ready.send(Err(anyhow::anyhow!("GetModuleHandleW failed")));
            return;
        }
        let class = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: ptr::null_mut(),
            hCursor: ptr::null_mut(),
            hbrBackground: ptr::null_mut(),
            lpszMenuName: ptr::null(),
            lpszClassName: CLASS_NAME.as_ptr(),
        };
        if RegisterClassW(&class) == 0 {
            let _ = ready.send(Err(anyhow::anyhow!("RegisterClassW failed")));
            return;
        }
        let hwnd = CreateWindowExW(
            0,
            CLASS_NAME.as_ptr(),
            "Skyber Kyber VLC presentation proof\0"
                .encode_utf16()
                .collect::<Vec<_>>()
                .as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            960,
            540,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null_mut(),
        );
        if hwnd.is_null() {
            let _ = ready.send(Err(anyhow::anyhow!("CreateWindowExW failed")));
            return;
        }
        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
        if ready.send(Ok(hwnd as usize)).is_err() {
            windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
            return;
        }
        let mut message = std::mem::zeroed();
        while GetMessageW(&mut message, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}
