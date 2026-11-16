//! Serverless room: a full mesh of direct, end-to-end encrypted QUIC
//! connections (iroh). NAT hole punching is automatic; n0's public relays
//! are only used for the initial rendezvous or when a direct path is
//! impossible. There is no server of our own.

use crate::flow::{Bucket, CtrlQueue, Push};
use crate::proto::{self, ALPN, Auth, DGRAM_VOICE, Msg, StreamInfo, TAG_CAMERA, TAG_CONTROL, TAG_WATCH};
use crate::video::{self, decode::FrameSlot};
use crate::voice::Voice;
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use iroh::endpoint::{Connection, presets};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, EndpointId};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{OnceCell, mpsc};

type BoxFuture<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'static>>;

#[derive(Debug, Clone)]
pub enum Event {
    Ready,
    /// Our current invite link.
    Ticket { link: String, expires: chrono::DateTime<chrono::Local> },
    /// Joining failed; we are out of the room again.
    JoinFailed(String),
    PeerJoined { id: EndpointId, name: String, avatar: Option<Vec<u8>> },
    PeerProfile { id: EndpointId, name: String, avatar: Option<Vec<u8>> },
    PeerLeft { id: EndpointId },
    PeerStream { id: EndpointId, info: Option<StreamInfo> },
    PeerCamera { id: EndpointId, on: bool },
    PeerVoice { id: EndpointId, on: bool },
    PeerMute { id: EndpointId, muted: bool, deafened: bool },
    RoomName(String),
    Chat { from: Option<EndpointId>, name: String, text: String },
    /// File posted in chat; `file_name` is already cleaned.
    File { from: Option<EndpointId>, name: String, file_name: String, data: Arc<Vec<u8>> },
    MyStream(Option<StreamInfo>),
    MyCamera(bool),
    MyVoice(bool),
    Watching(Option<EndpointId>),
    Status(String),
    Error(String),
}

impl Event {
    /// What the event holds in memory until the UI takes it, roughly.
    fn cost(&self) -> usize {
        let info = |i: &Option<StreamInfo>| i.as_ref().map_or(0, |i| i.encoder.len());
        let avatar = |a: &Option<Vec<u8>>| a.as_ref().map_or(0, |a| a.len());
        // Every variant listed: a new one with data must be counted here.
        crate::flow::EVENT_OVERHEAD
            + match self {
                Event::Chat { name, text, .. } => name.len() + text.len(),
                Event::File { name, file_name, data, .. } => name.len() + file_name.len() + data.len(),
                Event::PeerJoined { name, avatar: a, .. } | Event::PeerProfile { name, avatar: a, .. } => {
                    name.len() + avatar(a)
                }
                Event::PeerStream { info: i, .. } | Event::MyStream(i) => info(i),
                Event::Ticket { link: s, .. }
                | Event::JoinFailed(s)
                | Event::RoomName(s)
                | Event::Status(s)
                | Event::Error(s) => s.len(),
                Event::Ready
                | Event::PeerLeft { .. }
                | Event::PeerCamera { .. }
                | Event::PeerVoice { .. }
                | Event::PeerMute { .. }
                | Event::MyCamera(_)
                | Event::MyVoice(_)
                | Event::Watching(_) => 0,
            }
    }
}

/// Delivers events to the UI and wakes it up. Counts what the UI hasn't
/// taken yet, so peer readers can wait while it is behind (e.g. a hidden
/// window that draws no frames).
#[derive(Clone)]
pub struct Events {
    tx: std::sync::mpsc::Sender<(Event, usize)>,
    ctx: eframe::egui::Context,
    pending: Arc<crate::flow::Pending>,
}

/// The UI end of `Events`.
pub struct EventRx {
    rx: std::sync::mpsc::Receiver<(Event, usize)>,
    pending: Arc<crate::flow::Pending>,
}

impl EventRx {
    pub fn try_recv(&self) -> Option<Event> {
        let (e, cost) = self.rx.try_recv().ok()?;
        self.pending.remove(cost);
        Some(e)
    }

    #[cfg(test)]
    pub fn try_iter(&self) -> impl Iterator<Item = Event> + '_ {
        std::iter::from_fn(|| self.try_recv())
    }
}

impl Events {
    pub fn new(ctx: eframe::egui::Context) -> (Self, EventRx) {
        Self::with_budget(ctx, crate::flow::EVENT_BUDGET)
    }

    pub fn with_budget(ctx: eframe::egui::Context, budget: usize) -> (Self, EventRx) {
        let (tx, rx) = std::sync::mpsc::channel();
        let pending = Arc::new(crate::flow::Pending::new(budget));
        (Events { tx, ctx, pending: pending.clone() }, EventRx { rx, pending })
    }

    pub fn send(&self, e: Event) {
        let cost = e.cost();
        // Counted before it can be taken, so the count never goes below zero.
        self.pending.add(cost);
        if self.tx.send((e, cost)).is_err() {
            self.pending.remove(cost);
        }
        self.ctx.request_repaint();
    }

    pub fn repaint(&self) {
        self.ctx.request_repaint();
    }

    /// Returns once the UI has caught up to within the budget.
    async fn room(&self) {
        self.pending.room().await;
    }

    #[cfg(test)]
    fn pending(&self) -> usize {
        self.pending.get()
    }
}

struct PeerConn {
    name: String,
    conn: Connection,
    ctrl: Arc<CtrlQueue>,
    dialer: EndpointId,
    voice: bool,
    stream: Option<StreamInfo>,
    camera: Option<StreamInfo>,
}

/// Local mute/deafen state, kept even when not in voice.
#[derive(Debug, Clone, Copy, Default)]
pub struct MuteState {
    pub muted: bool,
    pub deafened: bool,
}

struct Watch {
    peer: EndpointId,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
struct State {
    in_room: bool,
    name: String,
    /// Our profile picture (PNG), kept across rooms.
    avatar: Option<Vec<u8>>,
    room: String,
    mute: MuteState,
    devices: crate::voice::Devices,
    voice_prefs: crate::voice::Prefs,
    peers: HashMap<EndpointId, PeerConn>,
    dialing: HashSet<EndpointId>,
    broadcast: Option<video::encode::Broadcast>,
    camera: Option<video::encode::Broadcast>,
    /// Decoded cameras shown in voice tiles, ours (preview) included.
    cam_views: HashMap<EndpointId, (Arc<FrameSlot>, tokio::task::JoinHandle<()>)>,
    watch: Option<Watch>,
    voice: Option<Arc<Voice>>,
    /// A voice start is in flight; its result is kept only if `voice_gen`
    /// is still the value it started with.
    voice_starting: bool,
    voice_gen: u64,
    /// Bumped on every room change: streams, cameras and connections
    /// started in an earlier room are discarded when they complete.
    room_gen: u64,
    invite: Option<Invite>,
    /// Shared by everyone in the room; proves membership for mesh links.
    room_secret: Option<[u8; 32]>,
}

impl State {
    fn active_invite(&self) -> Option<&Invite> {
        self.invite.as_ref().filter(|i| Instant::now() < i.valid_until)
    }
}

/// Our active invite (one per node).
struct Invite {
    token: [u8; 16],
    /// Monotonic, so changing the system clock doesn't extend it.
    valid_until: Instant,
    /// Unix seconds, shown in the link.
    expires: u32,
}

const INVITE_TTL: Duration = Duration::from_secs(24 * 3600);
const AUTH_TIMEOUT: Duration = Duration::from_secs(15);
const HELLO_TIMEOUT: Duration = Duration::from_secs(15);
/// Incoming connections that haven't authenticated yet.
const MAX_PENDING: usize = 32;

/// The other side refused our credentials.
#[derive(Debug)]
struct Denied;

impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("denied")
    }
}

impl std::error::Error for Denied {}

/// Context on errors from setting up the connection itself (before any
/// authentication), the only ones worth retrying.
#[derive(Debug)]
struct ConnectFailed;

impl std::fmt::Display for ConnectFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("connect failed")
    }
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(25);
const DIAL_ATTEMPTS: u32 = 3;
const DIAL_RETRY_DELAY: Duration = Duration::from_secs(2);
const DIAL_RETRY_WINDOW: Duration = Duration::from_secs(20);

fn alpn_mismatch(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("doesn't support any known protocol")
}

/// Whether a dial (join or mesh) that failed with `e` after `attempts`
/// tries is retried.
fn retry_dial(e: &anyhow::Error, attempts: u32, elapsed: Duration, same_room: bool) -> bool {
    same_room
        && attempts < DIAL_ATTEMPTS
        && elapsed < DIAL_RETRY_WINDOW
        && e.downcast_ref::<ConnectFailed>().is_some()
        && !alpn_mismatch(e)
}

/// What we prove when dialing.
#[derive(Clone, Copy)]
enum Cred {
    Invite([u8; 16]),
    Member,
}

/// Counts a pending incoming connection while alive.
struct Pending<'a>(&'a AtomicUsize);

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn exporter(conn: &Connection) -> Result<[u8; 32]> {
    let mut out = [0u8; 32];
    conn.export_keying_material(&mut out, b"bibicord/7 auth", b"")
        .map_err(|e| anyhow::anyhow!("keying material: {e:?}"))?;
    Ok(out)
}

#[derive(Clone)]
pub struct Node(Arc<Inner>);

struct Inner {
    rt: tokio::runtime::Handle,
    endpoint: OnceCell<(Endpoint, Router)>,
    events: Events,
    state: Mutex<State>,
    pub frames: Arc<FrameSlot>,
    key: iroh::SecretKey,
    pending: AtomicUsize,
    /// Authenticated connections waiting for the UI to catch up.
    at_gate: AtomicUsize,
    /// Per-peer budget of queued chat and files (`flow::BULK_BUDGET`).
    bulk_budget: usize,
}

#[derive(Debug, Clone)]
struct Handler(std::sync::Weak<Inner>);

impl ProtocolHandler for Handler {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let Some(inner) = self.0.upgrade() else { return Ok(()) };
        let node = Node(inner);
        if node.0.pending.fetch_add(1, Ordering::SeqCst) >= MAX_PENDING {
            node.0.pending.fetch_sub(1, Ordering::SeqCst);
            conn.close(4u32.into(), b"busy");
            return Ok(());
        }
        let pending = Pending(&node.0.pending);
        let authed = node.accept_auth(&conn).await;
        drop(pending);
        let res = match authed {
            Ok((send, recv, room_gen)) => node.run_peer(conn, false, send, recv, room_gen).await,
            Err(e) => {
                conn.close(2u32.into(), b"denied");
                Err(e)
            }
        };
        if let Err(e) = res {
            tracing::debug!("incoming peer ended: {e:#}");
        }
        Ok(())
    }
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Node")
    }
}

impl Node {
    pub fn new(rt: tokio::runtime::Handle, events: Events, key: iroh::SecretKey) -> Self {
        Self::with_bulk_budget(rt, events, key, crate::flow::BULK_BUDGET)
    }

    fn with_bulk_budget(rt: tokio::runtime::Handle, events: Events, key: iroh::SecretKey, bulk_budget: usize) -> Self {
        let node = Node(Arc::new(Inner {
            rt,
            endpoint: OnceCell::new(),
            events,
            state: Mutex::new(State::default()),
            frames: Arc::new(FrameSlot::default()),
            key,
            pending: AtomicUsize::new(0),
            at_gate: AtomicUsize::new(0),
            bulk_budget,
        }));
        let n = node.clone();
        node.0.rt.spawn(async move {
            match n.endpoint().await {
                Ok(_) => n.0.events.send(Event::Ready),
                Err(e) => n.0.events.send(Event::Error(format!("Sieć: {e:#}"))),
            }
        });
        node
    }

    pub fn frames(&self) -> Arc<FrameSlot> {
        self.0.frames.clone()
    }

    async fn endpoint(&self) -> Result<&Endpoint> {
        let weak = Arc::downgrade(&self.0);
        let (ep, _) = self
            .0
            .endpoint
            .get_or_try_init(|| async move {
                let ep = Endpoint::builder(presets::N0)
                    .secret_key(self.0.key.clone())
                    .alpns(vec![ALPN.to_vec()])
                    .bind()
                    .await?;
                let router = Router::builder(ep.clone()).accept(ALPN, Handler(weak)).spawn();
                anyhow::Ok((ep, router))
            })
            .await?;
        Ok(ep)
    }

    pub fn my_id(&self) -> Option<EndpointId> {
        self.0.endpoint.get().map(|(e, _)| e.id())
    }

    /// Graceful shutdown (call from a non-async thread).
    pub fn shutdown(&self) {
        self.leave();
        if let Some((_, router)) = self.0.endpoint.get() {
            let router = router.clone();
            let _ = self.0.rt.block_on(async move {
                tokio::time::timeout(Duration::from_secs(1), router.shutdown()).await
            });
        }
    }

    fn spawn(&self, f: impl std::future::Future<Output = ()> + Send + 'static) {
        self.0.rt.spawn(f);
    }

    fn emit(&self, e: Event) {
        self.0.events.send(e);
    }

    // ---------------------------------------------------------------- room

    pub fn create_room(&self, name: String, room: String) {
        {
            let mut s = self.0.state.lock().unwrap();
            s.in_room = true;
            s.name = name;
            s.room = room.clone();
            s.room_gen += 1;
            s.room_secret = Some(proto::random32());
        }
        self.emit(Event::RoomName(room));
        self.new_invite();
    }

