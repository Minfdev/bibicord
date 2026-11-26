//! Hardware decoding of a received bitstream into RGBA frames for the UI.

use super::hub::{START_CODE, Scanner};
use crate::ffmpeg;
use crate::proto::Codec;
use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use ffmpeg_sidecar::command::FfmpegCommand;
use ffmpeg_sidecar::event::{FfmpegEvent, LogLevel};
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

#[derive(Default)]
pub struct FrameSlot {
    latest: Mutex<Option<Frame>>,
    count: AtomicU64,
}

impl FrameSlot {
    pub fn take(&self) -> Option<Frame> {
        self.latest.lock().unwrap().take()
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }
}

/// Decodes one bitstream. When the parameter sets change at a keyframe (the
/// sender restarted its encoder, e.g. after a resolution change) the FFmpeg
/// decoder is started again from that keyframe: one FFmpeg run keeps the
/// frame size it started with and would scale everything to it.
pub struct Decoder {
    codec: Codec,
    slot: Arc<FrameSlot>,
    repaint: Arc<dyn Fn() + Send + Sync>,
    proc: Proc,
    scanner: Scanner,
    params: BTreeMap<u8, Vec<u8>>,
    /// Parameter sets the running FFmpeg started with (`None` before the
    /// first keyframe).
    active: Option<BTreeMap<u8, Vec<u8>>>,
}

impl Decoder {
    pub fn start(codec: Codec, slot: Arc<FrameSlot>, repaint: impl Fn() + Send + Sync + 'static) -> Result<Self> {
        let repaint: Arc<dyn Fn() + Send + Sync> = Arc::new(repaint);
        Ok(Decoder {
            codec,
            proc: Proc::start(codec, slot.clone(), repaint.clone())?,
            slot,
            repaint,
            scanner: Scanner::new(codec),
            params: BTreeMap::new(),
            active: None,
        })
    }

    /// Feeds received bitstream. Applies backpressure if decoding lags.
    pub async fn feed(&mut self, data: Bytes) -> Result<()> {
        let out = self.scanner.scan(&data);
        self.params.extend(out.params);
        let Some(at) = out.cut else { return self.proc.feed(data).await };
        let changed = self.active.as_ref().is_some_and(|a| *a != self.params);
        self.active = Some(self.params.clone());
        if !changed {
            return self.proc.feed(data).await;
        }
        tracing::info!("stream parameters changed, restarting decoder");
        // The old FFmpeg is killed when replaced; what came before the
        // keyframe belongs to the old stream.
        self.proc = Proc::start(self.codec, self.slot.clone(), self.repaint.clone())?;
        let mut entry = BytesMut::new();
        for nal in self.params.values() {
            entry.extend_from_slice(nal);
        }
        entry.extend_from_slice(&START_CODE);
        entry.extend_from_slice(&data[at..]);
        self.proc.feed(entry.freeze()).await
    }
}

/// One FFmpeg decoder process.
struct Proc {
    tx: mpsc::Sender<Bytes>,
    child: Arc<Mutex<ffmpeg_sidecar::child::FfmpegChild>>,
}

impl Drop for Proc {
    fn drop(&mut self) {
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Proc {
    fn start(codec: Codec, slot: Arc<FrameSlot>, repaint: Arc<dyn Fn() + Send + Sync>) -> Result<Self> {
        let mut child = FfmpegCommand::new_with_path(ffmpeg::require()?)
            .hide_banner()
            // Hardware decode, no frame threading (= no added frames of
            // latency). Note: `-fflags nobuffer` must NOT be used, it drops
            // the probed packets, i.e. the keyframe we start with.
            .args(["-hwaccel", "auto", "-threads", "1", "-flags", "low_delay"])
            .args(["-probesize", "32", "-analyzeduration", "0"])
            .args(["-f", codec.ffmpeg_format(), "-i", "pipe:0"])
            .args(["-an", "-fps_mode", "passthrough"])
            .args(["-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"])
            .spawn()
            .context("nie można uruchomić dekodera")?;
        let mut stdin = child.take_stdin().context("brak stdin dekodera")?;
        let iter = child.iter()?;

        let (tx, mut rx) = mpsc::channel::<Bytes>(64);
        std::thread::spawn(move || {
            while let Some(b) = rx.blocking_recv() {
                if stdin.write_all(&b).is_err() {
                    break;
                }
            }
        });
        std::thread::spawn(move || {
            for ev in iter {
                match ev {
                    FfmpegEvent::OutputFrame(f) => {
                        slot.count.fetch_add(1, Ordering::Relaxed);
                        *slot.latest.lock().unwrap() = Some(Frame {
                            width: f.width as usize,
                            height: f.height as usize,
                            rgba: f.data,
                        });
                        repaint();
                    }
                    FfmpegEvent::Log(LogLevel::Error | LogLevel::Fatal, l) => {
                        tracing::warn!("ffmpeg dec: {l}")
                    }
                    FfmpegEvent::Error(e) => tracing::warn!("ffmpeg dec: {e}"),
                    _ => {}
                }
            }
        });
        Ok(Proc { tx, child: Arc::new(Mutex::new(child)) })
    }

    async fn feed(&self, data: Bytes) -> Result<()> {
        self.tx.send(data).await.ok().context("dekoder zamknięty")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn encode(size: &str) -> Vec<u8> {
        let src = format!("testsrc2=size={size}:rate=30");
        let out = ffmpeg::command()
            .unwrap()
            .args(["-loglevel", "error", "-f", "lavfi", "-i", &src, "-frames:v", "30"])
            .args(["-c:v", "libx264", "-bf", "0", "-f", "h264", "pipe:1"])
            .output()
            .unwrap();
        assert!(out.status.success());
        out.stdout
    }

    async fn wait_for_size(slot: &FrameSlot, size: (usize, usize)) {
        let start = Instant::now();
        loop {
            if let Some(f) = slot.take() {
                if (f.width, f.height) == size {
                    assert_eq!(f.rgba.len(), f.width * f.height * 4);
                    return;
                }
            }
            assert!(start.elapsed() < Duration::from_secs(15), "no {size:?} frame");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The sender's encoder restarts at another resolution (e.g. a game
    /// changed the display mode): viewers get frames of the new size.
    /// Run with: BIBICORD_FFMPEG=/path/to/ffmpeg cargo test resolution_change -- --ignored
    #[tokio::test]
    #[ignore]
    async fn resolution_change() {
        ffmpeg::ensure(|_| {}).unwrap();
        let (a, b) = (encode("1280x720"), encode("640x480"));
        let slot = Arc::new(FrameSlot::default());
        let mut dec = Decoder::start(Codec::H264, slot.clone(), || {}).unwrap();
        for c in a.chunks(4096) {
            dec.feed(Bytes::copy_from_slice(c)).await.unwrap();
        }
        wait_for_size(&slot, (1280, 720)).await;
        for c in b.chunks(4096) {
            dec.feed(Bytes::copy_from_slice(c)).await.unwrap();
        }
        wait_for_size(&slot, (640, 480)).await;
    }
}