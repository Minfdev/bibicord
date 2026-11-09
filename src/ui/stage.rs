//! Voice stage: participant tiles, focused stream, call controls.

use super::sidebar::Member;
use super::theme::{self, W, c, f, mix, r, sp, ty};
use super::widgets::{self as w, Avatar, Button, IconButton, Kind, RoundKind, ic};
use super::App;
use eframe::egui::{
    self, Align2, Color32, CursorIcon, Pos2, Rect, Sense, Stroke, StrokeKind, TextureOptions,
    UiBuilder, Vec2,
};
use iroh::EndpointId;
use std::time::{Duration, Instant};

const CONTROLS_H: f32 = 84.0;
const STRIP_H: f32 = 112.0;
const GAP: f32 = 8.0;

impl App {
    pub(crate) fn stage(&mut self, ui: &mut egui::Ui) {
        self.pull_frame(ui.ctx());
        self.pull_cameras(ui.ctx());
        let full = ui.available_rect_before_wrap();
        ui.allocate_rect(full, Sense::hover());

        if self.fullscreen {
            self.video(ui, full, None);
            return;
        }

        let header = Rect::from_min_size(full.min, Vec2::new(full.width(), theme::HEADER_H));
        self.stage_header(ui, header);
        let body = Rect::from_min_max(
            Pos2::new(full.left() + sp::L, header.bottom() + sp::L),
            Pos2::new(full.right() - sp::L, full.bottom() - CONTROLS_H),
        );
        let controls = Rect::from_min_max(Pos2::new(full.left(), full.bottom() - CONTROLS_H), full.max);

        let members: Vec<Member> = self.members().into_iter().filter(|m| m.voice || m.live).collect();
        let mut watch = None;
        if let Some(peer) = self.watching {
            let strip_members: Vec<&Member> = members.iter().filter(|m| m.id != Some(peer)).collect();
            let (video_rect, strip) = if strip_members.is_empty() {
                (body, None)
            } else {
                let s = Rect::from_min_max(Pos2::new(body.left(), body.bottom() - STRIP_H), body.max);
                (Rect::from_min_max(body.min, Pos2::new(body.right(), s.top() - GAP * 1.5)), Some(s))
            };
            self.video(ui, video_rect, Some(peer));
            if let Some(strip) = strip {
                let tile_w = (STRIP_H * 16.0 / 9.0).min(strip.width() / strip_members.len() as f32 - GAP);
                let total = strip_members.len() as f32 * (tile_w + GAP) - GAP;
                let mut x = strip.center().x - total / 2.0;
                for m in strip_members {
                    let rect = Rect::from_min_size(Pos2::new(x, strip.top()), Vec2::new(tile_w, STRIP_H));
                    if let Some(id) = self.tile(ui, rect, m) {
                        watch = Some(id);
                    }
                    x += tile_w + GAP;
                }
            }
        } else if members.is_empty() {
            self.stage_empty(ui, body);
        } else {
            for (rect, m) in grid(body, members.len()).into_iter().zip(&members) {
                if let Some(id) = self.tile(ui, rect, m) {
                    watch = Some(id);
                }
            }
        }
        if let Some(id) = watch {
            self.watch(id);
        }
        self.controls(ui, controls);
    }

    /// Moves the newest decoded frame into the texture.
    fn pull_frame(&mut self, ctx: &egui::Context) {
        let slot = self.node.frames();
        if let Some(fr) = slot.take() {
            let img = egui::ColorImage::from_rgba_unmultiplied([fr.width, fr.height], &fr.rgba);
            self.video_size = [fr.width, fr.height];
            match &mut self.texture {
                Some(t) => t.set(img, TextureOptions::LINEAR),
                None => self.texture = Some(ctx.load_texture("video", img, TextureOptions::LINEAR)),
            }
        }
        let n = slot.count();
        if self.fps.0.elapsed() >= Duration::from_secs(1) {
            let dt = self.fps.0.elapsed().as_secs_f32();
            self.fps = (Instant::now(), n, n.saturating_sub(self.fps.1) as f32 / dt);
        }
    }

