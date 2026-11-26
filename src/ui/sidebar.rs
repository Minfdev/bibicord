//! Left column: room header, channels, people, voice + user panel.

use super::theme::{self, W, c, f, r, sp, ty};
use super::widgets::{self as w, Avatar, Button, IconButton, Kind, ic};
use super::{App, Dialog, View};
use crate::net::MuteState;
use eframe::egui::{self, Align2, Color32, Pos2, Rect, Sense, Stroke, UiBuilder, Vec2};
use iroh::EndpointId;
use std::time::Duration;

/// One person as shown in lists.
pub(crate) struct Member {
    pub id: Option<EndpointId>, // None = me
    pub name: String,
    pub color: Color32,
    pub avatar: Option<egui::TextureHandle>,
    pub voice: bool,
    pub live: bool,
    pub mute: MuteState,
    pub speaking: bool,
    pub camera: bool,
    /// Latest camera frame and its size.
    pub video: Option<(egui::TextureHandle, Vec2)>,
}

impl App {
    /// Everyone in the room, me first.
    pub(crate) fn members(&self) -> Vec<Member> {
        let voice = self.node.voice();
        let speaking = voice.as_ref().map(|v| v.speaking()).unwrap_or_default();
        let me_speaking = voice.as_ref().is_some_and(|v| v.me_speaking());
        let mut v = vec![Member {
            id: None,
            name: self.name.clone(),
            color: self.my_color(),
            avatar: self.my_avatar_tex.clone(),
            voice: self.my_voice,
            live: self.my_stream.is_some(),
            mute: self.mute,
            speaking: me_speaking && !self.mute.muted && !self.mute.deafened,
            camera: self.camera_on,
            video: self.my_id.and_then(|me| self.cam_tex.get(&me).cloned()).filter(|_| self.camera_on),
        }];
        for p in &self.peers {
            v.push(Member {
                id: Some(p.id),
                name: p.name.clone(),
                color: p.color,
                avatar: p.avatar.clone(),
                voice: p.voice,
                live: p.stream.is_some(),
                mute: p.mute,
                speaking: speaking.contains(&p.id) && p.voice,
                camera: p.camera,
                video: self.cam_tex.get(&p.id).cloned().filter(|_| p.camera),
            });
        }
        v
    }

