//! UI sound cues (joining, leaving, mute, deafen, streams, messages).
//!
//! All cues are synthesised at runtime: short sine "bloops" with a few
//! harmonics, a slight upward pitch glide at the attack and a small
//! damped-comb reverb. Nothing is copied from other apps.
//!
//! Playback has its own output stream (independent of the voice call), so
//! "disconnect" still sounds after the call ends and "deafen" plays while
//! deafened. The stream opens on demand and closes after a few idle seconds.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Cue {
    /// You join the voice call.
    Connect,
    /// You leave the voice call.
    Disconnect,
    /// Someone joins the call you are in.
    PeerJoin,
    /// Someone leaves the call you are in.
    PeerLeave,
    Mute,
    Unmute,
    Deafen,
    Undeafen,
    StreamStart,
    StreamStop,
    Message,
    /// Push-to-talk key pressed / released.
    PttOn,
    PttOff,
}

impl Cue {
    #[cfg(test)]
    pub const ALL: [Cue; 13] = [
        Cue::Connect,
        Cue::Disconnect,
        Cue::PeerJoin,
        Cue::PeerLeave,
        Cue::Mute,
        Cue::Unmute,
        Cue::Deafen,
        Cue::Undeafen,
        Cue::StreamStart,
        Cue::StreamStop,
        Cue::Message,
        Cue::PttOn,
        Cue::PttOff,
    ];
}

// ------------------------------------------------------------- synthesis

/// One note: start time (s), frequency (Hz), amplitude, decay time constant (s).
struct Note(f32, f32, f32, f32);

struct Score {
    notes: Vec<Note>,
    /// Pitch starts this fraction below the target and glides up (≈18 ms).
    glide: f32,
    /// Peak level after normalisation (relative loudness between cues).
    level: f32,
}

// Note frequencies (equal temperament, A4 = 440 Hz).
const C4: f32 = 261.63;
const G4: f32 = 392.00;
const AS4: f32 = 466.16;
const C5: f32 = 523.25;
const D5: f32 = 587.33;
const E5: f32 = 659.25;
const F5: f32 = 698.46;
const GS5: f32 = 830.61;
const G5: f32 = 783.99;
const A5: f32 = 880.00;
const B5: f32 = 987.77;
const C6: f32 = 1046.50;
const D6: f32 = 1174.66;
const E6: f32 = 1318.51;
const G6: f32 = 1567.98;

fn score(cue: Cue) -> Score {
    let s = |notes, glide, level| Score { notes, glide, level };
    match cue {
        Cue::Connect => s(vec![Note(0.0, C5, 1.0, 0.08), Note(0.07, G5, 0.9, 0.08), Note(0.14, C6, 0.75, 0.17)], 0.05, 1.0),
        Cue::Disconnect => s(vec![Note(0.0, G5, 0.85, 0.08), Note(0.07, D5, 0.95, 0.08), Note(0.14, G4, 1.0, 0.18)], 0.05, 1.0),
        Cue::PeerJoin => s(vec![Note(0.0, D5, 1.0, 0.09), Note(0.08, A5, 0.9, 0.13)], 0.06, 0.95),
        Cue::PeerLeave => s(vec![Note(0.0, A5, 0.9, 0.09), Note(0.08, D5, 1.0, 0.13)], 0.06, 0.95),
        Cue::Mute => s(vec![Note(0.0, F5, 1.0, 0.045), Note(0.055, AS4, 1.0, 0.06)], 0.04, 0.8),
        Cue::Unmute => s(vec![Note(0.0, AS4, 1.0, 0.045), Note(0.055, F5, 1.0, 0.06)], 0.04, 0.8),
        Cue::Deafen => s(vec![Note(0.0, C5, 1.0, 0.05), Note(0.06, G4, 1.0, 0.05), Note(0.12, C4, 1.0, 0.1)], 0.04, 0.85),
        Cue::Undeafen => s(vec![Note(0.0, C4, 1.0, 0.05), Note(0.06, G4, 1.0, 0.05), Note(0.12, C5, 1.0, 0.1)], 0.04, 0.85),
        Cue::StreamStart => s(
            vec![Note(0.0, E5, 0.8, 0.06), Note(0.06, GS5, 0.8, 0.06), Note(0.12, B5, 0.8, 0.06), Note(0.18, E6, 0.7, 0.2)],
            0.05,
            0.9,
        ),
        Cue::StreamStop => s(vec![Note(0.0, B5, 0.8, 0.06), Note(0.06, GS5, 0.8, 0.06), Note(0.12, E5, 0.9, 0.15)], 0.05, 0.9),
        Cue::Message => s(vec![Note(0.0, D6, 1.0, 0.05), Note(0.045, G6, 0.55, 0.07)], 0.2, 0.7),
        // Single quiet ticks: heard on every key press, so short and soft.
        Cue::PttOn => s(vec![Note(0.0, G5, 1.0, 0.035)], 0.03, 0.45),
        Cue::PttOff => s(vec![Note(0.0, D5, 1.0, 0.035)], 0.03, 0.4),
    }
}