    fn stage_header(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let y = rect.center().y;
        let x = rect.left() + sp::L;
        w::paint_icon(ui, Pos2::new(x + 10.0, y), ic::SPEAKER_HIGH, 21.0, c::TEXT_MUTED, false);
        w::paint_text(ui, Pos2::new(x + 28.0, y), Align2::LEFT_CENTER, "Głosowy", f(ty::HEADING), c::TEXT);
        if !self.stage_chat {
            let btn = Rect::from_center_size(rect.right_center() - Vec2::new(28.0, 0.0), Vec2::splat(32.0));
            let mut open = false;
            ui.scope_builder(UiBuilder::new().max_rect(btn), |ui| {
                open = IconButton::new(ic::CHAT_CIRCLE, "Pokaż czat").show(ui).clicked();
            });
            if self.unread > 0 {
                ui.painter().circle_filled(btn.right_top() + Vec2::new(-6.0, 6.0), 4.5, c::DANGER);
            }
            if open {
                self.stage_chat = true;
            }
        }
    }

    fn stage_empty(&mut self, ui: &mut egui::Ui, body: Rect) {
        let center = body.center() - Vec2::new(0.0, 24.0);
        ui.painter().circle_filled(center - Vec2::new(0.0, 56.0), 36.0, c::BG_SELECTED);
        w::paint_icon(ui, center - Vec2::new(0.0, 56.0), ic::SPEAKER_HIGH, 34.0, c::TEXT_SECONDARY, false);
        w::paint_text(ui, center + Vec2::new(0.0, 6.0), Align2::CENTER_CENTER, "Na głosowym jest cicho", f(ty::TITLE), c::TEXT);
        w::paint_text(
            ui,
            center + Vec2::new(0.0, 32.0),
            Align2::CENTER_CENTER,
            "Dołącz, żeby porozmawiać albo udostępnić ekran.",
            f(ty::SMALL),
            c::TEXT_MUTED,
        );
    }

