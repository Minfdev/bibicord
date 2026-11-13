//! Chat history and its limits: a long session (or a peer posting nonstop)
//! must not hold an unbounded amount of text and files.

use iroh::EndpointId;
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Entries kept: messages, files and system lines together.
pub(crate) const MAX_ITEMS: usize = 2000;
/// Bytes of message text and file data kept.
pub(crate) const MAX_BYTES: usize = 64 << 20;

pub(crate) enum ChatKind {
    Message { from: Option<EndpointId>, name: String, text: String },
    /// Kept in memory, saved only on request. `data` is `None` once the
    /// history needed the room (`trim`); the card stays.
    File { from: Option<EndpointId>, name: String, file_name: String, size: usize, data: Option<Arc<Vec<u8>>>, saved: bool },
    System { icon: &'static str, who: String, text: String, good: bool },
}

pub(crate) struct ChatItem {
    /// Stable for the item's life: its widgets keep their ids while
    /// others above are skipped or trimmed.
    pub id: u64,
    pub kind: ChatKind,
    pub time: chrono::DateTime<chrono::Local>,
    /// Height as last drawn, so it needn't be laid out while off screen.
    pub layout: Cell<Option<Layout>>,
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Layout {
    /// What the height depends on.
    pub width: f32,
    pub scale: f32,
    pub grouped: bool,
    pub height: f32,
}

impl ChatItem {
    pub fn new(kind: ChatKind) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        ChatItem { id: NEXT.fetch_add(1, Ordering::Relaxed), kind, time: chrono::Local::now(), layout: Cell::new(None) }
    }

    /// Bytes counted against `MAX_BYTES`.
    fn bytes(&self) -> usize {
        match &self.kind {
            ChatKind::Message { text, .. } => text.len(),
            ChatKind::File { data, .. } => data.as_ref().map_or(0, |d| d.len()),
            ChatKind::System { .. } => 0,
        }
    }

    pub fn from(&self) -> Option<EndpointId> {
        match &self.kind {
            ChatKind::Message { from, .. } | ChatKind::File { from, .. } => *from,
            ChatKind::System { .. } => None,
        }
    }
}

/// Brings `chat` within `max_items` and `max_bytes`: the oldest entries
/// beyond the count go first, then the oldest files lose their data (their
/// card stays), then, if text alone is still too much, the oldest entries
/// go. Our own and already saved files are no exception. Returns the height
/// last drawn of what was removed, if anything was.
pub(crate) fn trim(chat: &mut Vec<ChatItem>, max_items: usize, max_bytes: usize) -> Option<f32> {
    let mut cut = chat.len().saturating_sub(max_items);
    let mut bytes: usize = chat[cut..].iter().map(ChatItem::bytes).sum();
    for item in &mut chat[cut..] {
        if bytes <= max_bytes {
            break;
        }
        if let ChatKind::File { data: data @ Some(_), .. } = &mut item.kind {
            bytes -= data.take().map_or(0, |d| d.len());
            item.layout.set(None); // the card shows it's gone
        }
    }
    while bytes > max_bytes && cut < chat.len() {
        bytes -= chat[cut].bytes();
        cut += 1;
    }
    let height = chat.drain(..cut).map(|i| i.layout.get().map_or(0.0, |l| l.height)).sum();
    (cut > 0).then_some(height)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(text: &str) -> ChatItem {
        ChatItem::new(ChatKind::Message { from: None, name: "Ala".into(), text: text.into() })
    }

    fn file(n: usize) -> ChatItem {
        ChatItem::new(ChatKind::File {
            from: None,
            name: "Ala".into(),
            file_name: format!("{n}.bin"),
            size: n,
            data: Some(Arc::new(vec![0; n])),
            saved: false,
        })
    }

    fn system() -> ChatItem {
        ChatItem::new(ChatKind::System { icon: "", who: "Ola".into(), text: "dołącza".into(), good: true })
    }

    /// Short description: `m:text`, `f:size` (`f:size-` once freed), `s`.
    fn shape(chat: &[ChatItem]) -> Vec<String> {
        chat.iter()
            .map(|i| match &i.kind {
                ChatKind::Message { text, .. } => format!("m:{text}"),
                ChatKind::File { size, data, .. } => format!("f:{size}{}", if data.is_some() { "" } else { "-" }),
                ChatKind::System { .. } => "s".into(),
            })
            .collect()
    }

    #[test]
    fn short_history_unchanged() {
        let mut chat = vec![msg("a"), file(10), system(), msg("b")];
        assert!(trim(&mut chat, 4, 13).is_none());
        assert_eq!(shape(&chat), ["m:a", "f:10", "s", "m:b"]);
    }

    #[test]
    fn item_limit_drops_oldest() {
        let mut chat: Vec<_> = (0..10).map(|i| msg(&i.to_string())).collect();
        chat.insert(3, system());
        assert!(trim(&mut chat, 5, usize::MAX).is_some());
        assert_eq!(shape(&chat), ["m:5", "m:6", "m:7", "m:8", "m:9"]);
    }

    #[test]
    fn bytes_free_oldest_files_first() {
        let mut chat = vec![msg("aaaa"), file(10), msg("bb"), file(20), file(30)];
        // 66 bytes, 40 allowed: the 10 and 20 B files go, their cards stay.
        assert!(trim(&mut chat, 100, 40).is_none());
        assert_eq!(shape(&chat), ["m:aaaa", "f:10-", "m:bb", "f:20-", "f:30"]);
        assert!(chat.iter().all(|i| i.bytes() <= 30));
    }

    #[test]
    fn bytes_drop_oldest_when_text_is_too_much() {
        let mut chat = vec![msg("aaaa"), file(10), msg("bbbb"), msg("cccc")];
        // Even with the file freed, 12 bytes of text are over 8.
        assert!(trim(&mut chat, 100, 8).is_some());
        assert_eq!(shape(&chat), ["f:10-", "m:bbbb", "m:cccc"]);
        // The newest file counts too and is freed last.
        let mut chat = vec![msg("aaaa"), file(10)];
        assert!(trim(&mut chat, 100, 8).is_none());
        assert_eq!(shape(&chat), ["m:aaaa", "f:10-"]);
    }

    #[test]
    fn count_first_spares_files() {
        // Dropping the oldest entry for the count already brings the bytes in.
        let mut chat = vec![file(10), file(10), msg("a")];
        assert!(trim(&mut chat, 2, 11).is_some());
        assert_eq!(shape(&chat), ["f:10", "m:a"]);
    }

    #[test]
    fn real_limits() {
        let mut chat = Vec::new();
        for i in 0..100 {
            chat.push(file(1_000_000));
            chat.push(msg(&i.to_string()));
            trim(&mut chat, MAX_ITEMS, MAX_BYTES);
        }
        let kept = chat.iter().filter(|i| matches!(i.kind, ChatKind::File { data: Some(_), .. })).count();
        assert_eq!(kept, MAX_BYTES / 1_000_000);
        assert_eq!(chat.len(), 200);
        for i in 0..MAX_ITEMS {
            chat.push(msg(&i.to_string()));
            trim(&mut chat, MAX_ITEMS, MAX_BYTES);
        }
        assert_eq!(chat.len(), MAX_ITEMS);
        assert!(matches!(&chat[0].kind, ChatKind::Message { text, .. } if text == "0"));
    }
}
