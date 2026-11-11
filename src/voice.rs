//! Voice chat: microphone -> Opus (48 kHz mono, 20 ms) -> QUIC datagrams,
//! and per-peer adaptive jitter buffer + Opus decode (FEC, PLC) + mixing.
//!
//! Datagram payload: `[seq: u16 LE][flags: u8][opus]`. `seq` counts 20 ms
//! frames and advances even when nothing is sent (silence, mute), so it
//! doubles as a timestamp. `flags & 1` = the sender is speaking right now,
//! `flags & 2` = last packet of a talk spurt (the sender goes quiet),
//! `flags & 4` = pre-roll (recorded before the gate opened, sent in a burst).
//!
//! The receiver decodes on the audio thread, one frame at a time, in `seq`
//! order: a missing frame is rebuilt from the next packet's in-band FEC or
//! concealed by Opus PLC. The buffer depth adapts to network jitter (grows on
//! late packets, shrinks after calm periods) and absorbs clock drift by
//! dropping a frame when it gets too deep.

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use iroh::EndpointId;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const RATE: u32 = 48_000;
const FRAME: usize = 960; // 20 ms

/// Jitter buffer depth in frames (20 ms each).
const MIN_DEPTH: u64 = 3;
const START_DEPTH: u64 = 3;
const MAX_DEPTH: u64 = 15;
/// Played frames without a late packet before the depth shrinks by one.
const CALM_FRAMES: u32 = 500;
/// Two late packets closer than this (in played frames) grow the buffer;
/// an isolated one is covered by FEC.
const LATE_WINDOW: u32 = 150;
/// Frames of PLC to bridge an underrun before pausing playback.
const MAX_CONCEAL: u32 = 3;

/// Voice activity: frame RMS above both the noise floor (+12 dB) and an
/// absolute minimum (-56 dBFS). Sending continues for `HANGOVER` frames
/// after the last voiced frame so word endings and pauses are not cut.
const VAD_MIN: f32 = 0.0016;
const VAD_OVER_FLOOR: f32 = 2.5;
const HANGOVER: u32 = 50;
/// Frames sent from before the gate opened (soft word onsets).
const PREROLL: usize = 2;

const FLAG_SPEAKING: u8 = 1;
const FLAG_END: u8 = 2;
const FLAG_PREROLL: u8 = 4;

pub struct Voice {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
}

struct Shared {
    muted: AtomicBool,
    deafened: AtomicBool,
    noise_suppression: AtomicBool,
    /// Manual voice-activation threshold in dBFS, or `AUTO`.
    sensitivity: AtomicI32,
    /// Last microphone frame level in dBFS (`f32` bits), after suppression.
    mic_level: AtomicU32,
    out_rate: Mutex<u32>,
    peers: Mutex<HashMap<EndpointId, PeerAudio>>,
    me_speaking_until: Mutex<Instant>,
}

const AUTO: i32 = i32::MIN;

impl Shared {
    fn new(muted: bool, deafened: bool, prefs: Prefs) -> Self {
        let s = Shared {
            muted: AtomicBool::new(muted),
            deafened: AtomicBool::new(deafened),
            noise_suppression: AtomicBool::new(false),
            sensitivity: AtomicI32::new(AUTO),
            mic_level: AtomicU32::new((-100f32).to_bits()),
            out_rate: Mutex::new(RATE),
            peers: Mutex::new(HashMap::new()),
            me_speaking_until: Mutex::new(Instant::now()),
        };
        s.set_prefs(prefs);
        s
    }

    fn set_prefs(&self, p: Prefs) {
        self.noise_suppression.store(p.noise_suppression, Ordering::Relaxed);
        self.sensitivity.store(p.sensitivity.unwrap_or(AUTO), Ordering::Relaxed);
    }
}

#[derive(PartialEq, Clone, Copy)]
enum Play {
    /// Nothing to play; waiting for packets.
    Idle,
    /// Collecting `depth` frames before starting.
    Buffering,
    Playing,
}

struct PeerAudio {
    out_rate: u32,
    dec: opus::Decoder,
    resample: Linear,
    /// Decoded samples at the output rate.
    pcm: VecDeque<f32>,
    /// Received packets by extended sequence number.
    packets: std::collections::BTreeMap<u64, (u8, Vec<u8>)>,
    highest: Option<u64>,
    /// Next frame to play.
    next: u64,
    state: Play,
    depth: u64,
    concealed_run: u32,
    /// The last played packet ended a talk spurt.
    ended: bool,
    calm: u32,
    since_late: u32,
    since_drop: u32,
    speaking_until: Instant,
    stats: PeerStats,
}

/// Per-peer receive counters (for diagnostics).
#[derive(Debug, Default, Clone, Copy)]
pub struct PeerStats {
    pub received: u64,
    pub late: u64,
    pub fec: u64,
    pub concealed: u64,
    pub dropped: u64,
    pub underruns: u64,
    pub depth_ms: u64,
}

impl PeerAudio {
    fn new(out_rate: u32) -> Option<Self> {
        Some(PeerAudio {
            out_rate,
            dec: opus::Decoder::new(RATE, opus::Channels::Mono).ok()?,
            resample: Linear::new(RATE, out_rate),
            pcm: VecDeque::new(),
            packets: Default::default(),
            highest: None,
            next: 0,
            state: Play::Idle,
            depth: START_DEPTH,
            concealed_run: 0,
            ended: false,
            calm: 0,
            since_late: u32::MAX,
            since_drop: 0,
            speaking_until: Instant::now(),
            stats: PeerStats { depth_ms: START_DEPTH * 20, ..Default::default() },
        })
    }

