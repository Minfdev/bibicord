//! Modal dialogs (Go Live, Settings, Invite) and the error toast.

use super::theme::{self, W, c, f, r, sp, ty};
use super::widgets::{self as w, Button, IconButton, Kind, ic};
use super::{App, Dialog};
use crate::proto::Codec;
use eframe::egui::{self, Align, Align2, Layout, Margin, Stroke, Ui, Vec2};
use std::time::Duration;

const DIALOG_W: f32 = 440.0;

fn res_label(h: u32) -> String {
    match h {
        0 => "Źródło (natywna)".into(),
        2160 => "2160p (4K)".into(),
        h => format!("{h}p"),
    }
}

/// Title row with a close button. Returns true when close was clicked.
fn header(ui: &mut Ui, title: &str, subtitle: &str) -> bool {
    let mut close = false;
    egui::Frame::new()
        .inner_margin(Margin { left: 24, right: 16, top: 20, bottom: 4 })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(w::text(title, ty::TITLE, c::TEXT));
                    if !subtitle.is_empty() {
                        ui.add_space(sp::XXS);
                        ui.label(w::text(subtitle, ty::SMALL, c::TEXT_MUTED));
                    }
                });
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                    if IconButton::new(ic::X, "Zamknij").size(28.0).show(ui).clicked() {
                        close = true;
                    }
                });
            });
        });
    close
}

fn body(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .inner_margin(Margin { left: 24, right: 24, top: 12, bottom: 20 })
        .show(ui, add);
}

/// Footer strip on a darker surface, buttons aligned right.
fn footer(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .fill(c::BG_SIDEBAR)
        .corner_radius(egui::CornerRadius { nw: 0, ne: 0, sw: r::CARD, se: r::CARD })
        .inner_margin(Margin::symmetric(16, 14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.with_layout(Layout::right_to_left(Align::Center), add);
        });
}

fn combo_style(ui: &mut Ui) {
    let v = &mut ui.style_mut().visuals.widgets;
    for wv in [&mut v.inactive, &mut v.hovered, &mut v.active, &mut v.open] {
        wv.bg_fill = c::BG_RAISED;
        wv.weak_bg_fill = c::BG_RAISED;
    }
    v.hovered.bg_stroke = Stroke::new(1.0, c::BORDER_STRONG);
    ui.style_mut().spacing.button_padding = Vec2::new(12.0, 9.0);
}

/// Device picker: `None` means the system default.
fn device_combo(ui: &mut Ui, id: &str, value: &mut Option<String>, list: &[crate::voice::DeviceInfo]) -> bool {
    let before = value.clone();
    let selected = value
        .as_ref()
        .and_then(|v| list.iter().find(|d| &d.id == v))
        .map(|d| d.name.clone())
        .unwrap_or_else(|| "Domyślne systemowe".into());
    combo_style(ui);
    egui::ComboBox::from_id_salt(id)
        .width(ui.available_width())
        .selected_text(w::text(selected, ty::BODY, c::TEXT))
        .show_ui(ui, |ui| {
            ui.selectable_value(value, None, w::text("Domyślne systemowe", ty::SMALL_MEDIUM, c::TEXT));
            for d in list {
                ui.selectable_value(value, Some(d.id.clone()), w::text(&d.name, ty::SMALL_MEDIUM, c::TEXT));
            }
        });
    *value != before
}

/// One line of release notes with **bold** and `code` spans.
fn rich_line(text: &str, color: egui::Color32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let (mut bold, mut code) = (false, false);
    let mut buf = String::new();
    let flush = |job: &mut egui::text::LayoutJob, buf: &mut String, bold: bool, code: bool| {
        if buf.is_empty() {
            return;
        }
        let (font, col) = if code {
            (egui::FontId::monospace(12.5), c::TEXT)
        } else if bold {
            (theme::font(13.0, W::SemiBold), c::TEXT)
        } else {
            (f(ty::SMALL), color)
        };
        job.append(buf, 0.0, egui::TextFormat { font_id: font, color: col, ..Default::default() });
        buf.clear();
    };
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '`' {
            flush(&mut job, &mut buf, bold, code);
            code = !code;
        } else if ch == '*' && chars.peek() == Some(&'*') && !code {
            chars.next();
            flush(&mut job, &mut buf, bold, code);
            bold = !bold;
        } else {
            buf.push(ch);
        }
    }
    flush(&mut job, &mut buf, bold, code);
    job.wrap.max_width = f32::INFINITY;
    job
}

