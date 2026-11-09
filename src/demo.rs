//! Scripted runs for automated testing and screenshots, driven by env vars:
//! `BIBICORD_DEMO_NAME`, `BIBICORD_DEMO_ROOM`, `BIBICORD_DEMO_CREATE=<file>`
//! (writes invite), `BIBICORD_DEMO_JOIN=<file>` (reads invite),
//! `BIBICORD_DEMO_CHAT=a|b`, `BIBICORD_DEMO_FILES=<file>|<file>` (sent after chat), `BIBICORD_DEMO_DROP=1` (file drop overlay),
//! `BIBICORD_DEMO_SAVED=1` (first chat file shown as saved), `BIBICORD_DEMO_STREAM=1`, `BIBICORD_DEMO_WATCH=1`,
//! `BIBICORD_DEMO_VOICE=1`, `BIBICORD_DEMO_CAMERA=1`, `BIBICORD_DEMO_MUTE=1`, `BIBICORD_DEMO_VIEW=voice`,
//! `BIBICORD_DEMO_STAGE_CHAT=1`, `BIBICORD_DEMO_DIALOG=golive|settings|invite|update|share|profile`,
//! `BIBICORD_DEMO_AVATAR=<image>` (not saved),
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

pub fn save_png(path: &std::path::Path, img: &eframe::egui::ColorImage) {
    let [w, h] = img.size;
    let bytes: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
    if let Some(buf) = image::RgbaImage::from_raw(w as u32, h as u32, bytes) {
        if let Err(e) = buf.save(path) {
            tracing::warn!("screenshot: {e}");
        }
    }
}