    /// Frames between the play cursor and the newest packet.
    fn buffered(&self) -> u64 {
        match self.highest {
            Some(h) if h >= self.next => h - self.next + 1,
            _ => 0,
        }
    }

    fn insert(&mut self, seq: u16, flags: u8, payload: &[u8]) {
        // Extend the 16-bit counter relative to the newest packet seen.
        let ext = match self.highest {
            None => (1u64 << 32) + seq as u64,
            Some(h) => {
                let d = seq.wrapping_sub(h as u16) as i16 as i64;
                (h as i64 + d) as u64
            }
        };
        if let Some(h) = self.highest {
            // Sender restarted (new voice session): start over.
            if ext + 3000 < h {
                if let Some(fresh) = PeerAudio::new(self.out_rate) {
                    *self = PeerAudio { stats: self.stats, ..fresh };
                    return self.insert(seq, flags, payload);
                }
            }
        }
        self.stats.received += 1;
        if ext < self.next && self.state != Play::Idle {
            // Arrived after its playout time: the buffer is too shallow.
            self.stats.late += 1;
            if self.since_late < LATE_WINDOW {
                self.grow();
            }
            self.since_late = 0;
            return;
        }
        self.highest = Some(self.highest.map_or(ext, |h| h.max(ext)));
        self.packets.insert(ext, (flags, payload.to_vec()));
        while self.packets.len() > 100 {
            self.packets.pop_first();
        }
    }

    fn grow(&mut self) {
        self.depth = (self.depth + 1).min(MAX_DEPTH);
        self.calm = 0;
        self.stats.depth_ms = self.depth * 20;
    }

    fn decode(&mut self, data: &[u8], fec: bool) {
        let mut pcm = [0f32; FRAME * 6];
        // For a lost frame (PLC, or FEC from the next packet) the buffer size
        // is how much audio libopus makes up: exactly the one missing frame.
        // A received packet decodes to its own length (up to 120 ms).
        let len = if fec || data.is_empty() { FRAME } else { pcm.len() };
        let n = match self.dec.decode_float(data, &mut pcm[..len], fec) {
            Ok(n) => n.min(FRAME),
            Err(_) => return,
        };
        // Clock drift: play 0.5 % faster when the buffer is deeper than
        // the target, 0.5 % slower when shallower (inaudible).
        let b = self.buffered();
        let speed = if b > self.depth + 1 {
            1.005
        } else if b + 1 < self.depth {
            0.995
        } else {
            1.0
        };
        self.resample.set_speed(speed);
        let mut tmp = Vec::with_capacity(n * 2);
        self.resample.process(&pcm[..n], &mut tmp);
        self.pcm.extend(tmp);
    }

    /// Decodes the next frame into `pcm`. Returns false when there is
    /// nothing left to play.
    fn step(&mut self) -> bool {
        loop {
            match self.state {
                Play::Idle | Play::Buffering => {
                    let Some(&first) = self.packets.keys().next() else {
                        self.state = Play::Idle;
                        return false;
                    };
                    // New talk spurt, or the first frames are missing:
                    // start at the oldest packet (silence is not concealed).
                    if self.state == Play::Idle || first > self.next {
                        self.next = first;
                    }
                    self.state = Play::Buffering;
                    // Pre-roll frames arrive in one burst and do not prove the
                    // link is ahead: count depth from the first real-time frame.
                    let live = self
                        .packets
                        .iter()
                        .find(|(_, (f, _))| f & FLAG_PREROLL == 0)
                        .map(|(k, _)| *k);
                    let ready = match (live, self.highest) {
                        (Some(l), Some(h)) => h + 1 >= l + self.depth,
                        _ => false,
                    };
                    if !ready {
                        return false;
                    }
                    self.state = Play::Playing;
                    self.concealed_run = 0;
                }
                Play::Playing => break,
            }
        }

        // Far too deep (burst after a stall): drop a frame.
        self.since_drop += 1;
        if self.buffered() > self.depth + 5 && self.since_drop >= 5 {
            if let Some((_, data)) = self.packets.remove(&self.next) {
                let mut pcm = [0f32; FRAME * 6];
                let _ = self.dec.decode_float(&data, &mut pcm, false);
            }
            self.next += 1;
            self.since_drop = 0;
            self.stats.dropped += 1;
        }

        if self.ended && !self.packets.contains_key(&self.next) {
            // Talk spurt finished normally: no concealment, no penalty.
            self.ended = false;
            self.state = if self.packets.is_empty() { Play::Idle } else { Play::Buffering };
            return false;
        }
        if let Some((flags, data)) = self.packets.remove(&self.next) {
            self.decode(&data, false);
            if flags & FLAG_SPEAKING != 0 {
                self.speaking_until = Instant::now() + Duration::from_millis(250);
            }
            self.ended = flags & FLAG_END != 0;
            self.concealed_run = 0;
        } else if self.highest.is_some_and(|h| h > self.next) {
            // Lost: rebuild from the next packet's FEC, else conceal.
            if let Some((_, data)) = self.packets.get(&(self.next + 1)).cloned() {
                self.decode(&data, true);
                self.stats.fec += 1;
            } else {
                self.decode(&[], false);
                self.stats.concealed += 1;
            }
        } else {
            // Underrun: nothing newer has arrived.
            if self.concealed_run >= MAX_CONCEAL {
                self.state = Play::Buffering;
                self.stats.underruns += 1;
                self.grow();
                return false;
            }
            self.concealed_run += 1;
            self.decode(&[], false);
            self.stats.concealed += 1;
        }
        self.next += 1;
        self.calm += 1;
        self.since_late = self.since_late.saturating_add(1);
        if self.calm >= CALM_FRAMES && self.depth > MIN_DEPTH {
            self.depth -= 1;
            self.calm = 0;
            self.stats.depth_ms = self.depth * 20;
        }
        true
    }