    /// One participant tile. Returns a peer to watch when clicked.
    fn tile(&self, ui: &mut egui::Ui, rect: Rect, m: &Member) -> Option<EndpointId> {
        let id = ui.make_persistent_id(("tile", m.id.map(|i| *i.as_bytes())));
        let resp = ui.interact(rect, id, Sense::click());
        let hover = w::hover_t(ui, &resp);
        let p = ui.painter();
        let bg = mix(m.color, c::BG_STAGE, 0.62);
        p.rect_filled(rect, r::CARD, mix(bg, m.color, hover * 0.08));
        let avatar_r = (rect.height() * 0.2).clamp(16.0, 48.0);
        let speak = ui.ctx().animate_bool_with_time(id.with("speak"), m.speaking, if m.speaking { 0.08 } else { 0.3 });
        if let Some((tex, size)) = &m.video {
            // Fill the tile (crop the overflow); our own preview is mirrored.
            let (sx, sy) = (rect.width() / size.x, rect.height() / size.y);
            let (cw, ch) = if sx > sy { (1.0, sy / sx) } else { (sx / sy, 1.0) };
            let (x0, x1) = ((1.0 - cw) / 2.0, (1.0 + cw) / 2.0);
            let (x0, x1) = if m.id.is_none() { (x1, x0) } else { (x0, x1) };
            let uv = Rect::from_min_max(Pos2::new(x0, (1.0 - ch) / 2.0), Pos2::new(x1, (1.0 + ch) / 2.0));
            egui::Image::new((tex.id(), rect.size())).uv(uv).corner_radius(r::CARD).paint_at(ui, rect);
        } else {
            Avatar { name: &m.name, color: m.color, image: m.avatar.as_ref(), radius: avatar_r, speaking: speak, status: None }
                .paint(ui, rect.center() - Vec2::new(0.0, if rect.height() > 140.0 { 8.0 } else { 6.0 }));
        }
        if speak > 0.0 {
            ui.painter().rect_stroke(
                rect,
                r::CARD,
                Stroke::new(2.0, theme::alpha(c::SPEAKING, speak)),
                StrokeKind::Inside,
            );
        }

        // Name pill
        let small = rect.height() < 140.0;
        let font = if small { f(ty::CAPTION) } else { f(ty::SMALL_MEDIUM) };
        let name = if m.id.is_none() { format!("{} (Ty)", m.name) } else { m.name.clone() };
        let galley = ui.painter().layout_no_wrap(name, font, c::TEXT);
        let mute_icons = (m.mute.muted || m.mute.deafened) as usize + m.mute.deafened as usize;
        let icons_w = mute_icons as f32 * 18.0;
        let pill = Rect::from_min_size(
            rect.left_bottom() + Vec2::new(8.0, -8.0 - 24.0),
            Vec2::new((galley.size().x + icons_w + 16.0).min(rect.width() - 16.0), 24.0),
        );
        ui.painter().rect_filled(pill, r::ROW, Color32::from_black_alpha(150));
        let mut x = pill.left() + 8.0;
        if m.mute.muted || m.mute.deafened {
            w::paint_icon(ui, Pos2::new(x + 7.0, pill.center().y), ic::MICROPHONE_SLASH, 14.0, c::DANGER, false);
            x += 18.0;
        }
        if m.mute.deafened {
            w::paint_icon(ui, Pos2::new(x + 7.0, pill.center().y), ic::SPEAKER_SIMPLE_SLASH, 14.0, c::DANGER, false);
            x += 18.0;
        }
        ui.painter()
            .with_clip_rect(pill.shrink(2.0))
            .galley(Pos2::new(x, pill.center().y - galley.size().y / 2.0), galley, c::TEXT);

        let watchable = m.live && m.id.is_some();
        if m.live {
            w::live_badge(ui, rect.right_top() + Vec2::new(-8.0, 8.0), Align2::RIGHT_TOP, small);
        }
        if watchable && hover > 0.0 {
            ui.painter().rect_filled(rect, r::CARD, Color32::from_black_alpha((hover * 120.0) as u8));
            let label = "Obejrzyj stream";
            let g = ui.painter().layout_no_wrap(label.into(), f(ty::LABEL), c::TEXT);
            let b = Rect::from_center_size(rect.center(), g.size() + Vec2::new(28.0, 16.0));
            ui.painter().rect_filled(b, r::CONTROL, theme::alpha(c::BG_SELECTED, hover));
            ui.painter().galley_with_override_text_color(b.center() - g.size() / 2.0, g, theme::alpha(c::TEXT, hover));
        }
        let resp = if watchable { resp.on_hover_cursor(CursorIcon::PointingHand) } else { resp };
        (watchable && resp.clicked()).then_some(m.id).flatten()
    }