fn progress_bar(ui: &mut Ui, p: f32) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 6.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 3, c::BG_SELECTED);
    let mut done = rect;
    done.set_width(w * p.clamp(0.0, 1.0));
    ui.painter().rect_filled(done, 3, c::BRAND);
}

impl App {
    /// Compact "update available" card. Opens the update dialog on click.
    pub(crate) fn update_banner(&mut self, ui: &mut Ui) {
        use crate::update::State;
        let state = self.update.lock().unwrap().clone();
        let (title, sub, p) = match &state {
            State::Available(r) => (format!("Dostępna wersja {}", r.version), "Kliknij, aby zaktualizować".to_string(), None),
            State::Downloading(r, p) => (format!("Pobieranie {}", r.version), format!("{:.0}%", p * 100.0), Some(*p)),
            State::Installing(r) => (format!("Instalowanie {}", r.version), "Za chwilę restart…".to_string(), Some(1.0)),
            State::Failed(e) => ("Aktualizacja nie powiodła się".to_string(), e.clone(), None),
            _ => return,
        };
        let failed = matches!(state, State::Failed(_));
        let w_ = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(w_, 46.0), egui::Sense::click());
        let t = w::hover_t(ui, &resp);
        let accent = if failed { c::DANGER } else { c::SUCCESS };
        ui.painter().rect_filled(rect, r::CONTROL, theme::alpha(accent, 0.12 + 0.06 * t));
        ui.painter().rect_stroke(rect, r::CONTROL, Stroke::new(1.0, theme::alpha(accent, 0.35)), egui::StrokeKind::Inside);
        let icon = if failed { ic::WARNING_CIRCLE } else { ic::ARROW_CIRCLE_UP };
        w::paint_icon(ui, rect.left_center() + Vec2::new(20.0, 0.0), icon, 20.0, accent, true);
        let x = rect.left() + 40.0;
        let max = rect.right() - x - 8.0;
        w::paint_text_clipped(ui, egui::Pos2::new(x, rect.center().y - 8.0), &title, f(ty::SMALL_MEDIUM), c::TEXT, max);
        w::paint_text_clipped(ui, egui::Pos2::new(x, rect.center().y + 9.0), &sub, f(ty::CAPTION), c::TEXT_MUTED, max);
        if let Some(p) = p {
            let mut bar = rect.shrink2(Vec2::new(8.0, 0.0));
            bar.min.y = rect.bottom() - 4.0;
            bar.max.y = rect.bottom() - 2.0;
            bar.set_width(bar.width() * p.clamp(0.0, 1.0));
            ui.painter().rect_filled(bar, 1, accent);
        }
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        if resp.clicked() {
            if failed {
                // Retry: check again, then offer the update.
                *self.update.lock().unwrap() = State::Idle;
                crate::update::spawn_check(self.update.clone(), ui.ctx().clone());
            } else {
                self.dialog = Some(Dialog::Update);
            }
        }
    }

    pub(crate) fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(d) = self.dialog else { return };
        let modal = egui::Modal::new(egui::Id::unique(("dialog", d as u8)))
            .frame(w::modal_frame())
            .backdrop_color(egui::Color32::from_black_alpha(170))
            .show(ctx, |ui| {
                ui.set_width(DIALOG_W);
                ui.spacing_mut().item_spacing = Vec2::new(sp::S, sp::S);
                match d {
                    Dialog::GoLive => self.go_live_ui(ui),
                    Dialog::Settings => self.settings_ui(ui),
                    Dialog::Invite => self.invite_ui(ui),
                    Dialog::Update => self.update_ui(ui),
                    Dialog::Share => self.share_ui(ui),
                }
            });
        if modal.should_close() {
            self.dialog = None;
        }
    }

    fn go_live_ui(&mut self, ui: &mut Ui) {
        if header(ui, "Udostępnij ekran", "Strumień w jakości źródła, prosto do znajomych.") {
            self.dialog = None;
        }
        body(ui, |ui| {
            let s = &mut self.settings;
            w::field_label(ui, "Ekran");
            if self.displays.is_empty() {
                ui.label(w::text("Nie znaleziono ekranów.", ty::SMALL, c::TEXT_MUTED));
            } else {
                // Display cards: one per screen.
                ui.horizontal_wrapped(|ui| {
                    for (i, name) in self.displays.iter().enumerate() {
                        let sel = s.display == i;
                        let size = Vec2::new(((DIALOG_W - 48.0 - sp::S) / 2.0).floor(), 64.0);
                        let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
                        let t = w::hover_t(ui, &resp);
                        let bg = if sel { theme::alpha(c::BRAND, 0.14) } else { theme::mix(c::BG_RAISED, c::BG_SELECTED, t) };
                        ui.painter().rect_filled(rect, r::CONTROL, bg);
                        if sel {
                            ui.painter().rect_stroke(rect, r::CONTROL, Stroke::new(1.5, c::BRAND), egui::StrokeKind::Inside);
                        }
                        let fg = if sel { c::TEXT } else { c::TEXT_SECONDARY };
                        w::paint_icon(ui, rect.left_center() + Vec2::new(26.0, 0.0), ic::MONITOR, 24.0, fg, sel);
                        let title = format!("Ekran {}", i + 1);
                        let has_sub = name.trim() != title;
                        let dy = if has_sub { -8.0 } else { 0.0 };
                        w::paint_text_clipped(
                            ui,
                            rect.left_center() + Vec2::new(48.0, dy),
                            &title,
                            f(ty::LABEL),
                            fg,
                            rect.width() - 60.0,
                        );
                        if has_sub {
                            w::paint_text_clipped(
                                ui,
                                rect.left_center() + Vec2::new(48.0, 10.0),
                                name,
                                f(ty::CAPTION),
                                c::TEXT_MUTED,
                                rect.width() - 60.0,
                            );
                        }
                        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            s.display = i;
                        }
                    }
                });
            }

            ui.add_space(sp::M);
            w::field_label(ui, "Rozdzielczość");
            ui.horizontal_wrapped(|ui| {
                for h in [0, 2160, 1440, 1080, 720] {
                    let label = if h == 0 { "Źródło".to_string() } else { format!("{h}p") };
                    if w::chip(ui, &label, s.max_height == h).on_hover_text(res_label(h)).clicked() {
                        s.max_height = h;
                    }
                }
            });

            ui.add_space(sp::M);
            ui.columns(2, |cols| {
                w::field_label(&mut cols[0], "Klatki na sekundę");
                cols[0].horizontal(|ui| {
                    for fps in [30, 60, 120] {
                        if w::chip(ui, &fps.to_string(), s.fps == fps).clicked() {
                            s.fps = fps;
                        }
                    }
                });
                w::field_label(&mut cols[1], "Kodek");
                cols[1].horizontal(|ui| {
                    for (codec, label) in [(Codec::H264, "H.264"), (Codec::Hevc, "HEVC")] {
                        if w::chip(ui, label, s.codec == codec).clicked() {
                            s.codec = codec;
                        }
                    }
                });
            });

            ui.add_space(sp::M);
            w::field_label_value(ui, "Bitrate", &format!("{} Mb/s", s.bitrate_kbps / 1000));
            ui.spacing_mut().slider_width = ui.available_width();
            ui.add(
                egui::Slider::new(&mut s.bitrate_kbps, 4_000..=120_000)
                    .step_by(1000.0)
                    .show_value(false),
            );

            ui.add_space(sp::M);
            egui::Frame::new()
                .fill(theme::alpha(c::BRAND, 0.08))
                .corner_radius(r::CONTROL)
                .inner_margin(Margin::symmetric(12, 10))
                .show(ui, |ui| {
                    ui.horizontal_top(|ui| {
                        ui.label(w::icon(ic::INFO, 16.0, c::BRAND));
                        ui.add(
                            egui::Label::new(w::text(
                                "Obraz jest kodowany sprzętowo raz i trafia do widzów bez \
                                 transkodowania. Każdy widz pobiera pełny bitrate z Twojego łącza.",
                                ty::SMALL,
                                c::TEXT_SECONDARY,
                            ))
                            .wrap(),
                        );
                    });
                });
        });
        footer(ui, |ui| {
            let ok = Button::new("Rozpocznij stream", Kind::Primary)
                .icon(ic::BROADCAST)
                .enabled(!self.displays.is_empty())
                .show(ui);
            if ok.clicked() {
                self.dialog = None;
                self.stream_starting = true;
                self.node.start_stream(self.settings.clone());
            }
            if Button::new("Anuluj", Kind::Ghost).show(ui).clicked() {
                self.dialog = None;
            }
        });
    }

    fn settings_ui(&mut self, ui: &mut Ui) {
        if header(ui, "Ustawienia głosu", "Mikrofon, głośniki i kamera używane w rozmowie.") {
            self.dialog = None;
        }
        body(ui, |ui| {
            self.audio_settings(ui);
            ui.add_space(sp::L);
            w::separator(ui, c::BORDER);
            ui.add_space(sp::M);
            w::field_label(ui, "Kamera");
            let cams: Vec<_> = self.cameras.iter().map(|n| crate::voice::DeviceInfo { id: n.clone(), name: n.clone() }).collect();
            device_combo(ui, "dev_cam", &mut self.camera, &cams);
            if self.cameras.is_empty() {
                ui.label(w::text("Nie znaleziono kamery.", ty::SMALL, c::TEXT_MUTED));
            }
            ui.add_space(sp::L);
            w::separator(ui, c::BORDER);
            ui.add_space(sp::M);
            w::field_label(ui, "Twój profil");
            let mut edit = false;
            ui.horizontal(|ui| {
                w::Avatar {
                    name: &self.name,
                    color: self.my_color(),
                    image: self.my_avatar_tex.as_ref(),
                    radius: 18.0,
                    speaking: 0.0,
                    status: None,
                }
                .show(ui);
                ui.vertical(|ui| {
                    ui.label(w::text(&self.name, ty::HEADING, c::TEXT));
                    ui.label(w::text(
                        format!("Głos: Opus 48 kHz · v{}", env!("CARGO_PKG_VERSION")),
                        ty::CAPTION,
                        c::TEXT_MUTED,
                    ));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    edit = Button::new("Edytuj profil", Kind::Secondary).height(32.0).show(ui).clicked();
                });
            });
            if edit {
                self.dialog = None;
                self.open_profile();
            }
        });
        footer(ui, |ui| {
            if Button::new("Gotowe", Kind::Primary).show(ui).clicked() {
                self.dialog = None;
            }
        });
    }

    /// Devices, microphone processing and notification sounds. Works without
    /// a room: changes are stored and used by the next voice connection.
    pub(crate) fn audio_settings(&mut self, ui: &mut Ui) {
        let (inputs, outputs) = self.audio_devices.get_or_insert_with(crate::voice::list_devices).clone();
        let mut dev = self.node.devices();
        w::field_label(ui, "Mikrofon");
        let mut changed = device_combo(ui, "dev_in", &mut dev.input, &inputs);
        ui.add_space(sp::M);
        w::field_label(ui, "Głośniki / słuchawki");
        changed |= device_combo(ui, "dev_out", &mut dev.output, &outputs);
        ui.add_space(sp::L);
        w::separator(ui, c::BORDER);
        ui.add_space(sp::M);
        self.mic_ui(ui);
        ui.add_space(sp::L);
        w::separator(ui, c::BORDER);
        ui.add_space(sp::M);
        self.sounds_ui(ui, dev.output.clone());
        if changed {
            self.node.set_devices(dev);
        }
    }

    /// Microphone processing: noise suppression and sensitivity (dBFS) with a live level meter.
    fn mic_ui(&mut self, ui: &mut Ui) {
        use crate::mic::{DB_MAX, DB_MIN};
        let before = self.node.voice_prefs();
        let mut p = before;
        w::field_label(ui, "Redukcja szumów");
        ui.horizontal(|ui| {
            if w::chip(ui, "Włączona", p.noise_suppression).clicked() {
                p.noise_suppression = true;
            }
            if w::chip(ui, "Wyłączona", !p.noise_suppression).clicked() {
                p.noise_suppression = false;
            }
        });
        ui.label(w::text("Tłumi wentylatory, klawiaturę i szum tła (RNNoise).", ty::SMALL, c::TEXT_MUTED));
        ui.add_space(sp::M);

        let value = p.sensitivity.map_or("Automatyczna".into(), |d| format!("{d} dB"));
        w::field_label_value(ui, "Czułość mikrofonu", &value);
        ui.horizontal(|ui| {
            if w::chip(ui, "Automatyczna", p.sensitivity.is_none()).clicked() {
                p.sensitivity = None;
            }
            if w::chip(ui, "Ręczna", p.sensitivity.is_some()).clicked() && p.sensitivity.is_none() {
                p.sensitivity = Some(-50);
            }
        });
        ui.add_space(sp::S);
        let width = ui.available_width();
        if let Some(db) = p.sensitivity.as_mut() {
            ui.spacing_mut().slider_width = width;
            ui.add(egui::Slider::new(db, DB_MIN..=DB_MAX).show_value(false));
        }
        // Level meter: lit while the voice gate is open.
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 6.0), egui::Sense::hover());
        ui.painter().rect_filled(rect, 3.0, c::BG_BASE);
        let voice = self.node.voice();
        if let Some(v) = &voice {
            let t = ((v.mic_level_db() - DB_MIN as f32) / (DB_MAX - DB_MIN) as f32).clamp(0.0, 1.0);
            let mut lit = rect;
            lit.set_width(rect.width() * t);
            ui.painter().rect_filled(lit, 3.0, if v.me_speaking() { c::SUCCESS } else { c::TEXT_MUTED });
            ui.ctx().request_repaint_after(Duration::from_millis(50));
        }
        ui.add_space(sp::XS);
        let hint = match (&voice, p.sensitivity) {
            (None, _) => "Dołącz do kanału głosowego, aby zobaczyć poziom mikrofonu.",
            (Some(_), None) => "Próg dopasowuje się do szumu tła.",
            (Some(_), Some(_)) => "Mówisz, gdy poziom przekracza próg na suwaku.",
        };
        ui.label(w::text(hint, ty::SMALL, c::TEXT_MUTED));
        if p != before {
            self.node.set_voice_prefs(p);
            p.save();
        }
    }

    /// Sound cues: on/off, volume and a preview.
    fn sounds_ui(&mut self, ui: &mut Ui, output: Option<String>) {
        use crate::sounds::Cue;
        let before = self.sounds.prefs;
        let p = &mut self.sounds.prefs;
        let value = if p.enabled { format!("{} %", p.volume) } else { "Wyłączone".into() };
        w::field_label_value(ui, "Dźwięki powiadomień", &value);
        ui.horizontal(|ui| {
            if w::chip(ui, "Włączone", p.enabled).clicked() {
                p.enabled = true;
            }
            if w::chip(ui, "Wyłączone", !p.enabled).clicked() {
                p.enabled = false;
            }
        });
        ui.add_space(sp::S);
        let mut released = false;
        ui.add_enabled_ui(p.enabled, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().slider_width = ui.available_width() - 110.0;
                let r = ui.add(egui::Slider::new(&mut p.volume, 0..=100).show_value(false));
                released = r.drag_stopped() || (r.changed() && !r.dragged());
                ui.add_space(sp::S);
                if Button::new("Odtwórz", Kind::Secondary).icon(ic::PLAY).show(ui).clicked() {
                    released = true;
                }
            });
        });
        ui.label(w::text(
            "Dołączanie, wychodzenie, wyciszenie, streamy i nowe wiadomości.",
            ty::SMALL,
            c::TEXT_MUTED,
        ));
        if self.sounds.prefs != before {
            self.sounds.prefs.save();
        }
        if released {
            self.sounds.play(Cue::PeerJoin, output);
        }
    }

    fn invite_ui(&mut self, ui: &mut Ui) {
        let title = format!("Zaproś do: {}", self.room);
        if header(ui, &title, "Wyślij kod znajomemu. Połączenie jest bezpośrednie, bez serwera.") {
            self.dialog = None;
        }
        let ctx = ui.ctx().clone();
        body(ui, |ui| {
            w::field_label(ui, "Kod zaproszenia");
            let copied = self.was_copied(&ctx, "code");
            egui::Frame::new()
                .fill(c::BG_BASE)
                .corner_radius(r::CONTROL)
                .inner_margin(Margin { left: 12, right: 6, top: 6, bottom: 6 })
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let avail = ui.available_width() - 104.0;
                        let shown = self.ticket.as_deref().unwrap_or("Generowanie…");
                        let (rect, _) = ui.allocate_exact_size(Vec2::new(avail, 38.0), egui::Sense::hover());
                        let font = if self.ticket.is_some() {
                            egui::FontId::monospace(20.0)
                        } else {
                            theme::font(14.0, W::Regular)
                        };
                        w::paint_text_clipped(ui, rect.left_center(), shown, font, c::TEXT, avail);
                        let (label, kind, icon) = if copied {
                            ("Skopiowano", Kind::Success, ic::CHECK)
                        } else {
                            ("Kopiuj", Kind::Primary, ic::COPY)
                        };
                        let b = Button::new(label, kind).icon(icon).width(100.0).enabled(self.ticket.is_some()).show(ui);
                        if b.clicked() {
                            if let Some(t) = self.ticket.clone() {
                                self.copy(&ctx, "code", t);
                            }
                        }
                    });
                });
            ui.add_space(sp::L);
            for (n, step) in [
                "Skopiuj kod i wyślij go znajomemu.",
                "Znajomy wpisuje kod na ekranie startowym i klika Dołącz.",
                "Pokój działa, dopóki ktoś w nim jest.",
            ]
            .iter()
            .enumerate()
            {
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(22.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 11.0, c::BG_SELECTED);
                    w::paint_text(ui, rect.center(), Align2::CENTER_CENTER, &(n + 1).to_string(), theme::font(11.0, W::Bold), c::TEXT_SECONDARY);
                    ui.label(w::text(*step, ty::SMALL, c::TEXT_SECONDARY));
                });
            }
        });
        // Friends without the app: one message with installers + this code.
        let copied = self.was_copied(&ctx, "invite_msg");
        let mut open_share = false;
        footer(ui, |ui| {
            let (label, kind, icon) = if copied {
                ("Skopiowano", Kind::Success, ic::CHECK)
            } else {
                ("Kopiuj z linkiem", Kind::Secondary, ic::LINK)
            };
            let b = Button::new(label, kind).icon(icon).enabled(self.ticket.is_some()).show(ui);
            if b.on_hover_text("Wiadomość z linkami do instalatorów i kodem pokoju").clicked() {
                let m = self.share_message();
                self.copy(&ctx, "invite_msg", m);
            }
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                ui.add_space(sp::S);
                ui.label(w::text("Znajomy nie ma BibiCord?", ty::SMALL, c::TEXT_MUTED));
                if ui
                    .link(w::text("Więcej", ty::SMALL_MEDIUM, c::BRAND))
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    open_share = true;
                }
            });
        });
        if open_share {
            self.dialog = Some(Dialog::Share);
        }
    }

    /// "Invite friends to BibiCord": download links to send to people
    /// who don't have the app yet.
    fn share_ui(&mut self, ui: &mut Ui) {
        use crate::update::{DOWNLOAD_MACOS, DOWNLOAD_PAGE, DOWNLOAD_WINDOWS};
        if header(
            ui,
            "Zaproś znajomych do BibiCord",
            "Wyślij link. Instalacja to jeden plik, bez zakładania konta.",
        ) {
            self.dialog = None;
        }
        let ctx = ui.ctx().clone();
        body(ui, |ui| {
            w::field_label(ui, "Link do pobrania");
            let copied = self.was_copied(&ctx, "page");
            egui::Frame::new()
                .fill(c::BG_BASE)
                .corner_radius(r::CONTROL)
                .inner_margin(Margin { left: 12, right: 6, top: 6, bottom: 6 })
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let avail = ui.available_width() - 104.0;
                        let (rect, _) = ui.allocate_exact_size(Vec2::new(avail, 38.0), egui::Sense::hover());
                        let shown = DOWNLOAD_PAGE.trim_start_matches("https://");
                        w::paint_text_clipped(ui, rect.left_center(), shown, f(ty::SMALL_MEDIUM), c::TEXT, avail);
                        let (label, kind, icon) = if copied {
                            ("Skopiowano", Kind::Success, ic::CHECK)
                        } else {
                            ("Kopiuj", Kind::Primary, ic::COPY)
                        };
                        if Button::new(label, kind).icon(icon).width(100.0).show(ui).clicked() {
                            self.copy(&ctx, "page", DOWNLOAD_PAGE.into());
                        }
                    });
                });
            ui.label(w::text(
                "Zawsze prowadzi do najnowszej wersji. Znajomy wybiera plik dla swojego systemu.",
                ty::SMALL,
                c::TEXT_MUTED,
            ));

            ui.add_space(sp::M);
            w::field_label(ui, "Bezpośrednio do instalatora");
            for (key, icon, name, file, url) in [
                ("win", ic::WINDOWS_LOGO, "Windows 10 / 11", "BibiCord-Setup.exe", DOWNLOAD_WINDOWS),
                ("mac", ic::APPLE_LOGO, "macOS 14+ · Apple Silicon", "BibiCord.dmg", DOWNLOAD_MACOS),
            ] {
                let copied = self.was_copied(&ctx, key);
                egui::Frame::new()
                    .fill(c::BG_RAISED)
                    .corner_radius(r::CONTROL)
                    .inner_margin(Margin { left: 12, right: 6, top: 6, bottom: 6 })
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(w::icon(icon, 22.0, c::TEXT_SECONDARY));
                            ui.add_space(sp::XS);
                            ui.vertical(|ui| {
                                ui.label(w::text(name, ty::LABEL, c::TEXT));
                                ui.label(w::text(file, ty::CAPTION, c::TEXT_MUTED));
                            });
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let (icon, tip) = if copied { (ic::CHECK, "Skopiowano") } else { (ic::LINK, "Kopiuj link") };
                                let col = if copied { c::SUCCESS } else { c::TEXT_SECONDARY };
                                if IconButton::new(icon, tip).color(col).show(ui).clicked() {
                                    self.copy(&ctx, key, url.into());
                                }
                            });
                        });
                    });
            }
        });
        let copied = self.was_copied(&ctx, "share_msg");
        footer(ui, |ui| {
            let (label, kind, icon) = if copied {
                ("Skopiowano", Kind::Success, ic::CHECK)
            } else {
                ("Kopiuj wiadomość", Kind::Primary, ic::CHAT_TEXT)
            };
            let tip = if self.screen == super::Screen::Room && self.ticket.is_some() {
                "Tekst z linkami do obu instalatorów i kodem tego pokoju"
            } else {
                "Tekst z linkami do obu instalatorów"
            };
            if Button::new(label, kind).icon(icon).show(ui).on_hover_text(tip).clicked() {
                let m = self.share_message();
                self.copy(&ctx, "share_msg", m);
            }
            if Button::new("Otwórz stronę", Kind::Ghost).icon(ic::ARROW_SQUARE_OUT).show(ui).clicked() {
                ctx.open_url(egui::OpenUrl::new_tab(DOWNLOAD_PAGE));
            }
        });
    }

    fn update_ui(&mut self, ui: &mut Ui) {
        use crate::update::State;
        let state = self.update.lock().unwrap().clone();
        let release = match &state {
            State::Available(r) | State::Downloading(r, _) | State::Installing(r) => r.clone(),
            _ => {
                self.dialog = None;
                return;
            }
        };
        let busy = matches!(state, State::Downloading(..) | State::Installing(_));
        let title = format!("BibiCord {}", release.version);
        let sub = format!("Masz wersję {}. Aktualizacja zajmie chwilę, aplikacja uruchomi się ponownie.", crate::update::CURRENT);
        if header(ui, &title, &sub) && !busy {
            self.dialog = None;
        }
        body(ui, |ui| {
            w::field_label(ui, "Co nowego");
            let notes = if release.notes.is_empty() { "Poprawki i ulepszenia.".to_string() } else { release.notes.clone() };
            egui::Frame::new()
                .fill(c::BG_BASE)
                .corner_radius(r::CONTROL)
                .inner_margin(Margin::same(12))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                        for line in notes.lines() {
                            let line = line.trim_end();
                            if line.eq_ignore_ascii_case("## co nowego") {
                                continue;
                            }
                            // Download/install instructions are for the web page.
                            if line.starts_with("## Pobieranie") {
                                break;
                            }
                            if let Some(h) = line.strip_prefix("## ").or_else(|| line.strip_prefix("### ")) {
                                ui.add_space(sp::XS);
                                ui.label(w::text(h, ty::SMALL_MEDIUM, c::TEXT));
                            } else if let Some(item) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
                                ui.horizontal_top(|ui| {
                                    ui.label(w::text("•", ty::SMALL, c::TEXT_MUTED));
                                    ui.add(egui::Label::new(rich_line(item, c::TEXT_SECONDARY)).wrap());
                                });
                            } else if !line.is_empty() {
                                ui.add(egui::Label::new(rich_line(line, c::TEXT_SECONDARY)).wrap());
                            }
                        }
                    });
                });
            if let State::Downloading(_, p) = state {
                ui.add_space(sp::M);
                w::field_label_value(ui, "Pobieranie", &format!("{:.0}%", p * 100.0));
                progress_bar(ui, p);
            } else if matches!(state, State::Installing(_)) {
                ui.add_space(sp::M);
                ui.label(w::text("Instalowanie… aplikacja za chwilę uruchomi się ponownie.", ty::SMALL, c::TEXT_SECONDARY));
            }
        });
        let ctx = ui.ctx().clone();
        footer(ui, |ui| {
            let label = if busy { "Aktualizowanie…" } else { "Zaktualizuj teraz" };
            if Button::new(label, Kind::Primary).icon(ic::DOWNLOAD_SIMPLE).enabled(!busy).show(ui).clicked() {
                self.start_update(&ctx);
            }
            if !busy {
                if Button::new("Później", Kind::Ghost).show(ui).clicked() {
                    self.dialog = None;
                }
                if IconButton::new(ic::ARROW_SQUARE_OUT, "Otwórz stronę wydania").show(ui).clicked() {
                    ctx.open_url(egui::OpenUrl::new_tab(release.page.clone()));
                }
            }
        });
    }

    pub(crate) fn toast(&mut self, ctx: &egui::Context) {
        let Some((msg, at)) = &self.error else { return };
        if at.elapsed() > Duration::from_secs(7) {
            self.error = None;
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(500));
        let msg = msg.clone();
        let mut close = false;
        egui::Area::new(egui::Id::unique("toast"))
            .order(egui::Order::Tooltip)
            .anchor(Align2::CENTER_BOTTOM, Vec2::new(0.0, -24.0))
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(c::BG_RAISED)
                    .stroke(Stroke::new(1.0, theme::alpha(c::DANGER, 0.5)))
                    .corner_radius(r::CONTROL)
                    .shadow(theme::shadow_popup())
                    .inner_margin(Margin { left: 12, right: 6, top: 8, bottom: 8 })
                    .show(ui, |ui| {
                        ui.set_max_width(520.0);
                        ui.horizontal(|ui| {
                            ui.label(w::icon(ic::WARNING_CIRCLE, 20.0, c::DANGER));
                            ui.add(egui::Label::new(w::text(&msg, ty::SMALL_MEDIUM, c::TEXT)).wrap());
                            if IconButton::new(ic::X, "Zamknij").size(26.0).show(ui).clicked() {
                                close = true;
                            }
                        });
                    });
            });
        if close {
            self.error = None;
        }
    }
}