    /// Adds this peer's audio into `out`.
    fn mix_into(&mut self, out: &mut [f32]) {
        while self.pcm.len() < out.len() {
            if !self.step() {
                break;
            }
        }
        for o in out.iter_mut() {
            match self.pcm.pop_front() {
                Some(s) => *o += s,
                None => break,
            }
        }
    }
}

impl Drop for Voice {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Voice {
    /// Starts capture + playback. Encoded packets (with sequence number
    /// prefix) are sent to `packets`.
    pub fn start(
        packets: mpsc::UnboundedSender<Bytes>,
        devices: Devices,
        muted: bool,
        deafened: bool,
        prefs: Prefs,
    ) -> Result<Self> {
        let shared = Arc::new(Shared::new(muted, deafened, prefs));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        {
            let shared = shared.clone();
            let stop = stop.clone();
            // cpal streams are not Send on every platform: own them on a
            // dedicated thread for their whole life.
            std::thread::spawn(move || {
                if fake_audio() {
                    let _ = ready_tx.send(Ok(()));
                    run_fake(shared, packets, stop);
                    return;
                }
                match run_audio(shared, packets, &devices) {
                    Ok((a, b)) => {
                        let _ = ready_tx.send(Ok(()));
                        while !stop.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(100));
                        }
                        drop((a, b));
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            });
        }
        ready_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| anyhow!("urządzenie audio nie odpowiada"))??;
        Ok(Voice { shared, stop })
    }

    pub fn set_muted(&self, m: bool) {
        self.shared.muted.store(m, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub fn is_muted(&self) -> bool {
        self.shared.muted.load(Ordering::Relaxed)
    }

    pub fn set_deafened(&self, d: bool) {
        self.shared.deafened.store(d, Ordering::Relaxed);
    }

    /// Applies noise suppression / sensitivity changes live.
    pub fn set_prefs(&self, p: Prefs) {
        self.shared.set_prefs(p);
    }

    /// Current microphone level in dBFS (what the voice gate compares).
    pub fn mic_level_db(&self) -> f32 {
        f32::from_bits(self.shared.mic_level.load(Ordering::Relaxed))
    }

    /// Receive counters per peer.
    pub fn stats(&self) -> Vec<(EndpointId, PeerStats)> {
        self.shared.peers.lock().unwrap().iter().map(|(id, p)| (*id, p.stats)).collect()
    }

    pub fn me_speaking(&self) -> bool {
        *self.shared.me_speaking_until.lock().unwrap() > Instant::now()
    }

    pub fn speaking(&self) -> Vec<EndpointId> {
        let now = Instant::now();
        self.shared
            .peers
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, p)| p.speaking_until > now)
            .map(|(id, _)| *id)
            .collect()
    }

    pub fn remove_peer(&self, id: &EndpointId) {
        self.shared.peers.lock().unwrap().remove(id);
    }

    /// Handles one received voice datagram (without the type byte).
    pub fn receive(&self, from: EndpointId, data: &[u8]) {
        if data.len() < 4 {
            return;
        }
        let seq = u16::from_le_bytes([data[0], data[1]]);
        let out_rate = *self.shared.out_rate.lock().unwrap();
        let mut peers = self.shared.peers.lock().unwrap();
        let p = match peers.entry(from) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => match PeerAudio::new(out_rate) {
                Some(p) => e.insert(p),
                None => return,
            },
        };
        p.insert(seq, data[2], &data[3..]);
    }
}

fn rms(s: &[f32]) -> f32 {
    if s.is_empty() {
        return 0.0;
    }
    (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt()
}

/// Selected audio devices (cpal device ids); `None` = system default.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Devices {
    pub input: Option<String>,
    pub output: Option<String>,
}

/// Microphone processing preferences.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prefs {
    pub noise_suppression: bool,
    /// Voice-activation threshold in dBFS; `None` = automatic (adaptive).
    pub sensitivity: Option<i32>,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs { noise_suppression: true, sensitivity: None }
    }
}

fn prefs_file() -> std::path::PathBuf {
    crate::ffmpeg::data_dir().join("voice.txt")
}

impl Prefs {
    pub fn load() -> Self {
        std::fs::read_to_string(prefs_file()).map(|s| Prefs::parse(&s)).unwrap_or_default()
    }

    fn parse(s: &str) -> Self {
        let mut p = Prefs::default();
        for line in s.lines() {
            match line.split_once('=').map(|(k, v)| (k.trim(), v.trim())) {
                Some(("noise_suppression", v)) => p.noise_suppression = v != "0",
                Some(("sensitivity", v)) => {
                    p.sensitivity = v.parse::<i32>().ok().map(|d| d.clamp(crate::mic::DB_MIN, crate::mic::DB_MAX))
                }
                _ => {}
            }
        }
        p
    }

