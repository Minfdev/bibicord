//! Wire protocol shared by all peers.

use anyhow::{Result, bail};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Bumped on every incompatible change; mismatched versions get a clear
/// "update the app" error instead of garbled messages.
pub const ALPN: &[u8] = b"bibicord/7";

/// First byte of every bidirectional stream.
pub const TAG_CONTROL: u8 = 0;
pub const TAG_WATCH: u8 = 1;
pub const TAG_CAMERA: u8 = 2;

/// First byte of every datagram.
pub const DGRAM_VOICE: u8 = 1;

const MAX_FRAME: usize = 1 << 20;

/// Largest file sent in chat (test version: one control message, no chunking).
pub const MAX_FILE_BYTES: usize = 1_000_000;

/// Longest file name, in characters.
pub const MAX_FILE_NAME: usize = 100;

// A file message (data, name in UTF-8, postcard overhead) must fit in a frame.
const _: () = assert!(MAX_FILE_BYTES + 4 * MAX_FILE_NAME + 32 <= MAX_FRAME);

/// Longest chat message, in bytes of UTF-8.
pub const MAX_CHAT_BYTES: usize = 1_000_000;

// Larger frames are skipped by `read_msg`, so a message would vanish.
const _: () = assert!(MAX_CHAT_BYTES + 32 <= MAX_FRAME);

/// Cuts `text` to at most `MAX_CHAT_BYTES`, on a character boundary.
pub fn clamp_chat(text: &mut String) {
    if text.len() > MAX_CHAT_BYTES {
        text.truncate(text.floor_char_boundary(MAX_CHAT_BYTES));
    }
}

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

/// Proof sent before anything else on a connection (see `proof`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Auth {
    /// Joining with an invite link: keyed by the link's token.
    Invite { proof: [u8; 32] },
    /// Mesh connection inside a room: keyed by the room secret.
    Member { proof: [u8; 32] },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Msg {
    /// First message of the caller.
    Auth(Auth),
    /// Answer to a bad `Auth`; the connection is closed.
    Denied,
    /// Answer to a good `Auth`: the callee's own proof, and the room
    /// secret for someone joining with an invite.
    Welcome { proof: [u8; 32], room_secret: Option<[u8; 32]> },
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
    /// File posted in chat, at most `MAX_FILE_BYTES`.
    File {
        name: String,
        data: Vec<u8>,
    },
}

/// Longest display name, in characters.
pub const MAX_NAME: usize = 32;

/// Display name as typed by the user: trimmed, single line, bounded.
pub fn clean_name(name: &str) -> String {
    let one_line: String = name.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    one_line.trim().chars().take(MAX_NAME).collect::<String>().trim().to_string()
}

/// File name safe to offer in a save dialog: no path, no control characters
/// or separators, bounded length (the extension is kept), never empty.
pub fn clean_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let base: String = base
        .chars()
        .map(|c| if c.is_control() || matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect();
    let base = base.trim().trim_matches('.').trim();
    if base.is_empty() {
        return "plik".into();
    }
    if base.chars().count() <= MAX_FILE_NAME {
        return base.to_string();
    }
    let ext = base.rsplit_once('.').map(|(_, e)| e).filter(|e| e.chars().count() <= 16).unwrap_or("");
    let keep = if ext.is_empty() { MAX_FILE_NAME } else { MAX_FILE_NAME - ext.chars().count() - 1 };
    let stem: String = base.chars().take(keep).collect();
    let stem = stem.trim_end();
    if ext.is_empty() { stem.to_string() } else { format!("{stem}.{ext}") }
}

pub async fn write_msg(w: &mut (impl AsyncWrite + Unpin), msg: &Msg) -> Result<()> {
    let bytes = postcard::to_stdvec(msg)?;
    w.write_all(&(bytes.len() as u32).to_le_bytes()).await?;
    w.write_all(&bytes).await?;
    Ok(())
}

