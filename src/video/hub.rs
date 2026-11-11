//! Fan-out of a raw Annex B (H.264 / HEVC) bitstream to many viewers.
//!
//! The hub keeps the latest parameter sets and everything since the last
//! keyframe, so a viewer that joins mid-stream starts instantly instead of
//! waiting for the next keyframe. A viewer whose network can't keep up is
//! dropped back to "waiting for keyframe" instead of building up latency.

use crate::proto::Codec;
use bytes::{Bytes, BytesMut};
use std::collections::BTreeMap;
use std::sync::Mutex;
use tokio::sync::mpsc;

pub(super) const START_CODE: [u8; 4] = [0, 0, 0, 1];
const MAX_GOP_CACHE: usize = 48 << 20;
/// Chunks of encoder output buffered per viewer before it is considered
/// too slow (one chunk is roughly one frame).
const VIEWER_QUEUE: usize = 90;

pub struct Hub {
    inner: Mutex<Inner>,
}

struct Inner {
    scanner: Scanner,
    params: BTreeMap<u8, Vec<u8>>,
    gop: Vec<Bytes>,
    gop_bytes: usize,
    gop_valid: bool,
    viewers: Vec<Viewer>,
    closed: bool,
}

struct Viewer {
    tx: mpsc::Sender<Bytes>,
    synced: bool,
}

impl Hub {
    pub fn new(codec: Codec) -> Self {
        Hub {
            inner: Mutex::new(Inner {
                scanner: Scanner::new(codec),
                params: BTreeMap::new(),
                gop: Vec::new(),
                gop_bytes: 0,
                gop_valid: false,
                viewers: Vec::new(),
                closed: false,
            }),
        }
    }

    pub fn subscribe(&self) -> mpsc::Receiver<Bytes> {
        let (tx, rx) = mpsc::channel(VIEWER_QUEUE + 1);
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return rx;
        }
        let mut synced = false;
        if inner.gop_valid && !inner.gop.is_empty() {
            let mut all = BytesMut::with_capacity(inner.gop_bytes);
            for b in &inner.gop {
                all.extend_from_slice(b);
            }
            synced = tx.try_send(all.freeze()).is_ok();
        }
        inner.viewers.push(Viewer { tx, synced });
        rx
    }

    pub fn viewer_count(&self) -> usize {
        let inner = self.inner.lock().unwrap();
        inner.viewers.iter().filter(|v| !v.tx.is_closed()).count()
    }

    /// Ends the stream: all viewers see their channel close.
    pub fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.closed = true;
        inner.viewers.clear();
    }

    /// A new encoder took over (the old one died): its bitstream starts from
    /// scratch with fresh parameter sets and a keyframe. Viewers stay
    /// subscribed and simply continue with the new bitstream.
    pub fn restart(&self) {
        let mut inner = self.inner.lock().unwrap();
        let codec = inner.scanner.codec;
        inner.scanner = Scanner::new(codec);
        inner.params.clear();
        inner.gop.clear();
        inner.gop_bytes = 0;
        inner.gop_valid = false;
    }

    /// Feeds a chunk of encoder output (any size, any alignment).
    pub fn push(&self, chunk: Bytes) {
        let mut inner = self.inner.lock().unwrap();
        let inner = &mut *inner;
        let out = inner.scanner.scan(&chunk);
        for (ty, nal) in out.params {
            inner.params.insert(ty, nal);
        }
        let (pre, post) = match out.cut {
            Some(at) => (chunk.slice(..at), Some(chunk.slice(at..))),
            None => (chunk, None),
        };

        if !pre.is_empty() {
            if inner.gop_valid {
                inner.gop_bytes += pre.len();
                inner.gop.push(pre.clone());
            }
            send_synced(&mut inner.viewers, &pre);
        }

        if let Some(post) = post {
            // `post` starts at the header of a keyframe NAL. Build a
            // self-contained entry point: parameter sets + start code.
            let mut prefix = BytesMut::new();
            for nal in inner.params.values() {
                prefix.extend_from_slice(nal);
            }
            prefix.extend_from_slice(&START_CODE);
            let prefix = prefix.freeze();

            inner.gop.clear();
            inner.gop.push(prefix.clone());
            inner.gop.push(post.clone());
            inner.gop_bytes = prefix.len() + post.len();
            inner.gop_valid = true;

            inner.viewers.retain_mut(|v| {
                if v.synced {
                    match v.tx.try_send(post.clone()) {
                        Ok(()) => true,
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            v.synced = false;
                            true
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => false,
                    }
                } else {
                    // Resync at this keyframe, but only if there is room
                    // for the whole entry point.
                    if v.tx.is_closed() {
                        return false;
                    }
                    if v.tx.capacity() >= 2 {
                        let _ = v.tx.try_send(prefix.clone());
                        let _ = v.tx.try_send(post.clone());
                        v.synced = true;
                    }
                    true
                }
            });
        }

        if inner.gop_bytes > MAX_GOP_CACHE {
            inner.gop.clear();
            inner.gop_bytes = 0;
            inner.gop_valid = false;
        }
    }
}