    fn format(&self) -> String {
        let sens = self.sensitivity.map_or("auto".to_string(), |d| d.to_string());
        format!("noise_suppression={}\nsensitivity={sens}\n", self.noise_suppression as u8)
    }

    pub fn save(&self) {
        let _ = std::fs::create_dir_all(crate::ffmpeg::data_dir());
        let _ = std::fs::write(prefs_file(), self.format());
    }
}

/// One selectable audio device.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
}

/// Lists (inputs, outputs).
pub fn list_devices() -> (Vec<DeviceInfo>, Vec<DeviceInfo>) {
    let host = cpal::default_host();
    let info = |d: cpal::Device| {
        Some(DeviceInfo {
            id: d.id().ok()?.to_string(),
            name: d.description().ok()?.name().to_string(),
        })
    };
    let ins = host
        .input_devices()
        .map(|it| it.filter_map(info).collect())
        .unwrap_or_default();
    let outs = host
        .output_devices()
        .map(|it| it.filter_map(info).collect())
        .unwrap_or_default();
    (ins, outs)
}

/// Unit tests never touch real devices (and don't need the env var).
fn fake_audio() -> bool {
    cfg!(test) || std::env::var_os("BIBICORD_FAKE_AUDIO").is_some()
}

/// Encoder side: takes mono samples at `in_rate`, sends Opus packets.
fn capture_pipeline(
    shared: Arc<Shared>,
    packets: mpsc::UnboundedSender<Bytes>,
    in_rate: u32,
) -> Result<impl FnMut(&[f32]) + Send + 'static> {
    let mut enc = opus::Encoder::new(RATE, opus::Channels::Mono, opus::Application::Voip)?;
    enc.set_bitrate(opus::Bitrate::Bits(48_000))?;
    // In-band FEC: each packet also carries a low-bitrate copy of the
    // previous frame, which the receiver uses when a packet is lost.
    enc.set_inband_fec(true)?;
    enc.set_packet_loss_perc(10)?;
    let mut resample = Linear::new(in_rate, RATE);
    let mut pending: Vec<f32> = Vec::with_capacity(FRAME * 4);
    let mut seq: u16 = 0;
    let mut floor = VAD_MIN;
    let mut hang = 0u32;
    // Encoded but unsent frames, replayed when the gate opens.
    let mut preroll: VecDeque<Bytes> = VecDeque::with_capacity(PREROLL + 1);
    let mut denoise = crate::mic::Denoise::new();
    Ok(move |mono: &[f32]| {
        resample.process(mono, &mut pending);
        while pending.len() >= FRAME {
            let mut frame: Vec<f32> = pending.drain(..FRAME).collect();
            let this = seq;
            seq = seq.wrapping_add(1);
            if shared.noise_suppression.load(Ordering::Relaxed) {
                denoise.process(&mut frame);
            }
            // Noise floor: follows quiet frames down immediately, creeps up
            // ~1 dB/s so steady background noise does not count as speech.
            let level = rms(&frame);
            floor = if level < floor { level.max(1e-5) } else { floor * 1.0023 };
            let level_db = crate::mic::db(level);
            shared.mic_level.store(level_db.to_bits(), Ordering::Relaxed);
            let voiced = match shared.sensitivity.load(Ordering::Relaxed) {
                AUTO => level > VAD_MIN && level > floor * VAD_OVER_FLOOR,
                threshold => level_db >= threshold as f32,
            };
            if shared.muted.load(Ordering::Relaxed) {
                hang = 0;
                preroll.clear();
                continue;
            }
            let was_open = hang > 0;
            if voiced {
                hang = HANGOVER;
                *shared.me_speaking_until.lock().unwrap() = Instant::now() + Duration::from_millis(250);
            } else if hang > 0 {
                hang -= 1;
            }
            // Always encode, so the encoder state (and FEC) stays continuous.
            let mut out = [0u8; 1500];
            out[0..2].copy_from_slice(&this.to_le_bytes());
            out[2] = if voiced { FLAG_SPEAKING } else { 0 };
            if hang == 0 && was_open {
                out[2] |= FLAG_END;
            }
            let Ok(n) = enc.encode_float(&frame, &mut out[3..]) else { continue };
            let pkt = Bytes::copy_from_slice(&out[..n + 3]);
            if hang == 0 && !was_open {
                preroll.push_back(pkt);
                if preroll.len() > PREROLL {
                    preroll.pop_front();
                }
                continue;
            }
            for p in preroll.drain(..) {
                let mut p = p.to_vec();
                p[2] |= FLAG_PREROLL;
                let _ = packets.send(Bytes::from(p));
            }
            let _ = packets.send(pkt);
        }
    })
}

/// Playback side: mixes all peers into `frames` mono samples.
fn mixer(shared: Arc<Shared>, out_rate: u32) -> impl FnMut(usize) -> Vec<f32> + Send + 'static {
    *shared.out_rate.lock().unwrap() = out_rate;
    move |frames: usize| -> Vec<f32> {
        let mut out = vec![0f32; frames];
        {
            let mut peers = shared.peers.lock().unwrap();
            for p in peers.values_mut() {
                p.mix_into(&mut out);
            }
        }
        if shared.deafened.load(Ordering::Relaxed) {
            out.iter_mut().for_each(|s| *s = 0.0);
        }
        out.iter_mut().for_each(|s| *s = s.clamp(-1.0, 1.0));
        out
    }
}