    pub fn join(&self, link: &str, name: String) -> Result<()> {
        let invite = proto::parse_invite(link)?;
        if invite.id == self.0.key.public() {
            bail!("To Twoje własne zaproszenie");
        }
        let room_gen = {
            let mut s = self.0.state.lock().unwrap();
            s.in_room = true;
            s.name = name;
            s.room.clear();
            s.room_gen += 1;
            // Until `Welcome` we have neither: every incoming connection is refused.
            s.room_secret = None;
            s.invite = None;
            s.room_gen
        };
        let n = self.clone();
        self.spawn(async move {
            n.emit(Event::Status("Łączenie z pokojem…".into()));
            match n.dial_retry(EndpointAddr::from(invite.id), Cred::Invite(invite.token), room_gen).await {
                Ok(()) => {
                    if n.0.state.lock().unwrap().room_gen == room_gen {
                        n.new_invite(); // everyone in a room can invite
                    }
                }
                Err(e) => {
                    tracing::info!("join failed: {e:#}");
                    let unix = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
                    let expired = u64::from(invite.expires) <= unix;
                    let msg = if alpn_mismatch(&e) {
                        "Znajomy ma inną wersję BibiCord. Zaktualizujcie aplikację do najnowszej wersji."
                    } else if expired {
                        "Zaproszenie wygasło. Poproś o nowy link."
                    } else if e.downcast_ref::<Denied>().is_some() {
                        "Zaproszenie jest nieważne albo wygasło. Poproś o nowy link."
                    } else {
                        "Nie udało się dołączyć. Sprawdź, czy osoba, która wysłała link, ma otwarty BibiCord."
                    };
                    if n.leave_if(room_gen) {
                        n.emit(Event::Status(String::new()));
                        n.emit(Event::JoinFailed(msg.into()));
                    }
                }
            }
        });
        Ok(())
    }

    /// Replaces our invite: the previous link stops working at once.
    pub fn new_invite(&self) {
        {
            let mut s = self.0.state.lock().unwrap();
            if !s.in_room {
                return;
            }
            let unix = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
            let token: [u8; 16] = proto::random32()[..16].try_into().unwrap();
            let expires = (unix + INVITE_TTL).as_secs().min(u32::MAX as u64) as u32;
            s.invite = Some(Invite { token, valid_until: Instant::now() + INVITE_TTL, expires });
        }
        self.publish_invite();
    }

    /// Makes a new invite only if there is none or it expired.
    pub fn ensure_invite(&self) {
        if self.valid_invite().is_some() {
            self.publish_invite();
        } else {
            self.new_invite();
        }
    }

    /// Token of our invite, if it is still valid.
    fn valid_invite(&self) -> Option<[u8; 16]> {
        self.0.state.lock().unwrap().active_invite().map(|i| i.token)
    }

    /// Makes our invite expire now.
    #[cfg(test)]
    fn age_invite(&self) {
        if let Some(i) = &mut self.0.state.lock().unwrap().invite {
            i.valid_until = Instant::now();
        }
    }

    /// Emits the current link once the endpoint is reachable.
    fn publish_invite(&self) {
        let n = self.clone();
        self.spawn(async move {
            let Ok(ep) = n.endpoint().await else { return };
            // Joiners find us via address lookup, which is published once
            // the endpoint has a home relay.
            let _ = tokio::time::timeout(Duration::from_secs(8), ep.online()).await;
            let id = ep.id();
            let link = {
                let s = n.0.state.lock().unwrap();
                let Some(i) = &s.invite else { return };
                let link = proto::format_invite(&proto::InviteLink { id, token: i.token, expires: i.expires });
                let expires = chrono::DateTime::from_timestamp(i.expires as i64, 0).unwrap_or_default();
                (link, expires.with_timezone(&chrono::Local))
            };
            n.emit(Event::Ticket { link: link.0, expires: link.1 });
        });
    }

    /// `leave()` unless the room changed since `room_gen`.
    fn leave_if(&self, room_gen: u64) -> bool {
        let mut s = self.0.state.lock().unwrap();
        let same = s.room_gen == room_gen;
        if same {
            Self::leave_state(&mut s);
        }
        same
    }

    pub fn leave(&self) {
        Self::leave_state(&mut self.0.state.lock().unwrap());
    }

    fn leave_state(s: &mut State) {
        s.in_room = false;
        s.room.clear();
        for (_, p) in s.peers.drain() {
            p.conn.close(0u32.into(), b"bye");
        }
        s.dialing.clear();
        s.broadcast = None;
        s.camera = None;
        for (_, (_, task)) in s.cam_views.drain() {
            task.abort();
        }
        if let Some(w) = s.watch.take() {
            w.task.abort();
        }
        s.voice = None;
        s.voice_starting = false;
        s.voice_gen += 1;
        s.room_gen += 1;
        s.invite = None;
        s.room_secret = None;
    }

    /// Posts a chat message to everyone in the room; at most `MAX_CHAT_BYTES`.
    /// Returns the names of people it couldn't be queued for (see `queue_all`).
    pub fn send_chat(&self, text: String) -> Result<Vec<String>> {
        if text.len() > proto::MAX_CHAT_BYTES {
            bail!("Wiadomość jest za długa (maks. 1 MB)");
        }
        let msg = Msg::Chat { text };
        let s = self.0.state.lock().unwrap();
        let skipped = Self::queue_all(&s, &msg, true);
        let name = s.name.clone();
        drop(s);
        let Msg::Chat { text } = msg else { unreachable!() };
        self.emit(Event::Chat { from: None, name, text });
        Ok(skipped)
    }

    /// Posts a file to everyone in the room; at most `MAX_FILE_BYTES`.
    /// Returns the names of people it couldn't be queued for.
    pub fn send_file(&self, file_name: &str, data: Vec<u8>) -> Result<Vec<String>> {
        if data.len() > proto::MAX_FILE_BYTES {
            bail!("Plik jest za duży (maks. 1 MB)");
        }
        let file_name = proto::clean_file_name(file_name);
        let msg = Msg::File { name: file_name.clone(), data };
        let s = self.0.state.lock().unwrap();
        let skipped = Self::queue_all(&s, &msg, true);
        let name = s.name.clone();
        drop(s);
        let Msg::File { data, .. } = msg else { unreachable!() };
        self.emit(Event::File { from: None, name, file_name, data: Arc::new(data) });
        Ok(skipped)
    }

    /// Sets the picture sent to peers we meet from now on (no broadcast).
    pub fn set_avatar(&self, avatar: Option<Vec<u8>>) {
        self.0.state.lock().unwrap().avatar = avatar;
    }

    /// Changes name and picture and tells everyone in the room.
    pub fn set_profile(&self, name: String, avatar: Option<Vec<u8>>) {
        {
            let mut s = self.0.state.lock().unwrap();
            s.name = name.clone();
            s.avatar = avatar.clone();
        }
        self.broadcast_msg(Msg::Profile { name, avatar });
    }

    fn hello(&self, room_gen: u64) -> Option<Msg> {
        let s = self.0.state.lock().unwrap();
        (s.in_room && s.room_gen == room_gen).then(|| Self::hello_of(&s))
    }

    fn hello_of(s: &State) -> Msg {
        Msg::Hello {
            name: s.name.clone(),
            room: s.room.clone(),
            peers: s.peers.keys().copied().collect(),
            stream: s.broadcast.as_ref().map(|b| b.info.clone()),
            camera: s.camera.as_ref().map(|b| b.info.clone()),
            voice: s.voice.is_some(),
            muted: s.mute.muted,
            deafened: s.mute.deafened,
            avatar: s.avatar.clone(),
        }
    }

    fn broadcast_msg(&self, msg: Msg) {
        Self::queue_all(&self.0.state.lock().unwrap(), &msg, false);
    }