/// Renders a cue as mono samples at `rate`, peak = the cue's level (≤ 1).
pub fn render(cue: Cue, rate: u32) -> Vec<f32> {
    use std::f64::consts::TAU;
    let sc = score(cue);
    let r = rate as f32;
    let ring = |n: &Note| n.3 * 7.0; // until the note is ~-60 dB
    let end = sc.notes.iter().map(|n| n.0 + ring(n)).fold(0.0, f32::max) + 0.3;
    let len = (end * r) as usize;
    let mut dry = vec![0f32; len];
    for n in &sc.notes {
        let s0 = (n.0 * r) as usize;
        let mut phase = 0f64;
        for i in 0..((ring(n) * r) as usize).min(len - s0) {
            let t = i as f32 / r;
            let f = n.1 * (1.0 - sc.glide * (-t / 0.018).exp());
            phase += TAU * f as f64 / rate as f64;
            // 4 ms raised-cosine attack, exponential decay.
            let attack = if t < 0.004 { 0.5 - 0.5 * (std::f32::consts::PI * t / 0.004).cos() } else { 1.0 };
            let env = attack * (-t / n.3).exp();
            // Harmonics fade faster than the fundamental: bright onset, soft tail.
            let h = (-t / (n.3 * 0.5)).exp();
            let p = phase as f32;
            let x = p.sin() + 0.22 * h * (2.0 * p).sin() + 0.07 * h * h * (3.0 * p).sin();
            dry[s0 + i] += n.2 * env * x;
        }
    }

    // Small room: parallel damped comb filters, low-passed, mixed in quietly.
    let mut wet = vec![0f32; len];
    for (ms, fb) in [(29.7f32, 0.42f32), (37.1, 0.40), (41.1, 0.38), (43.7, 0.36)] {
        let d = (ms * r / 1000.0) as usize;
        let mut line = vec![0f32; len];
        let mut lp = 0f32;
        for i in 0..len {
            let back = if i >= d { line[i - d] } else { 0.0 };
            lp += 0.45 * (back - lp);
            line[i] = dry[i] + fb * lp;
            wet[i] += line[i];
        }
    }
    let mut out: Vec<f32> = dry.iter().zip(&wet).map(|(d, w)| d + 0.06 * w).collect();

    // Trim the inaudible tail, fade out, normalise.
    let peak = out.iter().fold(0f32, |m, s| m.max(s.abs())).max(1e-9);
    let last = out.iter().rposition(|s| s.abs() > peak * 0.003).unwrap_or(0);
    out.truncate((last + 1).min(len));
    let fade = ((0.03 * r) as usize).min(out.len());
    let n = out.len();
    for (k, s) in out[n - fade..].iter_mut().enumerate() {
        *s *= 1.0 - k as f32 / fade as f32;
    }
    let g = sc.level / peak;
    out.iter_mut().for_each(|s| *s *= g);
    out
}

// --------------------------------------------------------------- settings

/// User preferences: on/off and volume (0–100 %).
#[derive(Clone, Copy, PartialEq)]
pub struct Prefs {
    pub enabled: bool,
    pub volume: u8,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs { enabled: true, volume: 60 }
    }
}

fn prefs_file() -> std::path::PathBuf {
    crate::ffmpeg::data_dir().join("sounds.txt")
}

impl Prefs {
    pub fn load() -> Self {
        let mut p = Prefs::default();
        if let Ok(s) = std::fs::read_to_string(prefs_file()) {
            for line in s.lines() {
                match line.split_once('=') {
                    Some(("enabled", v)) => p.enabled = v.trim() != "0",
                    Some(("volume", v)) => p.volume = v.trim().parse::<u8>().unwrap_or(p.volume).min(100),
                    _ => {}
                }
            }
        }
        p
    }