/// Test mode without audio devices. With `BIBICORD_FAKE_TONE` the
/// "microphone" plays a tone that alternates between speech and silence.
fn run_fake(shared: Arc<Shared>, packets: mpsc::UnboundedSender<Bytes>, stop: Arc<AtomicBool>) {
    let Ok(mut capture) = capture_pipeline(shared.clone(), packets, RATE) else { return };
    let mut mix = mixer(shared, RATE);
    let tone = std::env::var_os("BIBICORD_FAKE_TONE").is_some();
    let start = Instant::now();
    let mut n: u64 = 0;
    while !stop.load(Ordering::SeqCst) {
        let t = start.elapsed().as_secs_f32();
        let speaking = tone && (t % 3.5) < 2.0;
        let frame: Vec<f32> = (0..FRAME)
            .map(|i| {
                if speaking {
                    let x = (n * FRAME as u64 + i as u64) as f32 / RATE as f32;
                    0.2 * (x * 2.0 * std::f32::consts::PI * 220.0).sin()
                } else {
                    0.0
                }
            })
            .collect();
        capture(&frame);
        let _ = mix(FRAME);
        n += 1;
        let next = start + Duration::from_millis(20 * n);
        if let Some(d) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(d);
        }
    }
}

fn find_device(host: &cpal::Host, id: Option<&str>, input: bool) -> Option<cpal::Device> {
    let id = id?.parse().ok()?;
    let d = host.device_by_id(&id)?;
    let ok = if input { d.default_input_config().is_ok() } else { d.default_output_config().is_ok() };
    ok.then_some(d)
}

fn run_audio(
    shared: Arc<Shared>,
    packets: mpsc::UnboundedSender<Bytes>,
    devices: &Devices,
) -> Result<(cpal::Stream, cpal::Stream)> {
    let host = cpal::default_host();
    let input = find_device(&host, devices.input.as_deref(), true)
        .or_else(|| host.default_input_device())
        .context("brak mikrofonu")?;
    let output = find_device(&host, devices.output.as_deref(), false)
        .or_else(|| host.default_output_device())
        .context("brak głośników")?;
    let in_cfg = input.default_input_config()?;
    let out_cfg = output.default_output_config()?;

    // ---- capture ----
    let in_ch = in_cfg.channels() as usize;
    let mut on_input = capture_pipeline(shared.clone(), packets, in_cfg.sample_rate())?;
    let err = |e| tracing::warn!("audio: {e}");
    let in_stream = match in_cfg.sample_format() {
        cpal::SampleFormat::F32 => input.build_input_stream::<f32, _, _>(
            in_cfg.config(),
            move |d: &[f32], _| on_input(&downmix(d, in_ch, |x| x)),
            err,
            None,
        )?,
        cpal::SampleFormat::I16 => input.build_input_stream::<i16, _, _>(
            in_cfg.config(),
            move |d: &[i16], _| on_input(&downmix(d, in_ch, |x| x as f32 / 32768.0)),
            err,
            None,
        )?,
        f => anyhow::bail!("nieobsługiwany format mikrofonu {f:?}"),
    };

    // ---- playback ----
    let out_ch = out_cfg.channels() as usize;
    let mut mix = mixer(shared.clone(), out_cfg.sample_rate());
    let out_stream = match out_cfg.sample_format() {
        cpal::SampleFormat::F32 => output.build_output_stream::<f32, _, _>(
            out_cfg.config(),
            move |d: &mut [f32], _| {
                let m = mix(d.len() / out_ch);
                for (f, s) in d.chunks_mut(out_ch).zip(m) {
                    f.fill(s);
                }
            },
            err,
            None,
        )?,
        cpal::SampleFormat::I16 => output.build_output_stream::<i16, _, _>(
            out_cfg.config(),
            move |d: &mut [i16], _| {
                let m = mix(d.len() / out_ch);
                for (f, s) in d.chunks_mut(out_ch).zip(m) {
                    f.fill((s * 32767.0) as i16);
                }
            },
            err,
            None,
        )?,
        f => anyhow::bail!("nieobsługiwany format głośników {f:?}"),
    };
    in_stream.play()?;
    out_stream.play()?;
    Ok((in_stream, out_stream))
}

fn downmix<T: Copy>(d: &[T], ch: usize, conv: impl Fn(T) -> f32) -> Vec<f32> {
    if ch <= 1 {
        return d.iter().map(|&x| conv(x)).collect();
    }
    d.chunks(ch)
        .map(|f| f.iter().map(|&x| conv(x)).sum::<f32>() / ch as f32)
        .collect()
}

/// Streaming linear resampler.
struct Linear {
    base: f64,
    step: f64,
    t: f64,
    prev: f32,
}

impl Linear {
    fn new(from: u32, to: u32) -> Self {
        let step = from as f64 / to as f64;
        Linear { base: step, step, t: 0.0, prev: 0.0 }
    }