fn send_synced(viewers: &mut Vec<Viewer>, data: &Bytes) {
    viewers.retain_mut(|v| {
        if !v.synced {
            return !v.tx.is_closed();
        }
        match v.tx.try_send(data.clone()) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                v.synced = false;
                true
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    });
}

/// Incremental Annex B scanner that finds keyframe boundaries and captures
/// parameter sets, without ever holding data back.
pub(super) struct Scanner {
    codec: Codec,
    zeros: u32,
    after_start_code: bool,
    capture: Option<(u8, Vec<u8>)>,
    prev_vcl_irap: bool,
    param_since_vcl: bool,
}

#[derive(Default)]
pub(super) struct ScanOut {
    /// Offset of the header byte of the first keyframe NAL in the chunk.
    pub cut: Option<usize>,
    /// Completed parameter set NALs (type, NAL with 4 byte start code).
    pub params: Vec<(u8, Vec<u8>)>,
}

impl Scanner {
    pub(super) fn new(codec: Codec) -> Self {
        Scanner {
            codec,
            zeros: 0,
            after_start_code: false,
            capture: None,
            prev_vcl_irap: false,
            param_since_vcl: false,
        }
    }

    pub(super) fn scan(&mut self, data: &[u8]) -> ScanOut {
        let mut out = ScanOut::default();
        for (i, &b) in data.iter().enumerate() {
            if self.after_start_code {
                self.after_start_code = false;
                let (ty, is_param, is_vcl, is_irap) = classify(self.codec, b);
                if is_param {
                    self.capture = Some((ty, START_CODE.to_vec()));
                    self.param_since_vcl = true;
                } else if is_vcl {
                    if is_irap
                        && (!self.prev_vcl_irap || self.param_since_vcl)
                        && out.cut.is_none()
                    {
                        out.cut = Some(i);
                    }
                    self.prev_vcl_irap = is_irap;
                    self.param_since_vcl = false;
                }
            }
            if let Some((_, buf)) = &mut self.capture {
                buf.push(b);
                if buf.len() > 64 * 1024 {
                    self.capture = None;
                }
            }
            if b == 0 {
                self.zeros += 1;
            } else {
                if b == 1 && self.zeros >= 2 {
                    if let Some((ty, mut buf)) = self.capture.take() {
                        buf.pop(); // the 0x01 of the next start code
                        while buf.last() == Some(&0) {
                            buf.pop();
                        }
                        if buf.len() > START_CODE.len() + 1 {
                            out.params.push((ty, buf));
                        }
                    }
                    self.after_start_code = true;
                }
                self.zeros = 0;
            }
        }
        out
    }
}

