//! First screen: name, create a room or join with an invite.

use super::theme::{self, W, c, f, r, sp, ty};
use super::widgets::{self as w, Button, Kind, ic};
use super::App;
use eframe::egui::{self, Align2, Key, Pos2, Sense, Vec2};

const CARD_W: f32 = 440.0;

impl App {
    pub(crate) fn start_screen(&mut self, root: &mut egui::Ui) {
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(c::BG_BASE))
            .show(root, |ui| {
                let avail = ui.available_rect_before_wrap();
                let card_h = 560.0;
                let top = (avail.height() - card_h).max(24.0) / 2.0;
                ui.add_space(top);
                ui.vertical_centered(|ui| {
                    egui::Frame::new()
                        .fill(c::BG_CONTENT)
                        .corner_radius(r::CARD)
                        .shadow(theme::shadow_overlay())
                        .inner_margin(egui::Margin::same(sp::XXL as i8))
                        .show(ui, |ui| {
                            ui.set_width(CARD_W - 2.0 * sp::XXL);
                            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                                if self.view == super::View::Profile {
                                    self.profile_card(ui)
                                } else {
                                    self.start_card(ui)
                                }
                            });
                        });
                });
            });
    }

    fn start_card(&mut self, ui: &mut egui::Ui) {
        let width = ui.available_width();
        // Mark + heading
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(44.0), Sense::hover());
            let logo = logo_texture(ui.ctx());
            ui.painter().image(
                logo.id(),
                rect,
                egui::Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                c::WHITE,
            );
            ui.add_space(sp::XS);
            ui.vertical(|ui| {
                ui.add_space(1.0);
                ui.label(w::text("Witaj w BibiCord", ty::DISPLAY, c::TEXT));
                ui.label(w::text(
                    "Głos, czat i streamy w jakości źródła. Bez serwera.",
                    ty::SMALL,
                    c::TEXT_MUTED,
                ));
            });
        });
        ui.add_space(sp::XL);

        w::field_label(ui, "Twoja nazwa");
        let mut open_profile = false;
        ui.horizontal(|ui| {
            let side = 42.0;
            let (rect, resp) = ui.allocate_exact_size(Vec2::splat(side), Sense::click());
            let resp = resp
                .on_hover_text("Profil: zdjęcie i nazwa")
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            let t = w::hover_t(ui, &resp);
            w::Avatar {
                name: &self.name,
                color: self.my_color(),
                image: self.my_avatar_tex.as_ref(),
                radius: side / 2.0,
                speaking: 0.0,
                status: None,
            }
            .paint(ui, rect.center());
            if t > 0.0 {
                ui.painter().circle_filled(rect.center(), side / 2.0, theme::alpha(c::BG_STAGE, 0.55 * t));
                w::paint_icon(ui, rect.center(), ic::CAMERA, 18.0, theme::alpha(c::TEXT, t), false);
            }
            open_profile = resp.clicked();
            ui.add_space(sp::S - ui.spacing().item_spacing.x);
            w::text_field(ui, &mut self.name, "np. Marcin", "start_name", width - side - sp::S);
        });
        if self.name.chars().count() > crate::proto::MAX_NAME {
            self.name = self.name.chars().take(crate::proto::MAX_NAME).collect();
        }
        if open_profile {
            self.open_profile();
        }
        ui.add_space(sp::L);

        w::field_label(ui, "Nazwa pokoju");
        let default_room = if self.name.trim().is_empty() {
            "Pokój".to_string()
        } else {
            format!("Pokój {}", self.name.trim())
        };
        w::text_field(ui, &mut self.room_input, &default_room, "start_room", width);
        ui.add_space(sp::M);

        let can = self.net_ready && !self.name.trim().is_empty();
        if Button::new("Utwórz pokój", Kind::Primary)
            .icon(ic::PLUS)
            .width(width)
            .height(42.0)
            .enabled(can)
            .show(ui)
            .clicked()
        {
            self.name = self.name.trim().to_string();
            let room = match self.room_input.trim() {
                "" => default_room,
                r => r.to_string(),
            };
            self.node.create_room(self.name.clone(), room);
            self.enter_room();
        }

        // "lub" divider
        ui.add_space(sp::L);
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 16.0), Sense::hover());
        let y = rect.center().y;
        let label = ui.painter().layout_no_wrap("lub".into(), f(ty::CAPTION), c::TEXT_MUTED);
        let lw = label.size().x + 24.0;
        ui.painter().hline(rect.left()..=rect.center().x - lw / 2.0, y, (1.0, c::BORDER));
        ui.painter().hline(rect.center().x + lw / 2.0..=rect.right(), y, (1.0, c::BORDER));
        ui.painter().galley(rect.center() - label.size() / 2.0, label, c::TEXT_MUTED);
        ui.add_space(sp::L);

        w::field_label(ui, "Zaproszenie");
        let join_w = 96.0;
        let mut join = false;
        ui.horizontal(|ui| {
            let resp = w::text_field(
                ui,
                &mut self.ticket_input,
                "Wklej link zaproszenia",
                "start_ticket",
                width - join_w - sp::S,
            );
            if resp.changed() {
                self.join_error = None;
            }
            join |= resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            let can_join = can && !self.ticket_input.trim().is_empty();
            join &= can_join;
            join |= Button::new("Dołącz", Kind::Secondary)
                .width(join_w)
                .height(42.0)
                .enabled(can_join)
                .show(ui)
                .clicked();
        });
        if let Some(err) = &self.join_error {
            ui.add_space(sp::XS);
            ui.label(w::text(err, ty::SMALL, c::DANGER));
        }
        if join {
            self.name = self.name.trim().to_string();
            match self.node.join(&self.ticket_input, self.name.clone()) {
                Ok(()) => {
                    self.join_error = None;
                    self.enter_room();
                }
                Err(e) => {
                    let e = format!("{e:#}");
                    // Only a garbled link can be fixed by copying it again.
                    let hint = if e == crate::proto::NOT_AN_INVITE { " Sprawdź, czy skopiowano cały link." } else { "" };
                    self.join_error = Some(format!("{e}.{hint}"));
                }
            }
        }

        // Footer: update + status
        ui.add_space(sp::XL);
        w::separator(ui, c::BORDER);
        ui.add_space(sp::M);
        if self.update_available().is_some() || matches!(*self.update.lock().unwrap(), crate::update::State::Failed(_)) {
            self.update_banner(ui);
            ui.add_space(sp::S);
        }
        let share = Button::new("Zaproś znajomych do BibiCord", Kind::Ghost)
            .icon(ic::USER_PLUS)
            .width(width)
            .height(36.0)
            .show(ui)
            .on_hover_text("Link do instalatora dla Windows i macOS");
        if share.clicked() {
            self.dialog = Some(super::Dialog::Share);
        }
        ui.add_space(sp::S);
        let ff = self.ffmpeg_status.lock().unwrap().clone();
        let (dot, line) = if !self.net_ready {
            (c::WARNING, "Łączenie z siecią…".to_string())
        } else if !ff.is_empty() {
            (c::WARNING, ff)
        } else if self.name.trim().is_empty() {
            (c::SUCCESS, "Sieć gotowa. Wpisz swoją nazwę.".to_string())
        } else {
            (c::SUCCESS, "Sieć gotowa".to_string())
        };
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::new(8.0, 16.0), Sense::hover());
            ui.painter().circle_filled(rect.center(), 4.0, dot);
            ui.label(w::text(line, ty::SMALL, c::TEXT_MUTED));
        });
        let _ = (Align2::LEFT_TOP, Pos2::ZERO, W::Regular);
    }
}

/// App icon (the same PNG as the window icon), decoded on first use.
fn logo_texture(ctx: &egui::Context) -> &'static egui::TextureHandle {
    static LOGO: std::sync::OnceLock<egui::TextureHandle> = std::sync::OnceLock::new();
    LOGO.get_or_init(|| {
        let png = include_bytes!("../../packaging/icon/icon-256.png");
        let img = image::load_from_memory(png).expect("icon-256.png").to_rgba8();
        let size = [img.width() as usize, img.height() as usize];
        let color = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
        ctx.load_texture("bibicord_logo", color, egui::TextureOptions::LINEAR)
    })
}