    /// Encodes `msg` once and queues it for every peer. `bulk` (chat, files)
    /// is skipped for peers whose queue is over budget: they aren't keeping
    /// up, and holding more for them would grow without bound. Returns their
    /// names. State messages always go in; a peer whose queue overflows
    /// anyway reads nothing and is disconnected.
    fn queue_all(s: &State, msg: &Msg, bulk: bool) -> Vec<String> {
        let frame = match proto::encode_msg(msg) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("encode: {e:#}");
                return Vec::new();
            }
        };
        let mut skipped = Vec::new();
        for (id, p) in &s.peers {
            match p.ctrl.push(frame.clone(), bulk) {
                Push::Queued => {}
                Push::Skipped => skipped.push(p.name.clone()),
                Push::Overflow => {
                    tracing::warn!("{id} reads nothing, closing");
                    p.conn.close(5u32.into(), b"backlog");
                }
            }
        }
        skipped.sort();
        skipped
    }

    fn dial(&self, addr: EndpointAddr, cred: Cred, room_gen: u64) -> BoxFuture<Result<()>> {
        let n = self.clone();
        Box::pin(async move { n.dial_inner(addr, cred, room_gen).await })
    }

    /// `dial`, retried while `retry_dial` allows.
    async fn dial_retry(&self, addr: EndpointAddr, cred: Cred, room_gen: u64) -> Result<()> {
        let started = Instant::now();
        let mut attempts = 1;
        loop {
            match self.dial(addr.clone(), cred, room_gen).await {
                Err(e) if retry_dial(&e, attempts, started.elapsed(), self.0.state.lock().unwrap().room_gen == room_gen) => {
                    tracing::info!("dial {} attempt {attempts} failed, retrying: {e:#}", addr.id);
                    attempts += 1;
                    tokio::time::sleep(DIAL_RETRY_DELAY).await;
                }
                res => return res,
            }
        }
    }

    /// Connects and authenticates for the room `room_gen`; the rest of the
    /// session runs in the background. Fails with `Denied` if the other side
    /// refused us, with `ConnectFailed` if we couldn't reach it.
    async fn dial_inner(&self, addr: EndpointAddr, cred: Cred, room_gen: u64) -> Result<()> {
        let id = addr.id;
        if id == self.0.key.public() {
            if let Cred::Invite(_) = cred {
                bail!("own invite");
            }
            return Ok(());
        }
        {
            let mut s = self.0.state.lock().unwrap();
            if !s.in_room || s.room_gen != room_gen {
                bail!("room changed");
            }
            if s.peers.contains_key(&id) || !s.dialing.insert(id) {
                return Ok(());
            }
        }
        let res = async {
            let ep = self.endpoint().await?.clone();
            let conn = match tokio::time::timeout(CONNECT_TIMEOUT, ep.connect(addr, ALPN)).await {
                Ok(Ok(conn)) => conn,
                Ok(Err(e)) => return Err(anyhow::Error::from(e).context(ConnectFailed)),
                Err(e) => return Err(anyhow::Error::from(e).context(ConnectFailed)),
            };
            match tokio::time::timeout(AUTH_TIMEOUT, self.dial_auth(&conn, cred, room_gen)).await {
                Ok(Ok((send, recv))) => Ok((conn, send, recv)),
                Ok(Err(e)) => {
                    conn.close(2u32.into(), b"auth");
                    Err(e)
                }
                Err(e) => {
                    conn.close(2u32.into(), b"auth");
                    Err(e.into())
                }
            }
        }
        .await;
        self.0.state.lock().unwrap().dialing.remove(&id);
        let (conn, send, recv) = res?;
        let n = self.clone();
        self.spawn(async move {
            if let Err(e) = n.run_peer(conn, true, send, recv, room_gen).await {
                tracing::debug!("outgoing peer ended: {e:#}");
            }
        });
        Ok(())
    }

    /// Caller side: `Auth`, then check the callee's proof in `Welcome`.
    async fn dial_auth(
        &self,
        conn: &Connection,
        cred: Cred,
        room_gen: u64,
    ) -> Result<(iroh::endpoint::SendStream, iroh::endpoint::RecvStream)> {
        let (me, remote) = (self.endpoint().await?.id(), conn.remote_id());
        let ex = exporter(conn)?;
        let key = match cred {
            Cred::Invite(token) => proto::invite_key(&token),
            Cred::Member => {
                let s = self.0.state.lock().unwrap();
                match s.room_secret {
                    Some(secret) if s.room_gen == room_gen => proto::room_key(&secret),
                    _ => bail!("not in this room"),
                }
            }
        };
        let proof = proto::proof(&key, proto::ROLE_CALLER, &ex, &me, &remote);
        let auth = match cred {
            Cred::Invite(_) => Auth::Invite { proof },
            Cred::Member => Auth::Member { proof },
        };
        let (mut send, mut recv) = conn.open_bi().await?;
        send.write_all(&[TAG_CONTROL]).await?;
        proto::write_msg(&mut send, &Msg::Auth(auth)).await?;
        let (theirs, secret) = match proto::read_msg(&mut recv).await? {
            Msg::Welcome { proof, room_secret } => (proof, room_secret),
            Msg::Denied => return Err(Denied.into()),
            _ => bail!("expected welcome"),
        };
        if !proto::proof_eq(&theirs, &proto::proof(&key, proto::ROLE_CALLEE, &ex, &me, &remote)) {
            bail!("bad welcome proof");
        }
        if let Cred::Invite(_) = cred {
            let secret = secret.context("no room secret")?;
            let mut s = self.0.state.lock().unwrap();
            if !s.in_room || s.room_gen != room_gen {
                bail!("left while joining");
            }
            s.room_secret = Some(secret);
        }
        Ok((send, recv))
    }

    /// Callee side: checks `Auth` before anything about the room is sent.
    /// Returns the control stream and the room it was checked against.
    async fn accept_auth(
        &self,
        conn: &Connection,
    ) -> Result<(iroh::endpoint::SendStream, iroh::endpoint::RecvStream, u64)> {
        let (me, remote) = (self.endpoint().await?.id(), conn.remote_id());
        let ex = exporter(conn)?;
        let (mut send, recv, auth) = tokio::time::timeout(AUTH_TIMEOUT, async {
            let (s, mut r) = conn.accept_bi().await?;
            let mut tag = [0u8];
            r.read_exact(&mut tag).await?;
            if tag[0] != TAG_CONTROL {
                bail!("unexpected first stream");
            }
            let msg = proto::read_msg(&mut r).await?;
            anyhow::Ok((s, r, msg))
        })
        .await??;
        let checked = {
            let s = self.0.state.lock().unwrap();
            let ok = |key: [u8; 32], proof: &[u8; 32]| {
                proto::proof_eq(proof, &proto::proof(&key, proto::ROLE_CALLER, &ex, &remote, &me)).then_some(key)
            };
            match (&auth, s.active_invite(), s.room_secret) {
                (Msg::Auth(Auth::Invite { proof }), Some(i), Some(secret)) if s.in_room => {
                    ok(proto::invite_key(&i.token), proof).map(|k| (k, Some(secret), s.room_gen))
                }
                (Msg::Auth(Auth::Member { proof }), _, Some(secret)) if s.in_room => {
                    ok(proto::room_key(&secret), proof).map(|k| (k, None, s.room_gen))
                }
                _ => None,
            }
        };
        let Some((key, room_secret, room_gen)) = checked else {
            let _ = proto::write_msg(&mut send, &Msg::Denied).await;
            let _ = send.finish();
            // Give `Denied` a moment to arrive; the caller closes on it.
            let _ = tokio::time::timeout(Duration::from_secs(3), conn.closed()).await;
            bail!("denied {remote}");
        };
        let proof = proto::proof(&key, proto::ROLE_CALLEE, &ex, &remote, &me);
        proto::write_msg(&mut send, &Msg::Welcome { proof, room_secret }).await?;
        Ok((send, recv, room_gen))
    }

    /// An authenticated connection: exchanges `Hello`, then runs until it
    /// closes. `room_gen` is the room the other side proved to belong to.
    async fn run_peer(
        &self,
        conn: Connection,
        dialed: bool,
        mut send: iroh::endpoint::SendStream,
        mut recv: iroh::endpoint::RecvStream,
        room_gen: u64,
    ) -> Result<()> {
        let me = self.0.key.public();
        let remote = conn.remote_id();
        // Not a word about a room the other side didn't prove to be in.
        // The caller speaks first. The callee answers with a `Hello` built
        // under the lock that adds the caller to `peers`, so of two
        // simultaneous joiners the later one always hears of the earlier.
        // Every connection adds events (a picture among them), so none is
        // taken in while the UI is behind: a peer reconnecting in a loop
        // can't grow them. While a hidden window holds a full budget,
        // joiners give up after the `Hello` timeout and stay alone in an
        // empty room; that is accepted.
        self.0.at_gate.fetch_add(1, Ordering::SeqCst);
        let gate = Pending(&self.0.at_gate);
        let ready = crate::flow::unless_closed(self.0.events.room(), conn.closed());
        let ready = tokio::time::timeout(HELLO_TIMEOUT, ready).await;
        drop(gate);
        if !matches!(ready, Ok(true)) {
            conn.close(4u32.into(), b"busy");
            bail!("UI behind");
        }
        if dialed {
            let hello = self.hello(room_gen).context("room changed")?;
            proto::write_msg(&mut send, &hello).await?;
        }
        let Msg::Hello { name, room, peers, stream, camera, voice, muted, deafened, avatar } =
            tokio::time::timeout(HELLO_TIMEOUT, proto::read_msg(&mut recv)).await??
        else {
            bail!("expected hello");
        };
        let name = peer_display_name(&name, &remote);
        let avatar = avatar.filter(|a| a.len() <= crate::avatar::MAX_BYTES);
        let (stream, camera) = (proto::clean_stream_info(stream), proto::clean_stream_info(camera));

        let ctrl = Arc::new(CtrlQueue::new(self.0.bulk_budget));
        let stable = conn.stable_id();
        let mut adopted_room = None;
        let reply = {
            let mut s = self.0.state.lock().unwrap();
            // Authenticated for an earlier room (e.g. we left and made a new
            // one while this `Hello` was held back): not a member here.
            if !s.in_room || s.room_gen != room_gen {
                conn.close(2u32.into(), b"left");
                return Ok(());
            }
            if s.room.is_empty() && !room.is_empty() {
                s.room = room.clone();
                adopted_room = Some(room);
            }
            // Both sides may dial each other at the same time. Keep exactly
            // one connection: the one dialed by the smaller id.
            let dialer = if dialed { me } else { remote };
            let preferred = me.min(remote);
            if let Some(existing) = s.peers.get(&remote) {
                if dialer == preferred && existing.dialer != preferred {
                    existing.conn.close(3u32.into(), b"duplicate");
                } else {
                    conn.close(3u32.into(), b"duplicate");
                    return Ok(());
                }
            }
            // Before the insert: our peer list doesn't name the caller.
            let reply = (!dialed).then(|| Self::hello_of(&s));
            s.peers.insert(
                remote,
                PeerConn {
                    name: name.clone(),
                    conn: conn.clone(),
                    ctrl: ctrl.clone(),
                    dialer,
                    voice,
                    stream: stream.clone(),
                    camera: None,
                },
            );
            reply
        };
        // Sent before the writer starts, so it comes first. On failure the
        // reader below ends at once and cleans up.
        if let Some(hello) = reply
            && proto::write_msg(&mut send, &hello).await.is_err()
        {
            conn.close(2u32.into(), b"hello");
        }
        if let Some(room) = adopted_room {
            self.emit(Event::RoomName(room));
        }
        self.emit(Event::PeerJoined { id: remote, name: name.clone(), avatar });
        self.emit(Event::PeerStream { id: remote, info: stream });
        self.emit(Event::PeerVoice { id: remote, on: voice });
        self.emit(Event::PeerMute { id: remote, muted, deafened });
        self.set_peer_camera(remote, camera);

        // Complete the mesh.
        for p in peers {
            if p != me {
                let n = self.clone();
                self.spawn(async move {
                    if let Err(e) = n.dial_retry(EndpointAddr::new(p), Cred::Member, room_gen).await {
                        tracing::debug!("mesh dial {p}: {e:#}");
                    }
                });
            }
        }

        // Writer. Bulk budget is given back only once a frame is written.
        {
            let ctrl = ctrl.clone();
            self.spawn(async move {
                while let Some((frame, cost)) = ctrl.next().await {
                    let ok = send.write_chunk(frame).await.is_ok();
                    ctrl.sent(cost);
                    if !ok {
                        break;
                    }
                }
            });
        }

        // Extra streams: video watch requests.
        {
            let n = self.clone();
            let conn = conn.clone();
            self.spawn(async move {
                while let Ok((s, mut r)) = conn.accept_bi().await {
                    let mut tag = [0u8];
                    if r.read_exact(&mut tag).await.is_err() {
                        continue;
                    }
                    if tag[0] == TAG_WATCH || tag[0] == TAG_CAMERA {
                        let n = n.clone();
                        tokio::spawn(async move { n.serve_viewer(s, tag[0] == TAG_CAMERA).await });
                    }
                }
            });
        }

        // Datagrams: voice.
        {
            let n = self.clone();
            let conn = conn.clone();
            self.spawn(async move {
                while let Ok(d) = conn.read_datagram().await {
                    if d.first() == Some(&DGRAM_VOICE) {
                        let v = n.0.state.lock().unwrap().voice.clone();
                        if let Some(v) = v {
                            v.receive(remote, &d[1..]);
                        }
                    }
                }
            });
        }

        // Reader. Before each message it waits for the rate limits and for
        // the UI to catch up with events (`Events::room`), instead of dropping
        // anything: QUIC flow control then stalls the sender, whose queue
        // refuses further chat and reports it. Waiting also holds back this
        // peer's state messages (voice, mute, …) behind its chat; they stay
        // in order and arrive late, which is the accepted cost.
        let mut msgs = Bucket::new(crate::flow::MSG_BURST, crate::flow::MSG_RATE, Instant::now());
        let mut bytes = Bucket::new(crate::flow::BYTE_BURST, crate::flow::BYTE_RATE, Instant::now());
        loop {
            let ready = async {
                loop {
                    let now = Instant::now();
                    let wait = msgs.wait(now, 1.0).max(bytes.wait(now, 0.0));
                    if wait.is_zero() {
                        break;
                    }
                    tokio::time::sleep(wait).await;
                }
                self.0.events.room().await;
            };
            if !crate::flow::unless_closed(ready, conn.closed()).await {
                break;
            }
            let Ok((msg, len)) = proto::read_frame(&mut recv).await else { break };
            let now = Instant::now();
            msgs.take(now, 1.0);
            bytes.take(now, len as f64);
            match msg {
                Msg::Chat { text } => {
                    if text.len() > proto::MAX_CHAT_BYTES {
                        tracing::warn!("dropping {} B chat message from {remote}", text.len());
                        continue;
                    }
                    let name = self.peer_name(&remote);
                    self.emit(Event::Chat { from: Some(remote), name, text });
                }
                Msg::File { name: file_name, data } => {
                    if data.len() > proto::MAX_FILE_BYTES {
                        tracing::warn!("dropping {} B file from {remote}", data.len());
                        continue;
                    }
                    let name = self.peer_name(&remote);
                    let file_name = proto::clean_file_name(&file_name);
                    self.emit(Event::File { from: Some(remote), name, file_name, data: Arc::new(data) });
                }
                Msg::Stream(info) => {
                    let info = proto::clean_stream_info(info);
                    let stop_watch = {
                        let mut s = self.0.state.lock().unwrap();
                        if let Some(p) = s.peers.get_mut(&remote) {
                            p.stream = info.clone();
                        }
                        info.is_none() && s.watch.as_ref().is_some_and(|w| w.peer == remote)
                    };
                    if stop_watch {
                        self.unwatch();
                    }
                    self.emit(Event::PeerStream { id: remote, info });
                }
                Msg::Camera(info) => self.set_peer_camera(remote, proto::clean_stream_info(info)),
                Msg::Voice(on) => {
                    {
                        let mut s = self.0.state.lock().unwrap();
                        if let Some(p) = s.peers.get_mut(&remote) {
                            p.voice = on;
                        }
                        if !on {
                            if let Some(v) = &s.voice {
                                v.remove_peer(&remote);
                            }
                        }
                    }
                    self.emit(Event::PeerVoice { id: remote, on });
                }
                Msg::Mute { muted, deafened } => {
                    self.emit(Event::PeerMute { id: remote, muted, deafened });
                }
                Msg::Profile { name, avatar } => {
                    let name = peer_display_name(&name, &remote);
                    let avatar = avatar.filter(|a| a.len() <= crate::avatar::MAX_BYTES);
                    if let Some(p) = self.0.state.lock().unwrap().peers.get_mut(&remote) {
                        p.name = name.clone();
                    }
                    self.emit(Event::PeerProfile { id: remote, name, avatar });
                }
                Msg::Hello { .. } | Msg::Auth(_) | Msg::Denied | Msg::Welcome { .. } => {}
            }
        }

        // Cleanup (only if this connection is still the active one).
        ctrl.close();
        let removed = {
            let mut s = self.0.state.lock().unwrap();
            if s.peers.get(&remote).is_some_and(|p| p.conn.stable_id() == stable) {
                s.peers.remove(&remote);
                if let Some((_, task)) = s.cam_views.remove(&remote) {
                    task.abort();
                }
                if let Some(v) = &s.voice {
                    v.remove_peer(&remote);
                }
                true
            } else {
                false
            }
        };
        if removed {
            if self.watching() == Some(remote) {
                self.unwatch();
            }
            self.emit(Event::PeerLeft { id: remote });
        }
        Ok(())
    }

    fn peer_name(&self, id: &EndpointId) -> String {
        self.0
            .state
            .lock()
            .unwrap()
            .peers
            .get(id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| id.fmt_short().to_string())
    }

    /// Short description of how we reach a peer: direct (P2P) or via relay.
    pub fn path_kind(&self, id: &EndpointId) -> Option<&'static str> {
        let s = self.0.state.lock().unwrap();
        let p = s.peers.get(id)?;
        let paths = p.conn.paths();
        let mut relay = false;
        for path in paths.iter() {
            if path.is_selected() {
                return Some(if path.is_relay() { "relay" } else { "P2P" });
            }
            relay |= path.is_relay();
        }
        Some(if relay { "relay" } else { "…" })
    }

    // -------------------------------------------------------------- video

    pub fn start_stream(&self, settings: video::encode::Settings) {
        let room = self.0.state.lock().unwrap().room_gen;
        let n = self.clone();
        self.spawn(async move {
            n.emit(Event::Status("Uruchamianie enkodera…".into()));
            let n2 = n.clone();
            let res = tokio::task::spawn_blocking(move || {
                video::encode::start(&settings, move |err| {
                    let n3 = n2.clone();
                    n2.0.rt.spawn(async move {
                        let had = n3.0.state.lock().unwrap().broadcast.take().is_some();
                        if had {
                            n3.broadcast_msg(Msg::Stream(None));
                            n3.emit(Event::MyStream(None));
                            n3.emit(Event::Error(format!("Stream przerwany: {err}")));
                        }
                    });
                })
            })
            .await;
            match res {
                Ok(Ok(b)) => {
                    let info = b.info.clone();
                    {
                        let mut s = n.0.state.lock().unwrap();
                        if s.room_gen != room {
                            return; // left while starting; dropping `b` stops the encoder
                        }
                        s.broadcast = Some(b);
                    }
                    n.broadcast_msg(Msg::Stream(Some(info.clone())));
                    n.emit(Event::MyStream(Some(info)));
                    n.emit(Event::Status(String::new()));
                }
                Ok(Err(e)) => n.emit(Event::Error(format!("Nie można streamować: {e:#}"))),
                Err(e) => n.emit(Event::Error(format!("{e}"))),
            }
        });
    }

    pub fn stop_stream(&self) {
        let b = self.0.state.lock().unwrap().broadcast.take();
        if b.is_some() {
            drop(b);
            self.broadcast_msg(Msg::Stream(None));
            self.emit(Event::MyStream(None));
        }
    }

    pub fn viewer_count(&self) -> usize {
        let s = self.0.state.lock().unwrap();
        s.broadcast.as_ref().map(|b| b.hub.viewer_count()).unwrap_or(0)
    }

    async fn serve_viewer(&self, mut send: iroh::endpoint::SendStream, camera: bool) {
        let hub = {
            let s = self.0.state.lock().unwrap();
            if camera { &s.camera } else { &s.broadcast }.as_ref().map(|b| b.hub.clone())
        };
        let Some(hub) = hub else {
            let _ = send.finish();
            return;
        };
        let mut rx = hub.subscribe();
        drop(hub);
        while let Some(chunk) = rx.recv().await {
            if send.write_chunk(chunk).await.is_err() {
                return;
            }
        }
        let _ = send.finish();
    }

    // ------------------------------------------------------------- camera

    /// Turns our camera on. Peers decode it into their voice tiles; we decode
    /// our own hub for the preview.
    pub fn start_camera(&self, name: Option<String>) {
        let room = self.0.state.lock().unwrap().room_gen;
        let n = self.clone();
        self.spawn(async move {
            let n2 = n.clone();
            let res = tokio::task::spawn_blocking(move || {
                video::encode::start(&video::encode::Settings::camera(name), move |err| {
                    let n3 = n2.clone();
                    n2.0.rt.spawn(async move {
                        if n3.0.state.lock().unwrap().camera.is_some() {
                            n3.stop_camera();
                            n3.emit(Event::Error(format!("Kamera przerwała pracę: {err}")));
                        }
                    });
                })
            })
            .await;
            match res {
                Ok(Ok(b)) => {
                    let (info, hub) = (b.info.clone(), b.hub.clone());
                    {
                        let mut s = n.0.state.lock().unwrap();
                        if s.room_gen != room || !s.in_room || s.voice.is_none() {
                            return n.emit(Event::MyCamera(false)); // left while starting
                        }
                        s.camera = Some(b);
                    }
                    if let Some(me) = n.my_id() {
                        let rx = hub.subscribe();
                        n.view_camera(me, info.codec, async move { Ok(rx) });
                    }
                    n.broadcast_msg(Msg::Camera(Some(info)));
                    n.emit(Event::MyCamera(true));
                }
                Ok(Err(e)) => {
                    n.emit(Event::MyCamera(false));
                    n.emit(Event::Error(format!("Nie można włączyć kamery: {e:#}")));
                }
                Err(e) => n.emit(Event::Error(format!("{e}"))),
            }
        });
    }

    pub fn stop_camera(&self) {
        let (b, view) = {
            let mut s = self.0.state.lock().unwrap();
            let view = self.my_id().and_then(|me| s.cam_views.remove(&me));
            (s.camera.take(), view)
        };
        if let Some((_, task)) = view {
            task.abort();
        }
        if b.is_some() {
            drop(b);
            self.broadcast_msg(Msg::Camera(None));
            self.emit(Event::MyCamera(false));
        }
    }

    /// Latest decoded camera frames, by person (our id = preview).
    pub fn camera_frames(&self) -> Vec<(EndpointId, Arc<FrameSlot>)> {
        let s = self.0.state.lock().unwrap();
        s.cam_views.iter().map(|(id, (slot, _))| (*id, slot.clone())).collect()
    }

    fn set_peer_camera(&self, id: EndpointId, info: Option<StreamInfo>) {
        let conn = {
            let mut s = self.0.state.lock().unwrap();
            let Some(p) = s.peers.get_mut(&id) else { return };
            p.camera = info.clone();
            let conn = p.conn.clone();
            if let Some((_, task)) = s.cam_views.remove(&id) {
                task.abort();
            }
            conn
        };
        if let Some(info) = &info {
            // ponytail: every camera is received even when nobody looks at the
            // voice stage; subscribe on demand if bandwidth becomes a problem.
            self.view_camera(id, info.codec, async move {
                let (mut send, mut recv) = conn.open_bi().await?;
                send.write_all(&[TAG_CAMERA]).await?;
                let (tx, rx) = mpsc::channel(4);
                tokio::spawn(async move {
                    let _send = send; // keep the request stream open
                    while let Ok(Some(chunk)) = recv.read_chunk(1 << 20).await {
                        if tx.send(chunk).await.is_err() {
                            break;
                        }
                    }
                });
                Ok(rx)
            });
        }
        self.emit(Event::PeerCamera { id, on: info.is_some() });
    }

    /// Decodes a camera bitstream (from `source`) into a frame slot for `id`'s tile.
    fn view_camera(
        &self,
        id: EndpointId,
        codec: proto::Codec,
        source: impl std::future::Future<Output = Result<mpsc::Receiver<Bytes>>> + Send + 'static,
    ) {
        let slot = Arc::new(FrameSlot::default());
        let events = self.0.events.clone();
        let slot2 = slot.clone();
        let task = self.0.rt.spawn(async move {
            let res: Result<()> = async {
                let mut rx = source.await?;
                let mut dec = video::decode::Decoder::start(codec, slot2, move || events.repaint())?;
                while let Some(chunk) = rx.recv().await {
                    dec.feed(chunk).await?;
                }
                Ok(())
            }
            .await;
            if let Err(e) = res {
                tracing::debug!("camera view ended: {e:#}");
            }
        });
        let old = self.0.state.lock().unwrap().cam_views.insert(id, (slot, task));
        if let Some((_, t)) = old {
            t.abort();
        }
    }

    pub fn watching(&self) -> Option<EndpointId> {
        self.0.state.lock().unwrap().watch.as_ref().map(|w| w.peer)
    }

    pub fn watch(&self, peer: EndpointId) {
        self.unwatch();
        let (conn, codec) = {
            let s = self.0.state.lock().unwrap();
            let Some(p) = s.peers.get(&peer) else { return };
            let Some(info) = &p.stream else { return };
            (p.conn.clone(), info.codec)
        };
        let n = self.clone();
        let task = self.0.rt.spawn(async move {
            let res: Result<()> = async {
                let (mut send, mut recv) = conn.open_bi().await?;
                send.write_all(&[TAG_WATCH]).await?;
                let events = n.0.events.clone();
                let mut dec = video::decode::Decoder::start(codec, n.0.frames.clone(), move || {
                    events.repaint()
                })?;
                while let Some(chunk) = recv.read_chunk(1 << 20).await? {
                    dec.feed(chunk).await?;
                }
                Ok(())
            }
            .await;
            if let Err(e) = res {
                tracing::debug!("watch ended: {e:#}");
            }
            let mut s = n.0.state.lock().unwrap();
            if s.watch.as_ref().is_some_and(|w| w.peer == peer) {
                s.watch = None;
                drop(s);
                n.emit(Event::Watching(None));
            }
        });
        self.0.state.lock().unwrap().watch = Some(Watch { peer, task });
        self.emit(Event::Watching(Some(peer)));
    }

    pub fn unwatch(&self) {
        let w = self.0.state.lock().unwrap().watch.take();
        if let Some(w) = w {
            w.task.abort();
            self.emit(Event::Watching(None));
        }
    }

    // -------------------------------------------------------------- voice

    pub fn voice(&self) -> Option<Arc<Voice>> {
        self.0.state.lock().unwrap().voice.clone()
    }

    pub fn join_voice(&self) {
        let mut s = self.0.state.lock().unwrap();
        if s.voice.is_none() && !s.voice_starting {
            self.start_voice(&mut s);
        }
    }

    /// Starts voice; supersedes any start already in flight.
    fn start_voice(&self, s: &mut State) {
        s.voice_starting = true;
        s.voice_gen += 1;
        let (id, devices, deafened, prefs) = (s.voice_gen, s.devices.clone(), s.mute.deafened, s.voice_prefs);
        let n = self.clone();
        self.spawn(async move {
            let (tx, mut rx) = crate::voice::packet_channel();
            // Start muted: `set_mute` can't reach a Voice that isn't stored
            // yet, and frames queued in `tx` meanwhile would go out later.
            // The real setting is applied below, under the same lock.
            let res = tokio::task::spawn_blocking(move || Voice::start(tx, devices, true, deafened, prefs)).await;
            let res = {
                let mut s = n.0.state.lock().unwrap();
                if s.voice_gen != id {
                    return; // cancelled or superseded; dropping the Voice stops it
                }
                s.voice_starting = false;
                res.map(|r| {
                    r.map(|v| {
                        v.set_muted(s.mute.muted || s.mute.deafened);
                        v.set_deafened(s.mute.deafened);
                        s.voice = Some(Arc::new(v));
                    })
                })
            };
            match res {
                Ok(Ok(())) => {
                    n.broadcast_msg(Msg::Voice(true));
                    n.emit(Event::MyVoice(true));
                    let n2 = n.clone();
                    tokio::spawn(async move {
                        while let Some(pkt) = rx.recv().await {
                            let mut d = Vec::with_capacity(pkt.len() + 1);
                            d.push(DGRAM_VOICE);
                            d.extend_from_slice(&pkt);
                            let d = Bytes::from(d);
                            let s = n2.0.state.lock().unwrap();
                            for p in s.peers.values().filter(|p| p.voice) {
                                let _ = p.conn.send_datagram(d.clone());
                            }
                        }
                    });
                }
                Ok(Err(e)) => n.emit(Event::Error(format!("Głos: {e:#}"))),
                Err(e) => n.emit(Event::Error(format!("Głos: {e}"))),
            }
        });
    }

    pub fn set_mute(&self, mute: MuteState) {
        let voice = {
            let mut s = self.0.state.lock().unwrap();
            s.mute = mute;
            s.voice.clone()
        };
        if let Some(v) = voice {
            v.set_muted(mute.muted || mute.deafened);
            v.set_deafened(mute.deafened);
        }
        self.broadcast_msg(Msg::Mute { muted: mute.muted, deafened: mute.deafened });
    }

    pub fn devices(&self) -> crate::voice::Devices {
        self.0.state.lock().unwrap().devices.clone()
    }

    /// Changes audio devices; reconnects voice if currently connected.
    pub fn set_devices(&self, devices: crate::voice::Devices) {
        let mut s = self.0.state.lock().unwrap();
        if s.devices == devices {
            return;
        }
        s.devices = devices;
        if s.voice.is_some() || s.voice_starting {
            s.voice = None;
            self.start_voice(&mut s);
        }
    }

    pub fn voice_prefs(&self) -> crate::voice::Prefs {
        self.0.state.lock().unwrap().voice_prefs
    }

    /// Changes microphone processing; applied live to an active call.
    pub fn set_voice_prefs(&self, prefs: crate::voice::Prefs) {
        let voice = {
            let mut s = self.0.state.lock().unwrap();
            s.voice_prefs = prefs;
            s.voice.clone()
        };
        if let Some(v) = voice {
            v.set_prefs(prefs);
        }
    }

    pub fn leave_voice(&self) {
        self.stop_camera(); // video is part of the call, as on Discord
        let (v, starting) = {
            let mut s = self.0.state.lock().unwrap();
            s.voice_gen += 1;
            (s.voice.take(), std::mem::take(&mut s.voice_starting))
        };
        // Also while (re)starting: after a device change peers still have
        // us in voice; an extra `Voice(false)` is harmless.
        if v.is_some() || starting {
            drop(v);
            self.broadcast_msg(Msg::Voice(false));
            self.emit(Event::MyVoice(false));
        }
    }
}

