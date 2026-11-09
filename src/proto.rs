//! Wire protocol shared by all peers.

use anyhow::{Context, Result, bail};
use iroh::{EndpointAddr, EndpointId};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Bumped on every incompatible change; mismatched versions get a clear
/// "update the app" error instead of garbled messages.
pub const ALPN: &[u8] = b"bibicord/5";

/// First byte of every bidirectional stream.
pub const TAG_CONTROL: u8 = 0;
pub const TAG_WATCH: u8 = 1;
pub const TAG_CAMERA: u8 = 2;

/// First byte of every datagram.
pub const DGRAM_VOICE: u8 = 1;

const MAX_FRAME: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Codec {
    H264,
    Hevc,
}

impl Codec {
    pub fn label(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::Hevc => "HEVC",
        }
    }

    /// Raw Annex B demuxer name in ffmpeg.
    pub fn ffmpeg_format(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamInfo {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub encoder: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Msg {
    Hello {
        name: String,
        room: String,
        peers: Vec<EndpointId>,
        stream: Option<StreamInfo>,
        /// Webcam, separate from the screen stream.
        camera: Option<StreamInfo>,
        voice: bool,
        muted: bool,
        deafened: bool,
        /// Profile picture, PNG (see `avatar`).
        avatar: Option<Vec<u8>>,
    },
    Chat {
        text: String,
    },
    Stream(Option<StreamInfo>),
    Camera(Option<StreamInfo>),
    Voice(bool),
    Mute {
        muted: bool,
        deafened: bool,
    },
    /// Display name or profile picture changed.
    Profile {
        name: String,
        avatar: Option<Vec<u8>>,
    },
}

/// Longest display name, in characters.
pub const MAX_NAME: usize = 32;

/// Display name as typed by the user: trimmed, single line, bounded.
pub fn clean_name(name: &str) -> String {
    let one_line: String = name.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    one_line.trim().chars().take(MAX_NAME).collect::<String>().trim().to_string()
}

pub async fn write_msg(w: &mut (impl AsyncWrite + Unpin), msg: &Msg) -> Result<()> {
    let bytes = postcard::to_stdvec(msg)?;
    w.write_all(&(bytes.len() as u32).to_le_bytes()).await?;
    w.write_all(&bytes).await?;
    Ok(())
}

pub async fn read_msg(r: &mut (impl AsyncRead + Unpin)) -> Result<Msg> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        bail!("frame too large: {len}");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(postcard::from_bytes(&buf)?)
}

// ------------------------------------------------------------------ invites
//
// An invite is a short code like `K7QX-3MFA-9TZP` (60 random bits, Crockford
// base32). Each app session picks a code and derives its endpoint key from
// it, so the code *is* the address: the joiner derives the same public key
// and finds the endpoint through iroh's address lookup (pkarr / DNS). The
// derivation is deliberately slow to make guessing codes from a public key
// impractical.

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CODE_LEN: usize = 12;
const KDF_ROUNDS: u32 = 200_000;

/// A fresh random invite code (60 bits).
pub fn new_code() -> String {
    let rnd = iroh::SecretKey::generate().to_bytes();
    let mut v = u64::from_le_bytes(rnd[..8].try_into().unwrap()) & ((1 << 60) - 1);
    let mut code = String::with_capacity(CODE_LEN);
    for _ in 0..CODE_LEN {
        code.push(CROCKFORD[(v & 31) as usize] as char);
        v >>= 5;
    }
    code
}

/// `K7QX3MFA9TZP` -> `K7QX-3MFA-9TZP`.
pub fn format_code(code: &str) -> String {
    code.as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect::<Vec<_>>()
        .join("-")
}

/// Normalises user input: case, separators, and look-alike letters.
pub fn parse_code(input: &str) -> Option<String> {
    let mut out = String::new();
    for ch in input.trim().chars() {
        let c = match ch.to_ascii_uppercase() {
            '-' | ' ' | '_' | '.' => continue,
            'O' => '0',
            'I' | 'L' => '1',
            'U' => 'V',
            c if CROCKFORD.contains(&(c as u8)) && c.is_ascii() => c,
            _ => return None,
        };
        out.push(c);
    }
    (out.len() == CODE_LEN).then_some(out)
}

/// Endpoint secret key for an invite code.
pub fn code_key(code: &str) -> iroh::SecretKey {
    let mut h = blake3::derive_key("bibicord 2026 invite code v1", code.as_bytes());
    for _ in 0..KDF_ROUNDS {
        h = *blake3::hash(&h).as_bytes();
    }
    iroh::SecretKey::from_bytes(&h)
}

/// Resolves an invite code to the address of the member who shared it.
pub fn decode_ticket(ticket: &str) -> Result<EndpointAddr> {
    let code = parse_code(ticket)
        .context("to nie jest kod zaproszenia BibiCord (12 znaków, np. K7QX-3MFA-9TZP)")?;
    Ok(EndpointAddr::from(code_key(&code).public()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_roundtrip_and_normalise() {
        let c = new_code();
        assert_eq!(c.len(), 12);
        let shown = format_code(&c);
        assert_eq!(shown.len(), 14);
        assert_eq!(parse_code(&shown.to_lowercase()).as_deref(), Some(c.as_str()));
        assert_eq!(parse_code(" k7qx 3mfa-9tzo ").as_deref(), Some("K7QX3MFA9TZ0"));
        assert!(parse_code("K7QX-3MFA").is_none());
        let a = decode_ticket(&shown).unwrap();
        assert_eq!(a.id, code_key(&c).public());
    }

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean_name("  Ala\n Kot "), "Ala  Kot");
        assert_eq!(clean_name(&"x".repeat(50)).chars().count(), MAX_NAME);
        assert_eq!(clean_name(" \t "), "");
    }
}
