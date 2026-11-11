//! Screen capture + hardware encoding.
//!
//! * Windows 10/11: FFmpeg `ddagrab` (Desktop Duplication, GPU frames)
//!   straight into NVENC / AMF, or QSV / Media Foundation / x264 as fallback.
//! * macOS: ScreenCaptureKit (native, NV12) piped into VideoToolbox.
//! * Camera: FFmpeg `dshow` (Windows) / `avfoundation` (macOS), same encoders.
//!
//! The encoder writes a raw Annex B bitstream which goes into the [`Hub`].

use super::Hub;
use crate::ffmpeg;
use crate::proto::{Codec, StreamInfo};
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// What gets captured.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Screen,
    /// Camera by its FFmpeg device name; `None` = the first camera found.
    Camera(Option<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub source: Source,
    pub codec: Codec,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub display: usize,
    /// 0 = native resolution of the source.
    pub max_height: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            source: Source::Screen,
            codec: Codec::H264,
            fps: 60,
            bitrate_kbps: 30_000,
            display: 0,
            max_height: 0,
        }
    }
}

impl Settings {
    /// Webcam: 720p30 at a Discord-like bitrate (each viewer gets its own copy).
    pub fn camera(name: Option<String>) -> Self {
        Settings { source: Source::Camera(name), fps: 30, bitrate_kbps: 2_500, ..Default::default() }
    }
}

pub struct Broadcast {
    pub info: StreamInfo,
    pub hub: Arc<Hub>,
    child: Arc<Mutex<Child>>,
    stopped: Arc<AtomicBool>,
    #[cfg(target_os = "macos")]
    capture: Option<screencapturekit::stream::SCStream>,
}

impl Drop for Broadcast {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        #[cfg(target_os = "macos")]
        if let Some(c) = self.capture.take() {
            let _ = c.stop_capture();
        }
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.hub.close();
    }
}

fn test_source() -> bool {
    std::env::var_os("BIBICORD_TEST_SOURCE").is_some()
}

/// Human readable list of capturable displays.
pub fn displays() -> Vec<String> {
    if test_source() {
        return vec!["Obraz testowy".into()];
    }
    platform::displays()
}

