//! Serverless room: a full mesh of direct, end-to-end encrypted QUIC
//! connections (iroh). NAT hole punching is automatic; n0's public relays
//! are only used for the initial rendezvous or when a direct path is
//! impossible. There is no server of our own.

use crate::proto::{self, ALPN, DGRAM_VOICE, Msg, StreamInfo, TAG_CAMERA, TAG_CONTROL, TAG_WATCH};
use crate::video::{self, decode::FrameSlot};
use crate::voice::Voice;
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use iroh::endpoint::{Connection, presets};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, EndpointId};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{OnceCell, mpsc};

type BoxFuture<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'static>>;

#[derive(Debug, Clone)]
pub enum Event {
    Ready,
    Ticket(String),
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

/// Delivers events to the UI and wakes it up.
#[derive(Clone)]
pub struct Events {
    tx: std::sync::mpsc::Sender<Event>,
    ctx: eframe::egui::Context,
}

impl Events {
    pub fn new(tx: std::sync::mpsc::Sender<Event>, ctx: eframe::egui::Context) -> Self {
        Events { tx, ctx }
    }

    pub fn send(&self, e: Event) {
        let _ = self.tx.send(e);
        self.ctx.request_repaint();
    }

    pub fn repaint(&self) {
        self.ctx.request_repaint();
    }
}

struct PeerConn {
    name: String,
    conn: Connection,
    ctrl: mpsc::UnboundedSender<Msg>,
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
}

#[derive(Clone)]
pub struct Node(Arc<Inner>);

struct Inner {
    rt: tokio::runtime::Handle,
    endpoint: OnceCell<(Endpoint, Router)>,
    events: Events,
    state: Mutex<State>,
    pub frames: Arc<FrameSlot>,
    /// This session's invite code; the endpoint key is derived from it.
    code: String,
}

#[derive(Debug, Clone)]
struct Handler(std::sync::Weak<Inner>);

impl ProtocolHandler for Handler {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let Some(inner) = self.0.upgrade() else { return Ok(()) };
        let node = Node(inner);
        if !node.0.state.lock().unwrap().in_room {
            conn.close(2u32.into(), b"not in a room");
            return Ok(());
        }
        if let Err(e) = node.run_peer(conn, false).await {
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
    pub fn new(rt: tokio::runtime::Handle, events: Events) -> Self {
        let node = Node(Arc::new(Inner {
            rt,
            endpoint: OnceCell::new(),
            events,
            state: Mutex::new(State::default()),
            frames: Arc::new(FrameSlot::default()),
            code: proto::new_code(),
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
                let code = self.0.code.clone();
                let key = tokio::task::spawn_blocking(move || proto::code_key(&code)).await?;
                let ep = Endpoint::builder(presets::N0)
                    .secret_key(key)
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
        }
        self.emit(Event::RoomName(room));
        let n = self.clone();
        self.spawn(async move { n.publish_ticket().await });
    }

    pub fn join(&self, ticket: &str, name: String) -> Result<()> {
        let ticket = ticket.trim().to_string();
        if proto::parse_code(&ticket).is_none() {
            proto::decode_ticket(&ticket)?; // fails with a readable message
        }
        {
            let mut s = self.0.state.lock().unwrap();
            s.in_room = true;
            s.name = name;
            s.room.clear();
        }
        let n = self.clone();
        self.spawn(async move {
            n.emit(Event::Status("Łączenie z pokojem…".into()));
            let addr = match tokio::task::spawn_blocking(move || proto::decode_ticket(&ticket)).await {
                Ok(Ok(a)) => a,
                Ok(Err(e)) => return n.emit(Event::Error(format!("Nie udało się dołączyć: {e:#}"))),
                Err(e) => return n.emit(Event::Error(format!("Nie udało się dołączyć: {e}"))),
            };
            if let Err(e) = n.dial(addr).await {
                tracing::info!("join failed: {e:#}");
                let msg = if format!("{e:#}").contains("doesn't support any known protocol") {
                    "Znajomy ma inną wersję BibiCord. Zaktualizujcie aplikację do najnowszej wersji."
                } else {
                    "Nie udało się dołączyć. Sprawdź kod i czy osoba, która go wysłała, \
                     ma otwarty BibiCord."
                };
                n.emit(Event::Error(msg.into()));
            }
            n.publish_ticket().await;
        });
        Ok(())
    }

    async fn publish_ticket(&self) {
        let Ok(ep) = self.endpoint().await else { return };
        // The code resolves via address lookup, which is published once the
        // endpoint has a home relay.
        let _ = tokio::time::timeout(Duration::from_secs(8), ep.online()).await;
        self.emit(Event::Ticket(proto::format_code(&self.0.code)));
    }

    pub fn leave(&self) {
        let mut s = self.0.state.lock().unwrap();
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
    }

    pub fn send_chat(&self, text: String) {
        let s = self.0.state.lock().unwrap();
        for p in s.peers.values() {
            let _ = p.ctrl.send(Msg::Chat { text: text.clone() });
        }
        let name = s.name.clone();
        drop(s);
        self.emit(Event::Chat { from: None, name, text });
    }

    /// Posts a file to everyone in the room; at most `MAX_FILE_BYTES`.
    pub fn send_file(&self, file_name: &str, data: Vec<u8>) -> Result<()> {
        if data.len() > proto::MAX_FILE_BYTES {
            bail!("Plik jest za duży (maks. 1 MB)");
        }
        let file_name = proto::clean_file_name(file_name);
        let s = self.0.state.lock().unwrap();
        for p in s.peers.values() {
            let _ = p.ctrl.send(Msg::File { name: file_name.clone(), data: data.clone() });
        }
        let name = s.name.clone();
        drop(s);
        self.emit(Event::File { from: None, name, file_name, data: Arc::new(data) });
        Ok(())
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

    fn hello(&self) -> Msg {
        let s = self.0.state.lock().unwrap();
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
        let s = self.0.state.lock().unwrap();
        for p in s.peers.values() {
            let _ = p.ctrl.send(msg.clone());
        }
    }

    fn dial(&self, addr: EndpointAddr) -> BoxFuture<Result<()>> {
        let n = self.clone();
        Box::pin(async move { n.dial_inner(addr).await })
    }

    async fn dial_inner(&self, addr: EndpointAddr) -> Result<()> {
        let ep = self.endpoint().await?.clone();
        let id = addr.id;
        if id == ep.id() {
            return Ok(());
        }
        {
            let mut s = self.0.state.lock().unwrap();
            if s.peers.contains_key(&id) || !s.dialing.insert(id) {
                return Ok(());
            }
        }
        let res = tokio::time::timeout(Duration::from_secs(25), ep.connect(addr, ALPN)).await;
        self.0.state.lock().unwrap().dialing.remove(&id);
        let conn = res.context("przekroczono czas")??;
        let n = self.clone();
        self.spawn(async move {
            if let Err(e) = n.run_peer(conn, true).await {
                tracing::debug!("outgoing peer ended: {e:#}");
            }
        });
        Ok(())
    }

    async fn run_peer(&self, conn: Connection, dialed: bool) -> Result<()> {
        let ep = self.endpoint().await?.clone();
        let me = ep.id();
        let remote = conn.remote_id();

        let (mut send, mut recv) = if dialed {
            let (mut s, r) = conn.open_bi().await?;
            s.write_all(&[TAG_CONTROL]).await?;
            (s, r)
        } else {
            let (s, mut r) = tokio::time::timeout(Duration::from_secs(15), conn.accept_bi()).await??;
            let mut tag = [0u8];
            r.read_exact(&mut tag).await?;
            if tag[0] != TAG_CONTROL {
                bail!("unexpected first stream");
            }
            (s, r)
        };
        proto::write_msg(&mut send, &self.hello()).await?;
        let Msg::Hello { name, room, peers, stream, camera, voice, muted, deafened, avatar } =
            tokio::time::timeout(Duration::from_secs(15), proto::read_msg(&mut recv)).await??
        else {
            bail!("expected hello");
        };
        let name = peer_display_name(&name, &remote);
        let avatar = avatar.filter(|a| a.len() <= crate::avatar::MAX_BYTES);

        let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
        let stable = conn.stable_id();
        let mut adopted_room = None;
        {
            let mut s = self.0.state.lock().unwrap();
            if s.room.is_empty() && !room.is_empty() {
                s.room = room.clone();
                adopted_room = Some(room);
            }
            if !s.in_room {
                conn.close(2u32.into(), b"left");
                return Ok(());
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
            s.peers.insert(
                remote,
                PeerConn {
                    name: name.clone(),
                    conn: conn.clone(),
                    ctrl: tx,
                    dialer,
                    voice,
                    stream: stream.clone(),
                    camera: None,
                },
            );
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
                    if let Err(e) = n.dial(EndpointAddr::new(p)).await {
                        tracing::debug!("mesh dial {p}: {e:#}");
                    }
                });
            }
        }

        // Writer.
        self.spawn(async move {
            while let Some(m) = rx.recv().await {
                if proto::write_msg(&mut send, &m).await.is_err() {
                    break;
                }
            }
        });

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

        // Reader.
        while let Ok(msg) = proto::read_msg(&mut recv).await {
            match msg {
                Msg::Chat { text } => {
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
                Msg::Camera(info) => self.set_peer_camera(remote, info),
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
                Msg::Hello { .. } => {}
            }
        }

        // Cleanup (only if this connection is still the active one).
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
                    n.0.state.lock().unwrap().broadcast = Some(b);
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
                        if !s.in_room || s.voice.is_none() {
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
                let dec = video::decode::Decoder::start(codec, slot2, move || events.repaint())?;
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
                let dec = video::decode::Decoder::start(codec, n.0.frames.clone(), move || {
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
        if self.voice().is_some() {
            return;
        }
        let n = self.clone();
        self.spawn(async move {
            let (tx, mut rx) = mpsc::unbounded_channel::<Bytes>();
            let (devices, mute, prefs) = {
                let s = n.0.state.lock().unwrap();
                (s.devices.clone(), s.mute, s.voice_prefs)
            };
            match tokio::task::spawn_blocking(move || {
                Voice::start(tx, devices, mute.muted || mute.deafened, mute.deafened, prefs)
            })
            .await
            {
                Ok(Ok(v)) => {
                    n.0.state.lock().unwrap().voice = Some(Arc::new(v));
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
        let in_voice = {
            let mut s = self.0.state.lock().unwrap();
            if s.devices == devices {
                return;
            }
            s.devices = devices;
            s.voice.is_some()
        };
        if in_voice {
            self.0.state.lock().unwrap().voice = None;
            self.join_voice();
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
        let v = self.0.state.lock().unwrap().voice.take();
        if v.is_some() {
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

    fn node() -> (Node, std::sync::mpsc::Receiver<Event>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let ev = Events::new(tx, eframe::egui::Context::default());
        (Node::new(tokio::runtime::Handle::current(), ev), rx)
    }

    async fn wait_for(
        rx: &std::sync::mpsc::Receiver<Event>,
        secs: u64,
        mut f: impl FnMut(&Event) -> bool,
    ) -> Event {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            while let Ok(e) = rx.try_recv() {
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
        let Event::Ticket(t) = wait_for(&arx, 20, |e| matches!(e, Event::Ticket(_))).await else {
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
        let Event::Ticket(t) = wait_for(&arx, 20, |e| matches!(e, Event::Ticket(_))).await else {
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
        let ctrl = b.0.state.lock().unwrap().peers.get(&a.my_id().unwrap()).unwrap().ctrl.clone();
        ctrl.send(Msg::File { name: "big.bin".into(), data: vec![0; proto::MAX_FILE_BYTES + 1] }).unwrap();
        ctrl.send(Msg::File { name: "huge.bin".into(), data: vec![0; 2 << 20] }).unwrap();
        b.send_chat("po pliku".into());
        let e = wait_for(&arx, 20, |e| matches!(e, Event::File { .. } | Event::Chat { .. } | Event::PeerLeft { .. })).await;
        assert!(matches!(&e, Event::Chat { text, .. } if text == "po pliku"), "{e:?}");
        assert!(a.0.state.lock().unwrap().peers.contains_key(&b.my_id().unwrap()));
        for n in [a, b, c] {
            n.leave();
        }
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
        let Event::Ticket(t) = wait_for(&arx, 20, |e| matches!(e, Event::Ticket(_))).await else {
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

        b.send_chat("cześć".into());
        wait_for(&arx, 10, |e| matches!(e, Event::Chat { text, from: Some(_), .. } if text == "cześć")).await;

        // Profile change reaches the others and is used for later chat.
        b.set_profile("Bobek".into(), Some(pic.clone()));
        wait_for(&arx, 10, |e| matches!(e, Event::PeerProfile { name, avatar: Some(_), .. } if name == "Bobek")).await;
        b.send_chat("już Bobek".into());
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
}

