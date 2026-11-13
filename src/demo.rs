//! Scripted runs for automated testing and screenshots, driven by env vars:
//! `BIBICORD_DEMO_NAME`, `BIBICORD_DEMO_ROOM`, `BIBICORD_DEMO_CREATE=<file>`
//! (writes invite), `BIBICORD_DEMO_JOIN=<file>` (reads invite),
//! `BIBICORD_DEMO_CHAT=a|b`, `BIBICORD_DEMO_FILES=<file>|<file>` (sent after chat), `BIBICORD_DEMO_DROP=1` (file drop overlay),
//! `BIBICORD_DEMO_SAVED=1` (first chat file shown as saved), `BIBICORD_DEMO_STREAM=1`, `BIBICORD_DEMO_WATCH=1`,
//! `BIBICORD_DEMO_VOICE=1`, `BIBICORD_DEMO_CAMERA=1`, `BIBICORD_DEMO_MUTE=1`, `BIBICORD_DEMO_VIEW=voice`,
//! `BIBICORD_DEMO_STAGE_CHAT=1`, `BIBICORD_DEMO_DIALOG=golive|settings|invite|update|share|profile`,
//! `BIBICORD_DEMO_AVATAR=<image>` (not saved),
//! `BIBICORD_DEMO_HISTORY=<n>` (n generated chat entries, files of 1 MB among
//! them: past the history limit the oldest show as no longer available),
//! `BIBICORD_DEMO_SCROLL=top|<px>` (chat scrolled to its start or that far
//! down, once), `BIBICORD_DEMO_TICK=<ms>` (a generated message that often),
//! `BIBICORD_DEMO_TIMING=1` (logs how long drawing the chat history takes),
//! `BIBICORD_DEMO_SHOT=<png>` + `BIBICORD_DEMO_SHOT_AFTER=<secs>`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

pub struct Demo {
    pub name: Option<String>,
    pub avatar: Option<PathBuf>,
    pub room: Option<String>,
    pub create: Option<PathBuf>,
    pub join: Option<PathBuf>,
    pub chat: Vec<String>,
    pub files: Vec<PathBuf>,
    pub stream: bool,
    pub watch: bool,
    pub voice: bool,
    pub camera: bool,
    pub mute: bool,
    pub view_voice: bool,
    pub stage_chat: bool,
    pub dialog: Option<String>,
    pub history: usize,
    pub scroll: Scroll,
    pub tick: Option<(Duration, Instant, usize)>,
    pub timing: bool,
    /// Drawing times of the chat history, logged in batches.
    pub samples: Vec<Duration>,
    pub shot: Option<PathBuf>,
    pub shot_at: Instant,
    pub shot_requested: bool,
    pub ticket_written: bool,
    pub chat_sent: bool,
    pub stream_started: bool,
    pub camera_started: bool,
    pub setup_done: bool,
    pub dialog_opened: bool,
}

impl Demo {
    pub fn from_env() -> Option<Demo> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let name = var("BIBICORD_DEMO_NAME")?;
        let after = var("BIBICORD_DEMO_SHOT_AFTER")
            .and_then(|s| s.parse().ok())
            .unwrap_or(8.0);
        Some(Demo {
            name: Some(name),
            avatar: var("BIBICORD_DEMO_AVATAR").map(PathBuf::from),
            room: var("BIBICORD_DEMO_ROOM"),
            create: var("BIBICORD_DEMO_CREATE").map(PathBuf::from),
            join: var("BIBICORD_DEMO_JOIN").map(PathBuf::from),
            chat: var("BIBICORD_DEMO_CHAT")
                .map(|s| s.split('|').map(String::from).collect())
                .unwrap_or_default(),
            files: var("BIBICORD_DEMO_FILES")
                .map(|s| s.split('|').map(PathBuf::from).collect())
                .unwrap_or_default(),
            stream: var("BIBICORD_DEMO_STREAM").is_some(),
            watch: var("BIBICORD_DEMO_WATCH").is_some(),
            voice: var("BIBICORD_DEMO_VOICE").is_some(),
            camera: var("BIBICORD_DEMO_CAMERA").is_some(),
            mute: var("BIBICORD_DEMO_MUTE").is_some(),
            view_voice: var("BIBICORD_DEMO_VIEW").as_deref() == Some("voice"),
            stage_chat: var("BIBICORD_DEMO_STAGE_CHAT").is_some(),
            dialog: var("BIBICORD_DEMO_DIALOG"),
            history: var("BIBICORD_DEMO_HISTORY").and_then(|s| s.parse().ok()).unwrap_or(0),
            scroll: Scroll {
                offset: var("BIBICORD_DEMO_SCROLL").and_then(|s| if s == "top" { Some(0.0) } else { s.parse().ok() }),
                frames: 0,
            },
            tick: var("BIBICORD_DEMO_TICK")
                .and_then(|s| s.parse().ok())
                .map(|ms| (Duration::from_millis(ms), Instant::now(), 0)),
            timing: var("BIBICORD_DEMO_TIMING").is_some(),
            samples: Vec::new(),
            shot: var("BIBICORD_DEMO_SHOT").map(PathBuf::from),
            shot_at: Instant::now() + Duration::from_secs_f32(after),
            shot_requested: false,
            ticket_written: false,
            chat_sent: false,
            stream_started: false,
            camera_started: false,
            setup_done: false,
            dialog_opened: false,
        })
    }
}