/// Returns (nal type, is parameter set, is VCL, is random access point).
fn classify(codec: Codec, header: u8) -> (u8, bool, bool, bool) {
    match codec {
        Codec::H264 => {
            let t = header & 0x1f;
            (t, t == 7 || t == 8, (1..=5).contains(&t), t == 5)
        }
        Codec::Hevc => {
            let t = (header >> 1) & 0x3f;
            (t, (32..=34).contains(&t), t <= 31, (16..=23).contains(&t))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nal(header: u8, len: usize) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, header];
        v.extend(std::iter::repeat_n(0xAB, len));
        v
    }

    #[test]
    fn finds_keyframes_and_params_across_chunks() {
        let mut stream = Vec::new();
        stream.extend(nal(0x67, 10)); // SPS
        stream.extend(nal(0x68, 4)); // PPS
        stream.extend(nal(0x65, 100)); // IDR
        stream.extend(nal(0x41, 50)); // P
        stream.extend(nal(0x67, 10));
        stream.extend(nal(0x68, 4));
        stream.extend(nal(0x65, 100));

        for split in [1usize, 3, 7, 64, 150] {
            let mut s = Scanner::new(Codec::H264);
            let mut cuts = Vec::new();
            let mut params = 0;
            for (ci, c) in stream.chunks(split).enumerate() {
                let o = s.scan(c);
                if let Some(at) = o.cut {
                    cuts.push(ci * split + at);
                }
                params += o.params.len();
            }
            assert_eq!(cuts, vec![15 + 9 + 4, 15 + 9 + 105 + 55 + 15 + 9 + 4], "split {split}");
            assert_eq!(params, 4);
        }
    }

    #[tokio::test]
    async fn late_viewer_starts_at_keyframe() {
        let hub = Hub::new(Codec::H264);
        let mut stream = Vec::new();
        stream.extend(nal(0x67, 10));
        stream.extend(nal(0x68, 4));
        stream.extend(nal(0x65, 100));
        stream.extend(nal(0x41, 50));
        hub.push(Bytes::from(stream));
        let mut rx = hub.subscribe();
        let first = rx.recv().await.unwrap();
        assert_eq!(&first[..5], &[0, 0, 0, 1, 0x67]);
        assert!(first.windows(5).any(|w| w == [0, 0, 0, 1, 0x65]));
        hub.push(Bytes::from(nal(0x41, 20)));
        let next = rx.recv().await.unwrap();
        assert_eq!(&next[..5], &[0, 0, 0, 1, 0x41]);
    }

    #[tokio::test]
    async fn viewers_continue_after_restart() {
        let hub = Hub::new(Codec::H264);
        let mut old = Vec::new();
        old.extend(nal(0x67, 10));
        old.extend(nal(0x68, 4));
        old.extend(nal(0x65, 100));
        old.extend(nal(0x41, 50));
        hub.push(Bytes::from(old));
        let mut rx = hub.subscribe();
        rx.recv().await.unwrap();

        // The encoder died mid-frame; a new one starts its own bitstream.
        hub.push(Bytes::from(vec![0, 0, 0, 1, 0x41, 0xAB]));
        rx.recv().await.unwrap();
        hub.restart();
        let mut late = hub.subscribe();
        assert!(late.try_recv().is_err(), "old keyframe must not be replayed");

        let mut new = Vec::new();
        new.extend(nal(0x67, 12)); // different SPS (e.g. new resolution)
        new.extend(nal(0x68, 4));
        new.extend(nal(0x65, 80));
        hub.push(Bytes::from(new.clone()));
        let mut got = Vec::new();
        while let Ok(b) = rx.try_recv() {
            got.extend_from_slice(&b);
        }
        assert_eq!(got, new, "a synced viewer gets the new stream untouched");

        // A viewer that joined during the gap starts at the new keyframe
        // with the new parameter sets only.
        let mut first = Vec::new();
        while let Ok(b) = late.try_recv() {
            first.extend_from_slice(&b);
        }
        assert_eq!(first, new);
    }
}