/// Starts capturing and encoding. `on_exit` is called if the encoder dies
/// on its own (not when the broadcast is dropped). When FFmpeg's own screen
/// capture is lost (Windows: a game going fullscreen, UAC, a resolution
/// change) the encoder is started again into the same [`Hub`] instead.
pub fn start(s: &Settings, on_exit: impl FnOnce(String) + Send + 'static) -> Result<Broadcast> {
    let (encoder, mut cmd_args, input) = if test_source() {
        let enc = pick_encoder(s.codec, |enc| {
            let mut a = lavfi_probe_input();
            a.extend(encoder_args(enc, s));
            a
        })?;
        let mut a: Vec<String> = ["-re", "-f", "lavfi", "-i"]
            .map(String::from)
            .to_vec();
        a.push(format!("testsrc2=size=1920x1080:rate={}", s.fps));
        (enc, a, Input::None { width: 1920, height: 1080 })
    } else if let Source::Camera(name) = &s.source {
        camera_input(s, name.as_deref())?
    } else {
        platform::input(s)?
    };
    cmd_args.extend(encoder_args(encoder, s));
    cmd_args.extend(
        ["-flush_packets", "1", "-f", s.codec.ffmpeg_format(), "pipe:1"].map(String::from),
    );

    tracing::info!("encoder: ffmpeg {}", cmd_args.join(" "));
    let pipe = matches!(input, Input::Pipe { .. });
    let (mut child, stdout, log) = spawn_encoder(&cmd_args, pipe)?;
    let stdin = child.stdin.take();

    let (width, height) = match &input {
        Input::Pipe { width, height, .. } | Input::None { width, height } => (*width, *height),
    };
    let info = StreamInfo {
        codec: s.codec,
        width,
        height,
        fps: s.fps,
        bitrate_kbps: s.bitrate_kbps,
        encoder: encoder.to_string(),
    };
    let hub = Arc::new(Hub::new(s.codec));
    let stopped = Arc::new(AtomicBool::new(false));
    let child = Arc::new(Mutex::new(child));

    {
        let (hub, stopped, child) = (hub.clone(), stopped.clone(), child.clone());
        // Natively captured frames (macOS) can't be fed into a new FFmpeg.
        let restartable = !pipe;
        std::thread::spawn(move || {
            let (mut stdout, mut log) = (stdout, log);
            let mut started = Instant::now();
            let mut recovery = Recovery::default();
            let mut buf = vec![0u8; 1 << 20];
            loop {
                loop {
                    match stdout.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => hub.push(Bytes::copy_from_slice(&buf[..n])),
                    }
                }
                let tail = log.join().unwrap_or_default();
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                let retry = if restartable {
                    recovery.next(&tail, started.elapsed(), Instant::now())
                } else {
                    None
                };
                let Some(delay) = retry else {
                    hub.close();
                    on_exit(match (tail.is_empty(), recovery.active()) {
                        (true, _) => "enkoder zakończył pracę".into(),
                        (false, false) => tail,
                        (false, true) => format!("nie udało się wznowić przechwytywania ekranu: {tail}"),
                    });
                    return;
                };
                tracing::warn!("capture lost, restarting the encoder in {delay:?}");
                std::thread::sleep(delay);
                // Under the lock: `Broadcast::drop` either sees the new
                // child and kills it, or we see `stopped` and don't start it.
                let mut c = child.lock().unwrap();
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                // Reap the old FFmpeg first: it may still hold the duplication.
                let _ = c.wait();
                match spawn_encoder(&cmd_args, false) {
                    Ok((new, out, l)) => {
                        *c = new;
                        (stdout, log) = (out, l);
                        started = Instant::now();
                        hub.restart();
                    }
                    Err(e) => {
                        drop(c);
                        hub.close();
                        on_exit(format!("{e:#}"));
                        return;
                    }
                }
            }
            hub.close();
        });
    }

    #[allow(unused_mut)]
    let mut b = Broadcast {
        info,
        hub,
        child,
        stopped: stopped.clone(),
        #[cfg(target_os = "macos")]
        capture: None,
    };

    if let Input::Pipe { width, height, start } = input {
        let stdin = stdin.context("brak stdin enkodera")?;
        #[cfg(target_os = "macos")]
        {
            b.capture = Some(start(stdin, width, height, stopped)?);
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (stdin, width, height, start, stopped);
        }
    }
    Ok(b)
}

/// Starts FFmpeg with `args`; returns it with its stdout and a thread that
/// collects the last lines of its log (joined after FFmpeg exits).
fn spawn_encoder(args: &[String], pipe_stdin: bool) -> Result<(Child, ChildStdout, JoinHandle<String>)> {
    let mut cmd = ffmpeg::command()?;
    cmd.args(["-loglevel", "error"]).args(args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.stdin(if pipe_stdin { Stdio::piped() } else { Stdio::null() });
    let mut child = cmd.spawn().context("nie można uruchomić FFmpeg")?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let log = std::thread::spawn(move || {
        let mut tail = VecDeque::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            tracing::warn!("ffmpeg enc: {line}");
            tail.push_back(line);
            if tail.len() > 8 {
                tail.pop_front();
            }
        }
        Vec::from(tail).join("\n")
    });
    Ok((child, stdout, log))
}

/// FFmpeg's screen capture stopped because the picture of the desktop is no
/// longer valid: `DXGI_ERROR_ACCESS_LOST` from Desktop Duplication (fullscreen
/// app, UAC / lock screen, mode change), or ddagrab noticing a new size or
/// format of the desktop. Windows expects the capture to be set up again.
fn capture_lost(log: &str) -> bool {
    log.contains("887a0026") || log.contains("Output parameters changed")
}

/// How long capture may stay down before the stream ends.
const RECOVERY_WINDOW: Duration = Duration::from_secs(30);
/// An encoder that ran this long had working capture: a later loss starts
/// a new recovery window.
const HEALTHY_RUN: Duration = Duration::from_secs(5);