    /// Playback speed factor (1.0 = nominal).
    fn set_speed(&mut self, speed: f64) {
        self.step = self.base * speed;
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        if self.step == 1.0 && self.t == 0.0 {
            out.extend_from_slice(input);
            self.prev = input[input.len() - 1];
            return;
        }
        // Sample positions: -1 is `prev`, 0.. are `input`.
        let last = input.len() as f64 - 1.0;
        while self.t < last {
            let i = self.t.floor();
            let frac = (self.t - i) as f32;
            let i = i as isize;
            let a = if i < 0 { self.prev } else { input[i as usize] };
            let b = input[(i + 1) as usize];
            out.push(a + (b - a) * frac);
            self.t += self.step;
        }
        self.t -= input.len() as f64;
        self.prev = input[input.len() - 1];
    }
}

/// Network simulator: the real Opus encoder/decoder and jitter buffer with a
/// synthetic link between them. Measures dropouts the listener would hear.
#[cfg(test)]
pub(crate) mod sim {
    use super::*;

    pub struct Net {
        pub name: &'static str,
        pub base_ms: f64,
        /// Mean of exponential per-packet jitter.
        pub jitter_ms: f64,
        pub loss: f64,
        /// Every `stall_every_s` the link freezes for `stall_ms` (Wi-Fi scan,
        /// power save, relay TCP retransmit), then delivers everything at once.
        pub stall_every_s: f64,
        pub stall_ms: f64,
        /// Sender clock relative to receiver (0.002 = 0.2 % faster).
        pub drift: f64,
        /// Peak speech level (linear, 1.0 = 0 dBFS).
        pub amp: f32,
        /// Background noise RMS.
        pub noise: f32,
        /// Talk 2 s, pause 1.5 s (instead of talking continuously).
        pub pauses: bool,
    }

    #[derive(Debug, Default)]
    pub struct Report {
        pub sent_pct: f64,
        pub dropouts: u32,
        pub silent_ms: u32,
        pub stats: PeerStats,
    }