pub async fn read_msg(r: &mut (impl AsyncRead + Unpin)) -> Result<Msg> {
    let mut len = [0u8; 4];
    loop {
        r.read_exact(&mut len).await?;
        let len = u32::from_le_bytes(len) as usize;
        if len > MAX_FRAME {
            // Skip it instead of dropping the peer (e.g. a too large file).
            tracing::warn!("skipping frame of {len} bytes");
            let skipped = tokio::io::copy(&mut (&mut *r).take(len as u64), &mut tokio::io::sink()).await?;
            if skipped < len as u64 {
                bail!("stream ended inside a frame");
            }
            continue;
        }
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf).await?;
        return Ok(postcard::from_bytes(&buf)?);
    }
}

// ------------------------------------------------------------------ invites
//
// An invite link carries the inviter's endpoint id (the address; iroh finds
// it through address lookup) and a random token valid for 24 h. The token
// never travels in the clear: the joiner proves it knows it with `proof`,
// bound to the TLS session. Members of a room share a random room secret,
// handed to joiners in `Msg::Welcome`, and prove it the same way.

pub const INVITE_PREFIX: &str = "bibicord://join/";
const INVITE_VERSION: u8 = 1;
const INVITE_LEN: usize = 1 + 32 + 16 + 4;

pub const NOT_AN_INVITE: &str = "To nie jest link zaproszenia BibiCord";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InviteLink {
    pub id: EndpointId,
    pub token: [u8; 16],
    /// Unix seconds; only a hint for the joiner (the inviter decides).
    pub expires: u32,
}

/// `bibicord://join/<base32>`.
pub fn format_invite(i: &InviteLink) -> String {
    let mut b = Vec::with_capacity(INVITE_LEN);
    b.push(INVITE_VERSION);
    b.extend_from_slice(i.id.as_bytes());
    b.extend_from_slice(&i.token);
    b.extend_from_slice(&i.expires.to_le_bytes());
    INVITE_PREFIX.to_string() + &data_encoding::BASE32_NOPAD.encode(&b).to_ascii_lowercase()
}

pub fn parse_invite(input: &str) -> Result<InviteLink> {
    let s = input.trim();
    let s = match s.get(..INVITE_PREFIX.len()) {
        Some(p) if p.eq_ignore_ascii_case(INVITE_PREFIX) => &s[INVITE_PREFIX.len()..],
        _ => s,
    };
    if is_old_code(s) {
        bail!("To kod ze starszej wersji BibiCord. Poproś o nowy link zaproszenia");
    }
    let Ok(b) = data_encoding::BASE32_NOPAD.decode(s.to_ascii_uppercase().as_bytes()) else { bail!(NOT_AN_INVITE) };
    if b.first().is_some_and(|v| *v > INVITE_VERSION) {
        bail!("Ten link wymaga nowszej wersji BibiCord. Zaktualizuj aplikację");
    }
    if b.len() != INVITE_LEN || b[0] != INVITE_VERSION {
        bail!(NOT_AN_INVITE);
    }
    let Ok(id) = EndpointId::from_bytes(b[1..33].try_into().unwrap()) else { bail!(NOT_AN_INVITE) };
    Ok(InviteLink {
        id,
        token: b[33..49].try_into().unwrap(),
        expires: u32::from_le_bytes(b[49..53].try_into().unwrap()),
    })
}

/// A 12-character invite code of BibiCord up to 0.4 (`K7QX-3MFA-9TZP`).
fn is_old_code(s: &str) -> bool {
    const CROCKFORD: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZOILU";
    let mut n = 0;
    for ch in s.chars() {
        match ch.to_ascii_uppercase() {
            '-' | ' ' | '_' | '.' => {}
            c if c.is_ascii() && CROCKFORD.contains(c) => n += 1,
            _ => return false,
        }
    }
    n == 12
}

/// 32 random bytes.
pub fn random32() -> [u8; 32] {
    iroh::SecretKey::generate().to_bytes()
}

