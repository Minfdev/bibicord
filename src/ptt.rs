//! Push-to-talk key: global (works while BibiCord is in the background,
//! e.g. during a game) and passive.
//!
//! The selected key or mouse button is polled every `POLL` instead of
//! hooking the input stream: nothing is intercepted, so the key keeps
//! working in other programs, there is no hook that the system could drop
//! after a slow callback, and a missed "release" event cannot leave the
//! microphone open.
//!
//! - Windows: `GetAsyncKeyState` (physical key/button state). A game running
//!   as administrator hides its input from a non-elevated BibiCord (UIPI),
//!   same as in Discord.
//! - macOS: `CGEventSourceKeyState` / `CGEventSourceButtonState` on the HID
//!   state; reading keys needs the "Input Monitoring" permission.
//!
//! The poller runs only while it is needed (push-to-talk in a call, or
//! while the settings wait for a new key).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

const POLL: Duration = Duration::from_millis(10);

/// The push-to-talk key is held right now (read by the voice pipeline).
pub static HELD: AtomicBool = AtomicBool::new(false);

/// A key or mouse button. `Key` holds the platform key code (Windows
/// virtual-key code, macOS `kVK_*`); `Mouse` the button number as users
/// know it: 3 = middle, 4 = back, 5 = forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binding {
    Key(u16),
    Mouse(u8),
}