/// `n` made-up chat entries, oldest first, ending now: a few people
/// talking in runs, messages of all lengths, a 1 MB file every 20th entry (from the 3rd)
/// and an occasional system line. `me` posts as `None`.
pub fn history(n: usize, me: &str) -> Vec<(crate::ui::ChatKind, chrono::DateTime<chrono::Local>)> {
    use crate::ui::ChatKind;
    let people: Vec<(Option<iroh::EndpointId>, String)> = std::iter::once((None, me.to_string()))
        .chain(["Ola", "Bartek", "Kasia"].iter().enumerate().map(|(k, name)| {
            (Some(iroh::SecretKey::from_bytes(&[k as u8 + 1; 32]).public()), name.to_string())
        }))
        .collect();
    let words = "no to może jutro wieczorem gramy dalej, ale najpierw muszę skończyć ten raport bo szef pyta                  a w ogóle widzieliście nowy odcinek? serio polecam, tylko bez spoilerów proszę";
    let words: Vec<&str> = words.split_whitespace().collect();
    let now = chrono::Local::now();
    let mut rng = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let mut who = 0;
    (0..n)
        .map(|i| {
            let time = now - chrono::Duration::seconds(((n - i) * 40) as i64);
            if next() % 6 == 0 {
                who = (next() % people.len() as u64) as usize;
            }
            let (from, name) = people[who].clone();
            let kind = if i % 20 == 2 {
                let file_name = format!("zdjęcie-{:04}.jpg", i + 1);
                let data = vec![(i % 251) as u8; 1_000_000];
                ChatKind::File { from, name, file_name, size: data.len(), data: Some(std::sync::Arc::new(data)), saved: false }
            } else if i % 97 == 50 {
                ChatKind::System { icon: egui_phosphor::regular::ARROW_RIGHT, who: name, text: "dołącza do pokoju.".into(), good: true }
            } else {
                let len = match next() % 10 {
                    0 => 60 + next() % 60,
                    1..=3 => 12 + next() % 20,
                    _ => 2 + next() % 8,
                };
                let text: Vec<&str> = (0..len).map(|_| words[(next() % words.len() as u64) as usize]).collect();
                ChatKind::Message { from, name, text: format!("#{} {}", i + 1, text.join(" ")) }
            };
            (kind, time)
        })
        .collect()
}

/// A scroll position set for the first chat frames only (until the
/// content size is known), then left to the user.
pub struct Scroll {
    offset: Option<f32>,
    frames: u32,
}

impl Scroll {
    pub fn take_frame(&mut self) -> Option<f32> {
        self.frames += 1;
        self.offset.filter(|_| self.frames <= 3)
    }
}

impl Demo {
    /// Records one drawing of `entries` chat entries; logs every 100.
    pub fn sample(&mut self, entries: usize, took: Duration) {
        self.samples.push(took);
        if self.samples.len() == 100 {
            self.samples.sort();
            let mean = self.samples.iter().sum::<Duration>() / 100;
            tracing::info!(
                "chat history, {entries} entries: median {:?}, mean {mean:?}, max {:?}",
                self.samples[50],
                self.samples[99]
            );
            self.samples.clear();
        }
    }
}

pub fn save_png(path: &std::path::Path, img: &eframe::egui::ColorImage) {
    let [w, h] = img.size;
    let bytes: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
    if let Some(buf) = image::RgbaImage::from_raw(w as u32, h as u32, bytes) {
        if let Err(e) = buf.save(path) {
            tracing::warn!("screenshot: {e}");
        }
    }
}