/// Decides whether to start the encoder again after it exited.
#[derive(Default)]
struct Recovery {
    /// When capture was lost, while recovering.
    since: Option<Instant>,
    attempts: u32,
}

impl Recovery {
    /// `log`: the end of the dead encoder's log, `ran`: how long it ran.
    /// Returns the delay before the next start, or `None` to give up.
    fn next(&mut self, log: &str, ran: Duration, now: Instant) -> Option<Duration> {
        if ran >= HEALTHY_RUN {
            self.since = None;
        }
        let since = match self.since {
            // While recovering anything may fail (e.g. "access denied"
            // while UAC is shown): keep trying until the window is over.
            Some(t) => t,
            None if capture_lost(log) => {
                self.attempts = 0;
                *self.since.insert(now)
            }
            None => return None,
        };
        if now.duration_since(since) > RECOVERY_WINDOW {
            return None;
        }
        self.attempts += 1;
        Some(Duration::from_millis(match self.attempts {
            1 => 100,
            2 => 250,
            3 => 500,
            _ => 1000,
        }))
    }

    fn active(&self) -> bool {
        self.since.is_some()
    }
}

#[allow(dead_code)]
enum Input {
    /// FFmpeg captures by itself.
    None { width: u32, height: u32 },
    /// We capture natively and pipe raw NV12 frames into ffmpeg's stdin.
    Pipe {
        width: u32,
        height: u32,
        start: Box<dyn FnOnce(ChildStdin, u32, u32, Arc<AtomicBool>) -> Result<CaptureHandle>>,
    },
}

#[cfg(target_os = "macos")]
type CaptureHandle = screencapturekit::stream::SCStream;
#[cfg(not(target_os = "macos"))]
type CaptureHandle = ();

const CAMERA_FORMAT: &str = if cfg!(windows) {
    "dshow"
} else if cfg!(target_os = "macos") {
    "avfoundation"
} else {
    "v4l2"
};

/// Names of connected cameras, as FFmpeg sees them. Blocking (runs ffmpeg).
pub fn cameras() -> Vec<String> {
    if test_source() {
        return vec!["Kamera testowa".into()];
    }
    let Ok(mut cmd) = ffmpeg::command() else { return Vec::new() };
    let dummy = if cfg!(windows) { "dummy" } else { "" };
    cmd.args(["-list_devices", "true", "-f", CAMERA_FORMAT, "-i", dummy])
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    match cmd.output() {
        Ok(out) => parse_cameras(&String::from_utf8_lossy(&out.stderr)),
        Err(_) => Vec::new(),
    }
}

/// Parses `-list_devices` output of dshow (old and new style) and avfoundation.
fn parse_cameras(log: &str) -> Vec<String> {
    let mut video = false;
    let mut out = Vec::new();
    for line in log.lines() {
        // Drop the `[dshow @ 0x..] ` prefix.
        let l = line.split_once("] ").map_or(line, |(_, rest)| rest).trim();
        if l.contains("video devices") {
            video = true;
        } else if l.contains("audio devices") {
            video = false;
        } else if let Some(name) = l.strip_suffix("(video)") {
            out.push(name.trim().trim_matches('"').to_string());
        } else if !video || l.ends_with(')') || l.starts_with("Alternative name") {
            continue;
        } else if l.starts_with('"') {
            out.push(l.trim_matches('"').to_string());
        } else if let Some((_, name)) = l.strip_prefix('[').and_then(|r| r.split_once("] ")) {
            if !name.starts_with("Capture screen") {
                out.push(name.to_string());
            }
        }
    }
    out
}