impl Binding {
    /// Stored form in `voice.txt`: `key:86`, `mouse:4`.
    pub fn format(self) -> String {
        match self {
            Binding::Key(k) => format!("key:{k}"),
            Binding::Mouse(b) => format!("mouse:{b}"),
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        let (kind, n) = s.trim().split_once(':')?;
        match kind {
            // Tab moves keyboard focus; it cannot be the push-to-talk key.
            "key" => n.parse().ok().filter(|k| *k != sys::TAB).map(Binding::Key),
            "mouse" => n.parse().ok().filter(|b| *b >= 3).map(Binding::Mouse),
            _ => None,
        }
    }

    /// Name shown in the settings.
    pub fn name(self) -> String {
        match self {
            Binding::Key(k) => sys::key_name(k),
            Binding::Mouse(3) => "Środkowy przycisk myszy".into(),
            Binding::Mouse(4) => "Przycisk myszy 4 (wstecz)".into(),
            Binding::Mouse(5) => "Przycisk myszy 5 (dalej)".into(),
            Binding::Mouse(b) => format!("Przycisk myszy {b}"),
        }
    }

    fn encode(b: Option<Self>) -> u32 {
        match b {
            None => 0,
            Some(Binding::Key(k)) => 0x1_0000 | k as u32,
            Some(Binding::Mouse(m)) => 0x2_0000 | m as u32,
        }
    }

    fn decode(v: u32) -> Option<Self> {
        match v >> 16 {
            1 => Some(Binding::Key(v as u16)),
            2 => Some(Binding::Mouse(v as u8)),
            _ => None,
        }
    }
}

/// Global key reading is possible (macOS: Input Monitoring granted).
pub fn has_permission() -> bool {
    sys::has_permission()
}

/// Asks the system for the permission (macOS shows its prompt once and
/// adds BibiCord to the Input Monitoring list).
pub fn request_permission() {
    sys::request_permission()
}

/// Opens the system settings page where the permission is granted.
pub fn open_permission_settings() {
    sys::open_permission_settings()
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Capture {
    Idle,
    Waiting,
    /// `None` = cancelled with Escape.
    Done(Option<Binding>),
}

struct Inner {
    binding: AtomicU32,
    /// Report the key state (push-to-talk mode in a call).
    active: AtomicBool,
    capture: Mutex<Capture>,
    on_change: Box<dyn Fn(bool) + Send + Sync>,
}

/// Owns the polling thread. `on_change` runs on that thread whenever the
/// key is pressed or released (used for the sound cue).
pub struct Listener {
    inner: Arc<Inner>,
    thread: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
}

impl Listener {
    pub fn new(on_change: impl Fn(bool) + Send + Sync + 'static) -> Self {
        Listener {
            inner: Arc::new(Inner {
                binding: AtomicU32::new(0),
                active: AtomicBool::new(false),
                capture: Mutex::new(Capture::Idle),
                on_change: Box::new(on_change),
            }),
            thread: None,
        }
    }

    /// Sets the key and whether push-to-talk is in use; starts or stops the
    /// poller as needed. Cheap, meant to be called every frame.
    pub fn sync(&mut self, binding: Option<Binding>, active: bool) {
        self.inner.binding.store(Binding::encode(binding), Ordering::Relaxed);
        self.inner.active.store(active, Ordering::Relaxed);
        let needed = active || self.capturing();
        match (&self.thread, needed) {
            (None, true) => {
                let stop = Arc::new(AtomicBool::new(false));
                let (inner, s) = (self.inner.clone(), stop.clone());
                match std::thread::Builder::new().name("ptt".into()).spawn(move || run(inner, s)) {
                    Ok(h) => self.thread = Some((stop, h)),
                    Err(e) => tracing::warn!("ptt: {e}"),
                }
            }
            (Some(_), false) => self.stop(),
            _ => {}
        }
    }

    fn stop(&mut self) {
        if let Some((stop, h)) = self.thread.take() {
            stop.store(true, Ordering::Relaxed);
            let _ = h.join();
        }
    }

    /// Waits for the next key or mouse button (left/right mouse buttons are
    /// ignored, Escape cancels). Keys already held do not count.
    pub fn start_capture(&mut self) {
        *self.inner.capture.lock().unwrap() = Capture::Waiting;
    }

    pub fn cancel_capture(&mut self) {
        *self.inner.capture.lock().unwrap() = Capture::Idle;
    }

    pub fn capturing(&self) -> bool {
        *self.inner.capture.lock().unwrap() == Capture::Waiting
    }

    /// The captured key: `Some(None)` when the capture was cancelled.
    pub fn take_captured(&mut self) -> Option<Option<Binding>> {
        let mut c = self.inner.capture.lock().unwrap();
        match *c {
            Capture::Done(b) => {
                *c = Capture::Idle;
                Some(b)
            }
            _ => None,
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Picks the key being captured from successive polls.
///
/// The first newly pressed key or mouse button wins (Escape cancels). A
/// modifier (Shift, Ctrl, Alt, Windows/Command) is taken only when it is
/// released on its own, so Shift+Tab used to move keyboard focus binds
/// nothing. Tab itself is never bound.
#[derive(Default)]
struct Capturer {
    /// Inputs down at the previous poll; `None` before the first one, so
    /// keys already held when the capture started do not count.
    before: Option<Vec<Binding>>,
    pending: Option<Binding>,
    /// Tab was pressed while `pending` was held.
    spoiled: bool,
}

impl Capturer {
    /// `down`: bindable inputs held now; `tab`: Tab held now. Returns the
    /// result when the capture ends (`Some(None)` = cancelled).
    fn step(&mut self, down: Vec<Binding>, tab: bool) -> Option<Option<Binding>> {
        let before = self.before.replace(down.clone())?;
        let pressed = down.iter().filter(|b| !before.contains(b)).copied();
        let (mods, keys): (Vec<Binding>, Vec<Binding>) = pressed.partition(|b| sys::is_modifier(*b));
        if let Some(b) = sys::pick(&keys) {
            return Some((b != sys::CANCEL).then_some(b));
        }
        if !mods.is_empty() {
            let all: Vec<Binding> = self.pending.into_iter().chain(mods).collect();
            self.pending = sys::pick(&all);
        }
        // After the update: Shift and Tab may show up in the same poll.
        if tab && self.pending.is_some() {
            self.spoiled = true;
        }
        if let Some(p) = self.pending.filter(|p| !down.contains(p)) {
            self.pending = None;
            if !std::mem::take(&mut self.spoiled) {
                return Some(Some(p));
            }
        }
        None
    }
}

fn run(inner: Arc<Inner>, stop: Arc<AtomicBool>) {
    let mut held = false;
    let mut capturer = Capturer::default();
    while !stop.load(Ordering::Relaxed) {
        if *inner.capture.lock().unwrap() == Capture::Waiting {
            let down: Vec<Binding> = sys::candidates().filter(|b| sys::is_down(*b)).collect();
            if let Some(result) = capturer.step(down, sys::is_down(Binding::Key(sys::TAB))) {
                let mut c = inner.capture.lock().unwrap();
                if *c == Capture::Waiting {
                    *c = Capture::Done(result);
                }
                capturer = Capturer::default();
            }
        } else {
            capturer = Capturer::default();
        }

        let now = inner.active.load(Ordering::Relaxed)
            && Binding::decode(inner.binding.load(Ordering::Relaxed)).is_some_and(sys::is_down);
        if now != held {
            held = now;
            HELD.store(now, Ordering::Relaxed);
            (inner.on_change)(now);
        }
        std::thread::sleep(POLL);
    }
    if held {
        HELD.store(false, Ordering::Relaxed);
        (inner.on_change)(false);
    }
}

// ------------------------------------------------------------------ Windows

#[cfg(windows)]
mod sys {
    use super::Binding;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;

    pub const CANCEL: Binding = Binding::Key(VK_ESCAPE);
    pub const TAB: u16 = VK_TAB;

    pub fn has_permission() -> bool {
        true
    }

    /// Side-specific Shift, Ctrl, Alt and the Windows keys.
    pub fn is_modifier(b: Binding) -> bool {
        matches!(b, Binding::Key(VK_LSHIFT..=VK_RMENU | VK_LWIN | VK_RWIN))
    }

    pub fn request_permission() {}

    pub fn open_permission_settings() {}

    /// Several inputs went down in one poll. AltGr (Polish and other
    /// layouts) reports left Ctrl together with right Alt: bind right Alt.
    pub fn pick(pressed: &[Binding]) -> Option<Binding> {
        let altgr = [Binding::Key(VK_LCONTROL), Binding::Key(VK_RMENU)];
        if altgr.iter().all(|k| pressed.contains(k)) {
            return Some(Binding::Key(VK_RMENU));
        }
        pressed.first().copied()
    }

    fn mouse_vk(b: u8) -> Option<u16> {
        match b {
            3 => Some(VK_MBUTTON),
            4 => Some(VK_XBUTTON1),
            5 => Some(VK_XBUTTON2),
            _ => None,
        }
    }

    pub fn is_down(b: Binding) -> bool {
        let vk = match b {
            Binding::Key(k) => Some(k),
            Binding::Mouse(m) => mouse_vk(m),
        };
        // SAFETY: plain Win32 call without pointers.
        vk.is_some_and(|vk| unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000 != 0)
    }

    /// Everything that can be bound: keys (side-specific modifiers instead
    /// of the generic Shift/Ctrl/Alt; no Tab) and the middle and side mouse
    /// buttons.
    pub fn candidates() -> impl Iterator<Item = Binding> {
        let generic = [VK_SHIFT, VK_CONTROL, VK_MENU, VK_PACKET, VK_TAB];
        (0x08u16..0xFF)
            .filter(move |vk| !generic.contains(vk))
            .map(Binding::Key)
            .chain((3..=5).map(Binding::Mouse))
    }

    pub fn key_name(vk: u16) -> String {
        let fixed = match vk {
            VK_LSHIFT => "Lewy Shift",
            VK_RSHIFT => "Prawy Shift",
            VK_LCONTROL => "Lewy Ctrl",
            VK_RCONTROL => "Prawy Ctrl",
            VK_LMENU => "Lewy Alt",
            VK_RMENU => "Prawy Alt",
            VK_LWIN => "Lewy Windows",
            VK_RWIN => "Prawy Windows",
            VK_SPACE => "Spacja",
            VK_CAPITAL => "Caps Lock",
            VK_TAB => "Tab",
            VK_RETURN => "Enter",
            VK_BACK => "Backspace",
            VK_LEFT => "Strzałka w lewo",
            VK_RIGHT => "Strzałka w prawo",
            VK_UP => "Strzałka w górę",
            VK_DOWN => "Strzałka w dół",
            VK_INSERT => "Insert",
            VK_DELETE => "Delete",
            VK_HOME => "Home",
            VK_END => "End",
            VK_PRIOR => "Page Up",
            VK_NEXT => "Page Down",
            _ => "",
        };
        if !fixed.is_empty() {
            return fixed.into();
        }
        if (VK_F1..=VK_F24).contains(&vk) {
            return format!("F{}", vk - VK_F1 + 1);
        }
        // Name from the keyboard layout (letters as printed on the key).
        // SAFETY: the buffer outlives the call and its length is passed.
        let sc = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC_EX) };
        if sc != 0 {
            let ext = if sc >> 8 == 0xE0 || sc >> 8 == 0xE1 { 1 << 24 } else { 0 };
            let lparam = (((sc & 0xFF) << 16) | ext) as i32;
            let mut buf = [0u16; 64];
            let n = unsafe { GetKeyNameTextW(lparam, buf.as_mut_ptr(), buf.len() as i32) };
            if n > 0 {
                let s = String::from_utf16_lossy(&buf[..n as usize]);
                // Layouts report letters in capitals ("V"); keep them so,
                // and title-case the rest ("NUM 5" -> "Num 5").
                if s.chars().count() == 1 {
                    return s;
                }
                return title_case(&s);
            }
        }
        format!("Klawisz {vk:#04X}")
    }

    fn title_case(s: &str) -> String {
        s.split(' ')
            .map(|w| {
                let mut c = w.chars();
                match c.next() {
                    Some(f) => f.to_uppercase().chain(c.flat_map(|x| x.to_lowercase())).collect(),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

// -------------------------------------------------------------------- macOS

#[cfg(target_os = "macos")]
mod sys {
    use super::Binding;

    pub const CANCEL: Binding = Binding::Key(0x35); // kVK_Escape
    pub const TAB: u16 = 0x30; // kVK_Tab

    /// Command, Shift, Option and Control (left and right).
    pub fn is_modifier(b: Binding) -> bool {
        matches!(b, Binding::Key(0x36..=0x38 | 0x3A..=0x3E))
    }

    /// `kCGEventSourceStateHIDSystemState`: the hardware state, regardless
    /// of which app is in front.
    const HID_STATE: i32 = 1;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceKeyState(state: i32, key: u16) -> bool;
        fn CGEventSourceButtonState(state: i32, button: u32) -> bool;
        fn CGPreflightListenEventAccess() -> bool;
        fn CGRequestListenEventAccess() -> bool;
    }

    pub fn has_permission() -> bool {
        // SAFETY: no arguments, available since macOS 10.15.
        unsafe { CGPreflightListenEventAccess() }
    }

    pub fn request_permission() {
        // SAFETY: as above; shows the system prompt at most once.
        unsafe { CGRequestListenEventAccess() };
    }

    /// Privacy & Security -> Input Monitoring. `open` handles the
    /// `x-apple.systempreferences:` scheme; a web browser would not.
    pub fn open_permission_settings() {
        let url = "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent";
        // Waited on a side thread so no zombie process is left behind.
        std::thread::spawn(move || {
            if let Err(e) = std::process::Command::new("/usr/bin/open").arg(url).status() {
                tracing::warn!("ptt: {e}");
            }
        });
    }

    pub fn is_down(b: Binding) -> bool {
        // SAFETY: plain value arguments.
        match b {
            Binding::Key(k) => unsafe { CGEventSourceKeyState(HID_STATE, k) },
            // CoreGraphics numbers buttons from 0 (left); users from 1.
            Binding::Mouse(m) => unsafe { CGEventSourceButtonState(HID_STATE, m as u32 - 1) },
        }
    }

    /// All key codes except Tab and every mouse button except left and right.
    pub fn candidates() -> impl Iterator<Item = Binding> {
        (0u16..0x80).filter(|k| *k != TAB).map(Binding::Key).chain((3..=32).map(Binding::Mouse))
    }

    pub fn pick(pressed: &[Binding]) -> Option<Binding> {
        pressed.first().copied()
    }

    pub fn key_name(k: u16) -> String {
        super::mac_key_name(k)
    }
}

// --------------------------------------------------------- other platforms

#[cfg(not(any(windows, target_os = "macos")))]
mod sys {
    use super::Binding;

    pub const CANCEL: Binding = Binding::Key(0);
    pub const TAB: u16 = u16::MAX;

    pub fn has_permission() -> bool {
        false
    }

    pub fn is_modifier(_: Binding) -> bool {
        false
    }

    pub fn request_permission() {}

    pub fn open_permission_settings() {}

    pub fn is_down(_: Binding) -> bool {
        false
    }

    pub fn candidates() -> impl Iterator<Item = Binding> {
        std::iter::empty()
    }

    pub fn pick(pressed: &[Binding]) -> Option<Binding> {
        pressed.first().copied()
    }

    pub fn key_name(k: u16) -> String {
        format!("Klawisz {k}")
    }
}

/// Key names for macOS virtual key codes (`kVK_*`, US layout labels).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn mac_key_name(k: u16) -> String {
    const LETTERS: &[(u16, &str)] = &[
        (0x00, "A"), (0x01, "S"), (0x02, "D"), (0x03, "F"), (0x04, "H"), (0x05, "G"), (0x06, "Z"),
        (0x07, "X"), (0x08, "C"), (0x09, "V"), (0x0A, "§"), (0x0B, "B"), (0x0C, "Q"), (0x0D, "W"),
        (0x0E, "E"), (0x0F, "R"), (0x10, "Y"), (0x11, "T"), (0x12, "1"), (0x13, "2"), (0x14, "3"),
        (0x15, "4"), (0x16, "6"), (0x17, "5"), (0x18, "="), (0x19, "9"), (0x1A, "7"), (0x1B, "-"),
        (0x1C, "8"), (0x1D, "0"), (0x1E, "]"), (0x1F, "O"), (0x20, "U"), (0x21, "["), (0x22, "I"),
        (0x23, "P"), (0x25, "L"), (0x26, "J"), (0x27, "'"), (0x28, "K"), (0x29, ";"), (0x2A, "\\"),
        (0x2B, ","), (0x2C, "/"), (0x2D, "N"), (0x2E, "M"), (0x2F, "."), (0x32, "`"), (0x41, "Num ."),
        (0x43, "Num *"),
    ];
    const OTHER: &[(u16, &str)] = &[
        (0x24, "Return"), (0x30, "Tab"), (0x31, "Spacja"), (0x33, "Delete"), (0x35, "Escape"),
        (0x36, "Prawy Command"), (0x37, "Lewy Command"), (0x38, "Lewy Shift"), (0x39, "Caps Lock"),
        (0x3A, "Lewy Option"), (0x3B, "Lewy Control"), (0x3C, "Prawy Shift"), (0x3D, "Prawy Option"),
        (0x3E, "Prawy Control"), (0x3F, "Fn"), (0x45, "Num +"), (0x47, "Num Clear"), (0x4B, "Num /"),
        (0x4C, "Num Enter"), (0x4E, "Num -"), (0x51, "Num ="), (0x72, "Help"), (0x73, "Home"),
        (0x74, "Page Up"), (0x75, "Forward Delete"), (0x77, "End"), (0x79, "Page Down"),
        (0x7B, "Strzałka w lewo"), (0x7C, "Strzałka w prawo"), (0x7D, "Strzałka w dół"), (0x7E, "Strzałka w górę"),
    ];
    const FKEYS: &[(u16, u8)] = &[
        (0x7A, 1), (0x78, 2), (0x63, 3), (0x76, 4), (0x60, 5), (0x61, 6), (0x62, 7), (0x64, 8),
        (0x65, 9), (0x6D, 10), (0x67, 11), (0x6F, 12), (0x69, 13), (0x6B, 14), (0x71, 15),
        (0x6A, 16), (0x40, 17), (0x4F, 18), (0x50, 19), (0x5A, 20),
    ];
    if let Some((_, n)) = LETTERS.iter().chain(OTHER.iter()).find(|(c, _)| *c == k) {
        return (*n).into();
    }
    if let Some((_, n)) = FKEYS.iter().find(|(c, _)| *c == k) {
        return format!("F{n}");
    }
    // Keypad digits: 0-7 are 0x52-0x59, 8 and 9 are 0x5B and 0x5C.
    match k {
        0x52..=0x59 => format!("Num {}", k - 0x52),
        0x5B => "Num 8".into(),
        0x5C => "Num 9".into(),
        _ => format!("Klawisz {k}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_roundtrip() {
        for b in [Binding::Key(0x56), Binding::Key(0x41), Binding::Mouse(3), Binding::Mouse(5), Binding::Mouse(9)] {
            assert_eq!(Binding::parse(&b.format()), Some(b));
            assert_eq!(Binding::decode(Binding::encode(Some(b))), Some(b));
        }
        assert_eq!(Binding::decode(Binding::encode(None)), None);
        // Left/right buttons cannot be bound (they would block clicking).
        assert_eq!(Binding::parse("mouse:1"), None);
        assert_eq!(Binding::parse("key:x"), None);
        assert_eq!(Binding::parse(""), None);
        assert_eq!(Binding::parse(&Binding::Key(sys::TAB).format()), None);
    }

    #[test]
    fn mac_names() {
        assert_eq!(mac_key_name(0x09), "V");
        assert_eq!(mac_key_name(0x3A), "Lewy Option");
        assert_eq!(mac_key_name(0x7A), "F1");
        assert_eq!(mac_key_name(0x5B), "Num 8");
        assert_eq!(mac_key_name(0x7E), "Strzałka w górę");
        assert_eq!(mac_key_name(0x7F), "Klawisz 127");
    }

    #[cfg(windows)]
    #[test]
    fn windows_names() {
        assert_eq!(Binding::Key(0x56).name(), "V");
        assert_eq!(Binding::Key(0xA4).name(), "Lewy Alt");
        assert_eq!(Binding::Key(0x7B).name(), "F12");
        assert_eq!(Binding::Mouse(4).name(), "Przycisk myszy 4 (wstecz)");
        // Candidates: no left/right mouse buttons, no generic modifiers.
        let all: Vec<_> = sys::candidates().collect();
        assert!(!all.contains(&Binding::Key(0x10)) && all.contains(&Binding::Key(0xA0)));
        assert!(!all.contains(&Binding::Key(0x09)), "Tab cannot be bound");
        assert!(all.contains(&Binding::Mouse(4)) && !all.iter().any(|b| matches!(b, Binding::Key(1 | 2))));
        // AltGr = left Ctrl + right Alt at once: right Alt is bound.
        assert_eq!(sys::pick(&[Binding::Key(0xA2), Binding::Key(0xA5)]), Some(Binding::Key(0xA5)));
        assert_eq!(sys::pick(&[Binding::Key(0xA2)]), Some(Binding::Key(0xA2)));
        assert_eq!(sys::pick(&[]), None);
    }

    /// Runs polls `(inputs down, Tab down)` until the capture ends.
    #[cfg(windows)]
    fn capture(polls: &[(&[Binding], bool)]) -> Option<Option<Binding>> {
        let mut c = Capturer::default();
        polls.iter().find_map(|(down, tab)| c.step(down.to_vec(), *tab))
    }

    #[cfg(windows)]
    #[test]
    fn capture_rules() {
        const V: Binding = Binding::Key(0x56);
        const LSHIFT: Binding = Binding::Key(0xA0);
        const LCTRL: Binding = Binding::Key(0xA2);
        const RALT: Binding = Binding::Key(0xA5);
        const ESC: Binding = Binding::Key(0x1B);
        // A key binds as soon as it is pressed.
        assert_eq!(capture(&[(&[], false), (&[V], false)]), Some(Some(V)));
        assert_eq!(capture(&[(&[], false), (&[Binding::Mouse(4)], false)]), Some(Some(Binding::Mouse(4))));
        // Escape cancels.
        assert_eq!(capture(&[(&[], false), (&[ESC], false)]), Some(None));
        // A key held when the capture starts counts only after release.
        assert_eq!(capture(&[(&[V], false), (&[V], false), (&[], false)]), None);
        assert_eq!(capture(&[(&[V], false), (&[], false), (&[V], false)]), Some(Some(V)));
        // Tab alone binds nothing.
        assert_eq!(capture(&[(&[], false), (&[], true), (&[], false)]), None);
        // A modifier binds on release, when pressed alone.
        assert_eq!(capture(&[(&[], false), (&[LSHIFT], false)]), None);
        assert_eq!(capture(&[(&[], false), (&[LSHIFT], false), (&[], false)]), Some(Some(LSHIFT)));
        // Shift+Tab (moving focus back) binds nothing; a later key does.
        let shift_tab: &[(&[Binding], bool)] =
            &[(&[], false), (&[LSHIFT], false), (&[LSHIFT], true), (&[LSHIFT], false), (&[], false)];
        assert_eq!(capture(shift_tab), None);
        // Shift and Tab first seen in the same poll.
        assert_eq!(capture(&[(&[], false), (&[LSHIFT], true), (&[], false)]), None);
        let mut then_v = shift_tab.to_vec();
        then_v.push((&[V], false));
        assert_eq!(capture(&then_v), Some(Some(V)));
        // Modifier + key: the key is bound (no combinations).
        assert_eq!(capture(&[(&[], false), (&[LSHIFT], false), (&[LSHIFT, V], false)]), Some(Some(V)));
        // AltGr (left Ctrl + right Alt), even a poll apart: right Alt.
        assert_eq!(capture(&[(&[], false), (&[LCTRL, RALT], false), (&[], false)]), Some(Some(RALT)));
        assert_eq!(capture(&[(&[], false), (&[LCTRL], false), (&[LCTRL, RALT], false), (&[], false)]), Some(Some(RALT)));
    }
}