    pub fn save(&self) {
        let _ = std::fs::create_dir_all(crate::ffmpeg::data_dir());
        let _ = std::fs::write(prefs_file(), format!("enabled={}\nvolume={}\n", self.enabled as u8, self.volume));
    }
}

// --------------------------------------------------------------- playback

/// Loudest cue at 100 % volume (≈ -6 dBFS) so cues never drown out voices.
const MAX_GAIN: f32 = 0.5;
/// Close the output stream after this long without sounds.
const IDLE_CLOSE: Duration = Duration::from_secs(4);

struct PlayCmd {
    cue: Cue,
    gain: f32,
    device: Option<String>,
}

pub struct Player {
    pub prefs: Prefs,
    tx: Option<mpsc::Sender<PlayCmd>>,
}

impl Player {
    pub fn new() -> Self {
        let off = std::env::var_os("BIBICORD_FAKE_AUDIO").is_some() || std::env::var_os("BIBICORD_NO_SOUNDS").is_some();
        let tx = (!off).then(|| {
            let (tx, rx) = mpsc::channel();
            std::thread::Builder::new()
                .name("sounds".into())
                .spawn(move || worker(rx))
                .ok()
                .map(|_| tx)
        });
        Player { prefs: Prefs::load(), tx: tx.flatten() }
    }

    /// Plays `cue` on the given output device (`None` = system default).
    pub fn play(&self, cue: Cue, device: Option<String>) {
        if !self.prefs.enabled || self.prefs.volume == 0 {
            return;
        }
        self.play_forced(cue, device);
    }

    /// Plays even when cues are switched off (volume preview).
    pub fn play_forced(&self, cue: Cue, device: Option<String>) {
        let v = self.prefs.volume as f32 / 100.0;
        // Perceptual volume curve.
        self.remote().play(cue, MAX_GAIN * v * v, device);
    }

    /// Current playback gain; 0 when cues are switched off.
    pub fn gain(&self) -> f32 {
        let v = self.prefs.volume as f32 / 100.0;
        if self.prefs.enabled { MAX_GAIN * v * v } else { 0.0 }
    }

    /// Handle for playing cues from other threads (push-to-talk key).
    pub fn remote(&self) -> Remote {
        Remote(self.tx.clone())
    }
}

/// Sends cues to the player thread; `gain` from `Player::gain`.
#[derive(Clone)]
pub struct Remote(Option<mpsc::Sender<PlayCmd>>);

impl Remote {
    pub fn play(&self, cue: Cue, gain: f32, device: Option<String>) {
        if let Some(tx) = &self.0
            && gain > 0.0
        {
            let _ = tx.send(PlayCmd { cue, gain, device });
        }
    }
}

struct Voice {
    samples: Arc<Vec<f32>>,
    pos: usize,
    gain: f32,
}

type Queue = Arc<Mutex<Vec<Voice>>>;

struct Out {
    _stream: cpal::Stream,
    device: Option<String>,
    rate: u32,
    queue: Queue,
}

fn worker(rx: mpsc::Receiver<PlayCmd>) {
    let mut out: Option<Out> = None;
    let mut cache: HashMap<(Cue, u32), Arc<Vec<f32>>> = HashMap::new();
    let mut last = Instant::now();
    loop {
        let cmd = if out.is_some() {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(c) => Some(c),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match rx.recv() {
                Ok(c) => Some(c),
                Err(_) => return,
            }
        };
        let Some(cmd) = cmd else {
            let idle = out.as_ref().is_some_and(|o| o.queue.lock().unwrap().is_empty());
            if idle && last.elapsed() > IDLE_CLOSE {
                out = None;
            }
            continue;
        };
        last = Instant::now();
        if out.as_ref().is_none_or(|o| o.device != cmd.device) {
            out = None;
            match open(cmd.device.clone()) {
                Ok(o) => out = Some(o),
                Err(e) => {
                    tracing::warn!("dźwięki: {e:#}");
                    continue;
                }
            }
        }
        let o = out.as_ref().unwrap();
        let samples = cache.entry((cmd.cue, o.rate)).or_insert_with(|| Arc::new(render(cmd.cue, o.rate))).clone();
        o.queue.lock().unwrap().push(Voice { samples, pos: 0, gain: cmd.gain });
    }
}

fn mix(queue: &Queue, frames: usize) -> Vec<f32> {
    let mut buf = vec![0f32; frames];
    let mut q = queue.lock().unwrap();
    for v in q.iter_mut() {
        let n = (v.samples.len() - v.pos).min(frames);
        for (o, s) in buf.iter_mut().zip(&v.samples[v.pos..v.pos + n]) {
            *o += s * v.gain;
        }
        v.pos += n;
    }
    q.retain(|v| v.pos < v.samples.len());
    buf.iter_mut().for_each(|s| *s = s.clamp(-1.0, 1.0));
    buf
}

