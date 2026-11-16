//! Bounds on what one peer can make us hold or do: our outgoing control
//! queue to it, events it caused that the UI hasn't taken yet, and how fast
//! we read its control messages. Excess is never dropped on the receiving
//! side: reading pauses and QUIC flow control pushes back on the sender,
//! whose own queue then refuses chat (and tells the user) instead of growing.

use bytes::Bytes;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// Chat messages and files queued for one peer, in bytes: room for about
/// 30 files of 1 MB dropped at once.
pub const BULK_BUDGET: usize = 32 << 20;
/// Messages queued for one peer. A peer this far behind on state messages
/// isn't reading at all, so its connection is closed.
pub const MAX_QUEUED: usize = 4096;
/// Chat and files stop at half, so state messages always find room.
const MAX_QUEUED_BULK: usize = MAX_QUEUED / 2;

/// Cost of events the UI hasn't taken yet, above which we stop reading
/// from peers.
pub const EVENT_BUDGET: usize = 32 << 20;
/// Added to every event's payload, so a flood of empty ones is bounded too.
pub const EVENT_OVERHEAD: usize = 256;

/// Control messages read from one peer: a burst, then a steady rate.
pub const MSG_BURST: f64 = 100.0;
pub const MSG_RATE: f64 = 20.0;
/// Bytes read from one peer's control stream: a burst, then a steady rate.
pub const BYTE_BURST: f64 = (32 << 20) as f64;
pub const BYTE_RATE: f64 = (4 << 20) as f64;

#[derive(Debug, PartialEq, Eq)]
pub enum Push {
    Queued,
    /// Chat or file over the peer's budget: not queued.
    Skipped,
    /// Too many messages: the queue is closed, close the connection.
    Overflow,
}

/// Encoded control messages waiting to be written to one peer.
pub struct CtrlQueue {
    inner: Mutex<Queue>,
    notify: Notify,
    budget: usize,
}

#[derive(Default)]
struct Queue {
    /// Frames with their bulk cost (0 for state messages).
    frames: VecDeque<(Bytes, usize)>,
    /// Bulk bytes queued or being written.
    bulk: usize,
    closed: bool,
}

impl CtrlQueue {
    pub fn new(budget: usize) -> Self {
        CtrlQueue { inner: Default::default(), notify: Notify::new(), budget }
    }

    /// `bulk` messages (chat, files) are refused over the budget; state
    /// messages are never refused, since losing one would leave the peer
    /// with a wrong picture of us.
    pub fn push(&self, frame: Bytes, bulk: bool) -> Push {
        let mut q = self.inner.lock().unwrap();
        if q.closed {
            return Push::Queued; // the connection is ending anyway
        }
        let cost = if bulk { frame.len() } else { 0 };
        if bulk && (q.bulk + cost > self.budget || q.frames.len() >= MAX_QUEUED_BULK) {
            return Push::Skipped;
        }
        if q.frames.len() >= MAX_QUEUED {
            q.closed = true;
            q.frames.clear();
            q.bulk = 0;
            drop(q);
            self.notify.notify_one();
            return Push::Overflow;
        }
        q.bulk += cost;
        q.frames.push_back((frame, cost));
        drop(q);
        self.notify.notify_one();
        Push::Queued
    }

    fn try_next(&self) -> Option<Option<(Bytes, usize)>> {
        let mut q = self.inner.lock().unwrap();
        if q.closed {
            return Some(None);
        }
        q.frames.pop_front().map(Some)
    }

    /// Next frame for the (single) writer; `None` once closed. Its bulk
    /// cost stays counted until `sent`.
    pub async fn next(&self) -> Option<(Bytes, usize)> {
        loop {
            if let Some(next) = self.try_next() {
                return next;
            }
            self.notify.notified().await;
        }
    }

    /// A frame from `next` has been written.
    pub fn sent(&self, cost: usize) {
        let mut q = self.inner.lock().unwrap();
        q.bulk = q.bulk.saturating_sub(cost);
    }

    pub fn close(&self) {
        let mut q = self.inner.lock().unwrap();
        q.closed = true;
        q.frames.clear();
        drop(q);
        self.notify.notify_one();
    }

    #[cfg(test)]
    pub fn bulk_bytes(&self) -> usize {
        self.inner.lock().unwrap().bulk
    }
}

/// Cost of events sent to the UI and not yet taken.
pub struct Pending {
    cost: AtomicUsize,
    budget: usize,
    notify: Notify,
}

impl Pending {
    pub fn new(budget: usize) -> Self {
        Pending { cost: AtomicUsize::new(0), budget, notify: Notify::new() }
    }

    pub fn add(&self, n: usize) {
        self.cost.fetch_add(n, Ordering::SeqCst);
    }

    pub fn remove(&self, n: usize) {
        let before = self.cost.fetch_sub(n, Ordering::SeqCst);
        if before > self.budget && before - n <= self.budget {
            self.notify.notify_waiters();
        }
    }

    pub fn get(&self) -> usize {
        self.cost.load(Ordering::SeqCst)
    }

    /// Returns once the cost is within the budget.
    pub async fn room(&self) {
        loop {
            let notified = self.notify.notified();
            let mut notified = std::pin::pin!(notified);
            // Registered before the check, so a `remove` in between wakes us.
            notified.as_mut().enable();
            if self.get() <= self.budget {
                return;
            }
            notified.await;
        }
    }
}

/// Runs `f` unless `closed` completes first; true if `f` finished.
pub async fn unless_closed(f: impl Future<Output = ()>, closed: impl Future) -> bool {
    tokio::select! {
        _ = f => true,
        _ = closed => false,
    }
}