fn camera_input(s: &Settings, name: Option<&str>) -> Result<(&'static str, Vec<String>, Input)> {
    let name = match name {
        Some(n) => n.to_string(),
        None => cameras().into_iter().next().context("nie znaleziono kamery")?,
    };
    let enc = pick_encoder(s.codec, |enc| {
        let mut a = lavfi_probe_input();
        a.extend(encoder_args(enc, s));
        a
    })?;
    let device = if cfg!(windows) { format!("video={name}") } else { name };
    let input = |mode: &[&str]| {
        let mut a: Vec<String> = ["-f", CAMERA_FORMAT].map(String::from).to_vec();
        if cfg!(windows) {
            a.extend(["-rtbufsize".into(), "100M".into()]);
        }
        a.extend(mode.iter().map(|m| m.to_string()));
        a.extend(["-i".into(), device.clone()]);
        a
    };
    // Ask for 720p30; cameras that refuse it run in their default mode.
    // ponytail: one fixed mode, list the camera's modes if 720p is commonly missing.
    let hd = ["-video_size", "1280x720", "-framerate", "30"];
    let mut probe = input(&hd);
    probe.extend(["-frames:v", "1", "-f", "null", "-"].map(String::from));
    let mut a = if ffmpeg::probe(&probe, Duration::from_secs(8)) {
        input(&hd)
    } else if cfg!(target_os = "macos") {
        input(&["-framerate", "30"]) // avfoundation rejects its 29.97 default on most cameras
    } else {
        input(&[])
    };
    // Cameras deliver YUYV or MJPEG; hardware encoders want NV12.
    a.extend(["-vf".into(), "format=nv12".into()]);
    Ok((enc, a, Input::None { width: 0, height: 0 }))
}

fn lavfi_probe_input() -> Vec<String> {
    ["-f", "lavfi", "-i", "color=c=black:s=1280x720:r=30", "-frames:v", "5"]
        .map(String::from)
        .to_vec()
}

fn encoder_candidates(codec: Codec) -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    return match codec {
        Codec::H264 => &["h264_videotoolbox", "libx264"],
        Codec::Hevc => &["hevc_videotoolbox", "libx265"],
    };
    #[cfg(not(target_os = "macos"))]
    return match codec {
        Codec::H264 => &["h264_nvenc", "h264_amf", "h264_qsv", "h264_mf", "libx264"],
        Codec::Hevc => &["hevc_nvenc", "hevc_amf", "hevc_qsv", "hevc_mf", "libx265"],
    };
}

static PICKED: Mutex<Option<HashMap<(Codec, String), &'static str>>> = Mutex::new(None);

/// Picks the first encoder that actually works on this machine. `probe_args`
/// builds a complete short ffmpeg run for a candidate.
fn pick_encoder(codec: Codec, probe_args: impl Fn(&str) -> Vec<String>) -> Result<&'static str> {
    let key = (codec, probe_args("x").join(" "));
    if let Some(e) = PICKED.lock().unwrap().as_ref().and_then(|m| m.get(&key)) {
        return Ok(e);
    }
    for &enc in encoder_candidates(codec) {
        let mut args = probe_args(enc);
        args.extend(["-f", "null", "-"].map(String::from));
        if ffmpeg::probe(&args, Duration::from_secs(10)) {
            tracing::info!("using encoder {enc}");
            PICKED
                .lock()
                .unwrap()
                .get_or_insert_with(HashMap::new)
                .insert(key, enc);
            return Ok(enc);
        }
        tracing::info!("encoder {enc} unavailable");
    }
    bail!("brak działającego enkodera {}", codec.label())
}