    fn video(&mut self, ui: &mut egui::Ui, rect: Rect, peer: Option<EndpointId>) {
        let peer = peer.or(self.watching);
        let radius = if self.fullscreen { 0 } else { r::CARD };
        let resp = ui.interact(rect, ui.make_persistent_id("video"), Sense::click());
        ui.painter().rect_filled(rect, radius, Color32::BLACK);
        if let Some(tex) = &self.texture {
            let [vw, vh] = self.video_size;
            let scale = (rect.width() / vw as f32).min(rect.height() / vh as f32);
            let r = Rect::from_center_size(rect.center(), Vec2::new(vw as f32, vh as f32) * scale);
            egui::Image::new((tex.id(), r.size())).corner_radius(radius).paint_at(ui, r);
        } else {
            ui.put(
                Rect::from_center_size(rect.center() - Vec2::new(0.0, 14.0), Vec2::splat(28.0)),
                egui::Spinner::new().size(28.0).color(c::TEXT_MUTED),
            );
            w::paint_text(
                ui,
                rect.center() + Vec2::new(0.0, 22.0),
                Align2::CENTER_CENTER,
                "Łączenie ze streamem…",
                f(ty::SMALL),
                c::TEXT_MUTED,
            );
        }

        // Overlay: who / what, and window controls.
        let show = !self.fullscreen || resp.hovered() || resp.contains_pointer();
        if show {
            let p = peer.and_then(|id| self.peer(&id));
            let name = p.map(|p| p.name.clone()).unwrap_or_default();
            let path = peer.and_then(|id| self.node.path_kind(&id)).unwrap_or("…");
            let mut meta = String::new();
            if self.video_size[0] > 0 {
                meta = format!("{}×{} · {:.0} fps · {}", self.video_size[0], self.video_size[1], self.fps.2, path);
            }
            let tl = rect.left_top() + Vec2::new(12.0, 12.0);
            let ng = ui.painter().layout_no_wrap(name, theme::font(14.0, W::SemiBold), c::TEXT);
            let mg = ui.painter().layout_no_wrap(meta, f(ty::CAPTION), c::TEXT_SECONDARY);
            let badge_w = 62.0;
            let pill = Rect::from_min_size(
                tl,
                Vec2::new(10.0 + badge_w + 8.0 + ng.size().x + 10.0 + mg.size().x + 12.0, 30.0),
            );
            ui.painter().rect_filled(pill, r::CONTROL, Color32::from_black_alpha(170));
            let b = w::live_badge(ui, pill.left_center() + Vec2::new(10.0, 0.0), Align2::LEFT_CENTER, true);
            let nx = b.right() + 8.0;
            ui.painter().galley(Pos2::new(nx, pill.center().y - ng.size().y / 2.0), ng.clone(), c::TEXT);
            ui.painter().galley(
                Pos2::new(nx + ng.size().x + 10.0, pill.center().y - mg.size().y / 2.0),
                mg,
                c::TEXT_SECONDARY,
            );

            let mut x = rect.right() - 12.0 - 36.0;
            let mut overlay_btn = |ui: &mut egui::Ui, icon_code: &str, tip: &str| -> bool {
                let r = Rect::from_min_size(Pos2::new(x, rect.top() + 12.0), Vec2::splat(36.0));
                x -= 44.0;
                let resp = ui.interact(r, ui.make_persistent_id(("vbtn", icon_code)), Sense::click());
                let t = w::hover_t(ui, &resp);
                ui.painter().rect_filled(r, r::CONTROL, Color32::from_black_alpha((170.0 + 60.0 * t) as u8));
                w::paint_icon(ui, r.center(), icon_code, 19.0, c::TEXT, false);
                resp.on_hover_text(tip).on_hover_cursor(CursorIcon::PointingHand).clicked()
            };
            if overlay_btn(ui, ic::X, "Przestań oglądać") {
                self.node.unwatch();
            }
            let (fi, ft) = if self.fullscreen {
                (ic::CORNERS_IN, "Zamknij pełny ekran (Esc)")
            } else {
                (ic::CORNERS_OUT, "Pełny ekran")
            };
            if overlay_btn(ui, fi, ft) {
                self.toggle_fullscreen(&ui.ctx().clone());
            }
        }
        if resp.double_clicked() {
            self.toggle_fullscreen(&ui.ctx().clone());
        }
    }

