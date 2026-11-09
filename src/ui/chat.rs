//! #czat: header, grouped messages, composer.

use super::theme::{self, W, c, f, r, sp, ty};
use super::widgets::{self as w, Avatar, Button, IconButton, Kind, ic};
use super::{App, ChatItem, ChatKind};
use crate::proto::MAX_FILE_BYTES;
use eframe::egui::{self, Align2, Key, Pos2, Rect, Sense, Stroke, UiBuilder, Vec2};
use std::path::Path;
use std::time::Instant;

const TOO_BIG: &str = "Plik jest za duży (maks. 1 MB)";

const GUTTER: f32 = 72.0;

impl App {
    /// `side`: compact panel next to the voice stage.
    pub(crate) fn chat_view(&mut self, ui: &mut egui::Ui, side: bool) {
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        self.chat_header(ui, side);
        egui::Panel::bottom(if side { "composer_side" } else { "composer" })
            .resizable(false)
            .frame(egui::Frame::new().fill(c::BG_CONTENT).inner_margin(egui::Margin {
                left: sp::L as i8,
                right: sp::L as i8,
                top: 0,
                bottom: sp::XL as i8,
            }))
            .show(ui, |ui| self.composer(ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(c::BG_CONTENT))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing = Vec2::ZERO;
                        self.messages(ui, side);
                    });
            });
    }

    fn chat_header(&mut self, ui: &mut egui::Ui, side: bool) {
        let full = ui.available_rect_before_wrap();
        let rect = Rect::from_min_size(full.min, Vec2::new(full.width(), theme::HEADER_H));
        ui.allocate_rect(rect, Sense::hover());
        ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, c::BG_BASE));
        let y = rect.center().y;
        let mut x = rect.left() + sp::L;
        w::paint_icon(ui, Pos2::new(x + 10.0, y), ic::HASH, 22.0, c::TEXT_MUTED, false);
        x += 28.0;
        let t = w::paint_text(ui, Pos2::new(x, y), Align2::LEFT_CENTER, "czat", f(ty::HEADING), c::TEXT);
        x = t.right() + sp::M;
        if side {
            let btn = Rect::from_center_size(rect.right_center() - Vec2::new(28.0, 0.0), Vec2::splat(32.0));
            let mut close = false;
            ui.scope_builder(UiBuilder::new().max_rect(btn), |ui| {
                close = IconButton::new(ic::X, "Zamknij czat").show(ui).clicked();
            });
            if close {
                self.stage_chat = false;
            }
        } else {
            ui.painter().vline(x, (y - 10.0)..=(y + 10.0), Stroke::new(1.0, c::BORDER_STRONG));
            x += sp::M;
            w::paint_text_clipped(
                ui,
                Pos2::new(x, y),
                "Wiadomości trafiają prosto do uczestników i nie są nigdzie zapisywane",
                f(ty::SMALL),
                c::TEXT_MUTED,
                rect.right() - x - sp::L,
            );
        }
    }

    fn messages(&mut self, ui: &mut egui::Ui, side: bool) {
        let width = ui.available_width();
        // Start of channel
        ui.add_space(if side { sp::L } else { sp::XXL });
        ui.horizontal(|ui| {
            ui.add_space(sp::L);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(0.0, sp::S);
                if !side {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(68.0), Sense::hover());
                    ui.painter().circle_filled(rect.center(), 34.0, c::BG_SELECTED);
                    w::paint_icon(ui, rect.center(), ic::HASH, 38.0, c::TEXT, false);
                    ui.add_space(sp::XS);
                }
                let title = if side { (18.0, W::Bold) } else { (28.0, W::Bold) };
                ui.label(w::text("Witaj na #czat!", title, c::TEXT));
                ui.set_max_width(width - 2.0 * sp::L);
                let room = if self.room.is_empty() { "tym pokoju".into() } else { format!("pokoju {}", self.room) };
                ui.label(w::text(
                    format!("To początek kanału w {room}. Historia znika, gdy wszyscy wyjdą."),
                    ty::SMALL,
                    c::TEXT_MUTED,
                ));
            });
        });
        ui.add_space(sp::L);
        w::separator(ui, c::BORDER);
        ui.add_space(sp::S);

        let my_color = self.my_color();
        let mut prev: Option<(Option<iroh::EndpointId>, chrono::DateTime<chrono::Local>)> = None;
        let mut save = None;
        for (i, item) in self.chat.iter().enumerate() {
            let ChatItem { kind, time } = item;
            match kind {
                ChatKind::Message { from, name, .. } | ChatKind::File { from, name, .. } => {
                    let grouped = prev.is_some_and(|(pf, pt)| pf == *from && (*time - pt).num_minutes() < 5);
                    let (color, avatar) = match from {
                        Some(id) => match self.peer(id) {
                            Some(p) => (p.color, p.avatar.as_ref()),
                            None => (theme::avatar_color(id.as_bytes()), self.gone_avatars.get(id)),
                        },
                        None => (my_color, self.my_avatar_tex.as_ref()),
                    };
                    message(ui, name, color, avatar, *time, grouped, width, |ui| match kind {
                        ChatKind::Message { text, .. } => {
                            ui.add(egui::Label::new(w::text(text, ty::BODY, c::TEXT_BODY)).wrap().selectable(true));
                        }
                        ChatKind::File { file_name, data, saved, .. } => {
                            if file_card(ui, file_name, data.len(), *saved) {
                                save = Some(i);
                            }
                        }
                        ChatKind::System { .. } => {}
                    });
                    prev = Some((*from, *time));
                }
                ChatKind::System { icon, who, text, good } => {
                    system_line(ui, icon, who, text, *good, *time, width);
                    prev = None;
                }
            }
        }
        ui.add_space(sp::L);
        if let Some(i) = save {
            self.save_file(i);
        }
    }

    /// Asks where to save chat file `i`; nothing is written without it.
    fn save_file(&mut self, i: usize) {
        let Some(ChatItem { kind: ChatKind::File { file_name, data, saved, .. }, .. }) = self.chat.get_mut(i) else {
            return;
        };
        // `file_name` is cleaned (no path), so it only pre-fills the dialog.
        let Some(path) = rfd::FileDialog::new().set_title("Zapisz plik").set_file_name(file_name.as_str()).save_file()
        else {
            return;
        };
        match std::fs::write(&path, data.as_slice()) {
            Ok(()) => *saved = true,
            Err(e) => self.error = Some((format!("Nie udało się zapisać pliku: {e}"), Instant::now())),
        }
    }

    /// Reads a file from disk and posts it in chat (size checked first).
    pub(crate) fn send_file_path(&mut self, path: &Path) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let res = match std::fs::metadata(path) {
            Ok(m) if m.is_dir() => Err("Można wysyłać tylko pliki, nie foldery.".to_string()),
            Ok(m) if m.len() > MAX_FILE_BYTES as u64 => Err(TOO_BIG.to_string()),
            Ok(_) => match std::fs::read(path) {
                Ok(data) => self.node.send_file(&name, data).map_err(|e| e.to_string()),
                Err(e) => Err(format!("Nie udało się odczytać pliku „{name}”: {e}")),
            },
            Err(e) => Err(format!("Nie udało się odczytać pliku „{name}”: {e}")),
        };
        if let Err(e) = res {
            self.error = Some((e, Instant::now()));
        }
    }

    fn pick_files(&mut self) {
        let files = rfd::FileDialog::new().set_title("Wyślij plik na #czat (maks. 1 MB)").pick_files();
        for path in files.unwrap_or_default() {
            self.send_file_path(&path);
        }
    }

    /// Files dragged onto the window: highlight while hovering, send on drop.
    pub(crate) fn file_drop(&mut self, ctx: &egui::Context) {
        let dropped: Vec<_> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        for path in dropped {
            self.send_file_path(&path);
        }
        if ctx.input(|i| i.raw.hovered_files.is_empty()) {
            return;
        }
        let screen = ctx.content_rect();
        egui::Area::new(egui::Id::unique("file_drop"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.min)
            .interactable(false)
            .show(ctx, |ui| {
                let p = ui.painter();
                p.rect_filled(screen, 0.0, theme::alpha(c::BG_STAGE, 0.75));
                let card = Rect::from_center_size(screen.center(), Vec2::new(360.0, 168.0));
                p.rect_filled(card, r::CARD, c::BG_CONTENT);
                p.rect_stroke(card, r::CARD, Stroke::new(2.0, c::BRAND), egui::StrokeKind::Inside);
                let x = card.center().x;
                w::paint_icon(ui, Pos2::new(x, card.top() + 48.0), ic::FILE_ARROW_UP, 40.0, c::BRAND, false);
                w::paint_text(ui, Pos2::new(x, card.top() + 98.0), Align2::CENTER_CENTER, "Upuść, aby wysłać na #czat", f(ty::TITLE), c::TEXT);
                w::paint_text(ui, Pos2::new(x, card.top() + 128.0), Align2::CENTER_CENTER, "Maksymalnie 1 MB na plik", f(ty::SMALL), c::TEXT_MUTED);
            });
    }

    fn composer(&mut self, ui: &mut egui::Ui) {
        let width = ui.available_width();
        let id = ui.make_persistent_id("chat_input");
        let focused = ui.memory(|m| m.has_focus(id));
        let has_text = !self.chat_input.trim().is_empty();
        let mut send = false;
        let mut attach = false;
        egui::Frame::new()
            .fill(c::BG_RAISED)
            .corner_radius(r::CONTROL)
            .stroke(Stroke::new(1.0, if focused { theme::alpha(c::BRAND, 0.7) } else { c::BG_RAISED }))
            .inner_margin(egui::Margin { left: 6, right: 6, top: 4, bottom: 4 })
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(sp::S, 0.0);
                    attach = IconButton::new(ic::PAPERCLIP, "Wyślij plik (maks. 1 MB)").size(34.0).show(ui).clicked();
                    let resp = egui::TextEdit::singleline(&mut self.chat_input)
                        .id(id)
                        .hint_text(w::text("Napisz na #czat", ty::BODY, c::TEXT_MUTED))
                        .font(f(ty::BODY))
                        .text_color(c::TEXT_BODY)
                        .frame(egui::Frame::NONE)
                        .margin(egui::Margin::symmetric(0, 8))
                        .desired_width(width - 6.0 - 6.0 - 2.0 * (34.0 + sp::S))
                        .show(ui)
                        .response;
                    send |= resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    let col = if has_text { c::BRAND } else { c::TEXT_MUTED };
                    send |= IconButton::new(ic::PAPER_PLANE_RIGHT, "Wyślij (Enter)")
                        .color(col)
                        .size(34.0)
                        .show(ui)
                        .clicked();
                });
            });
        if send {
            let text = self.chat_input.trim().to_string();
            if !text.is_empty() {
                self.node.send_chat(text);
            }
            self.chat_input.clear();
            ui.memory_mut(|m| m.request_focus(id));
        }
        if attach {
            self.pick_files();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn message(
    ui: &mut egui::Ui,
    name: &str,
    color: egui::Color32,
    avatar: Option<&egui::TextureHandle>,
    time: chrono::DateTime<chrono::Local>,
    grouped: bool,
    width: f32,
    body: impl FnOnce(&mut egui::Ui),
) {
    if !grouped {
        ui.add_space(sp::L);
    }
    let top = ui.cursor().min;
    let inner = ui.scope_builder(
        UiBuilder::new().max_rect(Rect::from_min_size(
            Pos2::new(top.x + GUTTER, top.y + 2.0),
            Vec2::new(width - GUTTER - sp::L, f32::INFINITY),
        )),
        |ui| {
            ui.spacing_mut().item_spacing = Vec2::new(0.0, 2.0);
            if !grouped {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(sp::S, 0.0);
                    ui.label(w::text(name, (15.0, W::SemiBold), c::TEXT));
                    ui.label(w::text(time.format("%H:%M").to_string(), ty::CAPTION, c::TEXT_MUTED));
                });
            }
            body(ui);
        },
    );
    let h = inner.response.rect.height() + 4.0;
    let row = Rect::from_min_size(top, Vec2::new(width, h));
    let hovered = ui.rect_contains_pointer(row);
    if hovered {
        ui.painter().with_clip_rect(row).rect_filled(row, 0.0, theme::alpha(c::BG_BASE, 0.35));
    }
    if !grouped {
        Avatar { name, color, image: avatar, radius: 20.0, speaking: 0.0, status: None }
            .paint(ui, Pos2::new(top.x + sp::L + 20.0, top.y + 22.0));
    } else if hovered {
        w::paint_text(
            ui,
            Pos2::new(top.x + GUTTER - 10.0, top.y + 13.0),
            Align2::RIGHT_CENTER,
            &time.format("%H:%M").to_string(),
            theme::font(11.0, W::Medium),
            c::TEXT_MUTED,
        );
    }
    ui.allocate_rect(row, Sense::hover());
}