pub const ROLE_CALLER: u8 = 0;
pub const ROLE_CALLEE: u8 = 1;

pub fn invite_key(token: &[u8; 16]) -> [u8; 32] {
    blake3::derive_key("bibicord 2026 invite proof v1", token)
}

pub fn room_key(secret: &[u8; 32]) -> [u8; 32] {
    blake3::derive_key("bibicord 2026 room proof v1", secret)
}

/// Proof of knowing `key` on one connection: `exporter` comes from the TLS
/// session, so a proof can't be replayed elsewhere; the ids and role keep
/// it from being reflected back.
pub fn proof(key: &[u8; 32], role: u8, exporter: &[u8; 32], caller: &EndpointId, callee: &EndpointId) -> [u8; 32] {
    let mut h = blake3::Hasher::new_keyed(key);
    h.update(&[role]);
    h.update(exporter);
    h.update(caller.as_bytes());
    h.update(callee.as_bytes());
    *h.finalize().as_bytes()
}

/// Constant-time comparison (`blake3::Hash` compares in constant time).
pub fn proof_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    blake3::Hash::from_bytes(*a) == blake3::Hash::from_bytes(*b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link() -> InviteLink {
        InviteLink { id: iroh::SecretKey::generate().public(), token: [7; 16], expires: 1_900_000_000 }
    }

    #[test]
    fn invite_links_roundtrip() {
        let l = link();
        let s = format_invite(&l);
        assert!(s.starts_with(INVITE_PREFIX));
        let data = &s[INVITE_PREFIX.len()..];
        assert_eq!(data, data.to_lowercase());
        assert_eq!(data.len(), 85); // 53 bytes, base32 without padding
        assert_eq!(parse_invite(&s).unwrap(), l);
        for v in [format!("  {s}\n"), s.to_uppercase(), data.to_string(), format!("\t{}  ", data.to_uppercase())] {
            assert_eq!(parse_invite(&v).unwrap(), l, "{v:?}");
        }
    }

    #[test]
    fn bad_invite_links() {
        let err = |s: &str| parse_invite(s).unwrap_err().to_string();
        let s = format_invite(&link());
        let enc = |b: &[u8]| data_encoding::BASE32_NOPAD.encode(b).to_lowercase();
        let mut raw = data_encoding::BASE32_NOPAD.decode(s[INVITE_PREFIX.len()..].to_uppercase().as_bytes()).unwrap();
        for bad in [&s[..s.len() - 1], &format!("{s}a"), "", "xyz!", "https://example.com/join", &enc(&raw[..52]), &enc(&[raw.clone(), vec![0]].concat())] {
            assert_eq!(err(bad), NOT_AN_INVITE, "{bad:?}");
        }
        raw[0] = 0;
        assert_eq!(err(&enc(&raw)), NOT_AN_INVITE);
        raw[0] = 2;
        assert!(err(&enc(&raw)).contains("nowszej wersji"));
        raw.truncate(20); // a future format may be shorter
        assert!(err(&enc(&raw)).contains("nowszej wersji"));
        for old in ["K7QX-3MFA-9TZP", " k7qx 3mfa 9tzo ", "K7QX3MFA9TZP"] {
            assert!(err(old).contains("starszej wersji"), "{old:?}");
        }
    }

    #[test]
    fn proofs_depend_on_everything() {
        let (a, b) = (iroh::SecretKey::generate().public(), iroh::SecretKey::generate().public());
        let (k, ex) = (room_key(&[1; 32]), [2; 32]);
        let p = proof(&k, ROLE_CALLER, &ex, &a, &b);
        assert!(proof_eq(&p, &proof(&k, ROLE_CALLER, &ex, &a, &b)));
        for other in [
            proof(&k, ROLE_CALLEE, &ex, &a, &b),
            proof(&room_key(&[3; 32]), ROLE_CALLER, &ex, &a, &b),
            proof(&invite_key(&[1; 16]), ROLE_CALLER, &ex, &a, &b),
            proof(&k, ROLE_CALLER, &[4; 32], &a, &b),
            proof(&k, ROLE_CALLER, &ex, &b, &a),
        ] {
            assert!(!proof_eq(&p, &other));
        }
    }

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean_name("  Ala\n Kot "), "Ala  Kot");
        assert_eq!(clean_name(&"x".repeat(50)).chars().count(), MAX_NAME);
        assert_eq!(clean_name(" \t "), "");
    }

    #[test]
    fn file_names_are_cleaned() {
        assert_eq!(clean_file_name("raport.pdf"), "raport.pdf");
        assert_eq!(clean_file_name("../../etc/passwd"), "passwd");
        assert_eq!(clean_file_name("..\\..\\Windows\\win.ini"), "win.ini");
        assert_eq!(clean_file_name("/abs/path/a b.txt"), "a b.txt");
        assert_eq!(clean_file_name("zły\nplik\0.txt"), "zły_plik_.txt");
        assert_eq!(clean_file_name("C:evil.txt"), "C_evil.txt");
        for bad in ["", "..", ".", "dir/", " / ", "\\"] {
            assert_eq!(clean_file_name(bad), "plik", "{bad:?}");
        }
        let long = format!("{}.tar.gz", "x".repeat(300));
        let c = clean_file_name(&long);
        assert_eq!(c.chars().count(), MAX_FILE_NAME);
        assert!(c.ends_with("x.gz"));
        assert_eq!(clean_file_name(&"ą".repeat(300)).chars().count(), MAX_FILE_NAME);
    }

    #[tokio::test]
    async fn longest_chat_fits_a_frame() {
        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        let text = "x".repeat(MAX_CHAT_BYTES);
        let sent = text.clone();
        let writer = tokio::spawn(async move { write_msg(&mut a, &Msg::Chat { text: sent }).await.unwrap() });
        assert!(matches!(read_msg(&mut b).await.unwrap(), Msg::Chat { text: t } if t == text));
        writer.await.unwrap();
    }

    #[test]
    fn chat_is_clamped_on_char_boundary() {
        let mut s = "x".repeat(MAX_CHAT_BYTES + 10);
        clamp_chat(&mut s);
        assert_eq!(s.len(), MAX_CHAT_BYTES);
        // "ą" is 2 bytes, the emoji 4: with odd padding both straddle the limit.
        for (pad, ch) in [(1, "ą"), (1, "😀"), (2, "😀"), (3, "😀")] {
            let mut s = "x".repeat(MAX_CHAT_BYTES - pad) + &ch.repeat(3);
            clamp_chat(&mut s);
            assert!(s.len() <= MAX_CHAT_BYTES && s.len() > MAX_CHAT_BYTES - 4, "{pad} {ch}");
            assert!(std::str::from_utf8(s.as_bytes()).is_ok());
            assert!(s.starts_with(&"x".repeat(MAX_CHAT_BYTES - pad)));
        }
        let mut s = "cześć 😀".to_string();
        clamp_chat(&mut s);
        assert_eq!(s, "cześć 😀");
    }

    #[tokio::test]
    async fn oversized_frame_is_skipped() {
        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        let big = Msg::File { name: "big.bin".into(), data: vec![1; MAX_FRAME + 10] };
        let small = Msg::File { name: "ok.bin".into(), data: vec![2; MAX_FILE_BYTES] };
        let writer = tokio::spawn(async move {
            write_msg(&mut a, &big).await.unwrap();
            write_msg(&mut a, &small).await.unwrap();
            write_msg(&mut a, &Msg::Chat { text: "dalej".into() }).await.unwrap();
        });
        let Msg::File { name, data } = read_msg(&mut b).await.unwrap() else { panic!() };
        assert_eq!((name.as_str(), data.len()), ("ok.bin", MAX_FILE_BYTES));
        assert!(matches!(read_msg(&mut b).await.unwrap(), Msg::Chat { text } if text == "dalej"));
        writer.await.unwrap();
    }
}