fn open(device: Option<String>) -> anyhow::Result<Out> {
    use anyhow::Context;
    let host = cpal::default_host();
    let dev = device
        .as_deref()
        .and_then(|id| id.parse().ok())
        .and_then(|id| host.device_by_id(&id))
        .filter(|d| d.default_output_config().is_ok())
        .or_else(|| host.default_output_device())
        .context("brak głośników")?;
    let cfg = dev.default_output_config()?;
    let ch = cfg.channels() as usize;
    let rate = cfg.sample_rate();
    let queue: Queue = Arc::default();
    let q = queue.clone();
    let err = |e| tracing::warn!("dźwięki: {e}");
    let stream = match cfg.sample_format() {
        cpal::SampleFormat::F32 => dev.build_output_stream::<f32, _, _>(
            cfg.config(),
            move |d: &mut [f32], _| {
                let m = mix(&q, d.len() / ch);
                for (f, s) in d.chunks_mut(ch).zip(m) {
                    f.fill(s);
                }
            },
            err,
            None,
        )?,
        cpal::SampleFormat::I16 => dev.build_output_stream::<i16, _, _>(
            cfg.config(),
            move |d: &mut [i16], _| {
                let m = mix(&q, d.len() / ch);
                for (f, s) in d.chunks_mut(ch).zip(m) {
                    f.fill((s * 32767.0) as i16);
                }
            },
            err,
            None,
        )?,
        f => anyhow::bail!("nieobsługiwany format głośników {f:?}"),
    };
    stream.play()?;
    Ok(Out { _stream: stream, device, rate, queue })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cues_are_clean() {
        for rate in [44_100, 48_000, 96_000] {
            for cue in Cue::ALL {
                let s = render(cue, rate);
                let secs = s.len() as f32 / rate as f32;
                assert!((0.15..1.4).contains(&secs), "{cue:?} {secs}s");
                assert!(s.iter().all(|x| x.is_finite() && x.abs() <= 1.0), "{cue:?}");
                assert!(s[0].abs() < 1e-3 && s.last().unwrap().abs() < 1e-3, "{cue:?} clicks");
                let peak = s.iter().fold(0f32, |m, x| m.max(x.abs()));
                assert!((peak - score(cue).level).abs() < 1e-3, "{cue:?} peak {peak}");
            }
        }
    }

    /// Plays every cue on the default output device:
    /// `cargo test play_all -- --ignored`
    #[test]
    #[ignore]
    fn play_all() {
        let mut p = Player::new();
        p.prefs.volume = 60;
        for cue in Cue::ALL {
            p.play_forced(cue, None);
            std::thread::sleep(Duration::from_millis(1100));
        }
    }

    /// Writes every cue as WAV for listening:
    /// `BIBICORD_SOUNDS_OUT=/tmp/cues cargo test export_wavs -- --ignored`
    #[test]
    #[ignore]
    fn export_wavs() {
        let dir = std::path::PathBuf::from(std::env::var("BIBICORD_SOUNDS_OUT").unwrap_or("target/sounds".into()));
        std::fs::create_dir_all(&dir).unwrap();
        for cue in Cue::ALL {
            let s = render(cue, 48_000);
            let mut w = Vec::new();
            let data = (s.len() * 2) as u32;
            w.extend_from_slice(b"RIFF");
            w.extend_from_slice(&(36 + data).to_le_bytes());
            w.extend_from_slice(b"WAVEfmt ");
            w.extend_from_slice(&16u32.to_le_bytes());
            w.extend_from_slice(&1u16.to_le_bytes());
            w.extend_from_slice(&1u16.to_le_bytes());
            w.extend_from_slice(&48_000u32.to_le_bytes());
            w.extend_from_slice(&96_000u32.to_le_bytes());
            w.extend_from_slice(&2u16.to_le_bytes());
            w.extend_from_slice(&16u16.to_le_bytes());
            w.extend_from_slice(b"data");
            w.extend_from_slice(&data.to_le_bytes());
            for x in s {
                w.extend_from_slice(&((x * MAX_GAIN * 32767.0) as i16).to_le_bytes());
            }
            std::fs::write(dir.join(format!("{cue:?}.wav")), w).unwrap();
        }
    }
}