/// "512 B", "12,3 kB", "1,0 MB" (decimal units, like the 1 MB limit).
fn file_size(n: usize) -> String {
    match n {
        0..1_000 => format!("{n} B"),
        1_000..1_000_000 => format!("{:.1} kB", n as f64 / 1e3).replace('.', ","),
        _ => format!("{:.1} MB", n as f64 / 1e6).replace('.', ","),
    }
}

/// File attachment in a message; returns true when "Zapisz" is clicked.
fn file_card(ui: &mut egui::Ui, file_name: &str, len: usize, saved: bool) -> bool {
    let mut clicked = false;
    ui.add_space(sp::XS);
    egui::Frame::new()
        .fill(c::BG_SIDEBAR)
        .stroke(Stroke::new(1.0, c::BORDER))
        .corner_radius(r::CONTROL)
        .inner_margin(egui::Margin::symmetric(sp::M as i8, sp::S as i8))
        .show(ui, |ui| {
            ui.set_width((ui.available_width()).min(420.0));
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(sp::M, 0.0);
                ui.label(w::icon(ic::FILE, 30.0, c::TEXT_SECONDARY));
                let button_w = 112.0;
                ui.vertical(|ui| {
                    ui.set_width(ui.available_width() - button_w - sp::M);
                    ui.spacing_mut().item_spacing = Vec2::new(0.0, 2.0);
                    ui.add(egui::Label::new(w::text(file_name, (15.0, W::SemiBold), c::TEXT)).truncate())
                        .on_hover_text(file_name);
                    ui.label(w::text(file_size(len), ty::CAPTION, c::TEXT_MUTED));
                });
                let (label, icon, kind) =
                    if saved { ("Zapisano", ic::CHECK, Kind::Success) } else { ("Zapisz", ic::DOWNLOAD_SIMPLE, Kind::Secondary) };
                clicked = Button::new(label, kind)
                    .icon(icon)
                    .width(button_w)
                    .height(34.0)
                    .show(ui)
                    .on_hover_text("Zapisz plik na dysku")
                    .clicked();
            });
        });
    clicked
}

fn system_line(
    ui: &mut egui::Ui,
    icon_code: &str,
    who: &str,
    what: &str,
    good: bool,
    time: chrono::DateTime<chrono::Local>,
    width: f32,
) {
    ui.add_space(sp::S);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 26.0), Sense::hover());
    let col = if good { c::SUCCESS } else { c::TEXT_MUTED };
    w::paint_icon(ui, Pos2::new(rect.left() + sp::L + 20.0, rect.center().y), icon_code, 18.0, col, false);
    let mut job = egui::text::LayoutJob::default();
    let fmt = |font, color| egui::TextFormat { font_id: font, color, ..Default::default() };
    job.append(who, 0.0, fmt(theme::font(15.0, W::SemiBold), c::TEXT));
    job.append(&format!(" {what}"), 0.0, fmt(f(ty::BODY), c::TEXT_MUTED));
    job.append(&format!("   {}", time.format("%H:%M")), 0.0, fmt(f(ty::CAPTION), c::TEXT_MUTED));
    let galley = ui.painter().layout_job(job);
    ui.painter().galley(
        Pos2::new(rect.left() + GUTTER, rect.center().y - galley.size().y / 2.0),
        galley,
        c::TEXT,
    );
}