    struct Rng(u64);
    impl Rng {
        fn f(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    pub fn run(net: &Net, secs: f64) -> Report {
        // Network simulation: the synthetic tone is not run through RNNoise.
        let shared = |_| Arc::new(Shared::new(false, false, Prefs { noise_suppression: false, sensitivity: None }));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut capture = capture_pipeline(shared(0), tx, RATE).unwrap();
        let rx_voice = Voice { shared: shared(1), stop: Arc::new(AtomicBool::new(false)) };
        let mut mix = mixer(rx_voice.shared.clone(), RATE);
        let from = iroh::SecretKey::from_bytes(&[7; 32]).public();
        let mut rng = Rng(0x9e3779b97f4a7c15);

        // Sender: one 20 ms frame per (20 ms * (1 - drift)) of receiver time.
        // Speech model: 220 Hz voice, ~4 syllables/s, phrase loudness varying
        // by up to -14 dB every 400 ms (continuous talking, no long pauses).
        let frames = (secs * 50.0) as usize;
        let mut arrivals: Vec<(f64, Bytes)> = Vec::new();
        let mut sent = 0;
        let mut phrase = 1.0f32;
        let talking = |k: usize| !net.pauses || (k * 20) % 3500 < 2000;
        let mut noise_rng = Rng(0x2545f4914f6cdd1d);
        for k in 0..frames {
            let t = k as f64 * 20.0 * (1.0 - net.drift);
            if k % 20 == 0 {
                phrase = 0.2 + 0.8 * rng.f() as f32;
            }
            let on = if talking(k) { 1.0 } else { 0.0 };
            let frame: Vec<f32> = (0..FRAME)
                .map(|i| {
                    let x = (k * FRAME + i) as f32 / RATE as f32;
                    let syllable = 0.15 + 0.85 * (x * std::f32::consts::PI * 4.0).sin().abs();
                    let n = net.noise * 1.73 * (2.0 * noise_rng.f() as f32 - 1.0);
                    on * net.amp * phrase * syllable * (x * 2.0 * std::f32::consts::PI * 220.0).sin() + n
                })
                .collect();
            capture(&frame);
            while let Ok(pkt) = rx.try_recv() {
                sent += 1;
                if rng.f() < net.loss {
                    continue;
                }
                let mut at = t + net.base_ms - net.jitter_ms * rng.f().max(1e-9).ln();
                if net.stall_every_s > 0.0 {
                    let period = net.stall_every_s * 1000.0;
                    let phase = at % period;
                    if phase < net.stall_ms {
                        at += net.stall_ms - phase;
                    }
                }
                arrivals.push((at, pkt));
            }
        }
        arrivals.sort_by(|a, b| a.0.total_cmp(&b.0));

        // Receiver: audio callback every 10 ms.
        let mut rep = Report { sent_pct: 100.0 * sent as f64 / frames as f64, ..Default::default() };
        let mut next = 0;
        let mut in_gap = false;
        let end = frames as f64 * 20.0;
        let mut t = 0.0;
        while t < end {
            while next < arrivals.len() && arrivals[next].0 <= t {
                rx_voice.receive(from, &arrivals[next].1);
                next += 1;
            }
            let out = mix(480);
            // Audio is expected if the sender talked during [t-400, t-30] ms
            // (covers any buffer depth up to 370 ms).
            let k_at = |ms: f64| (ms / (20.0 * (1.0 - net.drift))) as usize;
            let expected = (k_at(t - 400.0)..=k_at(t - 30.0)).all(|k| talking(k));
            // Skip start-up and the last 0.5 s (a faster sender finishes early).
            if t > 1000.0 && t < end - 500.0 && expected {
                // Dropout: >= 2 ms of digital silence inside this callback.
                let mut run = 0;
                let mut max = 0;
                for s in &out {
                    run = if *s == 0.0 { run + 1 } else { 0 };
                    max = max.max(run);
                }
                let silent = max >= 96;
                if silent {
                    rep.silent_ms += 10;
                    if !in_gap {
                        rep.dropouts += 1;
                    }
                }
                in_gap = silent;
            }
            t += 10.0;
        }
        if let Some((_, st)) = rx_voice.stats().into_iter().next() {
            rep.stats = st;
        }
        rep
    }

    pub fn scenarios() -> Vec<Net> {
        let n = |name, base_ms, jitter_ms, loss, stall_every_s, stall_ms, drift, amp| Net {
            name, base_ms, jitter_ms, loss, stall_every_s, stall_ms, drift, amp,
            noise: 0.0003, pauses: false,
        };
        let talk = |name, amp, noise| Net { pauses: true, noise, ..n(name, 25.0, 8.0, 0.01, 0.0, 0.0, 0.0, amp) };
        vec![
            n("idealna sieć", 20.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.3),
            n("LAN", 2.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.3),
            n("Wi-Fi: jitter 10 ms", 25.0, 10.0, 0.0, 0.0, 0.0, 0.0, 0.3),
            n("Wi-Fi: jitter 10 ms + 1% utraty", 25.0, 10.0, 0.01, 0.0, 0.0, 0.0, 0.3),
            n("Wi-Fi: przycięcia 120 ms co 3 s", 25.0, 5.0, 0.0, 3.0, 120.0, 0.0, 0.3),
            n("relay: jitter 25 ms", 45.0, 25.0, 0.0, 0.0, 0.0, 0.0, 0.3),
            n("internet: 3% utraty", 30.0, 5.0, 0.03, 0.0, 0.0, 0.0, 0.3),
            n("zegar nadawcy +0,3%", 20.0, 2.0, 0.0, 0.0, 0.0, 0.003, 0.3),
            n("zegar nadawcy -0,3%", 20.0, 2.0, 0.0, 0.0, 0.0, -0.003, 0.3),
            n("mikrofon: szczyty -20 dBFS", 20.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.1),
            n("mikrofon: szczyty -26 dBFS", 20.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.05),
            n("mikrofon: szczyty -32 dBFS", 20.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.025),
            talk("rozmowa z pauzami, cicho w tle", 0.1, 0.0003),
            talk("rozmowa z pauzami, wentylator -45 dB", 0.1, 0.0056),
            talk("rozmowa z pauzami, cichy mikrofon", 0.025, 0.0003),
        ]
    }

    /// Every scenario must be free of audible gaps.
    #[test]
    fn no_dropouts() {
        for net in scenarios() {
            let r = run(&net, 30.0);
            assert_eq!(r.dropouts, 0, "{}: {:?}", net.name, r);
            if !net.pauses {
                assert!(r.sent_pct > 99.0, "{}: {:?}", net.name, r);
            }
        }
    }

    #[test]
    fn report() {
        println!(
            "{:<36} {:>8} {:>8} {:>10} {:>5} {:>5} {:>7} {:>9}",
            "scenariusz (60 s mowy)", "wysłane", "przerwy", "cisza[ms]", "FEC", "PLC", "pominięte", "bufor[ms]"
        );
        for net in scenarios() {
            let r = run(&net, 60.0);
            let st = r.stats;
            println!(
                "{:<36} {:>7.0}% {:>8} {:>10} {:>5} {:>5} {:>7} {:>9}",
                net.name, r.sent_pct, r.dropouts, r.silent_ms, st.fec, st.concealed, st.dropped, st.depth_ms
            );
            if std::env::var_os("SIM_DEBUG").is_some() {
                println!("    {st:?}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 40 frames (0.8 s) of a voiced sound with rising pitch, encoded as in
    /// `capture_pipeline`; PLC (which repeats the past) can't fake it.
    fn glide_packets() -> Vec<Vec<u8>> {
        let mut enc = opus::Encoder::new(RATE, opus::Channels::Mono, opus::Application::Voip).unwrap();
        enc.set_bitrate(opus::Bitrate::Bits(48_000)).unwrap();
        enc.set_inband_fec(true).unwrap();
        enc.set_packet_loss_perc(10).unwrap();
        let mut phase = 0f32;
        (0..40)
            .map(|k| {
                let frame: Vec<f32> = (0..FRAME)
                    .map(|i| {
                        let t = (k * FRAME + i) as f32 / RATE as f32;
                        phase += 2.0 * std::f32::consts::PI * (150.0 + 500.0 * t) / RATE as f32;
                        0.3 * (phase.sin() + 0.5 * (2.0 * phase).sin() + 0.25 * (3.0 * phase).sin()) / 1.75
                    })
                    .collect();
                let mut out = [0u8; 1500];
                let n = enc.encode_float(&frame, &mut out).unwrap();
                out[..n].to_vec()
            })
            .collect()
    }

    /// Plays `pkts` through the receive path in real time (one packet in,
    /// 20 ms out) with a jitter buffer `depth` frames deep. `lost` never
    /// arrive, `early` arrive one tick early. Returns 20 ms output windows,
    /// frame `k` of the stream in window `k`.
    fn play(pkts: &[Vec<u8>], lost: &[usize], early: &[usize], depth: u64) -> (Vec<Vec<f32>>, PeerStats) {
        let mut p = PeerAudio::new(RATE).unwrap();
        p.depth = depth;
        let mut out = Vec::new();
        for k in 0..pkts.len() {
            for j in [k, k + 1] {
                if j < pkts.len() && !lost.contains(&j) && early.contains(&j) == (j > k) {
                    p.insert(j as u16, FLAG_SPEAKING, &pkts[j]);
                }
            }
            let mut w = vec![0f32; FRAME];
            p.mix_into(&mut w);
            out.push(w);
        }
        // Drift correction never kicked in, so samples passed through the
        // resampler unshifted and windows of different runs line up.
        assert!(p.resample.step == 1.0 && p.resample.t == 0.0);
        out.drain(..depth as usize - 1); // playback starts once `depth` frames are in
        (out, p.stats)
    }

    fn err(a: &[f32], b: &[f32]) -> f32 {
        rms(&a.iter().zip(b).map(|(x, y)| x - y).collect::<Vec<_>>())
    }

    /// A lost packet is rebuilt from the next one's FEC: both that frame and
    /// the next sound like the lossless stream.
    #[test]
    fn lost_frame_recovered_by_fec() {
        let pkts = glide_packets();
        let (truth, _) = play(&pkts, &[], &[], START_DEPTH);
        let (got, stats) = play(&pkts, &[20], &[], START_DEPTH);
        assert_eq!((stats.fec, stats.concealed), (1, 0));
        // Frame RMS is ~0.14; a 120 ms request was off by ~0.12 / ~0.05.
        for k in [20, 21] {
            let e = err(&got[k], &truth[k]);
            assert!(e < 0.02, "frame {k}: {e}");
        }
        for k in (0..20).chain(22..got.len()) {
            assert!(err(&got[k], &truth[k]) < 0.005, "frame {k}");
        }
    }

    /// Concealment asks libopus for exactly the missing 20 ms, in both the
    /// "lost" and the underrun path: output matches a plain decoder doing so.
    #[test]
    fn lost_frame_concealed_one_frame() {
        let pkts = glide_packets();
        // (lost, early, depth): 20 concealed (next one missing too), 21 from
        // FEC; then 20 concealed as an underrun.
        for (lost, early, depth) in [(&[20, 21][..], &[22][..], START_DEPTH), (&[20][..], &[][..], 1)] {
            let (got, stats) = play(&pkts, lost, early, depth);
            assert_eq!(stats.concealed, 1, "{lost:?}");
            let mut dec = opus::Decoder::new(RATE, opus::Channels::Mono).unwrap();
            for (k, w) in got.iter().enumerate() {
                let mut pcm = [0f32; FRAME];
                let (data, fec): (&[u8], bool) = match (lost.contains(&k), lost.contains(&(k + 1))) {
                    (false, _) => (&pkts[k], false),
                    (true, false) if k + 1 < pkts.len() && stats.fec > 0 => (&pkts[k + 1], true),
                    (true, _) => (&[], false),
                };
                assert_eq!(dec.decode_float(data, &mut pcm, fec).unwrap(), FRAME);
                // Same libopus and inputs on both sides, so equal; a 120 ms
                // request threw the frame after the gap off by ~0.04-0.09.
                let e = err(w, &pcm);
                assert!(e < 1e-3, "{lost:?} frame {k}: {e}");
            }
        }
    }

    #[test]
    fn prefs_roundtrip() {
        assert_eq!(Prefs::parse(""), Prefs::default());
        for p in [Prefs::default(), Prefs { noise_suppression: false, sensitivity: Some(-42) }] {
            assert_eq!(Prefs::parse(&p.format()), p);
        }
        assert_eq!(Prefs::parse("sensitivity=-200").sensitivity, Some(crate::mic::DB_MIN));
        assert_eq!(Prefs::parse("sensitivity=auto").sensitivity, None);
    }

    /// A manual threshold gates on level alone: -30 dBFS speech passes a
    /// -40 dB threshold and is blocked by a -20 dB one.
    #[test]
    fn manual_threshold_gates() {
        let sent = |threshold| {
            let shared = Arc::new(Shared::new(false, false, Prefs { noise_suppression: false, sensitivity: Some(threshold) }));
            let (tx, mut rx) = mpsc::unbounded_channel();
            let mut capture = capture_pipeline(shared.clone(), tx, RATE).unwrap();
            for k in 0..50 {
                let frame: Vec<f32> = (0..FRAME)
                    .map(|i| 0.0447 * ((k * FRAME + i) as f32 / RATE as f32 * 2.0 * std::f32::consts::PI * 220.0).sin())
                    .collect();
                capture(&frame);
            }
            let level = f32::from_bits(shared.mic_level.load(Ordering::Relaxed));
            assert!((level + 30.0).abs() < 0.5, "{level}");
            std::iter::from_fn(|| rx.try_recv().ok()).count()
        };
        assert!(sent(-40) >= 49);
        assert_eq!(sent(-20), 0);
    }

    #[test]
    fn resampler_keeps_rate() {
        let mut r = Linear::new(44_100, 48_000);
        let mut out = Vec::new();
        for _ in 0..100 {
            r.process(&[0.5; 441], &mut out);
        }
        assert!((out.len() as i64 - 48_000).abs() < 3, "{}", out.len());
        assert!(out[10..].iter().all(|s| (s - 0.5).abs() < 1e-6));
    }
}