    fn controls(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let center = rect.center();
        if !self.my_voice {
            let w = 220.0;
            let r = Rect::from_center_size(center, Vec2::new(w, 44.0));
            let label = if self.voice_starting { "Łączenie…" } else { "Dołącz do głosu" };
            let mut join = false;
            ui.scope_builder(UiBuilder::new().max_rect(r), |ui| {
                join = Button::new(label, Kind::Success)
                    .icon(ic::PHONE)
                    .width(w)
                    .height(44.0)
                    .enabled(!self.voice_starting)
                    .show(ui)
                    .clicked();
            });
            if join {
                self.join_voice();
            }
            return;
        }
        let n = 5.0;
        let total = n * 48.0 + (n - 1.0) * 12.0;
        let r = Rect::from_center_size(center, Vec2::new(total, 48.0));
        let mut act = 0;
        ui.scope_builder(
            UiBuilder::new().max_rect(r).layout(egui::Layout::left_to_right(egui::Align::Center)),
            |ui| {
                ui.spacing_mut().item_spacing = Vec2::new(12.0, 0.0);
                let muted = self.mute.muted || self.mute.deafened;
                let (i, k, t) = if muted {
                    (ic::MICROPHONE_SLASH, RoundKind::Off, "Włącz mikrofon")
                } else {
                    (ic::MICROPHONE, RoundKind::Normal, "Wycisz mikrofon")
                };
                if w::round_button(ui, i, t, k).clicked() {
                    act = 1;
                }
                let (i, k, t) = if self.mute.deafened {
                    (ic::SPEAKER_SIMPLE_SLASH, RoundKind::Off, "Włącz dźwięk")
                } else {
                    (ic::HEADPHONES, RoundKind::Normal, "Wyłącz dźwięk")
                };
                if w::round_button(ui, i, t, k).clicked() {
                    act = 2;
                }
                let (i, k, t) = if self.camera_on {
                    (ic::VIDEO_CAMERA, RoundKind::On, "Wyłącz kamerę")
                } else {
                    (ic::VIDEO_CAMERA_SLASH, RoundKind::Normal, "Włącz kamerę")
                };
                if w::round_button(ui, i, t, k).clicked() {
                    act = 5;
                }
                let (k, t) = if self.my_stream.is_some() {
                    (RoundKind::On, "Zakończ stream")
                } else {
                    (RoundKind::Normal, "Udostępnij ekran")
                };
                if w::round_button(ui, ic::MONITOR_ARROW_UP, t, k).clicked() {
                    act = 3;
                }
                if w::round_button(ui, ic::PHONE_X, "Rozłącz", RoundKind::Hangup).clicked() {
                    act = 4;
                }
            },
        );
        match act {
            1 => self.toggle_mic(),
            2 => self.toggle_deafen(),
            3 if self.my_stream.is_some() => self.node.stop_stream(),
            3 if crate::ffmpeg::path().is_some() && !self.stream_starting => self.open_go_live(),
            4 => self.node.leave_voice(),
            5 if crate::ffmpeg::path().is_some() => self.toggle_camera(),
            _ => {}
        }
    }
}

/// Lays out `n` 16:9 tiles in `area`, as large as possible, centred.
fn grid(area: Rect, n: usize) -> Vec<Rect> {
    let mut best = (0.0f32, 1usize);
    for cols in 1..=n {
        let rows = n.div_ceil(cols);
        let w = ((area.width() - GAP * (cols - 1) as f32) / cols as f32)
            .min((area.height() - GAP * (rows - 1) as f32) / rows as f32 * 16.0 / 9.0)
            .min(720.0);
        if w > best.0 {
            best = (w, cols);
        }
    }
    let (w, cols) = best;
    let h = w * 9.0 / 16.0;
    let rows = n.div_ceil(cols);
    let total_h = rows as f32 * h + (rows - 1) as f32 * GAP;
    let mut out = Vec::with_capacity(n);
    for row in 0..rows {
        let in_row = (n - row * cols).min(cols);
        let row_w = in_row as f32 * w + (in_row - 1) as f32 * GAP;
        let x0 = area.center().x - row_w / 2.0;
        let y = area.center().y - total_h / 2.0 + row as f32 * (h + GAP);
        for col in 0..in_row {
            out.push(Rect::from_min_size(Pos2::new(x0 + col as f32 * (w + GAP), y), Vec2::new(w, h)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_fits_and_keeps_aspect() {
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 600.0));
        for n in 1..=8 {
            let g = grid(area, n);
            assert_eq!(g.len(), n);
            for r in &g {
                assert!(area.expand(0.5).contains_rect(*r), "{n}: {r:?}");
                assert!((r.width() / r.height() - 16.0 / 9.0).abs() < 0.01);
            }
        }
    }
}