fn encoder_args(enc: &str, s: &Settings) -> Vec<String> {
    let k = s.bitrate_kbps;
    let mut a: Vec<String> = vec![
        "-c:v".into(),
        enc.into(),
        "-g".into(),
        (s.fps * 2).to_string(),
        "-bf".into(),
        "0".into(),
        "-b:v".into(),
        format!("{k}k"),
        "-maxrate".into(),
        format!("{k}k"),
        "-bufsize".into(),
        format!("{}k", k / 2),
    ];
    let extra: &[&str] = match enc {
        "h264_videotoolbox" => &["-realtime", "1", "-prio_speed", "1", "-profile:v", "high"],
        "hevc_videotoolbox" => &["-realtime", "1", "-prio_speed", "1", "-profile:v", "main"],
        "h264_nvenc" | "hevc_nvenc" => &[
            "-preset", "p4", "-tune", "ull", "-rc", "cbr", "-zerolatency", "1", "-forced-idr", "1",
        ],
        "h264_amf" | "hevc_amf" => &["-usage", "ultralowlatency", "-quality", "speed", "-rc", "cbr"],
        "h264_qsv" | "hevc_qsv" => &["-preset", "veryfast", "-look_ahead", "0", "-async_depth", "1"],
        "h264_mf" | "hevc_mf" => &["-hw_encoding", "1", "-scenario", "display_remoting", "-rate_control", "cbr"],
        "libx264" => &["-preset", "veryfast", "-tune", "zerolatency", "-pix_fmt", "yuv420p"],
        "libx265" => &["-preset", "ultrafast", "-tune", "zerolatency", "-pix_fmt", "yuv420p"],
        _ => &[],
    };
    a.extend(extra.iter().map(|s| s.to_string()));
    a
}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;

    pub fn displays() -> Vec<String> {
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CMONITORS};
        let n = unsafe { GetSystemMetrics(SM_CMONITORS) }.max(1);
        (1..=n).map(|i| format!("Monitor {i}")).collect()
    }

    fn capture_args(s: &Settings, gpu_direct: bool) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            format!(
                "ddagrab=output_idx={}:framerate={}:draw_mouse=1",
                s.display, s.fps
            ),
        ];
        if !gpu_direct {
            let mut vf = String::from("hwdownload,format=bgra,");
            if s.max_height > 0 {
                vf += &format!("scale=-2:'min(ih\\,{})':flags=bicubic:", s.max_height);
            } else {
                vf += "scale=";
            }
            vf += "out_color_matrix=bt709:out_range=tv,format=nv12";
            a.extend(["-vf".into(), vf]);
            a.extend(
                ["-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709", "-color_range", "tv"]
                    .map(String::from),
            );
        }
        a
    }

    fn gpu_direct(enc: &str, s: &Settings) -> bool {
        s.max_height == 0 && (enc.ends_with("_nvenc") || enc.ends_with("_amf"))
    }

    pub fn input(s: &Settings) -> Result<(&'static str, Vec<String>, Input)> {
        let enc = pick_encoder(s.codec, |enc| {
            let mut a = capture_args(s, gpu_direct(enc, s));
            a.extend(["-frames:v".into(), "3".into()]);
            a.extend(encoder_args(enc, s));
            a
        })?;
        Ok((
            enc,
            capture_args(s, gpu_direct(enc, s)),
            Input::None { width: 0, height: 0 },
        ))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use screencapturekit::cm::CMSampleBufferExt;
    use screencapturekit::cv::CVPixelBufferLockFlags;
    use screencapturekit::prelude::*;
    use screencapturekit::shareable_content::SCShareableContentInfo;
    use std::io::Write;
    use std::sync::mpsc::{SyncSender, sync_channel};

    pub fn displays() -> Vec<String> {
        match SCShareableContent::get() {
            Ok(c) => c
                .displays()
                .iter()
                .enumerate()
                .map(|(i, d)| format!("Ekran {} ({}×{})", i + 1, d.width(), d.height()))
                .collect(),
            Err(e) => {
                tracing::warn!("ScreenCaptureKit: {e:?}");
                vec!["Ekran 1".into()]
            }
        }
    }

    pub fn input(s: &Settings) -> Result<(&'static str, Vec<String>, Input)> {
        let enc = pick_encoder(s.codec, |enc| {
            let mut a = lavfi_probe_input();
            a.extend(encoder_args(enc, s));
            a
        })?;

        let content = SCShareableContent::get().map_err(|e| {
            anyhow::anyhow!(
                "brak dostępu do ekranu ({e:?}). Zezwól w Ustawienia → Prywatność → Nagrywanie ekranu"
            )
        })?;
        let displays = content.displays();
        let display = displays.get(s.display).or(displays.first()).context("brak ekranu")?;
        let filter = SCContentFilter::create()
            .with_display(display)
            .with_excluding_windows(&[])
            .build()
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let (pw, ph) = SCShareableContentInfo::for_filter(&filter)
            .map(|i| i.pixel_size())
            .filter(|(w, h)| *w > 0 && *h > 0)
            .unwrap_or((display.width(), display.height()));
        let (mut w, mut h) = (pw, ph);
        if s.max_height > 0 && ph > s.max_height {
            h = s.max_height;
            w = (pw as u64 * h as u64 / ph as u64) as u32;
        }
        let (w, h) = (w & !1, h & !1);
        let fps = s.fps;

        let args: Vec<String> = vec![
            "-f".into(),
            "rawvideo".into(),
            "-pix_fmt".into(),
            "nv12".into(),
            "-s".into(),
            format!("{w}x{h}"),
            "-framerate".into(),
            fps.to_string(),
            "-i".into(),
            "pipe:0".into(),
            "-colorspace".into(),
            "bt709".into(),
            "-color_primaries".into(),
            "bt709".into(),
            "-color_trc".into(),
            "bt709".into(),
            "-color_range".into(),
            "tv".into(),
        ];

        let start = Box::new(
            move |stdin: ChildStdin, w: u32, h: u32, stopped: Arc<AtomicBool>| -> Result<SCStream> {
                let config = SCStreamConfiguration::new()
                    .with_width(w)
                    .with_height(h)
                    .with_pixel_format(PixelFormat::YCbCr_420v)
                    .with_fps(fps)
                    .with_shows_cursor(true)
                    .with_queue_depth(5);
                let (tx, rx) = sync_channel::<Vec<u8>>(2);
                std::thread::spawn(move || {
                    let mut stdin = stdin;
                    while let Ok(frame) = rx.recv() {
                        if stdin.write_all(&frame).is_err() {
                            break;
                        }
                    }
                });
                let mut stream = SCStream::new(&filter, &config).map_err(|e| anyhow::anyhow!("{e:?}"))?;
                stream
                    .add_output_handler(
                        Sink { tx, w: w as usize, h: h as usize, stopped },
                        SCStreamOutputType::Screen,
                    )
                    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
                stream.start_capture().map_err(|e| {
                    anyhow::anyhow!(
                        "nie można przechwycić ekranu ({e:?}). Zezwól w Ustawienia → Prywatność → Nagrywanie ekranu"
                    )
                })?;
                Ok(stream)
            },
        );
        Ok((enc, args, Input::Pipe { width: w, height: h, start }))
    }

    struct Sink {
        tx: SyncSender<Vec<u8>>,
        w: usize,
        h: usize,
        stopped: Arc<AtomicBool>,
    }

    impl SCStreamOutputTrait for Sink {
        fn did_output_sample_buffer(&self, sample: CMSampleBuffer, ty: SCStreamOutputType) {
            if !matches!(ty, SCStreamOutputType::Screen) || self.stopped.load(Ordering::Relaxed) {
                return;
            }
            let Some(pb) = sample.pixel_buffer() else { return };
            let Ok(g) = pb.lock(CVPixelBufferLockFlags::READ_ONLY) else { return };
            if g.plane_count() < 2 || g.width_of_plane(0) < self.w || g.height_of_plane(0) < self.h {
                return;
            }
            let mut frame = Vec::with_capacity(self.w * self.h * 3 / 2);
            for plane in 0..2 {
                let rows = if plane == 0 { self.h } else { self.h / 2 };
                for r in 0..rows {
                    let Some(row) = (unsafe { g.plane_row(plane, r) }) else { return };
                    if row.len() < self.w {
                        return;
                    }
                    frame.extend_from_slice(&row[..self.w]);
                }
            }
            // Drop the frame if the encoder is busy rather than adding latency.
            let _ = self.tx.try_send(frame);
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod platform {
    use super::*;

    pub fn displays() -> Vec<String> {
        vec!["Ekran 1".into()]
    }

    pub fn input(s: &Settings) -> Result<(&'static str, Vec<String>, Input)> {
        let enc = pick_encoder(s.codec, |enc| {
            let mut a = lavfi_probe_input();
            a.extend(encoder_args(enc, s));
            a
        })?;
        let a = ["-f", "x11grab", "-framerate", &s.fps.to_string(), "-i", ":0.0"]
            .map(String::from)
            .to_vec();
        Ok((enc, a, Input::None { width: 0, height: 0 }))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_camera_lists() {
        let dshow = r#"[dshow @ 000001] "Integrated Webcam" (video)
[dshow @ 000001]   Alternative name "@device_pnp_\\?\usb#vid"
[dshow @ 000001] "OBS Virtual Camera" (none)
[dshow @ 000001] "Mikrofon (Realtek)" (audio)
dummy: Immediate exit requested"#;
        assert_eq!(super::parse_cameras(dshow), ["Integrated Webcam"]);

        let old = r#"[dshow @ 01] DirectShow video devices (some may be both video and audio devices)
[dshow @ 01]  "HD Pro Webcam C920"
[dshow @ 01]     Alternative name "@device_pnp_x"
[dshow @ 01] DirectShow audio devices
[dshow @ 01]  "Mikrofon"
"#;
        assert_eq!(super::parse_cameras(old), ["HD Pro Webcam C920"]);

        let mac = "[AVFoundation indev @ 0x1] AVFoundation video devices:\n\
                   [AVFoundation indev @ 0x1] [0] FaceTime HD Camera\n\
                   [AVFoundation indev @ 0x1] [1] Capture screen 0\n\
                   [AVFoundation indev @ 0x1] AVFoundation audio devices:\n\
                   [AVFoundation indev @ 0x1] [0] MacBook Pro Microphone\n";
        assert_eq!(super::parse_cameras(mac), ["FaceTime HD Camera"]);
    }

    #[test]
    fn restarts_only_after_capture_loss() {
        use super::{HEALTHY_RUN, RECOVERY_WINDOW, Recovery};
        use std::time::{Duration, Instant};
        let lost = "[Parsed_ddagrab_0 @ 1] AcquireNextFrame failed: 887a0026\n\
                    [in#0/lavfi @ 2] Error during demuxing: Generic error in an external library";
        let t0 = Instant::now();
        let long = HEALTHY_RUN * 10;

        // Other errors end the stream.
        assert_eq!(Recovery::default().next("Error while opening encoder", long, t0), None);

        // Capture lost: retried, also when the next tries fail differently
        // (UAC still shown), until the window is over.
        let mut r = Recovery::default();
        assert_eq!(r.next(lost, long, t0), Some(Duration::from_millis(100)));
        let denied = "Desktop duplication access denied";
        assert_eq!(r.next(denied, Duration::ZERO, t0 + Duration::from_secs(1)), Some(Duration::from_millis(250)));
        assert!(r.next(denied, Duration::ZERO, t0 + RECOVERY_WINDOW).is_some());
        assert_eq!(r.next(lost, Duration::ZERO, t0 + RECOVERY_WINDOW + Duration::from_secs(1)), None);
        assert!(r.active());

        // Capture that came back and ran for a while starts a new window.
        let mut r = Recovery::default();
        let mut t = t0;
        for _ in 0..20 {
            assert!(r.next(lost, long, t).is_some());
            t += Duration::from_secs(60);
        }
        // Back for a while, then a failure that isn't a capture loss.
        assert_eq!(r.next("Error while opening encoder", long, t), None);
        assert!(!r.active());

        // ddagrab noticing a new desktop size or format.
        assert!(Recovery::default().next("[Parsed_ddagrab_0 @ 1] Output parameters changed!", long, t0).is_some());
    }
    /// Real camera -> encoder -> hub. Needs a camera.
    /// Run with: BIBICORD_FFMPEG=/path/to/ffmpeg cargo test camera_stream -- --ignored
    #[test]
    #[ignore]
    fn camera_stream() {
        crate::ffmpeg::ensure(|_| {}).unwrap();
        println!("cameras: {:?}", super::cameras());
        let b = super::start(&super::Settings::camera(None), |e| panic!("encoder died: {e}")).unwrap();
        println!("{:?}", b.info);
        let mut rx = b.hub.subscribe();
        let start = std::time::Instant::now();
        let mut bytes = 0;
        while bytes < 50_000 {
            assert!(start.elapsed().as_secs() < 15, "only {bytes} B of video");
            bytes += rx.blocking_recv().expect("stream ended").len();
        }
        println!("{bytes} B after {:?}", start.elapsed());
    }
}