    pub(crate) fn sidebar(&mut self, root: &mut egui::Ui) {
        egui::Panel::left("sidebar")
            .exact_size(theme::SIDEBAR_W)
            .resizable(false)
            .frame(egui::Frame::new().fill(c::BG_SIDEBAR))
            .show(root, |ui| {
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                self.room_header(ui);
                egui::Panel::bottom("me_panel")
                    .resizable(false)
                    .frame(egui::Frame::new().inner_margin(egui::Margin::same(sp::S as i8)))
                    .show(ui, |ui| self.bottom_card(ui));
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(sp::S as i8, 0)))
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                            ui.spacing_mut().item_spacing = Vec2::new(0.0, 2.0);
                            self.channel_list(ui);
                        });
                    });
            });
    }

    fn room_header(&mut self, ui: &mut egui::Ui) {
        let full = ui.available_rect_before_wrap();
        let rect = Rect::from_min_size(full.min, Vec2::new(full.width(), theme::HEADER_H));
        ui.allocate_rect(rect, Sense::hover());
        let menu_rect = Rect::from_min_max(rect.min, Pos2::new(rect.right() - 44.0, rect.bottom()));
        let resp = ui.interact(menu_rect, ui.make_persistent_id("room_menu_btn"), Sense::click());
        let t = w::hover_t(ui, &resp);
        ui.painter().rect_filled(menu_rect, 0.0, theme::alpha(c::BG_RAISED, t));
        let title = if self.room.is_empty() { "Łączenie…" } else { &self.room };
        w::paint_text_clipped(
            ui,
            menu_rect.left_center() + Vec2::new(sp::L, 0.0),
            title,
            f(ty::HEADING),
            c::TEXT,
            menu_rect.width() - 48.0,
        );
        w::paint_icon(ui, menu_rect.right_center() - Vec2::new(14.0, 0.0), ic::CARET_DOWN, 14.0, c::TEXT_SECONDARY, false);
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);

        let btn = Rect::from_center_size(rect.right_center() - Vec2::new(24.0, 0.0), Vec2::splat(32.0));
        let mut invite = false;
        ui.scope_builder(UiBuilder::new().max_rect(btn), |ui| {
            invite = IconButton::new(ic::USER_PLUS, "Zaproś znajomych").show(ui).clicked();
        });
        if invite {
            self.open_invite();
        }
        ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, c::BG_BASE));

        let mut action = None;
        egui::Popup::menu(&resp)
            .frame(w::menu_frame())
            .width(rect.width() - 2.0 * sp::S)
            .show(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(0.0, 2.0);
                if w::menu_item(ui, "Zaproś znajomych", ic::USER_PLUS, false).clicked() {
                    action = Some(0);
                }
                if w::menu_item(ui, "Wyślij link do BibiCord", ic::DOWNLOAD_SIMPLE, false).clicked() {
                    action = Some(3);
                }
                if w::menu_item(ui, "Profil", ic::USER_CIRCLE, false).clicked() {
                    action = Some(4);
                }
                if w::menu_item(ui, "Ustawienia", ic::GEAR_SIX, false).clicked() {
                    action = Some(1);
                }
                ui.add_space(4.0);
                w::separator(ui, c::BORDER);
                ui.add_space(4.0);
                if w::menu_item(ui, "Opuść pokój", ic::SIGN_OUT, true).clicked() {
                    action = Some(2);
                }
            });
        match action {
            Some(0) => self.open_invite(),
            Some(1) => self.open_settings(),
            Some(2) => self.leave_room(&ui.ctx().clone()),
            Some(3) => self.dialog = Some(Dialog::Share),
            Some(4) => self.open_profile(),
            _ => {}
        }
    }

    fn channel_list(&mut self, ui: &mut egui::Ui) {
        let members = self.members();

        w::section_label(ui, "Kanały tekstowe");
        let (resp, rect, t) = w::row(ui, 34.0, self.view == View::Chat);
        let active = self.view == View::Chat;
        let strong = active || self.unread > 0;
        let fg = if strong { c::TEXT } else { theme::mix(c::TEXT_MUTED, c::TEXT_SECONDARY, t) };
        w::paint_icon(ui, rect.left_center() + Vec2::new(18.0, 0.0), ic::HASH, 19.0, fg, false);
        let font = if self.unread > 0 { theme::font(15.0, W::SemiBold) } else { f(ty::LABEL) };
        w::paint_text(ui, rect.left_center() + Vec2::new(36.0, 0.0), Align2::LEFT_CENTER, "czat", font, fg);
        if self.unread > 0 {
            let b = Rect::from_min_size(rect.right_center() - Vec2::new(36.0, 9.0), Vec2::new(28.0, 18.0));
            ui.scope_builder(UiBuilder::new().max_rect(b).layout(egui::Layout::right_to_left(egui::Align::Center)), |ui| {
                w::count_badge(ui, self.unread)
            });
            // Unread pip on the edge, like a notification marker.
            ui.painter().rect_filled(
                Rect::from_center_size(Pos2::new(rect.left() - sp::S, rect.center().y), Vec2::new(8.0, 8.0)),
                4,
                c::TEXT,
            );
        }
        if resp.clicked() {
            self.view = View::Chat;
        }

        w::section_label(ui, "Kanały głosowe");
        let in_voice: Vec<&Member> = members.iter().filter(|m| m.voice).collect();
        let (resp, rect, t) = w::row(ui, 34.0, self.view == View::Voice);
        let active = self.view == View::Voice;
        let fg = if active { c::TEXT } else { theme::mix(c::TEXT_MUTED, c::TEXT_SECONDARY, t) };
        let vicon = if in_voice.is_empty() { ic::SPEAKER_SIMPLE_HIGH } else { ic::SPEAKER_HIGH };
        let vcol = if !in_voice.is_empty() { c::SUCCESS } else { fg };
        w::paint_icon(ui, rect.left_center() + Vec2::new(18.0, 0.0), vicon, 19.0, vcol, false);
        w::paint_text(ui, rect.left_center() + Vec2::new(36.0, 0.0), Align2::LEFT_CENTER, "Głosowy", f(ty::LABEL), fg);
        if !in_voice.is_empty() {
            w::paint_text(
                ui,
                rect.right_center() - Vec2::new(10.0, 0.0),
                Align2::RIGHT_CENTER,
                &in_voice.len().to_string(),
                f(ty::CAPTION),
                c::TEXT_MUTED,
            );
        }
        let resp = resp.on_hover_text(if self.my_voice { "Pokaż scenę" } else { "Dołącz do głosu" });
        if resp.clicked() {
            self.view = View::Voice;
            self.join_voice();
        }

        let mut watch = None;
        for m in &in_voice {
            if let Some(id) = self.voice_member_row(ui, m) {
                watch = Some(id);
            }
        }

        w::section_label(ui, &format!("W pokoju — {}", members.len()));
        for m in &members {
            if let Some(id) = self.member_row(ui, m) {
                watch = Some(id);
            }
        }
        if self.peers.is_empty() {
            ui.add_space(sp::S);
            ui.horizontal(|ui| {
                ui.add_space(sp::S);
                ui.vertical(|ui| {
                    ui.set_max_width(ui.available_width() - sp::S);
                    ui.label(w::text(
                        "Nikogo tu jeszcze nie ma. Wyślij zaproszenie znajomym.",
                        ty::SMALL,
                        c::TEXT_MUTED,
                    ));
                    ui.add_space(sp::S);
                    if Button::new("Zaproś", Kind::Secondary)
                        .icon(ic::USER_PLUS)
                        .height(32.0)
                        .show(ui)
                        .clicked()
                    {
                        self.open_invite();
                    }
                });
            });
        }
        ui.add_space(sp::L);
        if let Some(id) = watch {
            self.watch(id);
        }
    }

    /// Compact participant row under the voice channel. Returns a peer to watch.
    fn voice_member_row(&self, ui: &mut egui::Ui, m: &Member) -> Option<EndpointId> {
        let (resp, rect, t) = w::row(ui, 30.0, false);
        let speak = ui.ctx().animate_bool_with_time(
            resp.id.with("speak"),
            m.speaking,
            if m.speaking { 0.08 } else { 0.3 },
        );
        Avatar { name: &m.name, color: m.color, image: m.avatar.as_ref(), radius: 10.0, speaking: speak, status: None }
            .paint(ui, rect.left_center() + Vec2::new(46.0, 0.0));
        let mut right = rect.right() - 6.0;
        let mut clicked_live = false;
        if m.live {
            let b = w::live_badge(ui, Pos2::new(right, rect.center().y), Align2::RIGHT_CENTER, true);
            right = b.left() - 6.0;
            clicked_live = m.id.is_some();
        }
        if m.camera {
            w::paint_icon(ui, Pos2::new(right - 8.0, rect.center().y), ic::VIDEO_CAMERA, 15.0, c::TEXT_MUTED, false);
            right -= 20.0;
        }
        if m.mute.deafened {
            w::paint_icon(ui, Pos2::new(right - 8.0, rect.center().y), ic::SPEAKER_SIMPLE_SLASH, 15.0, c::DANGER, false);
            right -= 20.0;
        }
        if m.mute.muted || m.mute.deafened {
            w::paint_icon(ui, Pos2::new(right - 8.0, rect.center().y), ic::MICROPHONE_SLASH, 15.0, c::DANGER, false);
            right -= 20.0;
        }
        let fg = if m.speaking { c::TEXT } else { theme::mix(c::TEXT_MUTED, c::TEXT_SECONDARY, t + 0.4) };
        w::paint_text_clipped(
            ui,
            rect.left_center() + Vec2::new(66.0, 0.0),
            &m.name,
            f(ty::SMALL_MEDIUM),
            fg,
            right - rect.left() - 72.0,
        );
        let mut tip = String::new();
        if let Some(id) = m.id {
            let st = self.node.voice().and_then(|v| v.stats().into_iter().find(|(p, _)| *p == id));
            if let Some((_, st)) = st {
                tip = format!(
                    "Bufor: {} ms · odebrane: {}\nOdtworzone z FEC: {} · zamaskowane: {} · spóźnione: {}",
                    st.depth_ms, st.received, st.fec, st.concealed, st.late
                );
            }
            if let Some(path) = self.node.path_kind(&id) {
                let path = if path == "relay" { "przez serwer pośredniczący" } else { "bezpośrednie (P2P)" };
                tip = if tip.is_empty() { format!("Połączenie: {path}") } else { format!("{tip}\nPołączenie: {path}") };
            }
        }
        if clicked_live {
            tip = if tip.is_empty() { "Obejrzyj stream".into() } else { format!("Obejrzyj stream\n\n{tip}") };
        }
        let resp = if tip.is_empty() { resp } else { resp.on_hover_text(tip) };
        (resp.clicked() && clicked_live).then(|| m.id).flatten()
    }

    /// Room member with connection status. Returns a peer to watch.
    fn member_row(&self, ui: &mut egui::Ui, m: &Member) -> Option<EndpointId> {
        let (resp, rect, t) = w::row(ui, 42.0, false);
        let path = m.id.and_then(|id| self.node.path_kind(&id));
        let (dot, tip) = match (m.id, path) {
            (None, _) => (c::SUCCESS, "To Ty".to_string()),
            (_, Some("P2P")) => (c::SUCCESS, "Połączenie bezpośrednie (P2P)".to_string()),
            (_, Some("relay")) => (c::WARNING, "Połączenie przez relay (pośrednie)".to_string()),
            _ => (c::TEXT_MUTED, "Nawiązywanie połączenia…".to_string()),
        };
        let surface = theme::mix(c::BG_SIDEBAR, c::BG_RAISED, t);
        Avatar { name: &m.name, color: m.color, image: m.avatar.as_ref(), radius: 15.0, speaking: 0.0, status: Some((dot, surface)) }
            .paint(ui, rect.left_center() + Vec2::new(24.0, 0.0));
        let mut right = rect.right() - 8.0;
        if m.live {
            let b = w::live_badge(ui, Pos2::new(right, rect.center().y), Align2::RIGHT_CENTER, true);
            right = b.left() - 6.0;
        }
        let x = rect.left() + 48.0;
        let name = if m.id.is_none() { format!("{} (Ty)", m.name) } else { m.name.clone() };
        let fg = theme::mix(c::TEXT_SECONDARY, c::TEXT, t);
        w::paint_text_clipped(ui, Pos2::new(x, rect.center().y - 8.0), &name, f(ty::LABEL), fg, right - x);
        let sub = if m.live {
            "Udostępnia ekran"
        } else if m.voice {
            "Na kanale głosowym"
        } else if path == Some("relay") {
            "Połączenie przez relay"
        } else {
            "W pokoju"
        };
        w::paint_text_clipped(ui, Pos2::new(x, rect.center().y + 9.0), sub, f(ty::CAPTION), c::TEXT_MUTED, right - x);
        let resp = resp.on_hover_text(if m.live && m.id.is_some() { format!("{tip}. Kliknij, aby oglądać.") } else { tip });
        (resp.clicked() && m.live).then_some(m.id).flatten()
    }

    fn bottom_card(&mut self, ui: &mut egui::Ui) {
        egui::Frame::new()
            .fill(c::BG_BASE)
            .corner_radius(r::CARD)
            .inner_margin(egui::Margin::same(sp::S as i8))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = Vec2::new(sp::XS, sp::S);
                if !matches!(
                    *self.update.lock().unwrap(),
                    crate::update::State::Idle | crate::update::State::Checking | crate::update::State::UpToDate
                ) {
                    self.update_banner(ui);
                }
                if self.my_voice || self.voice_starting || self.my_stream.is_some() || self.stream_starting {
                    self.voice_panel(ui);
                    w::separator(ui, c::BORDER);
                }
                self.user_row(ui);
            });
    }

    fn voice_panel(&mut self, ui: &mut egui::Ui) {
        let width = ui.available_width();
        if self.my_voice || self.voice_starting {
            ui.horizontal(|ui| {
                ui.set_height(36.0);
                let (col, title) = if self.voice_starting {
                    (c::WARNING, "Łączenie z głosem…")
                } else {
                    (c::SUCCESS, "Połączono z głosem")
                };
                let (ir, _) = ui.allocate_exact_size(Vec2::new(26.0, 36.0), Sense::hover());
                w::paint_icon(ui, ir.center(), ic::CELL_SIGNAL_FULL, 20.0, col, false);
                let (tr, _) = ui.allocate_exact_size(Vec2::new(width - 26.0 - 40.0, 36.0), Sense::hover());
                w::paint_text(ui, tr.left_center() - Vec2::new(0.0, 8.0), Align2::LEFT_CENTER, title, theme::font(13.0, W::SemiBold), col);
                w::paint_text_clipped(
                    ui,
                    tr.left_center() + Vec2::new(0.0, 9.0),
                    &format!("Głosowy / {}", self.room),
                    f(ty::CAPTION),
                    c::TEXT_MUTED,
                    tr.width(),
                );
                if IconButton::new(ic::PHONE_X, "Rozłącz").show(ui).clicked() {
                    self.node.leave_voice();
                }
            });
        }
        if let Some(info) = &self.my_stream {
            let viewers = self.node.viewer_count();
            ui.horizontal(|ui| {
                ui.add_space(2.0);
                w::live_badge_inline(ui);
                ui.add_space(4.0);
                let n = match viewers {
                    0 => "brak widzów".to_string(),
                    1 => "1 widz".to_string(),
                    n => format!("{n} widzów"),
                };
                ui.label(w::text(format!("{} fps · {n}", info.fps), ty::CAPTION, c::TEXT_MUTED));
            });
        }
        // Camera and screen side by side, as on Discord.
        let ffmpeg = crate::ffmpeg::path().is_some();
        let half = (width - ui.spacing().item_spacing.x) / 2.0;
        let (mut camera, mut screen) = (false, false);
        ui.horizontal(|ui| {
            let (label, kind, tip) = match (self.camera_on, self.camera_starting) {
                (true, _) => ("Kamera", Kind::Primary, "Wyłącz kamerę"),
                (false, true) => ("Kamera…", Kind::Secondary, "Uruchamianie kamery"),
                _ => ("Kamera", Kind::Secondary, "Włącz kamerę"),
            };
            let icon = if self.camera_on { ic::VIDEO_CAMERA } else { ic::VIDEO_CAMERA_SLASH };
            let enabled = ffmpeg && self.my_voice && !self.camera_starting;
            camera = Button::new(label, kind)
                .icon(icon)
                .width(half)
                .height(34.0)
                .enabled(enabled)
                .show(ui)
                .on_hover_text(if self.my_voice { tip } else { "Dołącz do głosu, aby włączyć kamerę" })
                .clicked();
            let (label, kind, tip) = match (self.my_stream.is_some(), self.stream_starting) {
                (true, _) => ("Zakończ", Kind::Danger, "Zakończ stream"),
                (false, true) => ("Ekran…", Kind::Secondary, "Uruchamianie streamu"),
                _ => ("Ekran", Kind::Secondary, "Udostępnij ekran"),
            };
            screen = Button::new(label, kind)
                .icon(ic::MONITOR_ARROW_UP)
                .width(half)
                .height(34.0)
                .enabled(ffmpeg && !self.stream_starting)
                .show(ui)
                .on_hover_text(tip)
                .clicked();
        });
        if camera {
            self.toggle_camera();
        }
        if screen && self.my_stream.is_some() {
            self.node.stop_stream();
        } else if screen {
            self.open_go_live();
        }
        if !self.status.is_empty() {
            ui.label(w::text(&self.status, ty::CAPTION, c::TEXT_MUTED));
        }
    }

    fn user_row(&mut self, ui: &mut egui::Ui) {
        let width = ui.available_width();
        ui.horizontal(|ui| {
            ui.set_height(40.0);
            let (ar, _) = ui.allocate_exact_size(Vec2::new(36.0, 40.0), Sense::hover());
            let buttons_w = 3.0 * 32.0;
            let (tr, _) = ui.allocate_exact_size(Vec2::new(width - 40.0 - buttons_w - 4.0, 40.0), Sense::hover());
            // Avatar + name open the profile page.
            let me_rect = ar.union(tr).expand2(Vec2::new(2.0, 0.0));
            let me = ui
                .interact(me_rect, ui.make_persistent_id("me_profile"), Sense::click())
                .on_hover_text("Profil: nazwa i zdjęcie")
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            let t = w::hover_t(ui, &me);
            let surface = theme::mix(c::BG_BASE, c::BG_RAISED, t);
            ui.painter().rect_filled(me_rect, r::ROW, surface);
            if me.clicked() {
                self.open_profile();
            }
            let me_speaking = self.node.voice().is_some_and(|v| v.me_speaking()) && !self.mute.muted;
            let speak = ui.ctx().animate_bool_with_time(ui.scope_id().with("me_speak"), me_speaking, 0.2);
            Avatar {
                name: &self.name,
                color: self.my_color(),
                image: self.my_avatar_tex.as_ref(),
                radius: 16.0,
                speaking: speak,
                status: Some((c::SUCCESS, surface)),
            }
            .paint(ui, ar.center());
            w::paint_text_clipped(ui, tr.left_center() - Vec2::new(0.0, 8.0), &self.name, theme::font(14.0, W::SemiBold), c::TEXT, tr.width());
            let sub = if self.mute.deafened {
                "Dźwięk wyłączony"
            } else if self.mute.muted {
                "Wyciszony"
            } else if self.my_stream.is_some() {
                "Udostępnia ekran"
            } else if self.camera_on {
                "Kamera włączona"
            } else if self.my_voice {
                "Na kanale głosowym"
            } else {
                "W pokoju"
            };
            w::paint_text_clipped(ui, tr.left_center() + Vec2::new(0.0, 9.0), sub, f(ty::CAPTION), c::TEXT_MUTED, tr.width());

            let muted = self.mute.muted || self.mute.deafened;
            let (mi, mc, mt) = if muted {
                (ic::MICROPHONE_SLASH, c::DANGER, "Włącz mikrofon")
            } else {
                (ic::MICROPHONE, c::TEXT_SECONDARY, "Wycisz mikrofon")
            };
            if IconButton::new(mi, mt).color(mc).show(ui).clicked() {
                self.toggle_mic();
            }
            let (di, dc, dt) = if self.mute.deafened {
                (ic::SPEAKER_SIMPLE_SLASH, c::DANGER, "Włącz dźwięk")
            } else {
                (ic::HEADPHONES, c::TEXT_SECONDARY, "Wyłącz dźwięk")
            };
            if IconButton::new(di, dt).color(dc).show(ui).clicked() {
                self.toggle_deafen();
            }
            if IconButton::new(ic::GEAR_SIX, "Ustawienia").show(ui).clicked() {
                self.open_settings();
            }
        });
        let _ = Duration::ZERO;
    }
}