/// Token bucket. Time comes from the caller; `take` may overdraw, the
/// debt is paid off before the next `wait` returns zero.
pub struct Bucket {
    burst: f64,
    rate: f64,
    tokens: f64,
    at: Instant,
}

impl Bucket {
    pub fn new(burst: f64, rate: f64, now: Instant) -> Self {
        Bucket { burst, rate, tokens: burst, at: now }
    }

    fn refill(&mut self, now: Instant) {
        if now > self.at {
            let dt = (now - self.at).as_secs_f64();
            self.tokens = (self.tokens + dt * self.rate).min(self.burst);
            self.at = now;
        }
    }

    /// How long until `n` tokens are there.
    pub fn wait(&mut self, now: Instant, n: f64) -> Duration {
        self.refill(now);
        if self.tokens >= n { Duration::ZERO } else { Duration::from_secs_f64((n - self.tokens) / self.rate) }
    }

    pub fn take(&mut self, now: Instant, n: f64) {
        self.refill(now);
        self.tokens -= n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(n: usize) -> Bytes {
        Bytes::from(vec![0u8; n])
    }

    #[test]
    fn bulk_over_budget_is_skipped() {
        let q = CtrlQueue::new(1000);
        assert_eq!(q.push(frame(600), true), Push::Queued);
        assert_eq!(q.push(frame(600), true), Push::Skipped);
        assert_eq!(q.push(frame(400), true), Push::Queued);
        assert_eq!(q.bulk_bytes(), 1000);
        // State messages don't count against the budget and always go in.
        assert_eq!(q.push(frame(5000), false), Push::Queued);
        assert_eq!(q.bulk_bytes(), 1000);
    }

    #[tokio::test]
    async fn budget_returns_after_write() {
        let q = CtrlQueue::new(1000);
        assert_eq!(q.push(frame(1000), true), Push::Queued);
        let (f, cost) = q.next().await.unwrap();
        assert_eq!((f.len(), cost), (1000, 1000));
        // Taken but not written yet: still counted.
        assert_eq!(q.push(frame(1), true), Push::Skipped);
        q.sent(cost);
        assert_eq!(q.push(frame(1000), true), Push::Queued);
    }

    #[test]
    fn bulk_leaves_room_for_state() {
        let q = CtrlQueue::new(usize::MAX);
        for _ in 0..MAX_QUEUED_BULK {
            assert_eq!(q.push(frame(0), true), Push::Queued);
        }
        assert_eq!(q.push(frame(0), true), Push::Skipped);
        assert_eq!(q.push(frame(0), false), Push::Queued);
    }

    #[tokio::test]
    async fn hard_limit_closes() {
        let q = CtrlQueue::new(BULK_BUDGET);
        for _ in 0..MAX_QUEUED {
            assert_eq!(q.push(frame(1), false), Push::Queued);
        }
        assert_eq!(q.push(frame(1), false), Push::Overflow);
        assert!(q.next().await.is_none(), "closed");
    }

    #[tokio::test]
    async fn close_wakes_writer() {
        let q = std::sync::Arc::new(CtrlQueue::new(BULK_BUDGET));
        let q2 = q.clone();
        let writer = tokio::spawn(async move { q2.next().await.is_none() });
        tokio::time::sleep(Duration::from_millis(20)).await;
        q.close();
        assert!(tokio::time::timeout(Duration::from_secs(1), writer).await.unwrap().unwrap());
    }

    #[tokio::test]
    async fn pending_room() {
        let p = std::sync::Arc::new(Pending::new(1000));
        p.add(600);
        p.room().await; // within budget
        p.add(600);
        assert_eq!(p.get(), 1200);
        let p2 = p.clone();
        let waiter = tokio::spawn(async move { p2.room().await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "over the budget");
        p.remove(100);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "still over");
        p.remove(100);
        tokio::time::timeout(Duration::from_secs(1), waiter).await.unwrap().unwrap();
        assert_eq!(p.get(), 1000);
    }

    #[tokio::test]
    async fn waiting_ends_on_close() {
        let p = Pending::new(10);
        p.add(11);
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let wait = unless_closed(p.room(), rx);
        let mut wait = std::pin::pin!(wait);
        assert!(tokio::time::timeout(Duration::from_millis(50), wait.as_mut()).await.is_err());
        drop(tx);
        assert!(!tokio::time::timeout(Duration::from_secs(1), wait).await.unwrap());
        assert!(unless_closed(async {}, std::future::pending::<()>()).await);
    }

    #[test]
    fn bucket() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let ms = |d: Duration| (d.as_secs_f64() * 1000.0).round() as u64;
        let mut b = Bucket::new(100.0, 20.0, t0);
        for _ in 0..100 {
            assert_eq!(b.wait(t0, 1.0), Duration::ZERO);
            b.take(t0, 1.0);
        }
        // Burst used up: one token every 50 ms.
        assert_eq!(ms(b.wait(t0, 1.0)), 50);
        assert_eq!(b.wait(t0 + Duration::from_millis(50), 1.0), Duration::ZERO);
        // Refills to the burst, not beyond.
        assert_eq!(b.wait(t0 + s(3600), 100.0), Duration::ZERO);
        assert_eq!(ms(b.wait(t0 + s(3600), 101.0)), 50);

        // Overdraft: waits until the debt is paid.
        let mut b = Bucket::new(10.0, 5.0, t0);
        b.take(t0, 30.0);
        assert_eq!(ms(b.wait(t0, 0.0)), 4000);
        assert_eq!(b.wait(t0 + s(4), 0.0), Duration::ZERO);
    }
}