/// A peer's name as we show it; never empty.
fn peer_display_name(name: &str, id: &EndpointId) -> String {
    let n = proto::clean_name(name);
    if n.is_empty() { id.fmt_short().to_string() } else { n }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn node() -> (Node, EventRx) {
        let (ev, rx) = Events::new(eframe::egui::Context::default());
        (Node::new(tokio::runtime::Handle::current(), ev, iroh::SecretKey::generate()), rx)
    }

    /// Queues `msg` to `to` as is, skipping the sender's checks.
    fn push_raw(n: &Node, to: EndpointId, msg: Msg) {
        let ctrl = n.0.state.lock().unwrap().peers.get(&to).unwrap().ctrl.clone();
        assert_eq!(ctrl.push(proto::encode_msg(&msg).unwrap(), false), Push::Queued);
    }

    async fn wait_for(
        rx: &EventRx,
        secs: u64,
        mut f: impl FnMut(&Event) -> bool,
    ) -> Event {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            while let Some(e) = rx.try_recv() {
                if f(&e) {
                    return e;
                }
            }
            assert!(Instant::now() < deadline, "timeout");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Frames decoded so far for `id`'s camera tile on `n`.
    fn cam_count(n: &Node, id: EndpointId) -> u64 {
        n.camera_frames().iter().find(|(i, _)| *i == id).map_or(0, |(_, s)| s.count())
    }

    async fn wait_cam_frames(n: &Node, id: EndpointId, min: u64) {
        let start = Instant::now();
        while cam_count(n, id) < min {
            assert!(start.elapsed() < Duration::from_secs(30), "camera frames: {}", cam_count(n, id));
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// `MyVoice` events that arrive within `ms`; long enough for a fake
    /// voice start to finish.
    async fn my_voice_events(rx: &EventRx, ms: u64) -> Vec<bool> {
        tokio::time::sleep(Duration::from_millis(ms)).await;
        rx.try_iter().filter_map(|e| if let Event::MyVoice(on) = e { Some(on) } else { None }).collect()
    }

    /// Leaving the room (and entering another) while voice is starting
    /// discards the start; a later join works normally.
    #[tokio::test]
    async fn leave_cancels_voice_start() {
        let (a, rx) = node();
        a.create_room("Ala".into(), "A".into());
        a.join_voice();
        a.leave();
        a.create_room("Ala".into(), "B".into());
        assert_eq!(my_voice_events(&rx, 500).await, [false; 0]);
        assert!(a.voice().is_none());

        a.join_voice();
        wait_for(&rx, 10, |e| matches!(e, Event::MyVoice(true))).await;
        assert!(!a.voice().unwrap().is_muted());
        a.leave();
    }

    /// Hanging up while voice is starting cancels it and clears the UI flag.
    #[tokio::test]
    async fn leave_voice_cancels_voice_start() {
        let (a, rx) = node();
        a.create_room("Ala".into(), "A".into());
        a.join_voice();
        a.leave_voice();
        assert_eq!(my_voice_events(&rx, 500).await, [false]);
        assert!(a.voice().is_none());

        a.join_voice();
        wait_for(&rx, 10, |e| matches!(e, Event::MyVoice(true))).await;
        assert!(a.voice().is_some());
        a.leave();
    }

    /// Muting (or deafening) while voice is starting holds once it runs.
    #[tokio::test]
    async fn mute_during_voice_start() {
        let (a, rx) = node();
        a.create_room("Ala".into(), "A".into());
        for mute in [MuteState { muted: true, deafened: false }, MuteState { muted: false, deafened: true }] {
            a.set_mute(MuteState::default());
            a.join_voice();
            tokio::task::yield_now().await; // the start task reaches spawn_blocking
            a.set_mute(mute);
            wait_for(&rx, 10, |e| matches!(e, Event::MyVoice(true))).await;
            assert!(a.voice().unwrap().is_muted(), "{mute:?}");
            a.leave_voice();
        }
        a.leave();
    }

    /// A second join or a device change during the start leaves one voice;
    /// a device change in a call reconnects without `MyVoice(false)`.
    #[tokio::test]
    async fn voice_start_runs_once() {
        let (a, rx) = node();
        a.create_room("Ala".into(), "A".into());
        a.join_voice();
        a.join_voice();
        a.set_devices(crate::voice::Devices { input: Some("mic".into()), output: None });
        let e = wait_for(&rx, 10, |e| matches!(e, Event::MyVoice(_))).await;
        assert!(matches!(e, Event::MyVoice(true)), "{e:?}");
        assert_eq!(my_voice_events(&rx, 500).await, [false; 0]);
        assert!(a.voice().is_some());

        a.set_devices(crate::voice::Devices::default());
        let e = wait_for(&rx, 10, |e| matches!(e, Event::MyVoice(_))).await;
        assert!(matches!(e, Event::MyVoice(true)), "{e:?}");
        assert_eq!(my_voice_events(&rx, 500).await, [false; 0]);
        assert!(a.voice().is_some());
        a.leave();
    }

    /// Hanging up while voice restarts after a device change tells peers.
    /// Run with: cargo test e2e_leave_voice_during_device_switch -- --ignored
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn e2e_leave_voice_during_device_switch() {
        let (a, arx) = node();
        let (b, brx) = node();
        a.create_room("Ala".into(), "Test".into());
        let Event::Ticket { link: t, .. } = wait_for(&arx, 20, |e| matches!(e, Event::Ticket { .. })).await else {
            unreachable!()
        };
        b.join(&t, "Bob".into()).unwrap();
        wait_for(&arx, 30, |e| matches!(e, Event::PeerJoined { .. })).await;
        let ida = a.my_id().unwrap();
        a.join_voice();
        wait_for(&brx, 30, |e| matches!(e, Event::PeerVoice { id, on: true } if *id == ida)).await;

        a.set_devices(crate::voice::Devices { input: Some("mic".into()), output: None });
        a.leave_voice();
        wait_for(&brx, 10, |e| matches!(e, Event::PeerVoice { id, on: false } if *id == ida)).await;
        assert!(a.voice().is_none());
        a.leave();
        b.leave();
    }

    /// Camera next to a screen stream: peers (also late joiners) decode it
    /// without asking, the sender sees a preview, leaving voice stops it.
    /// Run with: BIBICORD_FFMPEG=/path/to/ffmpeg cargo test e2e_camera -- --ignored
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn e2e_camera() {
        unsafe {
            std::env::set_var("BIBICORD_TEST_SOURCE", "1");
            std::env::set_var("BIBICORD_FAKE_AUDIO", "1");
        }
        tokio::task::spawn_blocking(|| crate::ffmpeg::ensure(|_| {})).await.unwrap().unwrap();
        let (a, arx) = node();
        let (b, brx) = node();
        a.create_room("Ala".into(), "Test".into());
        let Event::Ticket { link: t, .. } = wait_for(&arx, 20, |e| matches!(e, Event::Ticket { .. })).await else {
            unreachable!()
        };
        b.join(&t, "Bob".into()).unwrap();
        wait_for(&brx, 30, |e| matches!(e, Event::PeerJoined { .. })).await;
        a.join_voice();
        b.join_voice();
        wait_for(&arx, 10, |e| matches!(e, Event::MyVoice(true))).await;
        let ida = a.my_id().unwrap();

        a.start_camera(None);
        wait_for(&arx, 60, |e| matches!(e, Event::MyCamera(true))).await;
        wait_for(&brx, 10, |e| matches!(e, Event::PeerCamera { on: true, .. })).await;
        wait_cam_frames(&a, ida, 10).await; // preview
        wait_cam_frames(&b, ida, 10).await;

        // The screen stream is independent of the camera.
        a.start_stream(Default::default());
        wait_for(&brx, 60, |e| matches!(e, Event::PeerStream { info: Some(_), .. })).await;
        let before = cam_count(&b, ida);
        wait_cam_frames(&b, ida, before + 30).await;

        let (c, crx) = node();
        c.join(&t, "Cyryl".into()).unwrap();
        wait_for(&crx, 30, |e| matches!(e, Event::PeerCamera { id, on: true } if *id == ida)).await;
        wait_cam_frames(&c, ida, 10).await;

        a.leave_voice();
        wait_for(&brx, 10, |e| matches!(e, Event::PeerCamera { id, on: false } if *id == ida)).await;
        assert!(b.camera_frames().is_empty());
        let s = a.0.state.lock().unwrap();
        assert!(s.camera.is_none() && s.broadcast.is_some());
        drop(s);
        for n in [a, b, c] {
            n.leave();
        }
    }

    /// Too long a message is refused before anything is sent or echoed;
    /// one of exactly the limit goes through.
    #[tokio::test]
    async fn send_chat_limit() {
        let (a, rx) = node();
        a.create_room("Ala".into(), "A".into());
        assert!(a.send_chat("x".repeat(proto::MAX_CHAT_BYTES + 1)).is_err());
        a.send_chat("x".repeat(proto::MAX_CHAT_BYTES)).unwrap();
        let chats: Vec<usize> = rx
            .try_iter()
            .filter_map(|e| if let Event::Chat { from: None, text, .. } = e { Some(text.len()) } else { None })
            .collect();
        assert_eq!(chats, [proto::MAX_CHAT_BYTES]);
        a.leave();
    }

    /// Chat at the length limit over the real network arrives intact; longer
    /// messages from a peer that skips the check are dropped (whether they
    /// fit in a frame or not) without losing the connection.
    /// Run with: cargo test e2e_long_chat -- --ignored
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn e2e_long_chat() {
        let (a, arx) = node();
        let (b, brx) = node();
        a.create_room("Ala".into(), "Test".into());
        let Event::Ticket { link: t, .. } = wait_for(&arx, 20, |e| matches!(e, Event::Ticket { .. })).await else {
            unreachable!()
        };
        b.join(&t, "Bob".into()).unwrap();
        wait_for(&arx, 30, |e| matches!(e, Event::PeerJoined { .. })).await;
        wait_for(&brx, 30, |e| matches!(e, Event::PeerJoined { .. })).await;

        let long: String = "zażółć 😀 ".repeat(proto::MAX_CHAT_BYTES / 16);
        let mut long = long + &"x".repeat(proto::MAX_CHAT_BYTES);
        proto::clamp_chat(&mut long);
        b.send_chat(long.clone()).unwrap();
        let Event::Chat { text, .. } = wait_for(&arx, 20, |e| matches!(e, Event::Chat { from: Some(_), .. })).await
        else {
            unreachable!()
        };
        assert!(text == long, "message changed: {} B", text.len());

        // Over the limit but within a frame: the receiver's check drops it.
        // Far over: `read_msg` skips the frame.
        let ida = a.my_id().unwrap();
        push_raw(&b, ida, Msg::Chat { text: "y".repeat(proto::MAX_CHAT_BYTES + 1000) });
        push_raw(&b, ida, Msg::Chat { text: "z".repeat(2 << 20) });
        b.send_chat("po długiej".into()).unwrap();
        let e = wait_for(&arx, 20, |e| matches!(e, Event::Chat { from: Some(_), .. } | Event::PeerLeft { .. })).await;
        assert!(matches!(&e, Event::Chat { text, .. } if text == "po długiej"), "{e:?}");
        assert!(a.0.state.lock().unwrap().peers.contains_key(&b.my_id().unwrap()));
        a.leave();
        b.leave();
    }

    /// Invites without the network: rotation, expiry, cleanup on leave.
    #[tokio::test]
    async fn invite_lifecycle() {
        let (a, _rx) = node();
        a.new_invite();
        assert!(a.valid_invite().is_none(), "no invite outside a room");
        a.create_room("Ala".into(), "A".into());
        let t1 = a.valid_invite().unwrap();
        assert!(a.0.state.lock().unwrap().room_secret.is_some());
        a.ensure_invite();
        assert_eq!(a.valid_invite(), Some(t1), "a valid invite is kept");
        a.new_invite();
        let t2 = a.valid_invite().unwrap();
        assert_ne!(t1, t2);
        a.age_invite();
        assert!(a.valid_invite().is_none(), "expired");
        a.ensure_invite();
        assert!(a.valid_invite().is_some_and(|t| t != t2));
        a.leave();
        let s = a.0.state.lock().unwrap();
        assert!(s.invite.is_none() && s.room_secret.is_none());
    }

    /// Only failures to reach the other side (inviter or mesh peer) are
    /// retried, a few times, early.
    #[test]
    fn retry_dial_decision() {
        let connect = || anyhow::anyhow!("no addressing information").context(ConnectFailed);
        let s = Duration::from_secs(1);
        assert!(retry_dial(&connect(), 1, s, true));
        assert!(retry_dial(&connect(), 2, s, true));
        assert!(!retry_dial(&connect(), 3, s, true), "3 attempts at most");
        assert!(!retry_dial(&connect(), 1, Duration::from_secs(20), true), "too late");
        assert!(!retry_dial(&connect(), 1, s, false), "room changed");
        let alpn = anyhow::anyhow!("peer doesn't support any known protocol").context(ConnectFailed);
        assert!(!retry_dial(&alpn, 1, s, true), "ALPN mismatch");
        assert!(!retry_dial(&anyhow::Error::from(Denied), 1, s, true), "denied");
        assert!(!retry_dial(&anyhow::anyhow!("bad welcome proof"), 1, s, true), "bad welcome");
        assert!(!retry_dial(&anyhow::anyhow!("expected hello"), 1, s, true), "after auth");
    }

    /// A dial started for an earlier room is refused before connecting.
    #[tokio::test]
    async fn dial_stale_room() {
        let (a, _rx) = node();
        a.create_room("Ala".into(), "Pokój 1".into());
        let old = a.0.state.lock().unwrap().room_gen;
        a.leave();
        a.create_room("Ala".into(), "Pokój 2".into());
        let secret = a.0.state.lock().unwrap().room_secret;
        let to = EndpointAddr::from(iroh::SecretKey::generate().public());
        for cred in [Cred::Invite([1; 16]), Cred::Member] {
            let res = tokio::time::timeout(Duration::from_secs(2), a.dial(to.clone(), cred, old)).await.expect("connected");
            assert_eq!(res.unwrap_err().to_string(), "room changed");
        }
        let s = a.0.state.lock().unwrap();
        assert_eq!(s.room_secret, secret);
        assert!(s.dialing.is_empty());
    }

    /// Our own link is recognised before the endpoint is up.
    #[tokio::test]
    async fn join_own_link() {
        let (ev, _rx) = Events::new(eframe::egui::Context::default());
        let key = iroh::SecretKey::generate();
        let a = Node::new(tokio::runtime::Handle::current(), ev, key.clone());
        let link = proto::format_invite(&proto::InviteLink { id: key.public(), token: [1; 16], expires: u32::MAX });
        assert_eq!(a.join(&link, "Ala".into()).unwrap_err().to_string(), "To Twoje własne zaproszenie");
        assert!(!a.0.state.lock().unwrap().in_room);
        a.create_room("Ala".into(), "A".into());
        let room_gen = a.0.state.lock().unwrap().room_gen;
        let res = a.dial(EndpointAddr::from(key.public()), Cred::Invite([1; 16]), room_gen).await;
        assert!(res.is_err());
    }

    // ------------------------------------------------ invites over the network

    async fn ticket(rx: &EventRx) -> String {
        let Event::Ticket { link, .. } = wait_for(rx, 20, |e| matches!(e, Event::Ticket { .. })).await else {
            unreachable!()
        };
        link
    }

    /// Joins and waits until `b` has met the inviter.
    async fn join_ok(b: &Node, rx: &EventRx, link: &str) {
        b.join(link, "Bob".into()).unwrap();
        let e = wait_for(rx, 40, |e| matches!(e, Event::PeerJoined { .. } | Event::JoinFailed(_))).await;
        assert!(matches!(e, Event::PeerJoined { .. }), "{e:?}");
    }

    async fn join_fails(b: &Node, rx: &EventRx, link: &str) -> String {
        b.join(link, "Bob".into()).unwrap();
        let e = wait_for(rx, 40, |e| matches!(e, Event::PeerJoined { .. } | Event::JoinFailed(_))).await;
        let Event::JoinFailed(msg) = e else { panic!("joined: {e:?}") };
        assert!(!b.0.state.lock().unwrap().in_room, "left after a failed join");
        msg
    }

    async fn raw_endpoint() -> Endpoint {
        let ep = Endpoint::builder(presets::N0)
            .secret_key(iroh::SecretKey::generate())
            .alpns(vec![ALPN.to_vec()])
            .bind()
            .await
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(8), ep.online()).await;
        ep
    }

    /// A hand-written peer: control stream opened, nothing sent yet.
    struct Raw {
        conn: Connection,
        send: iroh::endpoint::SendStream,
        recv: iroh::endpoint::RecvStream,
        ex: [u8; 32],
        me: EndpointId,
        to: EndpointId,
    }

    impl Raw {
        async fn dial(ep: &Endpoint, to: EndpointId) -> Raw {
            let conn = tokio::time::timeout(Duration::from_secs(30), ep.connect(EndpointAddr::from(to), ALPN))
                .await
                .unwrap()
                .unwrap();
            let (mut send, recv) = conn.open_bi().await.unwrap();
            send.write_all(&[TAG_CONTROL]).await.unwrap();
            let ex = exporter(&conn).unwrap();
            Raw { conn, send, recv, ex, me: ep.id(), to }
        }

        fn member(&self, secret: &[u8; 32]) -> Msg {
            let proof = proto::proof(&proto::room_key(secret), proto::ROLE_CALLER, &self.ex, &self.me, &self.to);
            Msg::Auth(Auth::Member { proof })
        }

        fn invite(&self, token: &[u8; 16]) -> Msg {
            let proof = proto::proof(&proto::invite_key(token), proto::ROLE_CALLER, &self.ex, &self.me, &self.to);
            Msg::Auth(Auth::Invite { proof })
        }

        async fn send(&mut self, m: &Msg) {
            let _ = proto::write_msg(&mut self.send, m).await;
        }

        /// Next message, or `None` if the stream closed or stayed silent.
        async fn reply(&mut self) -> Option<Msg> {
            self.reply_within(Duration::from_secs(10)).await
        }

        async fn reply_within(&mut self, t: Duration) -> Option<Msg> {
            tokio::time::timeout(t, proto::read_msg(&mut self.recv)).await.ok()?.ok()
        }
    }

    fn fake_hello() -> Msg {
        Msg::Hello {
            name: "Mallory".into(),
            room: String::new(),
            peers: vec![],
            stream: None,
            camera: None,
            voice: false,
            muted: false,
            deafened: false,
            avatar: None,
        }
    }

    /// `id` never made it into `a`'s room.
    async fn assert_outside(a: &Node, rx: &EventRx, id: EndpointId) {
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!a.0.state.lock().unwrap().peers.contains_key(&id), "in peers");
        assert!(!rx.try_iter().any(|e| matches!(e, Event::PeerJoined { id: j, .. } if j == id)), "PeerJoined");
    }

    /// Joining by link works; the link names exactly the inviter.
    /// Run the invite tests with: cargo test e2e_invite -- --ignored
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn e2e_invite_join() {
        let (a, arx) = node();
        let (b, brx) = node();
        a.create_room("Ala".into(), "Test".into());
        let link = ticket(&arx).await;
        assert_eq!(Some(proto::parse_invite(&link).unwrap().id), a.my_id());
        join_ok(&b, &brx, &link).await;
        wait_for(&arx, 10, |e| matches!(e, Event::PeerJoined { name, .. } if name == "Bob")).await;
        assert_eq!(b.0.state.lock().unwrap().room_secret, a.0.state.lock().unwrap().room_secret);
        a.leave();
        b.leave();
    }

    /// (#2) A link to an earlier room doesn't open a later one.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn e2e_invite_old_room() {
        let (a, arx) = node();
        let (b, brx) = node();
        a.create_room("Ala".into(), "Pokój 1".into());
        let l1 = ticket(&arx).await;
        a.leave();
        a.create_room("Ala".into(), "Pokój 2".into());
        ticket(&arx).await;
        let msg = join_fails(&b, &brx, &l1).await;
        assert!(msg.contains("nieważne"), "{msg}");
        assert!(a.0.state.lock().unwrap().peers.is_empty());
        a.leave();
    }

    /// Knowing someone's id is not enough: no proof, a wrong room secret or
    /// a wrong token get `Denied` and nothing else.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn e2e_invite_required() {
        let (a, arx) = node();
        a.create_room("Ala".into(), "Test".into());
        ticket(&arx).await;
        let ep = raw_endpoint().await;
        for case in 0..3 {
            let mut r = Raw::dial(&ep, a.my_id().unwrap()).await;
            let m = match case {
                0 => fake_hello(),
                1 => r.member(&proto::random32()),
                _ => r.invite(&[5; 16]),
            };
            r.send(&m).await;
            let got = r.reply().await;
            assert!(matches!(got, None | Some(Msg::Denied)), "case {case}: {got:?}");
            assert!(r.reply().await.is_none(), "case {case}: more after Denied");
            assert_outside(&a, &arx, ep.id()).await;
            r.conn.close(0u32.into(), b"");
        }
        a.leave();
    }

    /// A former member's room secret doesn't open the next room.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn e2e_invite_former_member() {
        let (a, arx) = node();
        let (b, brx) = node();
        a.create_room("Ala".into(), "Pokój 1".into());
        join_ok(&b, &brx, &ticket(&arx).await).await;
        let old = b.0.state.lock().unwrap().room_secret.unwrap();
        b.leave();
        a.leave();
        a.create_room("Ala".into(), "Pokój 2".into());
        ticket(&arx).await;
        let ep = raw_endpoint().await;
        let mut r = Raw::dial(&ep, a.my_id().unwrap()).await;
        let m = r.member(&old);
        r.send(&m).await;
        let got = r.reply().await;
        assert!(matches!(got, None | Some(Msg::Denied)), "{got:?}");
        assert_outside(&a, &arx, ep.id()).await;
        a.leave();
    }

    /// Authenticated for one room, `Hello` held back until the victim is in
    /// a new room: not let in.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn e2e_invite_held_hello() {
        let (a, arx) = node();
        a.create_room("Ala".into(), "Pokój 1".into());
        ticket(&arx).await;
        let secret = a.0.state.lock().unwrap().room_secret.unwrap();
        let ep = raw_endpoint().await;
        let mut r = Raw::dial(&ep, a.my_id().unwrap()).await;
        let m = r.member(&secret);
        r.send(&m).await;
        assert!(matches!(r.reply().await, Some(Msg::Welcome { room_secret: None, .. })));
        // The callee waits for our `Hello` before saying anything.
        let got = r.reply_within(Duration::from_secs(2)).await;
        assert!(got.is_none(), "{got:?}");
        a.leave();
        a.create_room("Ala".into(), "Pokój 2".into());
        tokio::time::sleep(Duration::from_millis(500)).await;
        r.send(&fake_hello()).await;
        let got = r.reply_within(Duration::from_secs(3)).await;
        assert!(!matches!(&got, Some(Msg::Hello { room, .. }) if room == "Pokój 2"), "{got:?}");
        assert!(got.is_none(), "{got:?}");
        assert_outside(&a, &arx, ep.id()).await;
        a.leave();
    }

    /// A new link revokes the previous one.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn e2e_invite_revoked() {
        let (a, arx) = node();
        let (b, brx) = node();
        a.create_room("Ala".into(), "Test".into());
        let l1 = ticket(&arx).await;
        a.new_invite();
        let l2 = ticket(&arx).await;
        assert_ne!(l1, l2);
        let msg = join_fails(&b, &brx, &l1).await;
        assert!(msg.contains("nieważne"), "{msg}");
        join_ok(&b, &brx, &l2).await;
        a.leave();
        b.leave();
    }

    /// An expired invite is refused.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn e2e_invite_expired() {
        let (a, arx) = node();
        let (b, brx) = node();
        a.create_room("Ala".into(), "Test".into());
        let link = ticket(&arx).await;
        a.age_invite();
        let msg = join_fails(&b, &brx, &link).await;
        assert!(msg.contains("nieważne"), "{msg}");
        assert!(a.0.state.lock().unwrap().peers.is_empty());
        a.leave();
    }

    /// C joins with B's link and still meets A: the A–C link is
    /// authenticated with the room secret.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn e2e_invite_mesh() {
        let (a, arx) = node();
        let (b, brx) = node();
        let (c, crx) = node();
        a.create_room("Ala".into(), "Test".into());
        join_ok(&b, &brx, &ticket(&arx).await).await;
        // B made its own invite after joining; its `Ticket` may have been
        // passed over while waiting for `PeerJoined`, so ask again.
        let t = Instant::now();
        while b.valid_invite().is_none() {
            assert!(t.elapsed() < Duration::from_secs(5), "no invite after joining");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        b.ensure_invite();
        let lb = ticket(&brx).await;
        assert_eq!(Some(proto::parse_invite(&lb).unwrap().id), b.my_id());
        c.join(&lb, "Cyryl".into()).unwrap();
        let mut seen = HashSet::new();
        while seen.len() < 2 {
            let Event::PeerJoined { name, .. } = wait_for(&crx, 40, |e| matches!(e, Event::PeerJoined { .. })).await else {
                unreachable!()
            };
            seen.insert(name);
        }
        assert_eq!(seen, HashSet::from(["Ala".to_string(), "Bob".to_string()]));
        wait_for(&arx, 20, |e| matches!(e, Event::PeerJoined { name, .. } if name == "Cyryl")).await;
        assert!(a.0.state.lock().unwrap().peers.contains_key(&c.my_id().unwrap()));
        for n in [a, b, c] {
            n.leave();
        }
    }

    /// Waits until `rx` has seen `PeerJoined` for every name in `names`.
    async fn wait_joined(rx: &EventRx, names: &[&str]) {
        let mut seen = HashSet::new();
        while !names.iter().all(|n| seen.contains(*n)) {
            let Event::PeerJoined { name, .. } = wait_for(rx, 40, |e| matches!(e, Event::PeerJoined { .. })).await else {
                unreachable!()
            };
            seen.insert(name);
        }
    }

    /// B and C join with A's link at the same time: all three meet.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn e2e_invite_simultaneous() {
        let (a, arx) = node();
        let (b, brx) = node();
        let (c, crx) = node();
        a.create_room("Ala".into(), "Test".into());
        let link = ticket(&arx).await;
        b.join(&link, "Bob".into()).unwrap();
        c.join(&link, "Cyryl".into()).unwrap();
        wait_joined(&arx, &["Bob", "Cyryl"]).await;
        wait_joined(&brx, &["Ala", "Cyryl"]).await;
        wait_joined(&crx, &["Ala", "Bob"]).await;
        for n in [&a, &b, &c] {
            assert_eq!(n.0.state.lock().unwrap().peers.len(), 2);
        }
        for n in [a, b, c] {
            n.leave();
        }
    }

    /// Connections that never authenticate are capped; past the cap new
    /// ones are closed at once, and once they go a normal join works.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn e2e_invite_pending_limit() {
        let (a, arx) = node();
        let (b, brx) = node();
        a.create_room("Ala".into(), "Test".into());
        let link = ticket(&arx).await;
        let to = EndpointAddr::from(a.my_id().unwrap());
        let ep = raw_endpoint().await;
        let connect = || async {
            tokio::time::timeout(Duration::from_secs(30), ep.connect(to.clone(), ALPN)).await.unwrap()
        };
        let start = Instant::now();
        let mut silent = vec![connect().await.unwrap()];
        let t = Instant::now();
        for _ in 1..MAX_PENDING {
            silent.push(connect().await.unwrap());
        }
        while a.0.pending.load(Ordering::SeqCst) < MAX_PENDING {
            assert!(t.elapsed() < Duration::from_secs(5), "pending: {}", a.0.pending.load(Ordering::SeqCst));
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let t = Instant::now();
        if let Ok(extra) = connect().await {
            let closed = tokio::time::timeout(Duration::from_secs(5), extra.closed()).await;
            assert!(closed.is_ok(), "connection over the limit kept open");
        }
        // Well before the first silent ones time out on their own.
        assert!(start.elapsed() < AUTH_TIMEOUT - Duration::from_secs(3), "{:?}", start.elapsed());
        tracing::info!("over the limit closed in {:?}", t.elapsed());
        for c in &silent {
            c.close(0u32.into(), b"");
        }
        let t = Instant::now();
        while a.0.pending.load(Ordering::SeqCst) > 0 {
            assert!(t.elapsed() < Duration::from_secs(5), "pending left");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        join_ok(&b, &brx, &link).await;
        a.leave();
        b.leave();
    }

    /// We check the callee too: a `Welcome` with a wrong proof gets no
    /// `Hello`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn e2e_invite_bad_welcome() {
        let srv = raw_endpoint().await;
        let srv_id = srv.id();
        let server = tokio::spawn(async move {
            let conn = tokio::time::timeout(Duration::from_secs(40), srv.accept()).await.unwrap().unwrap().await.unwrap();
            let (mut s, mut r) = conn.accept_bi().await.unwrap();
            let mut tag = [0u8];
            r.read_exact(&mut tag).await.unwrap();
            let auth = proto::read_msg(&mut r).await.unwrap();
            assert!(matches!(auth, Msg::Auth(Auth::Member { .. })), "{auth:?}");
            proto::write_msg(&mut s, &Msg::Welcome { proof: [9; 32], room_secret: None }).await.unwrap();
            let next = tokio::time::timeout(Duration::from_secs(5), proto::read_msg(&mut r)).await;
            matches!(next, Ok(Ok(Msg::Hello { .. })))
        });
        let (b, brx) = node();
        b.create_room("Bob".into(), "Test".into());
        ticket(&brx).await;
        let room_gen = b.0.state.lock().unwrap().room_gen;
        let res = b.dial(EndpointAddr::from(srv_id), Cred::Member, room_gen).await;
        assert!(res.is_err(), "accepted a bad Welcome");
        let hello = tokio::time::timeout(Duration::from_secs(60), server).await.unwrap().unwrap();
        assert!(!hello, "sent Hello after a bad Welcome");
        assert!(b.0.state.lock().unwrap().peers.is_empty());
        b.leave();
    }

    /// Files in chat over the real network: everyone in the room gets the
    /// exact bytes, too large files are refused by the sender and dropped by
    /// the receiver without losing the connection.
    /// Run with: cargo test e2e_file -- --ignored
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn e2e_file() {
        let (a, arx) = node();
        let (b, brx) = node();
        let (c, crx) = node();
        a.create_room("Ala".into(), "Test".into());
        let Event::Ticket { link: t, .. } = wait_for(&arx, 20, |e| matches!(e, Event::Ticket { .. })).await else {
            unreachable!()
        };
        b.join(&t, "Bob".into()).unwrap();
        c.join(&t, "Cyryl".into()).unwrap();
        wait_for(&brx, 30, |e| matches!(e, Event::PeerJoined { name, .. } if name == "Cyryl")).await;
        wait_for(&crx, 30, |e| matches!(e, Event::PeerJoined { name, .. } if name == "Bob")).await;

        let data: Vec<u8> = (0..proto::MAX_FILE_BYTES as u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        b.send_file("../../raport.bin", data.clone()).unwrap();
        for rx in [&arx, &crx] {
            let Event::File { from, name, file_name, data: got } =
                wait_for(rx, 20, |e| matches!(e, Event::File { .. })).await
            else {
                unreachable!()
            };
            assert_eq!((from, name.as_str(), file_name.as_str()), (b.my_id(), "Bob", "raport.bin"));
            assert!(*got == data, "file bytes differ");
        }
        let Event::File { from: None, .. } = wait_for(&brx, 1, |e| matches!(e, Event::File { .. })).await else {
            panic!("no local echo")
        };

        // Too large: refused by the sender.
        assert!(b.send_file("big.bin", vec![0; proto::MAX_FILE_BYTES + 1]).is_err());

        // Too large from a peer that skips the check: dropped, connection kept.
        let ida = a.my_id().unwrap();
        push_raw(&b, ida, Msg::File { name: "big.bin".into(), data: vec![0; proto::MAX_FILE_BYTES + 1] });
        push_raw(&b, ida, Msg::File { name: "huge.bin".into(), data: vec![0; 2 << 20] });
        b.send_chat("po pliku".into()).unwrap();
        let e = wait_for(&arx, 20, |e| matches!(e, Event::File { .. } | Event::Chat { .. } | Event::PeerLeft { .. })).await;
        assert!(matches!(&e, Event::Chat { text, .. } if text == "po pliku"), "{e:?}");
        assert!(a.0.state.lock().unwrap().peers.contains_key(&b.my_id().unwrap()));
        for n in [a, b, c] {
            n.leave();
        }
    }

    /// What waits for the UI is counted from `send` until it is taken.
    #[test]
    fn event_cost_counted_until_taken() {
        use crate::flow::EVENT_OVERHEAD;
        let (ev, rx) = Events::with_budget(eframe::egui::Context::default(), 1000);
        ev.send(Event::Chat { from: None, name: "Ala".into(), text: "x".repeat(500) });
        assert_eq!(ev.pending(), EVENT_OVERHEAD + 503);
        ev.send(Event::Status(String::new()));
        assert_eq!(ev.pending(), 2 * EVENT_OVERHEAD + 503);
        assert!(matches!(rx.try_recv(), Some(Event::Chat { .. })));
        assert_eq!(ev.pending(), EVENT_OVERHEAD);
        assert!(rx.try_recv().is_some());
        assert!(rx.try_recv().is_none());
        assert_eq!(ev.pending(), 0);
        drop(rx);
        ev.send(Event::Status(String::new()));
        assert_eq!(ev.pending(), 0, "nobody to take it");
    }

    /// Every event pays for the text and bytes it holds, whoever sent them.
    #[test]
    fn event_cost_covers_all_data() {
        use crate::flow::EVENT_OVERHEAD;
        let big = || "x".repeat(1 << 20);
        let info = StreamInfo { codec: proto::Codec::H264, width: 1, height: 1, fps: 1, bitrate_kbps: 1, encoder: big() };
        let id = iroh::SecretKey::from_bytes(&[1; 32]).public();
        for e in [
            Event::PeerStream { id, info: Some(info.clone()) },
            Event::MyStream(Some(info)),
            Event::RoomName(big()),
            Event::Status(big()),
            Event::Error(big()),
            Event::JoinFailed(big()),
            Event::Ticket { link: big(), expires: chrono::Local::now() },
            Event::PeerJoined { id, name: big(), avatar: None },
            Event::PeerProfile { id, name: String::new(), avatar: Some(vec![0; 1 << 20]) },
        ] {
            let what: String = format!("{e:?}").chars().take(40).collect();
            assert_eq!(e.cost(), EVENT_OVERHEAD + (1 << 20), "{what}");
        }
    }

    /// While the UI is over its budget a new connection adds no events and
    /// no peer; once the UI catches up, the waiting join completes.
    /// Run with: cargo test e2e_join_while_ui_behind -- --ignored
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn e2e_join_while_ui_behind() {
        /// The next event matching `f`; on timeout says which `stage` it was.
        async fn stage(rx: &EventRx, secs: u64, stage: &str, mut f: impl FnMut(&Event) -> bool) -> Event {
            let deadline = Instant::now() + Duration::from_secs(secs);
            loop {
                if let Some(e) = rx.try_iter().find(|e| f(e)) {
                    return e;
                }
                assert!(Instant::now() < deadline, "timeout: {stage}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        let (aev, arx) = Events::with_budget(eframe::egui::Context::default(), 1 << 20);
        let a = Node::new(tokio::runtime::Handle::current(), aev, iroh::SecretKey::generate());
        let (b, brx) = node();
        a.create_room("Ala".into(), "Test".into());
        let t = ticket(&arx).await;
        a.0.events.send(Event::Chat { from: None, name: "Ala".into(), text: "x".repeat(2 << 20) });
        let before = a.0.events.pending();
        b.join(&t, "Bob".into()).unwrap();

        // Bob gets through authentication and waits at the gate.
        let start = Instant::now();
        while a.0.at_gate.load(Ordering::SeqCst) == 0 {
            if let Some(Event::JoinFailed(msg)) = brx.try_iter().find(|e| matches!(e, Event::JoinFailed(_))) {
                panic!("join failed before reaching the gate: {msg}");
            }
            assert!(start.elapsed() < Duration::from_secs(40), "timeout: connection reaching the gate");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(a.0.at_gate.load(Ordering::SeqCst), 1, "left the gate while the UI was behind");
        assert_eq!(a.0.events.pending(), before);
        assert!(a.0.state.lock().unwrap().peers.is_empty(), "joined while the UI was behind");

        // The UI catches up within the `Hello` timeout.
        assert!(matches!(arx.try_recv(), Some(Event::Chat { .. })));
        stage(&arx, 20, "Ala meeting Bob after the UI caught up", |e| matches!(e, Event::PeerJoined { .. })).await;
        let e = stage(&brx, 20, "Bob meeting Ala after the UI caught up", |e| {
            matches!(e, Event::PeerJoined { .. } | Event::JoinFailed(_))
        })
        .await;
        assert!(matches!(e, Event::PeerJoined { .. }), "{e:?}");
        assert_eq!(a.0.at_gate.load(Ordering::SeqCst), 0);
        a.leave();
        b.leave();
    }

    const SLOW_BUDGET: usize = 4 << 20;

    /// `a` (event budget `SLOW_BUDGET`) and `b` (bulk budget the same) in a
    /// room; `a`'s UI then takes nothing while `b` sends files until its
    /// queue to `a` has refused 20. Returns how many were queued.
    async fn stalled_pair() -> (Node, EventRx, Node, usize) {
        let bound = SLOW_BUDGET + proto::MAX_FILE_BYTES + 4096;
        let rt = tokio::runtime::Handle::current();
        let (aev, arx) = Events::with_budget(eframe::egui::Context::default(), SLOW_BUDGET);
        let a = Node::new(rt.clone(), aev, iroh::SecretKey::generate());
        let (bev, _brx) = Events::new(eframe::egui::Context::default());
        let b = Node::with_bulk_budget(rt, bev, iroh::SecretKey::generate(), SLOW_BUDGET);
        a.create_room("Ala".into(), "Test".into());
        let t = ticket(&arx).await;
        b.join(&t, "Bob".into()).unwrap();
        wait_for(&arx, 30, |e| matches!(e, Event::PeerJoined { .. })).await;
        let ida = a.my_id().unwrap();
        while !b.0.state.lock().unwrap().peers.contains_key(&ida) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        while arx.try_recv().is_some() {}

        // From now on `a`'s UI takes nothing.
        let file = vec![7u8; proto::MAX_FILE_BYTES];
        let (mut queued, mut refused, mut sent_bytes) = (0, 0, 0);
        let start = Instant::now();
        while refused < 20 {
            assert!(start.elapsed() < Duration::from_secs(90), "never refused: {queued} queued");
            match b.send_file("x.bin", file.clone()).unwrap().as_slice() {
                [] => queued += 1,
                [who] if who == "Ala" => refused += 1,
                other => panic!("{other:?}"),
            }
            sent_bytes += file.len();
            assert!(a.0.events.pending() <= bound, "pending {} B", a.0.events.pending());
            // Held by the sender, the receiver and QUIC buffers in between.
            assert!(queued <= 2 * SLOW_BUDGET / proto::MAX_FILE_BYTES + 8, "sender queued {queued} files");
            tokio::time::sleep(Duration::from_millis(if refused > 0 { 50 } else { 10 })).await;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        let pending = a.0.events.pending();
        println!("{queued} queued, {refused} refused, {} MB tried, {pending} B pending", sent_bytes >> 20);
        assert!(pending <= bound, "pending {pending} B");
        assert!(pending > SLOW_BUDGET / 2, "the receiver wasn't the one holding back: {pending} B");
        assert!(sent_bytes > 4 * SLOW_BUDGET);
        (a, arx, b, queued)
    }

    /// A receiver whose UI takes no events (hidden window): it holds no
    /// more than its event budget plus one message in flight, the sender's
    /// queue refuses the rest and says so, nothing that was queued is lost,
    /// and the connection works again once the UI catches up.
    /// Run with: cargo test e2e_slow_receiver -- --ignored
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn e2e_slow_receiver() {
        let (a, arx, b, queued) = stalled_pair().await;
        let mut files = 0;
        let start = Instant::now();
        while files < queued {
            assert!(start.elapsed() < Duration::from_secs(60), "{files} of {queued} files");
            for e in arx.try_iter() {
                match e {
                    Event::File { data, .. } => {
                        assert_eq!(data.len(), proto::MAX_FILE_BYTES);
                        files += 1;
                    }
                    Event::PeerLeft { .. } => panic!("disconnected"),
                    _ => {}
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(b.send_chat("już jest".into()).unwrap(), Vec::<String>::new());
        let e = wait_for(&arx, 20, |e| matches!(e, Event::Chat { .. } | Event::File { .. } | Event::PeerLeft { .. })).await;
        assert!(matches!(&e, Event::Chat { text, .. } if text == "już jest"), "{e:?}");
        assert!(a.0.state.lock().unwrap().peers.contains_key(&b.my_id().unwrap()));
        a.leave();
        b.leave();
    }

    /// Leaving while a reader waits for the UI ends it: no task is left
    /// holding the node.
    /// Run with: cargo test e2e_slow_receiver_leave -- --ignored
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn e2e_slow_receiver_leave() {
        let (a, _arx, b, _) = stalled_pair().await;
        a.leave();
        let start = Instant::now();
        while Arc::strong_count(&a.0) > 1 {
            assert!(start.elapsed() < Duration::from_secs(10), "{} references left", Arc::strong_count(&a.0));
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        b.leave();
    }

    /// Past the burst, control messages from a peer are read at the
    /// steady rate; none are lost on the way.
    /// Run with: cargo test e2e_rate_limit -- --ignored
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn e2e_rate_limit() {
        let (a, arx) = node();
        let (b, brx) = node();
        a.create_room("Ala".into(), "Test".into());
        b.join(&ticket(&arx).await, "Bob".into()).unwrap();
        wait_for(&arx, 30, |e| matches!(e, Event::PeerJoined { .. })).await;
        wait_for(&brx, 30, |e| matches!(e, Event::PeerJoined { .. })).await;
        let n = crate::flow::MSG_BURST as usize + 60;
        let start = Instant::now();
        for i in 0..n {
            assert!(b.send_chat(i.to_string()).unwrap().is_empty());
        }
        let mut got = Vec::new();
        while got.len() < n {
            let Event::Chat { text, .. } = wait_for(&arx, 20, |e| matches!(e, Event::Chat { .. })).await else {
                unreachable!()
            };
            got.push(text.parse::<usize>().unwrap());
        }
        let took = start.elapsed();
        println!("{n} messages in {took:?}");
        assert_eq!(got, (0..n).collect::<Vec<_>>());
        // 60 past the burst at 20/s: 3 s, minus what refills meanwhile.
        assert!(took >= Duration::from_millis(2500), "{took:?}");
        assert!(took < Duration::from_secs(8), "{took:?}");
        a.leave();
        b.leave();
    }

    /// Two peers over the real network: chat, then a test-pattern stream.
    /// Run with: BIBICORD_FFMPEG=/path/to/ffmpeg cargo test -- --ignored
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn e2e_chat_and_stream() {
        let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).try_init();
        unsafe {
            std::env::set_var("BIBICORD_TEST_SOURCE", "1");
            std::env::set_var("BIBICORD_FAKE_AUDIO", "1");
            std::env::set_var("BIBICORD_FAKE_TONE", "1");
        }
        tokio::task::spawn_blocking(|| crate::ffmpeg::ensure(|_| {})).await.unwrap().unwrap();

        let (a, arx) = node();
        let (b, brx) = node();
        let pic = vec![7u8; 1000];
        a.set_avatar(Some(pic.clone()));
        a.create_room("Ala".into(), "Test".into());
        let Event::Ticket { link: t, .. } = wait_for(&arx, 20, |e| matches!(e, Event::Ticket { .. })).await else {
            unreachable!()
        };
        b.join(&t, "Bob".into()).unwrap();
        wait_for(&arx, 30, |e| matches!(e, Event::PeerJoined { name, .. } if name == "Bob")).await;
        wait_for(&brx, 30, |e| matches!(e, Event::PeerJoined { name, avatar: Some(p), .. } if name == "Ala" && *p == pic)).await;

        // A third peer joins through A and must find B on its own (mesh).
        let (c, crx) = node();
        c.join(&t, "Cyryl".into()).unwrap();
        wait_for(&crx, 30, |e| matches!(e, Event::PeerJoined { name, .. } if name == "Bob")).await;
        wait_for(&brx, 30, |e| matches!(e, Event::PeerJoined { name, .. } if name == "Cyryl")).await;
        c.leave();

        b.send_chat("cześć".into()).unwrap();
        wait_for(&arx, 10, |e| matches!(e, Event::Chat { text, from: Some(_), .. } if text == "cześć")).await;

        // Profile change reaches the others and is used for later chat.
        b.set_profile("Bobek".into(), Some(pic.clone()));
        wait_for(&arx, 10, |e| matches!(e, Event::PeerProfile { name, avatar: Some(_), .. } if name == "Bobek")).await;
        b.send_chat("już Bobek".into()).unwrap();
        wait_for(&arx, 10, |e| matches!(e, Event::Chat { name, .. } if name == "Bobek")).await;

        // Voice both ways; it keeps running while the stream below is watched.
        a.join_voice();
        b.join_voice();
        wait_for(&arx, 10, |e| matches!(e, Event::MyVoice(true))).await;
        wait_for(&brx, 10, |e| matches!(e, Event::MyVoice(true))).await;
        let voice_start = Instant::now();

        a.start_stream(video::encode::Settings {
            bitrate_kbps: 20_000,
            ..Default::default()
        });
        let Event::MyStream(Some(info)) =
            wait_for(&arx, 60, |e| matches!(e, Event::MyStream(_) | Event::Error(_))).await
        else {
            panic!("stream failed to start")
        };
        println!("streaming: {info:?}");
        let Event::PeerStream { id, .. } =
            wait_for(&brx, 10, |e| matches!(e, Event::PeerStream { info: Some(_), .. })).await
        else {
            unreachable!()
        };
        println!("path: {:?}", b.path_kind(&id));
        b.watch(id);
        let start = Instant::now();
        let slot = b.frames();
        let mut first = None;
        while slot.count() < 120 {
            if first.is_none() && slot.count() > 0 {
                first = Some(start.elapsed());
            }
            assert!(start.elapsed() < Duration::from_secs(30), "no frames ({})", slot.count());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let f = slot.take().unwrap();
        println!(
            "first frame after {:?}, 120 frames after {:?}, {}x{}",
            first.unwrap(),
            start.elapsed(),
            f.width,
            f.height
        );
        assert_eq!((f.width, f.height), (1920, 1080));

        // Files both ways (1 MB each) don't interrupt the stream or voice.
        let file: Vec<u8> = (0..proto::MAX_FILE_BYTES).map(|i| (i % 251) as u8).collect();
        let frames = slot.count();
        a.send_file("a.bin", file.clone()).unwrap();
        b.send_file("b.bin", file.clone()).unwrap();
        wait_for(&brx, 10, |e| matches!(e, Event::File { from: Some(_), data, .. } if **data == file)).await;
        wait_for(&arx, 10, |e| matches!(e, Event::File { from: Some(_), data, .. } if **data == file)).await;
        let t = Instant::now();
        while slot.count() < frames + 60 {
            assert!(t.elapsed() < Duration::from_secs(5), "stream stalled after file");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(b.watching().is_some());

        // Voice from A as heard by B (fake tone: 2 s on, 1.5 s off).
        tokio::time::sleep(Duration::from_secs(3)).await;
        let secs = voice_start.elapsed().as_secs_f64();
        let st = b.voice().unwrap().stats();
        let (_, st) = st.iter().find(|(id, _)| *id == a.my_id().unwrap()).copied().unwrap();
        println!("voice A->B over {secs:.1} s while streaming: {st:?}");
        let expected = secs * 50.0 * 2.0 / 3.5;
        assert!(st.received as f64 > expected * 0.6, "too few voice packets: {st:?}");
        assert!(st.underruns <= 2, "{st:?}");
        a.stop_stream();
        wait_for(&brx, 10, |e| matches!(e, Event::Watching(None))).await;
        a.leave();
        b.leave();
    }

    /// Two nodes, A streams its real screen to B. Settings from the
    /// environment: BIBICORD_TEST_CODEC=hevc, BIBICORD_TEST_MAX_HEIGHT=720.
    async fn screen_stream() -> (Node, EventRx, Node, EventRx) {
        let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).try_init();
        unsafe {
            std::env::set_var("BIBICORD_FAKE_AUDIO", "1");
        }
        tokio::task::spawn_blocking(|| crate::ffmpeg::ensure(|_| {})).await.unwrap().unwrap();
        let env = |k: &str| std::env::var(k).ok();
        let settings = video::encode::Settings {
            codec: if env("BIBICORD_TEST_CODEC").as_deref() == Some("hevc") { proto::Codec::Hevc } else { proto::Codec::H264 },
            max_height: env("BIBICORD_TEST_MAX_HEIGHT").and_then(|s| s.parse().ok()).unwrap_or(0),
            fps: 30,
            bitrate_kbps: 8_000,
            ..Default::default()
        };

        let (a, arx) = node();
        let (b, brx) = node();
        a.create_room("Ala".into(), "Test".into());
        let Event::Ticket { link: t, .. } = wait_for(&arx, 20, |e| matches!(e, Event::Ticket { .. })).await else {
            unreachable!()
        };
        b.join(&t, "Bob".into()).unwrap();
        wait_for(&brx, 30, |e| matches!(e, Event::PeerJoined { .. })).await;
        a.start_stream(settings);
        let Event::MyStream(Some(info)) =
            wait_for(&arx, 90, |e| matches!(e, Event::MyStream(_) | Event::Error(_))).await
        else {
            panic!("stream failed to start")
        };
        println!("streaming: {info:?}");
        let Event::PeerStream { id, .. } =
            wait_for(&brx, 10, |e| matches!(e, Event::PeerStream { info: Some(_), .. })).await
        else {
            unreachable!()
        };
        b.watch(id);
        (a, arx, b, brx)
    }

    /// Real screen capture that survives losing it: while it runs, switch a
    /// game to fullscreen, Alt+Tab, change the display resolution or open UAC.
    /// The stream must not end and frames must keep coming.
    /// Run with: BIBICORD_FFMPEG=/path/to/ffmpeg BIBICORD_TEST_SECS=60 \
    ///   cargo test e2e_screen_capture_loss -- --ignored --nocapture
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn e2e_screen_capture_loss() {
        let secs = std::env::var("BIBICORD_TEST_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(30);
        let (a, arx, b, _brx) = screen_stream().await;
        println!(">>> {secs} s: go fullscreen / change resolution now");
        let slot = b.frames();
        let start = Instant::now();
        let mut size = None;
        let mut last_frame = Instant::now();
        let mut longest_gap = Duration::ZERO;
        let mut count = slot.count();
        while start.elapsed() < Duration::from_secs(secs) {
            while let Some(e) = arx.try_recv() {
                assert!(!matches!(e, Event::Error(_) | Event::MyStream(None)), "stream ended: {e:?}");
            }
            if let Some(f) = slot.take() {
                if size != Some((f.width, f.height)) {
                    println!("{:>5.1} s: {}x{}", start.elapsed().as_secs_f32(), f.width, f.height);
                    size = Some((f.width, f.height));
                }
            }
            if slot.count() != count {
                count = slot.count();
                let gap = last_frame.elapsed();
                if gap > Duration::from_millis(300) {
                    println!("{:>5.1} s: no picture for {:.2} s", start.elapsed().as_secs_f32(), gap.as_secs_f32());
                }
                longest_gap = longest_gap.max(gap);
                last_frame = Instant::now();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        println!("{count} frames, longest gap {longest_gap:?}");
        // BIBICORD_TEST_STOP_WHILE_LOST=1: the screen is still gone at the
        // end, the stream is stopped in the middle of recovering it.
        let lost_at_end = std::env::var_os("BIBICORD_TEST_STOP_WHILE_LOST").is_some();
        assert_eq!(last_frame.elapsed() > Duration::from_secs(2), lost_at_end, "frames at the end");
        assert!(b.watching().is_some());
        a.stop_stream();
        a.leave();
        b.leave();
    }

    /// Screen capture that doesn't come back (here: another desktop shown
    /// for longer than the recovery window) ends the stream with a message
    /// instead of restarting forever.
    /// Run with: BIBICORD_FFMPEG=/path/to/ffmpeg \
    ///   cargo test e2e_screen_capture_gone -- --ignored --nocapture
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn e2e_screen_capture_gone() {
        let (a, arx, b, brx) = screen_stream().await;
        println!(">>> take the screen away for good now");
        let start = Instant::now();
        let Event::Error(msg) = wait_for(&arx, 120, |e| matches!(e, Event::Error(_))).await else {
            unreachable!()
        };
        println!("{:>5.1} s: {msg}", start.elapsed().as_secs_f32());
        assert!(msg.contains("nie udało się wznowić przechwytywania ekranu"), "{msg}");
        assert!(a.0.state.lock().unwrap().broadcast.is_none());
        wait_for(&brx, 10, |e| matches!(e, Event::Watching(None))).await;
        a.leave();
        b.leave();
    }
}