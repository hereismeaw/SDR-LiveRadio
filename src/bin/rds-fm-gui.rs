//! Native desktop GUI (egui/eframe): FM/AM radio with RDS, traffic, EQ and recording.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, FontData, FontFamily, FontId, Layout, Pos2, Rect, RichText, Sense,
    Stroke,
};
use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};
use egui_extras::{Column, TableBuilder};
use rds_fm::app::{App, Band, Demod, Mode};
use rds_fm::audio::{output_devices, EQ_FREQS, EQ_PRESETS};
use rds_fm::demod::StereoMode;
use rds_fm::device::{DriverPaths, Kind};
use rds_fm::record::Format;
use rds_fm::station::{oda_name, pi_info, rtplus_name, Station, PTY_NAMES};
use rds_fm::text;
use rds_fm::tmc::{dp_text, load_location_names, BANGKOK_STATIONS};
use rds_fm::worker::{self, WorkerCfg};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DEFAULT_IMAGES: &str = r"C:\Program Files\UHD\share\uhd\images";

const GOOD: Color32 = Color32::from_rgb(46, 160, 67);
const WARN: Color32 = Color32::from_rgb(230, 140, 20);
const BAD: Color32 = Color32::from_rgb(220, 60, 60);

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Station,
    RdsPlus,
    Traffic,
    Audio,
    Record,
    Stream,
    Presets,
    Logbook,
}

/// Every UI change is collected here and applied under one lock after drawing.
type Edit = Box<dyn FnOnce(&mut App)>;

struct Gui {
    app: Arc<Mutex<App>>,
    selected: Option<u32>,
    freq_ui: f64, // Hz
    only_rds: bool,
    tab: Tab,
    audio_devices: Vec<String>,
    tmc_csv: String,
    tmc_location_csv: String,
    tmc_location_error: Option<String>,
    tmc_job: Arc<Mutex<Option<String>>>,
    vu: [f32; 2],
    last_frame: Instant,
    // stream tab edit buffers (applied with a button, so typing does not restart streams)
    bind_buf: String,
    ice_buf: rds_fm::broadcast::IcecastCfg,
    // presets / logbook
    preset_csv: String,
    log_filter: String,
    log_kind: String,
    /// Last frequency sent while dragging (live retune is throttled to raster changes).
    drag_sent: Option<f64>,
    scroll_acc: f32,
}

fn fmt_db(v: f32, unit: &str) -> String {
    if v.is_finite() {
        format!("{v:.1} {unit}")
    } else {
        "–".into()
    }
}

fn fmt_age(secs: f32) -> String {
    if secs < 60.0 {
        format!("{secs:.0} s")
    } else if secs < 3600.0 {
        format!("{:.0} min", secs / 60.0)
    } else {
        format!("{:.0} h", secs / 3600.0)
    }
}

fn fmt_freq(hz: f64) -> String {
    if hz < 30e6 {
        format!("{:.0} kHz", hz / 1e3)
    } else if Band::of_freq(hz) == Band::Fm {
        format!("{:.2} MHz", hz / 1e6)
    } else {
        format!("{:.3} MHz", hz / 1e6)
    }
}

fn bar(ui: &mut egui::Ui, frac: f32, w: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(w, 8.0), Sense::hover());
    ui.painter().rect_filled(rect, 3.0, ui.visuals().faint_bg_color.gamma_multiply(2.5));
    let fill = Rect::from_min_size(rect.min, vec2(rect.width() * frac.clamp(0.0, 1.0), rect.height()));
    ui.painter().rect_filled(fill, 3.0, color);
}

fn level_frac(db: f32) -> f32 {
    if db.is_finite() {
        (db + 90.0) / 80.0
    } else {
        0.0
    }
}

fn kv(ui: &mut egui::Ui, k: &str, v: impl Into<String>) {
    ui.label(RichText::new(k).weak());
    ui.add(egui::Label::new(v.into()).wrap());
    ui.end_row();
}

fn kv_mono(ui: &mut egui::Ui, k: &str, v: impl Into<String>) {
    ui.label(RichText::new(k).weak());
    ui.add(egui::Label::new(RichText::new(v.into()).monospace().small()).wrap());
    ui.end_row();
}

impl Gui {
    fn new(app: Arc<Mutex<App>>) -> Self {
        let (freq_ui, selected, bind, ice) = {
            let a = app.lock().unwrap();
            let (f, s) = match a.mode {
                Mode::Listen { hz, .. } => (hz as f64, Some((hz / 1000) as u32)),
                _ => (a.freq_of(a.band), None),
            };
            (f, s, a.stream.bind.clone(), a.stream.icecast.clone())
        };
        let docs = std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default().join("Documents");
        Self {
            app,
            selected,
            freq_ui,
            only_rds: true,
            tab: Tab::Station,
            audio_devices: output_devices(),
            tmc_csv: rds_fm::tmc::default_event_csv().display().to_string(),
            tmc_location_csv: App::data_dir().join("tmc_locations.csv").display().to_string(),
            tmc_location_error: None,
            tmc_job: Arc::new(Mutex::new(None)),
            vu: [0.0; 2],
            last_frame: Instant::now(),
            bind_buf: bind,
            ice_buf: ice,
            preset_csv: docs.join("rds-fm-presets.csv").display().to_string(),
            log_filter: String::new(),
            log_kind: "all".into(),
            drag_sent: None,
            scroll_acc: 0.0,
        }
    }

    /// Drag-to-tune: follow the pointer on the band raster. While dragging inside the
    /// current capture the worker moves the channel in place (instant); a frequency
    /// outside it is tuned when the mouse is released.
    fn drag_tune(&mut self, a: &App, f: f64, released: bool, ed: &mut Vec<Edit>) {
        let band = Band::of_freq(f);
        let (lo, hi) = band.range();
        let step = band.step();
        let snapped = ((f / step).round() * step).clamp(lo, hi);
        self.freq_ui = snapped;
        self.selected = Some((snapped / 1e3).round() as u32);
        let current = match a.mode {
            Mode::Listen { hz, .. } => Some(hz as f64),
            _ => None,
        };
        let inside = a
            .capture_window
            .is_some_and(|(wl, wh)| current.is_some() && snapped > wl + 150e3 && snapped < wh - 150e3 && Band::of_freq(current.unwrap()) == band);
        let auto_rx = a.rx_auto && band == Band::Fm && a.selected_device().is_some_and(|d| d.kind == Kind::Uhd);
        let send = if released {
            current.map(|c| (c - snapped).abs() > 1.0).unwrap_or(true)
        } else {
            !auto_rx && inside && self.drag_sent != Some(snapped) && current.map(|c| (c - snapped).abs() > 1.0).unwrap_or(false)
        };
        if send {
            self.drag_sent = Some(snapped);
            ed.push(Box::new(move |a| {
                // in-place retune keeps the saved settings untouched until release
                let dirty = a.settings_dirty;
                a.listen(snapped);
                if !released {
                    a.settings_dirty = dirty;
                }
            }));
        }
        if released {
            self.drag_sent = None;
        }
    }

    fn tuning_dial(&mut self, ui: &mut egui::Ui, a: &App, ed: &mut Vec<Edit>) {
        let band = a.band;
        let (lo, hi) = band.range();
        let (resp, painter) = ui.allocate_painter(vec2(ui.available_width(), 46.0), Sense::click_and_drag());
        let rect = resp.rect;
        let v = ui.visuals().clone();
        painter.rect_filled(rect, 6.0, v.extreme_bg_color);
        let scale = Rect::from_min_max(pos2(rect.left() + 14.0, rect.top()), pos2(rect.right() - 14.0, rect.bottom()));
        let x_of = |f: f64| scale.left() + ((f - lo) / (hi - lo)) as f32 * scale.width();
        let f_of = |x: f32| lo + ((x - scale.left()) / scale.width()).clamp(0.0, 1.0) as f64 * (hi - lo);
        let weak = v.weak_text_color();
        let tick = v.widgets.noninteractive.fg_stroke.color.gamma_multiply(0.6);
        // scale ticks
        let span = hi - lo;
        let major = [1e3, 10e3, 50e3, 100e3, 500e3, 1e6, 2e6, 5e6].into_iter().find(|s| span / s <= 14.0).unwrap_or(5e6);
        let minor = major / 5.0;
        let small = FontId::proportional(10.5);
        let mut t = (lo / minor).ceil() * minor;
        while t <= hi + 1.0 {
            let x = x_of(t);
            let is_major = ((t / major).round() * major - t).abs() < 1.0;
            let h = if is_major { 12.0 } else { 6.0 };
            painter.line_segment([pos2(x, rect.bottom() - 4.0 - h), pos2(x, rect.bottom() - 4.0)], Stroke::new(1.0, tick));
            if is_major {
                let label = if hi < 30e6 { format!("{:.0}", t / 1e3) } else { format!("{:.0}", t / 1e6) };
                painter.text(pos2(x, rect.bottom() - 18.0), Align2::CENTER_BOTTOM, label, small.clone(), weak);
            }
            t += minor;
        }
        // presets and RDS stations
        for p in a.presets.iter().filter(|p| p.freq_hz >= lo && p.freq_hz <= hi) {
            painter.text(pos2(x_of(p.freq_hz), rect.top() + 2.0), Align2::CENTER_TOP, "★", FontId::proportional(11.0), WARN);
        }
        if band == Band::Fm {
            for s in a.stations.values().filter(|s| s.has_rds()) {
                let x = x_of(s.freq_khz as f64 * 1e3);
                painter.circle_filled(pos2(x, rect.top() + 18.0), 2.5, GOOD);
            }
        }
        // needle
        let nx = x_of(self.freq_ui.clamp(lo, hi));
        painter.line_segment([pos2(nx, rect.top() + 2.0), pos2(nx, rect.bottom() - 2.0)], Stroke::new(2.5, BAD));
        let txt = fmt_freq(self.freq_ui);
        let galley = painter.layout_no_wrap(txt.clone(), FontId::monospace(12.0), Color32::WHITE);
        let w = galley.size().x + 10.0;
        let bx = (nx - w / 2.0).clamp(rect.left() + 2.0, rect.right() - w - 2.0);
        let r = Rect::from_min_size(pos2(bx, rect.top() + 2.0), vec2(w, 16.0));
        painter.rect_filled(r, 3.0, BAD);
        painter.text(r.center(), Align2::CENTER_CENTER, txt, FontId::monospace(12.0), Color32::WHITE);

        let resp = resp
            .on_hover_cursor(egui::CursorIcon::ResizeHorizontal)
            .on_hover_text_at_pointer("Drag to tune · scroll wheel: one channel step");
        if let Some(p) = resp.interact_pointer_pos() {
            if resp.dragged() || resp.clicked() {
                self.drag_tune(a, f_of(p.x), resp.clicked(), ed);
            }
        }
        if resp.drag_stopped() {
            self.drag_tune(a, self.freq_ui, true, ed);
        }
        if resp.hovered() {
            self.scroll_acc += ui.input(|i| i.smooth_scroll_delta.y + i.smooth_scroll_delta.x);
            let steps = (self.scroll_acc / 40.0).trunc();
            if steps != 0.0 {
                self.scroll_acc -= steps * 40.0;
                let f = (self.freq_ui + steps as f64 * band.step()).clamp(lo, hi);
                self.drag_tune(a, f, true, ed);
            }
        }
    }

    // ------------------------------------------------------------------ stream tab

    fn stream_tab(&mut self, ui: &mut egui::Ui, a: &App, ed: &mut Vec<Edit>) {
        let st = a.stream.clone();
        ui.label(RichText::new("Internet radio").strong());
        let mut on = st.server;
        if ui.checkbox(&mut on, "Built-in stream server (Icecast-compatible, MP3)").changed() {
            ed.push(Box::new(move |a| {
                a.stream.server = on;
                a.settings_dirty = true;
            }));
        }
        ui.horizontal(|ui| {
            ui.label("Listen on");
            ui.add(egui::TextEdit::singleline(&mut self.bind_buf).desired_width(150.0));
            if ui.add_enabled(self.bind_buf != st.bind, egui::Button::new("Apply")).clicked() {
                let b = self.bind_buf.clone();
                ed.push(Box::new(move |a| {
                    a.stream.bind = b;
                    a.settings_dirty = true;
                }));
            }
        });
        let ok = a.stream_status.starts_with("on air");
        ui.colored_label(if ok { GOOD } else { ui.visuals().weak_text_color() }, format!("Server: {}", a.stream_status));
        let base = a.stream_status.strip_prefix("on air at ").map(|s| s.trim_end_matches('/').to_string());
        if let Some(b) = &base {
            ui.horizontal(|ui| {
                if ui.button("🌐 Open player page").clicked() {
                    let _ = std::process::Command::new("cmd").args(["/C", "start", "", b]).spawn();
                }
                if ui.button("📋 Copy URL").clicked() {
                    ui.ctx().copy_text(format!("{b}/"));
                }
            });
        }
        ui.horizontal(|ui| {
            ui.label("Bitrate");
            let mut k = st.kbps;
            for v in [64, 96, 128, 192, 256, 320] {
                ui.radio_value(&mut k, v, v.to_string());
            }
            ui.label("kbps");
            if k != st.kbps {
                ed.push(Box::new(move |a| {
                    a.stream.kbps = k;
                    a.settings_dirty = true;
                }));
            }
        });
        let mut sm = st.stream_main;
        if ui.checkbox(&mut sm, "Stream the station I am listening to").changed() {
            ed.push(Box::new(move |a| {
                a.stream.stream_main = sm;
                a.settings_dirty = true;
            }));
        }
        ui.separator();
        ui.label(RichText::new("More stations at the same time (FM)").strong());
        ui.label(
            RichText::new(
                "Extra stations are demodulated from the same capture, so they must lie within the \
                 device bandwidth of the station you listen to: about ±1.8 MHz on the B210, ±0.85 MHz on the RTL-SDR.",
            )
            .small()
            .weak(),
        );
        let mut del = None;
        egui::Grid::new("streams").num_columns(4).striped(true).show(ui, |ui| {
            for (i, s) in a.streams.iter().enumerate() {
                let mut en = s.enabled;
                if ui.checkbox(&mut en, "").changed() {
                    ed.push(Box::new(move |a| {
                        a.streams[i].enabled = en;
                        a.settings_dirty = true;
                    }));
                }
                ui.monospace(fmt_freq(s.freq_hz));
                let key = a.station_key((s.freq_hz / 1e3).round() as u32);
                let ps = a.stations.get(&key).map(|x| x.ps_display().0).unwrap_or_default();
                ui.monospace(ps);
                if ui.small_button("✕").clicked() {
                    del = Some(i);
                }
                ui.end_row();
            }
        });
        if let Some(i) = del {
            ed.push(Box::new(move |a| {
                a.streams.remove(i);
                a.settings_dirty = true;
            }));
        }
        ui.horizontal(|ui| {
            let f = self.selected.map(|k| k as f64 * 1e3).unwrap_or(self.freq_ui);
            if ui.button(format!("➕ Add {}", fmt_freq(f))).clicked() && !a.streams.iter().any(|s| (s.freq_hz - f).abs() < 1.0) {
                ed.push(Box::new(move |a| {
                    a.streams.push(rds_fm::app::StreamCfg { freq_hz: f, enabled: true });
                    a.streams.sort_by(|x, y| x.freq_hz.total_cmp(&y.freq_hz));
                    a.settings_dirty = true;
                }));
            }
            if let Mode::Listen { hz, .. } = a.mode {
                let playing = a.station_key((hz as f64 / 1e3).round() as u32);
                let near: Vec<f64> = a
                    .stations
                    .values()
                    .filter(|s| s.has_rds() && (s.freq_khz as f64 * 1e3 - hz as f64).abs() < 1.75e6
                        && s.freq_khz != playing && s.freq_khz as u64 * 1000 != hz)
                    .map(|s| s.freq_khz as f64 * 1e3)
                    .collect();
                if !near.is_empty() && ui.button(format!("➕ Add {} RDS stations nearby", near.len())).clicked() {
                    ed.push(Box::new(move |a| {
                        for f in near {
                            if !a.streams.iter().any(|s| (s.freq_hz - f).abs() < 1.0) {
                                a.streams.push(rds_fm::app::StreamCfg { freq_hz: f, enabled: true });
                            }
                        }
                        a.streams.sort_by(|x, y| x.freq_hz.total_cmp(&y.freq_hz));
                        a.settings_dirty = true;
                    }));
                }
            }
        });
        if !a.live.streams.is_empty() {
            ui.separator();
            ui.label(RichText::new("On air").strong());
            let info = a.hub.list();
            egui::Grid::new("onair").num_columns(3).striped(true).show(ui, |ui| {
                for s in &a.live.streams {
                    ui.monospace(fmt_freq(s.freq_hz));
                    if s.on_air {
                        let m = info.iter().find(|m| m.mount == s.mount);
                        let url = base.as_ref().map(|b| format!("{b}/{}", s.mount)).unwrap_or(format!("/{}", s.mount));
                        ui.hyperlink_to(url.clone(), url);
                        ui.label(m.map(|m| format!("{} listening · {}", m.listeners, m.title)).unwrap_or_default());
                    } else {
                        ui.colored_label(WARN, "not streamed");
                        ui.label(RichText::new(&s.note).small());
                    }
                    ui.end_row();
                }
            });
        } else if a.stream.server {
            ui.weak("Streams go on air while you listen (▶ Listen).");
        }
        ui.separator();
        ui.label(RichText::new("Push to an Icecast server").strong());
        egui::Grid::new("ice").num_columns(2).show(ui, |ui| {
            ui.label("Enabled");
            ui.checkbox(&mut self.ice_buf.enabled, "");
            ui.end_row();
            ui.label("Host");
            ui.text_edit_singleline(&mut self.ice_buf.host);
            ui.end_row();
            ui.label("Port");
            ui.add(egui::DragValue::new(&mut self.ice_buf.port).range(1..=65535));
            ui.end_row();
            ui.label("User");
            ui.text_edit_singleline(&mut self.ice_buf.user);
            ui.end_row();
            ui.label("Password");
            ui.add(egui::TextEdit::singleline(&mut self.ice_buf.password).password(true));
            ui.end_row();
        });
        if ui.add_enabled(self.ice_buf != st.icecast, egui::Button::new("Apply Icecast settings")).clicked() {
            let c = self.ice_buf.clone();
            ed.push(Box::new(move |a| {
                a.stream.icecast = c;
                a.settings_dirty = true;
            }));
        }
        ui.label(
            RichText::new(
                "Mounts are /fm9700 for 97.00 MHz, /am891 for 891 kHz. Listeners outside your network need \
                 port forwarding on your router (or use an Icecast server). The password is stored in \
                 %APPDATA%\\rds-fm\\settings.json.",
            )
            .small()
            .weak(),
        );
    }

    // ------------------------------------------------------------------ presets

    fn presets_tab(&mut self, ui: &mut egui::Ui, a: &App, ed: &mut Vec<Edit>) {
        ui.horizontal(|ui| {
            let f = self.selected.map(|k| k as f64 * 1e3).unwrap_or(self.freq_ui);
            let name = a.stations.get(&a.station_key((f / 1e3).round() as u32)).map(|s| s.ps_display().0.trim().to_string()).unwrap_or_default();
            if ui.button(format!("★ Save {}", fmt_freq(f))).clicked() {
                ed.push(Box::new(move |a| a.add_preset(&name, f)));
            }
            let rds: Vec<(String, f64)> = a
                .stations
                .values()
                .filter(|s| s.ps.is_some())
                .map(|s| (s.ps_display().0.trim().to_string(), s.freq_khz as f64 * 1e3))
                .collect();
            if !rds.is_empty() && ui.button(format!("★ Save all {} RDS stations", rds.len())).clicked() {
                ed.push(Box::new(move |a| {
                    for (n, f) in rds {
                        a.add_preset(&n, f);
                    }
                }));
            }
        });
        ui.add_space(4.0);
        if a.presets.is_empty() {
            ui.weak("No saved stations yet.");
        }
        let mut del = None;
        egui::Grid::new("presets").num_columns(5).striped(true).show(ui, |ui| {
            for (i, p) in a.presets.iter().enumerate() {
                let playing = matches!(a.mode, Mode::Listen { hz, .. } if (hz as f64 - p.freq_hz).abs() < 1.0);
                let f = p.freq_hz;
                if ui.add(egui::Button::new(if playing { "■" } else { "▶" }).small()).clicked() {
                    if playing {
                        ed.push(Box::new(|a| a.mode = Mode::Idle));
                    } else {
                        self.freq_ui = f;
                        self.selected = Some((f / 1e3).round() as u32);
                        ed.push(Box::new(move |a| a.listen(f)));
                    }
                }
                ui.label(RichText::new(fmt_freq(p.freq_hz)).monospace().color(if playing { GOOD } else { ui.visuals().text_color() }));
                let mut name = p.name.clone();
                if ui.add(egui::TextEdit::singleline(&mut name).desired_width(120.0)).changed() {
                    ed.push(Box::new(move |a| {
                        a.presets[i].name = name;
                        a.settings_dirty = true;
                    }));
                }
                let mut notes = p.notes.clone();
                if ui.add(egui::TextEdit::singleline(&mut notes).desired_width(140.0).hint_text("notes")).changed() {
                    ed.push(Box::new(move |a| {
                        a.presets[i].notes = notes;
                        a.settings_dirty = true;
                    }));
                }
                if ui.small_button("✕").clicked() {
                    del = Some(i);
                }
                ui.end_row();
            }
        });
        if let Some(i) = del {
            ed.push(Box::new(move |a| {
                a.presets.remove(i);
                a.settings_dirty = true;
            }));
        }
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("CSV");
            ui.add(egui::TextEdit::singleline(&mut self.preset_csv).desired_width(240.0));
        });
        ui.horizontal(|ui| {
            if ui.button("Export").clicked() {
                let mut s = String::from("\u{feff}name,frequency_mhz,notes\n");
                for p in &a.presets {
                    s += &format!("\"{}\",{:.4},\"{}\"\n", p.name.replace('"', "\"\""), p.freq_hz / 1e6, p.notes.replace('"', "\"\""));
                }
                let path = self.preset_csv.clone();
                let r = std::fs::write(&path, s);
                ed.push(Box::new(move |a| a.push_log(match r {
                    Ok(()) => format!("Presets exported to {path}"),
                    Err(e) => format!("Export failed: {e}"),
                })));
            }
            if ui.button("Import").clicked() {
                let path = self.preset_csv.clone();
                ed.push(Box::new(move |a| match std::fs::read_to_string(&path) {
                    Ok(s) => {
                        let mut n = 0;
                        for line in s.lines().skip(1) {
                            let cols: Vec<String> = line.split(',').map(|c| c.trim().trim_matches('"').to_string()).collect();
                            if let (Some(name), Some(f)) = (cols.first(), cols.get(1).and_then(|f| f.parse::<f64>().ok())) {
                                a.add_preset(name.trim_start_matches('\u{feff}'), f * 1e6);
                                n += 1;
                            }
                        }
                        a.push_log(format!("Imported {n} presets from {path}"));
                    }
                    Err(e) => a.push_log(format!("Import failed: {e}")),
                }));
            }
        });
    }

    // ------------------------------------------------------------------ logbook

    fn logbook_tab(&mut self, ui: &mut egui::Ui, a: &App, ed: &mut Vec<Edit>) {
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.log_filter).hint_text("search…").desired_width(150.0));
            egui::ComboBox::from_id_salt("logkind").selected_text(self.log_kind.clone()).width(80.0).show_ui(ui, |ui| {
                for k in ["all", "station", "ps", "rt", "rt+", "ta", "eon", "tmc", "oda", "record"] {
                    ui.selectable_value(&mut self.log_kind, k.to_string(), k);
                }
            });
            if ui.button("📂 CSV").on_hover_text("Open the logbook file").clicked() {
                let _ = std::process::Command::new("explorer").arg(format!("/select,{}", App::logbook_path().display())).spawn();
            }
            if ui.button("Clear view").clicked() {
                ed.push(Box::new(|a| a.logbook.clear()));
            }
        });
        let f = self.log_filter.to_lowercase();
        let rows: Vec<&rds_fm::app::LogEntry> = a
            .logbook
            .iter()
            .rev()
            .filter(|e| self.log_kind == "all" || e.kind == self.log_kind)
            .filter(|e| f.is_empty() || e.text.to_lowercase().contains(&f) || format!("{:.2}", e.freq_hz / 1e6).contains(&f))
            .take(600)
            .collect();
        ui.label(RichText::new(format!("{} entries (file: {})", a.logbook.len(), App::logbook_path().display())).small().weak());
        egui::Grid::new("logbook").num_columns(4).striped(true).show(ui, |ui| {
            for e in rows {
                ui.label(RichText::new(&e.time[5..]).monospace().small());
                ui.label(RichText::new(fmt_freq(e.freq_hz)).monospace().small());
                ui.label(RichText::new(&e.kind).small().strong());
                ui.add(egui::Label::new(RichText::new(&e.text).small()).wrap());
                ui.end_row();
            }
        });
    }

    // ------------------------------------------------------------------ top bar

    fn top_bar(&mut self, ui: &mut egui::Ui, a: &App, ed: &mut Vec<Edit>) {
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("RDS FM Monitor").strong().size(17.0));
            ui.add_space(6.0);
            let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
            ui.painter().circle_filled(r.center(), 4.5, if a.error.is_some() { BAD } else { GOOD });
            ui.label(&a.status);
            if let Some(d) = &a.live.dual_rx {
                ui.label(format!("Using RX {}", d.selected)).on_hover_ui(|ui| {
                    for i in 0..2 {
                        ui.label(format!("RX {i} · {:.1} dBFS · {:.1} dB pilot", d.level_db[i], d.pilot_db[i]));
                    }
                });
            }
            if a.clip > 0.001 {
                ui.colored_label(WARN, format!("⚠ clip {:.2}% — lower the gain", a.clip * 100.0));
            }
            if let Some(r) = &a.rec_status {
                let s = r.seconds as u64;
                ui.colored_label(BAD, RichText::new(format!("● REC {:02}:{:02}", s / 60, s % 60)).strong());
            }
            let on_air = a.live.streams.iter().filter(|s| s.on_air).count();
            if on_air > 0 {
                let listeners: usize = a.hub.list().iter().map(|m| m.listeners).sum();
                ui.colored_label(GOOD, RichText::new(format!("📡 ON AIR {on_air} · {listeners} listening")).strong());
            }
        });
        // traffic announcement banner
        if a.traffic_alert {
            let ta: Vec<String> = a
                .stations
                .values()
                .filter(|s| s.ta_active() && s.last_seen.elapsed() < Duration::from_secs(60))
                .map(|s| format!("{} {}", fmt_freq(s.freq_khz as f64 * 1e3), s.ps_display().0.trim()))
                .collect();
            if !ta.is_empty() {
                egui::Frame::new().fill(WARN).corner_radius(4.0).inner_margin(6.0).show(ui, |ui| {
                    ui.label(RichText::new(format!("🚦 TRAFFIC ANNOUNCEMENT — {}", ta.join(" · "))).strong().color(Color32::BLACK));
                });
            }
        }
        ui.add_space(2.0);
        ui.horizontal_wrapped(|ui| {
            // device
            let cur = a.selected_device().map(|d| d.label.clone()).unwrap_or_else(|| "(searching…)".into());
            let mut pick: Option<String> = None;
            egui::ComboBox::from_id_salt("device").width(250.0).selected_text(cur).show_ui(ui, |ui| {
                for d in &a.devices {
                    let label = if d.present { d.label.clone() } else { format!("○ {}", d.label) };
                    if ui.selectable_label(d.id == a.device_id, label).clicked() {
                        pick = Some(d.id.clone());
                    }
                }
            });
            if let Some(id) = pick {
                ed.push(Box::new(move |a| {
                    a.device_id = id;
                    a.settings_dirty = true;
                }));
            }
            if ui.button("⟳").on_hover_text("Search for devices again").clicked() {
                ed.push(Box::new(|a| a.rescan_devices = true));
            }
            ui.separator();
            // band
            for b in Band::ALL {
                if ui.selectable_label(a.band == b, b.name()).clicked() && a.band != b {
                    let listening = matches!(a.mode, Mode::Listen { .. });
                    let f = a.freq_of(b);
                    self.freq_ui = f;
                    ed.push(Box::new(move |a| {
                        a.band = b;
                        a.settings_dirty = true;
                        if listening || b != Band::Fm {
                            if listening {
                                a.listen(f);
                            } else if a.mode == Mode::Scan {
                                a.mode = Mode::Idle;
                            }
                        }
                    }));
                }
            }
            ui.separator();
            // frequency
            let band = a.band;
            let (lo, hi) = band.range();
            let step = band.step();
            let listening = matches!(a.mode, Mode::Listen { .. });
            if !matches!(a.mode, Mode::Listen { .. }) || Band::of_freq(self.freq_ui) != band {
                if Band::of_freq(self.freq_ui) != band {
                    self.freq_ui = a.freq_of(band);
                }
            }
            let mut newf = None;
            if ui.button("◀").on_hover_text(format!("−{}", fmt_freq(step))).clicked() {
                newf = Some((self.freq_ui - step).max(lo));
            }
            let (mut v, unit, speed, dec) = if band == Band::Fm || band == Band::Air {
                (self.freq_ui / 1e6, " MHz", step / 1e6 / 4.0, if band == Band::Fm { 2 } else { 3 })
            } else {
                (self.freq_ui / 1e3, " kHz", step / 1e3 / 4.0, 0)
            };
            let scale = if unit == " MHz" { 1e6 } else { 1e3 };
            let r = ui.add(
                egui::DragValue::new(&mut v)
                    .range(lo / scale..=hi / scale)
                    .speed(speed)
                    .fixed_decimals(dec)
                    .suffix(unit),
            );
            if r.changed() {
                self.freq_ui = v * scale;
            }
            if r.drag_stopped() || r.lost_focus() {
                // snap to the band raster
                let snapped = ((self.freq_ui / step).round() * step).clamp(lo, hi);
                self.freq_ui = snapped;
                if listening {
                    newf = Some(snapped);
                }
            }
            if ui.button("▶").on_hover_text(format!("+{}", fmt_freq(step))).clicked() {
                newf = Some((self.freq_ui + step).min(hi));
            }
            if let Some(f) = newf {
                self.freq_ui = f;
                if listening {
                    ed.push(Box::new(move |a| a.listen(f)));
                }
            }
            let f = self.freq_ui;
            let playing_this = matches!(a.mode, Mode::Listen { hz, .. } if (hz as f64 - f).abs() < 1.0);
            if ui.add(egui::Button::new(RichText::new("▶ Listen").strong()).selected(playing_this)).clicked() {
                ed.push(Box::new(move |a| a.listen(f)));
                self.selected = Some((f / 1e3).round() as u32);
            }
            if ui.add_enabled(band == Band::Fm, egui::Button::new("📡 Scan FM").selected(a.mode == Mode::Scan)).clicked() {
                ed.push(Box::new(|a| {
                    a.band = Band::Fm;
                    a.mode = Mode::Scan;
                    a.push_log("Scanning the FM band");
                }));
            }
            if ui.add_enabled(a.mode != Mode::Idle, egui::Button::new("■ Stop")).clicked() {
                ed.push(Box::new(|a| a.mode = Mode::Idle));
            }
            ui.separator();
            // gain
            let kind = a.selected_device().map(|d| d.kind).unwrap_or(Kind::Uhd);
            let gmax = if kind == Kind::RtlSdr { 49.6 } else { 76.0 };
            let mut g = a.gain.min(gmax);
            ui.label("Gain");
            let r = ui.add_enabled(!a.agc, egui::Slider::new(&mut g, 0.0..=gmax).step_by(0.5).suffix(" dB"));
            if r.changed() {
                ed.push(Box::new(move |a| {
                    a.gain = g;
                    a.settings_dirty = true;
                }));
            }
            if kind == Kind::RtlSdr {
                let mut agc = a.agc;
                if ui.checkbox(&mut agc, "AGC").changed() {
                    ed.push(Box::new(move |a| {
                        a.agc = agc;
                        a.settings_dirty = true;
                    }));
                }
                let mut bt = a.bias_tee;
                if ui.checkbox(&mut bt, "Bias-T").on_hover_text("4.5 V on the antenna port for an active antenna / LNA").changed() {
                    ed.push(Box::new(move |a| {
                        a.bias_tee = bt;
                        a.settings_dirty = true;
                    }));
                }
            } else {
                let previous = if a.rx_dual { "Dual RX" } else if a.rx_auto { "Auto RX" } else { "Manual RX" };
                let mut reception = previous;
                egui::ComboBox::from_id_salt("rx_mode").selected_text(reception).width(82.0).show_ui(ui, |ui| {
                    ui.selectable_value(&mut reception, "Manual RX", "Manual RX");
                    ui.selectable_value(&mut reception, "Auto RX", "Auto RX · compare all ports");
                    ui.selectable_value(&mut reception, "Dual RX", "Dual RX · two channels at once");
                });
                if reception != previous {
                    ed.push(Box::new(move |a| {
                        a.rx_auto = reception == "Auto RX";
                        a.rx_dual = reception == "Dual RX";
                        a.settings_dirty = true;
                    }));
                }
                let dual = a.rx_dual && a.band == Band::Fm;
                let automatic = !dual && a.rx_auto && matches!(a.mode, Mode::Listen { demod: Demod::Wfm, .. });
                if !automatic {
                    if !dual {
                        let mut rx = a.rx_channel;
                        egui::ComboBox::from_id_salt("rx_channel").selected_text(format!("RX {rx}")).width(55.0).show_ui(ui, |ui| {
                            ui.selectable_value(&mut rx, 0, "RX 0");
                            ui.selectable_value(&mut rx, 1, "RX 1");
                        }).response.on_hover_text("Select the UHD receive channel. RX 1 requires a dual-channel device.");
                        if rx != a.rx_channel {
                            ed.push(Box::new(move |a| {
                                a.rx_channel = rx;
                                a.settings_dirty = true;
                            }));
                        }
                    }
                    let mut ant = a.antenna.clone();
                    let label = if dual { format!("RX 0: {ant}") } else { ant.clone() };
                    egui::ComboBox::from_id_salt("antenna").selected_text(label).width(70.0).show_ui(ui, |ui| {
                        ui.selectable_value(&mut ant, "TX/RX".to_string(), "TX/RX");
                        ui.selectable_value(&mut ant, "RX2".to_string(), "RX2");
                    });
                    if ant != a.antenna {
                        ed.push(Box::new(move |a| {
                            a.antenna = ant;
                            a.settings_dirty = true;
                        }));
                    }
                }
                if dual {
                    let mut ant = a.rx1_antenna.clone();
                    egui::ComboBox::from_id_salt("rx1_antenna").selected_text(format!("RX 1: {ant}")).show_ui(ui, |ui| {
                        ui.selectable_value(&mut ant, "TX/RX".to_string(), "TX/RX");
                        ui.selectable_value(&mut ant, "RX2".to_string(), "RX2");
                    });
                    if ant != a.rx1_antenna {
                        ed.push(Box::new(move |a| {
                            a.rx1_antenna = ant;
                            a.settings_dirty = true;
                        }));
                    }
                }
                if let Some(survey) = &a.rx_survey {
                    let selected = &survey.measurements[survey.selected];
                    let mut details = vec!["19 kHz pilot SNR · RF contrast".to_owned()];
                    details.extend(survey.measurements.iter().map(|m| format!(
                        "RX {} · {}    {:.1} dB · {:.1} dB{}",
                        m.channel, m.antenna, m.pilot_db, m.contrast_db, if m.clipped { " (clipped)" } else { "" },
                    )));
                    ui.label(format!("RX {} · {}", selected.channel, selected.antenna)).on_hover_text(details.join("\n"));
                    if ui.add_enabled(a.rec_status.is_none(), egui::Button::new("Recheck")).clicked() {
                        ed.push(Box::new(|a| a.rx_recheck = true));
                    }
                }
            }
        });
        if let Some(err) = &a.error {
            ui.colored_label(BAD, err);
        }
        ui.add_space(4.0);
    }

    // ------------------------------------------------------------------ now playing

    fn now_playing(&mut self, ui: &mut egui::Ui, a: &App) {
        let Mode::Listen { hz, demod } = a.mode else { return };
        let st = a.stations.get(&a.station_key((hz as f64 / 1e3).round() as u32));
        let dt = self.last_frame.elapsed().as_secs_f32();
        for i in 0..2 {
            let p = a.live.peak[i];
            self.vu[i] = if p > self.vu[i] { p } else { (self.vu[i] - dt * 0.8).max(p) };
        }
        egui::Frame::group(ui.style()).inner_margin(8.0).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new(fmt_freq(hz as f64)).size(28.0).strong().monospace());
                    ui.horizontal(|ui| {
                        let (txt, col) = match demod {
                            Demod::Am => ("AM", ui.visuals().weak_text_color()),
                            Demod::Wfm if a.live.stereo_blend => ("STEREO", GOOD),
                            Demod::Wfm if a.live.pilot_locked => ("MONO (forced)", WARN),
                            Demod::Wfm => ("MONO", ui.visuals().weak_text_color()),
                        };
                        ui.label(RichText::new(txt).strong().color(col));
                        if st.is_some_and(|s| s.tp == Some(true)) {
                            ui.label(RichText::new("TP").strong());
                        }
                        if st.is_some_and(|s| s.ta_active()) {
                            ui.label(RichText::new("TA").strong().color(WARN));
                        }
                    });
                });
                ui.add_space(16.0);
                ui.vertical(|ui| {
                    if let Some(s) = st {
                        let (ps, full) = s.ps_display();
                        let t = RichText::new(if ps.is_empty() { "—".into() } else { ps }).monospace().size(26.0).strong();
                        ui.label(if full { t } else { t.weak() });
                        let (rt, full) = s.rt_display();
                        if !rt.is_empty() {
                            let t = RichText::new(rt).size(14.0);
                            ui.add(egui::Label::new(if full { t } else { t.weak() }).wrap());
                        }
                        let title = s.rtplus.tags.get(&1);
                        let artist = s.rtplus.tags.get(&4);
                        if title.is_some() || artist.is_some() {
                            ui.label(RichText::new(format!(
                                "♪ {}{}",
                                title.cloned().unwrap_or_default(),
                                artist.map(|x| format!(" — {x}")).unwrap_or_default()
                            )).strong());
                        }
                        if let Some(p) = s.pty {
                            ui.label(RichText::new(PTY_NAMES[p as usize]).weak());
                        }
                    } else if demod == Demod::Wfm {
                        ui.weak("Waiting for RDS…");
                    } else {
                        ui.weak("AM — no RDS on this band");
                    }
                });
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Signal").weak());
                            bar(ui, level_frac(a.live.signal_db), 120.0, ui.visuals().hyperlink_color);
                            ui.label(RichText::new(fmt_db(a.live.signal_db, "dBFS")).small());
                        });
                        for (i, n) in ["L", "R"].iter().enumerate() {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(*n).weak().monospace());
                                let v = self.vu[i];
                                let col = if v > 0.95 { BAD } else if v > 0.7 { WARN } else { GOOD };
                                bar(ui, v, 150.0, col);
                            });
                        }
                        ui.label(RichText::new(format!(
                            "{} · buffer {:.0} ms{}",
                            a.live.audio_device.clone().unwrap_or_else(|| "no audio output".into()),
                            a.live.audio_buffer_s * 1e3,
                            if a.live.dropped_blocks > 0 { format!(" · {} dropped", a.live.dropped_blocks) } else { String::new() }
                        )).small().weak());
                    });
                });
            });
        });
        ui.add_space(6.0);
    }

    // ------------------------------------------------------------------ spectrum

    fn spectrum(&mut self, ui: &mut egui::Ui, a: &App, ed: &mut Vec<Edit>) {
        let (resp, painter) = ui.allocate_painter(ui.available_size(), Sense::click_and_drag());
        let rect = resp.rect;
        let v = ui.visuals().clone();
        let accent = v.selection.bg_fill;
        let weak = v.weak_text_color();
        let grid = v.widgets.noninteractive.bg_stroke.color;
        painter.rect_filled(rect, 4.0, v.extreme_bg_color);
        let plot = Rect::from_min_max(pos2(rect.left() + 44.0, rect.top() + 34.0), pos2(rect.right() - 10.0, rect.bottom() - 20.0));
        if plot.height() < 30.0 {
            return;
        }
        // live (listen) view or band (scan) view
        let live = match (a.mode, &a.live.spectrum) {
            (Mode::Listen { .. }, Some((lo, hi, d))) => Some((*lo, *hi, d)),
            _ => None,
        };
        let (f_lo, f_hi) = match live {
            Some((lo, hi, _)) => (lo, hi),
            None => (87.5e6, 108.0e6),
        };
        let x_of = |f: f64| plot.left() + ((f - f_lo) / (f_hi - f_lo)) as f32 * plot.width();
        let f_of = |x: f32| f_lo + ((x - plot.left()) / plot.width()) as f64 * (f_hi - f_lo);
        let small = FontId::proportional(11.0);

        let pts: Vec<(f64, f32)> = match live {
            Some((lo, hi, d)) => {
                let n = d.len();
                d.iter().enumerate().map(|(i, &db)| (lo + (hi - lo) * (i as f64 + 0.5) / n as f64, db)).collect()
            }
            None => a.spectrum.iter().map(|(&k, &db)| (k as f64 * 1e3, db)).collect(),
        };
        if let (None, Some((lo, hi))) = (live, a.capture_window) {
            let l = x_of(lo).max(plot.left());
            let h = x_of(hi).min(plot.right());
            if h > l {
                painter.rect_filled(Rect::from_min_max(pos2(l, plot.top()), pos2(h, plot.bottom())), 0.0, accent.gamma_multiply(0.12));
            }
        }
        // x grid
        let span = f_hi - f_lo;
        let gstep = [10e3, 25e3, 50e3, 100e3, 250e3, 500e3, 1e6, 2e6].into_iter().find(|s| span / s <= 12.0).unwrap_or(2e6);
        let mut g = (f_lo / gstep).ceil() * gstep;
        while g <= f_hi {
            let x = x_of(g);
            painter.line_segment([pos2(x, plot.top()), pos2(x, plot.bottom())], Stroke::new(0.5, grid));
            let label = if span < 3e6 { fmt_freq(g) } else { format!("{:.0}", g / 1e6) };
            painter.text(pos2(x, plot.bottom() + 3.0), Align2::CENTER_TOP, label, small.clone(), weak);
            g += gstep;
        }
        if pts.is_empty() {
            painter.text(plot.center(), Align2::CENTER_CENTER, "Waiting for spectrum…", FontId::proportional(14.0), weak);
            return;
        }
        let mut vals: Vec<f32> = pts.iter().map(|p| p.1).filter(|v| v.is_finite()).collect();
        vals.sort_by(|x, y| x.total_cmp(y));
        let lo_db = vals[vals.len() / 20] - 4.0;
        let hi_db = (vals[vals.len() - 1] + 4.0).max(lo_db + 25.0);
        let y_of = |db: f32| plot.bottom() - ((db - lo_db) / (hi_db - lo_db)).clamp(0.0, 1.0) * plot.height();
        let dstep = ((hi_db - lo_db) / 4.0 / 5.0).ceil() * 5.0;
        let mut db = (lo_db / dstep).ceil() * dstep;
        while db < hi_db {
            let y = y_of(db);
            painter.line_segment([pos2(plot.left(), y), pos2(plot.right(), y)], Stroke::new(0.5, grid));
            painter.text(pos2(plot.left() - 4.0, y), Align2::RIGHT_CENTER, format!("{db:.0}"), small.clone(), weak);
            db += dstep;
        }
        painter.text(pos2(rect.left() + 4.0, plot.top() - 14.0), Align2::LEFT_CENTER, "dBFS", small.clone(), weak);
        // trace, broken where nothing was captured
        let line = Stroke::new(1.3, accent);
        let gap = if live.is_some() { f64::MAX } else { 60e3 };
        let mut seg: Vec<Pos2> = Vec::new();
        let mut prev: Option<f64> = None;
        for &(f, db) in &pts {
            if prev.is_some_and(|p| f - p > gap) {
                if seg.len() > 1 {
                    painter.line(std::mem::take(&mut seg), line);
                }
                seg.clear();
            }
            seg.push(pos2(x_of(f), y_of(db)));
            prev = Some(f);
        }
        if seg.len() > 1 {
            painter.line(seg, line);
        }
        // tuned channel
        if let Mode::Listen { hz, demod } = a.mode {
            let half = if demod == Demod::Wfm { 100e3 } else { a.audio.am_bw_hz as f64 };
            let (l, r) = (x_of(hz as f64 - half), x_of(hz as f64 + half));
            painter.rect_filled(Rect::from_min_max(pos2(l, plot.top()), pos2(r, plot.bottom())), 0.0, GOOD.gamma_multiply(0.15));
            painter.line_segment([pos2(x_of(hz as f64), plot.top()), pos2(x_of(hz as f64), plot.bottom())], Stroke::new(1.5, GOOD));
        }
        // FM station labels
        let label_font = FontId::proportional(11.5);
        let mut lane_end = [f32::MIN; 2];
        for s in a.stations.values() {
            let f = s.freq_khz as f64 * 1e3;
            if f < f_lo || f > f_hi {
                continue;
            }
            let x = x_of(f);
            let sel = self.selected.map(|k| a.station_key(k)) == Some(s.freq_khz);
            let color = if s.has_rds() { GOOD } else { weak };
            if live.is_none() {
                painter.line_segment([pos2(x, plot.top()), pos2(x, plot.bottom())], Stroke::new(if sel { 2.0 } else { 1.0 }, color.gamma_multiply(0.6)));
            }
            if s.has_rds() || sel {
                let (ps, _) = s.ps_display();
                let label = if ps.trim().is_empty() { format!("{:.1}", f / 1e6) } else { ps.trim().to_string() };
                let w = painter.layout_no_wrap(label.clone(), label_font.clone(), color).size().x;
                let lane = if x - w / 2.0 > lane_end[0] + 4.0 { 0 } else { 1 };
                painter.text(pos2(x, rect.top() + 4.0 + lane as f32 * 14.0), Align2::CENTER_TOP, label, label_font.clone(), if sel { v.strong_text_color() } else { color });
                lane_end[lane] = x + w / 2.0;
            }
        }
        if let Some(p) = resp.hover_pos().filter(|p| plot.contains(*p)) {
            let f = f_of(p.x);
            painter.line_segment([pos2(p.x, plot.top()), pos2(p.x, plot.bottom())], Stroke::new(1.0, v.strong_text_color().gamma_multiply(0.5)));
            let anchor = if p.x > plot.center().x { Align2::RIGHT_BOTTOM } else { Align2::LEFT_BOTTOM };
            let dx = if p.x > plot.center().x { -6.0 } else { 6.0 };
            painter.text(pos2(p.x + dx, plot.bottom() - 4.0), anchor, fmt_freq(f), small.clone(), v.strong_text_color());
        }
        // drag on the spectrum = tune (fine-tuning inside the live view)
        if let Some(p) = resp.interact_pointer_pos() {
            if resp.dragged() {
                let f = f_of(p.x.clamp(plot.left(), plot.right()));
                if Band::of_freq(f) == a.band {
                    self.drag_tune(a, f, false, ed);
                }
                painter.line_segment([pos2(x_of(self.freq_ui), plot.top()), pos2(x_of(self.freq_ui), plot.bottom())], Stroke::new(2.0, BAD));
            }
        }
        if resp.drag_stopped() {
            self.drag_tune(a, self.freq_ui, true, ed);
            return;
        }
        let resp = resp
            .on_hover_cursor(egui::CursorIcon::Crosshair)
            .on_hover_text_at_pointer("Click: select · Double-click: listen · Drag: tune");
        if let Some(p) = resp.interact_pointer_pos().filter(|p| plot.contains(*p)) {
            let f = f_of(p.x);
            let band = Band::of_freq(f);
            let nearest = a
                .stations
                .keys()
                .copied()
                .min_by_key(|k| (*k as f64 * 1e3 - f).abs() as i64)
                .filter(|k| (*k as f64 * 1e3 - f).abs() < 150e3 && band == Band::Fm);
            let snapped = nearest.map(|k| k as f64 * 1e3).unwrap_or((f / band.step()).round() * band.step());
            if resp.double_clicked() {
                self.freq_ui = snapped;
                ed.push(Box::new(move |a| a.listen(snapped)));
            } else if resp.clicked() {
                self.selected = nearest.or(Some((snapped / 1e3) as u32));
                self.freq_ui = snapped;
            }
        }
    }

    // ------------------------------------------------------------------ table

    fn table(&mut self, ui: &mut egui::Ui, a: &App, ed: &mut Vec<Edit>) {
        let total = a.stations.len();
        let with_rds = a.stations.values().filter(|s| s.has_rds()).count();
        ui.horizontal(|ui| {
            ui.strong(format!("FM stations: {total} · {with_rds} with RDS"));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.checkbox(&mut self.only_rds, "RDS only");
            });
        });
        ui.add_space(4.0);
        let rows: Vec<&Station> = a.stations.values().filter(|s| !self.only_rds || s.has_rds()).collect();
        if rows.is_empty() {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.weak(if total == 0 {
                    "No FM stations yet — press 📡 Scan FM, or ▶ Listen to a frequency"
                } else {
                    "No RDS decoded yet (untick \"RDS only\" to see all stations)"
                });
            });
            return;
        }
        let selected = &mut self.selected;
        let freq_ui = &mut self.freq_ui;
        let playing = match a.mode {
            Mode::Listen { hz, .. } => Some(a.station_key((hz as f64 / 1e3).round() as u32)),
            _ => None,
        };
        TableBuilder::new(ui)
            .striped(true)
            .sense(Sense::click())
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::exact(70.0))
            .column(Column::exact(104.0))
            .column(Column::initial(110.0).clip(true))
            .column(Column::remainder().at_least(160.0).clip(true))
            .column(Column::exact(44.0))
            .column(Column::exact(118.0))
            .column(Column::exact(40.0))
            .column(Column::exact(96.0))
            .header(22.0, |mut h| {
                for t in ["MHz", "PS", "PTY", "RadioText", "PI", "Signal", "RDS", ""] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|mut body| {
                for s in rows {
                    body.row(26.0, |mut row| {
                        row.set_selected(selected.map(|k| a.station_key(k)) == Some(s.freq_khz));
                        let dim = !s.has_rds();
                        row.col(|ui| {
                            let mut t = RichText::new(format!("{:.2}", s.freq_khz as f64 / 1000.0)).monospace().strong();
                            if playing == Some(s.freq_khz) {
                                t = t.color(GOOD);
                            }
                            ui.label(if dim { t.weak() } else { t });
                        });
                        row.col(|ui| {
                            let (ps, full) = s.ps_display();
                            let t = RichText::new(if ps.is_empty() { "—".into() } else { ps }).monospace().size(15.0);
                            ui.label(if full { t.strong() } else { t.weak() });
                        });
                        row.col(|ui| {
                            ui.label(s.pty.map(|p| PTY_NAMES[p as usize]).unwrap_or(""));
                        });
                        row.col(|ui| {
                            let (rt, full) = s.rt_display();
                            let t = RichText::new(rt.clone());
                            ui.label(if full { t } else { t.weak() }).on_hover_text(rt);
                        });
                        row.col(|ui| {
                            ui.label(RichText::new(s.pi.map(|p| format!("{p:04X}")).unwrap_or_default()).monospace());
                        });
                        row.col(|ui| {
                            bar(ui, level_frac(s.level_db), 54.0, ui.visuals().hyperlink_color);
                            ui.label(RichText::new(fmt_db(s.level_db, "")).small());
                        });
                        row.col(|ui| {
                            ui.label(if s.has_rds() { format!("{:.0}%", s.quality * 100.0) } else { "–".into() });
                        });
                        row.col(|ui| {
                            ui.label(RichText::new(if s.pilot_db > 15.0 { "Stereo" } else { "Mono" }).small());
                            if s.tp == Some(true) {
                                ui.label(RichText::new("TP").small().strong());
                            }
                            if s.ta == Some(true) {
                                ui.label(RichText::new("TA").small().strong().color(WARN));
                            }
                        });
                        let r = row.response();
                        if r.clicked() {
                            *selected = Some(s.freq_khz);
                            *freq_ui = s.freq_khz as f64 * 1e3;
                        }
                        if r.double_clicked() {
                            let f = s.freq_khz as f64 * 1e3;
                            ed.push(Box::new(move |a| a.listen(f)));
                        }
                    });
                }
            });
    }

    // ------------------------------------------------------------------ tabs

    fn station_tab(&mut self, ui: &mut egui::Ui, a: &App, ed: &mut Vec<Edit>) {
        let Some(s) = self.selected.and_then(|k| a.stations.get(&a.station_key(k))) else {
            ui.weak("Click a station in the table or on the spectrum.\nDouble-click to listen.");
            return;
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new(fmt_freq(s.freq_khz as f64 * 1e3)).size(19.0).strong().monospace());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let f = s.freq_khz as f64 * 1e3;
                let playing = matches!(a.mode, Mode::Listen { hz, .. } if hz / 1000 == s.freq_khz as u64);
                if !playing && ui.button("▶ Listen").clicked() {
                    ed.push(Box::new(move |a| a.listen(f)));
                }
            });
        });
        let (ps, full) = s.ps_display();
        let t = RichText::new(if ps.is_empty() { "—".into() } else { ps }).monospace().size(30.0).strong();
        ui.label(if full { t } else { t.weak() });
        let (rt, full) = s.rt_display();
        if rt.is_empty() {
            ui.weak("No RadioText yet");
        } else {
            let t = RichText::new(rt).size(15.0);
            ui.add(egui::Label::new(if full { t } else { t.weak() }).wrap());
        }
        ui.separator();
        egui::Grid::new("station_grid").num_columns(2).spacing([12.0, 5.0]).striped(true).show(ui, |ui| {
            if let Some(pi) = s.pi {
                kv(ui, "PI", format!("{pi:04X}  ({})", pi_info(pi)));
            }
            if let Some(p) = s.pty {
                kv(ui, "PTY", format!("{p} · {}", PTY_NAMES[p as usize]));
            }
            if let Some(p) = &s.ptyn {
                kv(ui, "PTYN", text::decode(p).0);
            }
            if let Some(l) = &s.long_ps {
                kv(ui, "Long PS", l.clone());
            }
            if let Some(ct) = &s.ct {
                kv(ui, "Clock (CT)", ct.clone());
            }
            if let Some((d, h, m)) = s.pin {
                kv(ui, "PIN", format!("day {d}, {h:02}:{m:02}"));
            }
            let flags: Vec<&str> = [
                (s.tp == Some(true)).then_some("TP"),
                (s.ta == Some(true)).then_some("TA"),
                s.music.map(|m| if m { "Music" } else { "Speech" }),
            ]
            .into_iter()
            .flatten()
            .collect();
            if !flags.is_empty() {
                kv(ui, "Flags", flags.join(" · "));
            }
            let di = s.di_text();
            if !di.is_empty() {
                kv(ui, "DI", di);
            }
            if !s.af.is_empty() {
                kv(ui, "AF", s.af.iter().map(|f| format!("{:.1}", *f as f64 / 10.0)).collect::<Vec<_>>().join(", ") + " MHz");
            }
            if let Some(e) = s.ecc {
                kv(ui, "ECC", format!("{e:02X}"));
            }
            if let Some(l) = s.lang {
                kv(ui, "Language code", format!("{l}"));
            }
            let cs: Vec<String> = [("PS", &s.ps), ("RT", &s.rt)]
                .iter()
                .filter_map(|(n, v)| v.as_ref().map(|b| format!("{n} {}", text::decode(b).1)))
                .collect();
            if !cs.is_empty() {
                kv(ui, "Charset", cs.join(" · "));
            }
            kv(ui, "Level", fmt_db(s.level_db, "dBFS"));
            kv(ui, "Pilot 19 kHz", fmt_db(s.pilot_db, "dB"));
            kv(ui, "RDS 57 kHz", fmt_db(s.rds_snr_db, "dB"));
            let mut seen = format!("{} ago", fmt_age(s.last_seen.elapsed().as_secs_f32()));
            if let Some(t) = s.last_rds {
                seen += &format!(" · RDS {} ago", fmt_age(t.elapsed().as_secs_f32()));
            }
            kv(ui, "Last seen", seen);
            if let Some(p) = &s.ps {
                kv_mono(ui, "PS hex", text::hex(p));
            }
            if let Some(r) = &s.rt {
                kv_mono(ui, "RT hex", text::hex(r));
            }
        });
    }

    fn rdsplus_tab(&mut self, ui: &mut egui::Ui, a: &App) {
        let Some(s) = self.selected.and_then(|k| a.stations.get(&a.station_key(k))) else {
            ui.weak("Select a station.");
            return;
        };
        egui::CollapsingHeader::new("RadioText+").default_open(true).show(ui, |ui| {
            if s.rtplus.tags.is_empty() {
                ui.weak(if s.oda.contains_key(&rds_fm::station::AID_RTPLUS) { "Announced, waiting for tags…" } else { "Not broadcast by this station" });
            } else {
                egui::Grid::new("rtp").num_columns(2).striped(true).show(ui, |ui| {
                    for (t, txt) in &s.rtplus.tags {
                        kv(ui, &rtplus_name(*t), txt.clone());
                    }
                });
                ui.label(RichText::new(if s.rtplus.item_running { "item running" } else { "item stopped" }).small().weak());
            }
        });
        egui::CollapsingHeader::new("Reception statistics").default_open(true).show(ui, |ui| {
            egui::Grid::new("stats").num_columns(2).striped(true).show(ui, |ui| {
                let bler = if s.blocks_total > 0 { 100.0 * (1.0 - s.blocks_ok as f64 / s.blocks_total as f64) } else { 0.0 };
                kv(ui, "Blocks OK", format!("{} / {} (BLER {:.1}%)", s.blocks_ok, s.blocks_total, bler));
                kv(ui, "Corrected", format!("{} (burst ≤5 bits)", s.blocks_corrected));
                kv(ui, "Last capture", format!("{:.0}% blocks OK", s.quality * 100.0));
                kv(ui, "Groups", format!("{}", s.groups_total));
                kv_mono(ui, "By type", s.group_counts.iter().map(|(k, v)| format!("{k}:{v}")).collect::<Vec<_>>().join("  "));
            });
        });
        if s.rt_history.len() > 1 || s.ps_history.len() > 1 {
            egui::CollapsingHeader::new("History").default_open(false).show(ui, |ui| {
                for p in s.ps_history.iter().rev() {
                    ui.label(RichText::new(format!("PS [{}]", text::decode(p).0)).monospace().small());
                }
                for r in s.rt_history.iter().rev() {
                    ui.label(RichText::new(format!("RT {}", text::decode(r).0)).small());
                }
            });
        }
        if !s.oda.is_empty() {
            egui::CollapsingHeader::new("Open Data Applications").default_open(true).show(ui, |ui| {
                for (aid, g) in &s.oda {
                    ui.label(format!("{aid:04X}  {} on group {g}", oda_name(*aid)));
                }
                if let Some(e) = &s.ert {
                    ui.label(format!("eRT: {e}"));
                }
            });
        }
        if !s.eon.is_empty() {
            egui::CollapsingHeader::new(format!("Other networks (EON) — {}", s.eon.len())).default_open(true).show(ui, |ui| {
                egui::Grid::new("eon").num_columns(5).striped(true).show(ui, |ui| {
                    for h in ["PI", "PS", "PTY", "TP/TA", "AF"] {
                        ui.strong(h);
                    }
                    ui.end_row();
                    for (pi, e) in &s.eon {
                        ui.monospace(format!("{pi:04X}"));
                        ui.monospace(e.ps().unwrap_or_default());
                        ui.label(e.pty.map(|p| PTY_NAMES[p as usize & 31]).unwrap_or(""));
                        ui.label(format!(
                            "{}{}",
                            if e.tp == Some(true) { "TP " } else { "" },
                            if e.ta == Some(true) { "TA" } else { "" }
                        ));
                        ui.label(e.af.iter().map(|f| format!("{:.1}", *f as f64 / 10.0)).collect::<Vec<_>>().join(" "));
                        ui.end_row();
                    }
                });
            });
        }
        egui::CollapsingHeader::new("Raw groups (latest first)").default_open(false).show(ui, |ui| {
            for g in s.raw.iter().rev().take(60) {
                let b = g.blocks;
                let label = b[1].map(|v| rds_fm::rds::group_label((v >> 12) as u8, (v >> 11) & 1 == 1)).unwrap_or("?".into());
                let hex: Vec<String> = b.iter().map(|x| x.map(|v| format!("{v:04X}")).unwrap_or("----".into())).collect();
                ui.label(RichText::new(format!("{:>4}  {}  {:.1}s", label, hex.join(" "), g.at.elapsed().as_secs_f32())).monospace().small());
            }
        });
    }

    fn traffic_tab(&mut self, ui: &mut egui::Ui, a: &App, ed: &mut Vec<Edit>) {
        let mut alert = a.traffic_alert;
        if ui.checkbox(&mut alert, "Show a banner during traffic announcements (TA)").changed() {
            ed.push(Box::new(move |a| {
                a.traffic_alert = alert;
                a.settings_dirty = true;
            }));
        }
        ui.separator();
        ui.strong("Traffic programme (TP) stations");
        let tp: Vec<&Station> = a.stations.values().filter(|s| s.tp == Some(true)).collect();
        if tp.is_empty() {
            ui.weak("None found yet.");
        }
        egui::Grid::new("tp").num_columns(3).striped(true).show(ui, |ui| {
            for s in tp {
                ui.monospace(fmt_freq(s.freq_khz as f64 * 1e3));
                ui.monospace(s.ps_display().0);
                if s.ta_active() {
                    let since = s.ta_since.map(|t| format!(" ({})", fmt_age(t.elapsed().as_secs_f32()))).unwrap_or_default();
                    ui.colored_label(WARN, format!("TA ON{since}"));
                } else {
                    ui.weak("no announcement");
                }
                ui.end_row();
            }
        });
        let eon_ta: Vec<String> = a
            .stations
            .values()
            .flat_map(|s| s.eon.iter().filter(|(_, e)| e.ta == Some(true)).map(move |(pi, e)| format!("{} ({pi:04X}) via {}", e.ps().unwrap_or_default().trim(), s.ps_display().0.trim())))
            .collect();
        if !eon_ta.is_empty() {
            ui.add_space(4.0);
            ui.colored_label(WARN, format!("EON announcements: {}", eon_ta.join(", ")));
        }
        ui.separator();
        ui.strong("RDS-TMC");
        ui.horizontal_wrapped(|ui| {
            ui.label("Bangkok");
            for (khz, station) in BANGKOK_STATIONS {
                let hz = f64::from(khz) * 1000.0;
                let receiving = matches!(a.mode, Mode::Listen { hz: f, .. } if f == u64::from(khz) * 1000);
                if ui.add(egui::Button::new(fmt_freq(hz)).selected(receiving)).on_hover_text(station).clicked() {
                    self.selected = Some(khz);
                    self.freq_ui = hz;
                    ed.push(Box::new(move |a| a.listen(hz)));
                }
            }
            ui.hyperlink_to("iTIC", "https://iticfoundation.org/en/about-us/how-does-itic-work/");
        });
        let tmc: Vec<&Station> = a.stations.values().filter(|s| s.tmc_service_pi().is_some() || s.tmc.groups > 0).collect();
        if tmc.is_empty() {
            ui.weak("Waiting for an ALERT-C service announcement (ODA 3A, AID CD46/CD47).");
        }
        for s in tmc {
            let sys = &s.tmc.system;
            ui.label(RichText::new(format!("{} {}", fmt_freq(s.freq_khz as f64 * 1e3), s.ps_display().0)).strong());
            ui.label(format!(
                "LTN {} {} · SID {} · scope {} · {} groups ({} tuning)",
                sys.ltn.map(|l| l.to_string()).unwrap_or("?".into()),
                if sys.ltn == Some(0) { "(encrypted)" } else { "" },
                sys.sid.map(|l| l.to_string()).unwrap_or("?".into()),
                sys.scope.map(|sc| ["I", "N", "R", "U"].iter().zip(sc).filter(|(_, on)| *on).map(|(n, _)| *n).collect::<String>()).unwrap_or("?".into()),
                s.tmc.groups,
                s.tmc.tuning_groups
            ));
            if s.tmc.messages.is_empty() {
                let status = match sys.ltn {
                    Some(0) => "This service uses encrypted location codes.",
                    None => "Waiting for the location table number from the station.",
                    Some(_) => "Waiting for repeated ALERT-C messages.",
                };
                ui.weak(status);
            }
            egui::ScrollArea::vertical().id_salt(("tmc", s.freq_khz)).max_height(220.0).show(ui, |ui| {
                egui::Grid::new(("tmcg", s.freq_khz)).num_columns(6).striped(true).show(ui, |ui| {
                    for h in ["Event", "Location", "Dir", "Ext", "Duration", "Seen"] {
                        ui.strong(h);
                    }
                    ui.end_row();
                    for m in s.tmc.messages.iter().rev() {
                        let name = a.tmc_names.get(&m.event).cloned().unwrap_or_default();
                        ui.label(format!("{}{}", m.event, if name.is_empty() { String::new() } else { format!(" {name}") }));
                        let location_name = s.tmc_service_pi().filter(|_| !m.multi_group)
                            .and_then(|pi| a.tmc_locations.get(&(pi, m.ltn, m.location)));
                        if let Some(name) = location_name {
                            ui.label(name).on_hover_text(format!("LTN {} · code {}", m.ltn, m.location));
                        } else {
                            ui.monospace(m.location.to_string()).on_hover_text(format!("LTN {}", m.ltn));
                        }
                        ui.label(if m.direction_neg { "−" } else { "+" });
                        ui.label(m.extent.to_string());
                        if m.multi_group {
                            let words = m.optional_words.iter().map(|w| format!("{w:07X}")).collect::<Vec<_>>();
                            ui.label("Multi-group").on_hover_text(words.join(" "));
                        } else {
                            ui.label(m.dp.map(dp_text).unwrap_or_default());
                        }
                        ui.label(format!("{}× {}", m.count, fmt_age(m.last_seen.elapsed().as_secs_f32())));
                        ui.end_row();
                    }
                });
            });
        }
        ui.add_space(6.0);
        ui.label(RichText::new(format!("Event names: {} loaded", a.tmc_names.len())).strong());
        ui.horizontal(|ui| {
            let busy = self.tmc_job.lock().unwrap().as_deref() == Some("downloading…");
            if ui.add_enabled(!busy, egui::Button::new("⬇ Download event list")).on_hover_text(rds_fm::tmc::EVENT_LIST_URL).clicked() {
                *self.tmc_job.lock().unwrap() = Some("downloading…".into());
                let job = self.tmc_job.clone();
                let app = self.app.clone();
                std::thread::spawn(move || {
                    let dest = rds_fm::tmc::default_event_csv();
                    let msg = match rds_fm::tmc::download_event_list(&dest) {
                        Ok(n) => {
                            let names = rds_fm::tmc::load_event_names(&dest);
                            let mut a = app.lock().unwrap();
                            a.tmc_names = names;
                            a.push_log(format!("TMC: {n} event names saved to {}", dest.display()));
                            format!("{n} events downloaded")
                        }
                        Err(e) => format!("failed: {e}"),
                    };
                    *job.lock().unwrap() = Some(msg);
                });
            }
            if let Some(m) = self.tmc_job.lock().unwrap().as_ref() {
                ui.label(m);
            }
        });
        ui.horizontal(|ui| {
            ui.label("Event CSV");
            ui.text_edit_singleline(&mut self.tmc_csv);
            if ui.button("Load").clicked() {
                let p = PathBuf::from(self.tmc_csv.clone());
                ed.push(Box::new(move |a| {
                    let names = rds_fm::tmc::load_event_names(&p);
                    a.push_log(format!("Loaded {} TMC event names from {}", names.len(), p.display()));
                    a.tmc_names = names;
                }));
            }
        });
        ui.horizontal(|ui| {
            ui.label("Location CSV");
            ui.text_edit_singleline(&mut self.tmc_location_csv).on_hover_text("UTF-8 · pi;ltn;location;name");
            if ui.button("Load").clicked() {
                match load_location_names(&PathBuf::from(&self.tmc_location_csv)) {
                    Ok(names) => {
                        self.tmc_location_error = None;
                        ed.push(Box::new(move |a| a.tmc_locations = names));
                    }
                    Err(e) => self.tmc_location_error = Some(e),
                }
            }
        });
        if let Some(error) = &self.tmc_location_error {
            ui.colored_label(BAD, error);
        }
        ui.horizontal(|ui| {
            ui.weak(format!("{} location names loaded", a.tmc_locations.len()));
            ui.hyperlink_to("Thailand Location Table", "https://iticfoundation.org/location-table/");
        });
        ui.label(
            RichText::new(
                "The event list comes from GNU Radio gr-rds (GPL-2.0+) and stays on this PC. \
                 Import a matching Location CSV to show names for single-group messages.",
            )
            .small()
            .weak(),
        );
    }

    fn audio_tab(&mut self, ui: &mut egui::Ui, a: &App, ed: &mut Vec<Edit>) {
        let au = a.audio.clone();
        egui::Grid::new("audio").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Output");
            ui.horizontal(|ui| {
                let cur = au.output.clone().unwrap_or_else(|| "System default".into());
                let mut pick: Option<Option<String>> = None;
                egui::ComboBox::from_id_salt("outdev").width(220.0).selected_text(cur).show_ui(ui, |ui| {
                    if ui.selectable_label(au.output.is_none(), "System default").clicked() {
                        pick = Some(None);
                    }
                    for d in &self.audio_devices {
                        if ui.selectable_label(au.output.as_deref() == Some(d.as_str()), d).clicked() {
                            pick = Some(Some(d.clone()));
                        }
                    }
                });
                if ui.button("⟳").clicked() {
                    self.audio_devices = output_devices();
                }
                if let Some(p) = pick {
                    ed.push(Box::new(move |a| {
                        a.audio.output = p;
                        a.settings_dirty = true;
                        // restart the listen session so the new device is opened
                        if let Mode::Listen { hz, .. } = a.mode {
                            a.mode = Mode::Idle;
                            a.push_log("Audio output changed");
                            a.listen(hz as f64);
                        }
                    }));
                }
            });
            ui.end_row();
            ui.label("Volume");
            ui.horizontal(|ui| {
                let mut v = au.volume;
                if ui.add(egui::Slider::new(&mut v, 0.0..=1.5).show_value(false)).changed() {
                    ed.push(Box::new(move |a| {
                        a.audio.volume = v;
                        a.settings_dirty = true;
                    }));
                }
                let mut m = au.muted;
                let label = if m { "🔇 Muted" } else { "🔊" };
                if ui.toggle_value(&mut m, label).changed() {
                    ed.push(Box::new(move |a| {
                        a.audio.muted = m;
                        a.settings_dirty = true;
                    }));
                }
            });
            ui.end_row();
            ui.label("FM stereo");
            ui.horizontal(|ui| {
                let mut s = au.stereo;
                ui.radio_value(&mut s, StereoMode::Auto, "Auto (stereo when available)");
                ui.radio_value(&mut s, StereoMode::Mono, "Mono");
                if s != au.stereo {
                    ed.push(Box::new(move |a| {
                        a.audio.stereo = s;
                        a.settings_dirty = true;
                    }));
                }
            });
            ui.end_row();
            ui.label("De-emphasis");
            ui.horizontal(|ui| {
                let mut d = au.deemph_us;
                ui.radio_value(&mut d, Some(50.0), "50 µs (TH/EU)");
                ui.radio_value(&mut d, Some(75.0), "75 µs (US)");
                ui.radio_value(&mut d, None, "Off");
                if d != au.deemph_us {
                    ed.push(Box::new(move |a| {
                        a.audio.deemph_us = d;
                        a.settings_dirty = true;
                    }));
                }
            });
            ui.end_row();
            ui.label("AM bandwidth");
            ui.horizontal(|ui| {
                let mut bw = au.am_bw_hz;
                for (v, n) in [(2_500.0, "2.5k"), (3_500.0, "3.5k"), (4_500.0, "4.5k"), (6_000.0, "6k"), (8_000.0, "8k"), (10_000.0, "10k")] {
                    ui.radio_value(&mut bw, v, n);
                }
                if bw != au.am_bw_hz {
                    ed.push(Box::new(move |a| {
                        a.audio.am_bw_hz = bw;
                        a.settings_dirty = true;
                    }));
                }
            });
            ui.end_row();
        });
        ui.separator();
        // ---- enhancement
        let en0 = a.enhance.clone();
        let mut en = en0.clone();
        ui.label(RichText::new("Sound enhancement").strong());
        egui::Grid::new("enh").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.checkbox(&mut en.denoise, "AI noise reduction").on_hover_text(
                "RNNoise recurrent neural network (Xiph) removes hiss and static. Best on AM and weak FM; \
                 on music, keep the mix below 100% so the texture stays.",
            );
            ui.add_enabled(en.denoise, egui::Slider::new(&mut en.denoise_mix, 0.0..=1.0).text("mix"));
            ui.end_row();
            ui.checkbox(&mut en.bwe, "Bandwidth extension").on_hover_text(
                "Harmonic exciter: regenerates the octave above the broadcast cut-off (15 kHz FM, filter width AM) \
                 for a brighter, CD-like top end. It adds plausible detail, it cannot restore what was never transmitted.",
            );
            ui.add_enabled(en.bwe, egui::Slider::new(&mut en.bwe_amount, 0.0..=1.0).text("amount"));
            ui.end_row();
            ui.label("Stereo width");
            ui.add(egui::Slider::new(&mut en.width, 0.0..=2.0).text("× (1 = off)"));
            ui.end_row();
        });
        ui.horizontal(|ui| {
            if ui.button("CD-like preset").on_hover_text("light denoise + bandwidth extension + loudness EQ").clicked() {
                en = rds_fm::enhance::EnhanceSettings { denoise: true, denoise_mix: 0.35, bwe: true, bwe_amount: 0.4, width: 1.15 };
                ed.push(Box::new(|a| {
                    a.audio.eq_enabled = true;
                    a.audio.eq_gains = EQ_PRESETS[3].1;
                    a.audio.eq_preset = EQ_PRESETS[3].0.into();
                }));
            }
            if ui.button("AM clean-up").clicked() {
                en = rds_fm::enhance::EnhanceSettings { denoise: true, denoise_mix: 0.9, bwe: true, bwe_amount: 0.25, width: 1.0 };
            }
            if ui.button("Off").clicked() {
                en = rds_fm::enhance::EnhanceSettings::default();
            }
        });
        if en != en0 {
            ed.push(Box::new(move |a| {
                a.enhance = en;
                a.settings_dirty = true;
            }));
        }
        ui.separator();
        ui.horizontal(|ui| {
            let mut en = au.eq_enabled;
            if ui.checkbox(&mut en, RichText::new("Equalizer").strong()).changed() {
                ed.push(Box::new(move |a| {
                    a.audio.eq_enabled = en;
                    a.settings_dirty = true;
                }));
            }
            let mut pick = None;
            egui::ComboBox::from_id_salt("eqp").selected_text(au.eq_preset.clone()).show_ui(ui, |ui| {
                for (n, g) in EQ_PRESETS {
                    if ui.selectable_label(au.eq_preset == n, n).clicked() {
                        pick = Some((n, g));
                    }
                }
            });
            if let Some((n, g)) = pick {
                ed.push(Box::new(move |a| {
                    a.audio.eq_gains = g;
                    a.audio.eq_preset = n.into();
                    a.audio.eq_enabled = true;
                    a.settings_dirty = true;
                }));
            }
        });
        ui.add_enabled_ui(au.eq_enabled, |ui| {
            ui.horizontal(|ui| {
                for i in 0..10 {
                    ui.vertical(|ui| {
                        let mut g = au.eq_gains[i];
                        let r = ui.add(egui::Slider::new(&mut g, -12.0..=12.0).vertical().step_by(0.5).show_value(false));
                        if r.changed() {
                            ed.push(Box::new(move |a| {
                                a.audio.eq_gains[i] = g;
                                a.audio.eq_preset = "Custom".into();
                                a.settings_dirty = true;
                            }));
                        }
                        r.on_hover_text(format!("{:+.1} dB", g));
                        let f = EQ_FREQS[i];
                        ui.label(RichText::new(if f >= 1000.0 { format!("{}k", f / 1000.0) } else { format!("{f}") }).small());
                    });
                }
            });
        });
    }

    fn record_tab(&mut self, ui: &mut egui::Ui, a: &App, ed: &mut Vec<Edit>) {
        let r = a.rec.clone();
        let listening = matches!(a.mode, Mode::Listen { .. });
        ui.horizontal(|ui| {
            if let Some(st) = &a.rec_status {
                let s = st.seconds as u64;
                if ui.add(egui::Button::new(RichText::new("■ Stop recording").strong()).fill(BAD.gamma_multiply(0.6))).clicked() {
                    ed.push(Box::new(|a| a.rec_request = Some(false)));
                }
                ui.label(RichText::new(format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)).monospace().strong());
            } else if ui.add_enabled(listening, egui::Button::new(RichText::new("● Record").strong())).clicked() {
                ed.push(Box::new(|a| a.rec_request = Some(true)));
            }
            if !listening {
                ui.weak("Start listening to record");
            }
        });
        if let Some(st) = &a.rec_status {
            ui.label(RichText::new(st.path.display().to_string()).small());
        }
        ui.separator();
        ui.add_enabled_ui(a.rec_status.is_none(), |ui| {
            egui::Grid::new("rec").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Format");
                ui.horizontal(|ui| {
                    let mut f = r.format;
                    for x in Format::ALL {
                        ui.radio_value(&mut f, x, x.name());
                    }
                    if f != r.format {
                        ed.push(Box::new(move |a| {
                            a.rec.format = f;
                            a.settings_dirty = true;
                        }));
                    }
                });
                ui.end_row();
                match r.format {
                    Format::Wav => {
                        ui.label("Bit depth");
                        ui.horizontal(|ui| {
                            let mut b = r.wav_bits;
                            ui.radio_value(&mut b, 16, "16-bit");
                            ui.radio_value(&mut b, 24, "24-bit");
                            ui.radio_value(&mut b, 32, "32-bit float");
                            if b != r.wav_bits {
                                ed.push(Box::new(move |a| {
                                    a.rec.wav_bits = b;
                                    a.settings_dirty = true;
                                }));
                            }
                        });
                    }
                    Format::Flac => {
                        ui.label("Bit depth");
                        ui.horizontal(|ui| {
                            let mut b = r.flac_bits;
                            ui.radio_value(&mut b, 16, "16-bit");
                            ui.radio_value(&mut b, 24, "24-bit");
                            if b != r.flac_bits {
                                ed.push(Box::new(move |a| {
                                    a.rec.flac_bits = b;
                                    a.settings_dirty = true;
                                }));
                            }
                        });
                    }
                    Format::Mp3 => {
                        ui.label("Bitrate (CBR)");
                        ui.horizontal(|ui| {
                            let mut k = r.mp3_kbps;
                            for v in [128, 160, 192, 256, 320] {
                                ui.radio_value(&mut k, v, format!("{v}"));
                            }
                            ui.label("kbps");
                            if k != r.mp3_kbps {
                                ed.push(Box::new(move |a| {
                                    a.rec.mp3_kbps = k;
                                    a.settings_dirty = true;
                                }));
                            }
                        });
                    }
                    Format::Aac => {
                        ui.label("Bitrate");
                        ui.horizontal(|ui| {
                            let mut k = r.aac_kbps;
                            for v in [48, 64, 96, 128, 192, 256] {
                                ui.radio_value(&mut k, v, format!("{v}"));
                            }
                            ui.label("kbps");
                            let mut he = r.aac_he;
                            ui.checkbox(&mut he, "HE-AAC");
                            if k != r.aac_kbps || he != r.aac_he {
                                ed.push(Box::new(move |a| {
                                    a.rec.aac_kbps = k;
                                    a.rec.aac_he = he;
                                    a.settings_dirty = true;
                                }));
                            }
                        });
                    }
                }
                ui.end_row();
                ui.label("Sample rate");
                ui.horizontal(|ui| {
                    let mut sr = r.sample_rate;
                    ui.radio_value(&mut sr, 44_100, "44.1 kHz");
                    ui.radio_value(&mut sr, 48_000, "48 kHz");
                    ui.radio_value(&mut sr, 88_200, "88.2");
                    ui.radio_value(&mut sr, 96_000, "96 kHz");
                    let mut st = r.stereo;
                    ui.separator();
                    ui.radio_value(&mut st, true, "Stereo");
                    ui.radio_value(&mut st, false, "Mono");
                    if sr != r.sample_rate || st != r.stereo {
                        ed.push(Box::new(move |a| {
                            a.rec.sample_rate = sr;
                            a.rec.stereo = st;
                            a.settings_dirty = true;
                        }));
                    }
                });
                ui.end_row();
                ui.label("Folder");
                ui.horizontal(|ui| {
                    let mut p = r.folder.display().to_string();
                    if ui.add(egui::TextEdit::singleline(&mut p).desired_width(260.0)).changed() {
                        ed.push(Box::new(move |a| {
                            a.rec.folder = PathBuf::from(p);
                            a.settings_dirty = true;
                        }));
                    }
                    if ui.button("📂").on_hover_text("Open folder").clicked() {
                        let _ = std::fs::create_dir_all(&r.folder);
                        let _ = std::process::Command::new("explorer").arg(&r.folder).spawn();
                    }
                });
                ui.end_row();
            });
        });
        ui.label(
            RichText::new(
                "Files are named date time · frequency · PS. MP3 gets ID3 tags from RDS (title = RadioText, artist = PS). \
                 FLAC/MP3/AAC are encoded from a temporary WAV when you stop.",
            )
            .small()
            .weak(),
        );
    }

    fn log_view(&self, ui: &mut egui::Ui, a: &App) {
        ui.add_space(4.0);
        ui.strong("Event log");
        egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
            for line in &a.log {
                ui.label(RichText::new(line).monospace().size(12.0));
            }
        });
    }
}

impl eframe::App for Gui {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.ctx().request_repaint_after(Duration::from_millis(100));
        let mut ed: Vec<Edit> = Vec::new();
        let shared = self.app.clone();
        {
            let a = shared.lock().unwrap();
            egui::Panel::top("top").show(ui, |ui| self.top_bar(ui, &a, &mut ed));
            egui::Panel::top("dial").show(ui, |ui| {
                ui.add_space(3.0);
                self.tuning_dial(ui, &a, &mut ed);
                ui.add_space(3.0);
            });
            egui::Panel::top("spectrum")
                .resizable(true)
                .default_size(190.0)
                .size_range(110.0..=420.0)
                .show(ui, |ui| self.spectrum(ui, &a, &mut ed));
            egui::Panel::bottom("log").resizable(true).default_size(120.0).show(ui, |ui| self.log_view(ui, &a));
            egui::Panel::right("tabs").resizable(true).default_size(420.0).show(ui, |ui| {
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    for (t, n) in [
                        (Tab::Station, "Station"),
                        (Tab::RdsPlus, "RDS+"),
                        (Tab::Traffic, "Traffic"),
                        (Tab::Audio, "Audio"),
                        (Tab::Record, "Record"),
                        (Tab::Stream, "Stream"),
                        (Tab::Presets, "Presets"),
                        (Tab::Logbook, "Logbook"),
                    ] {
                        ui.selectable_value(&mut self.tab, t, n);
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.tab {
                    Tab::Station => self.station_tab(ui, &a, &mut ed),
                    Tab::RdsPlus => self.rdsplus_tab(ui, &a),
                    Tab::Traffic => self.traffic_tab(ui, &a, &mut ed),
                    Tab::Audio => self.audio_tab(ui, &a, &mut ed),
                    Tab::Record => self.record_tab(ui, &a, &mut ed),
                    Tab::Stream => self.stream_tab(ui, &a, &mut ed),
                    Tab::Presets => self.presets_tab(ui, &a, &mut ed),
                    Tab::Logbook => self.logbook_tab(ui, &a, &mut ed),
                });
            });
            egui::CentralPanel::default_margins().show(ui, |ui| {
                self.now_playing(ui, &a);
                self.table(ui, &a, &mut ed);
            });
        }
        self.last_frame = Instant::now();
        if !ed.is_empty() {
            let mut a = self.app.lock().unwrap();
            for e in ed {
                e(&mut a);
            }
            if a.settings_dirty {
                a.save_settings();
            }
        }
    }
}

/// Windows ships Thai fonts; add one as a fallback so PS/RT in TIS-620 render.
fn setup_fonts(ctx: &egui::Context) {
    for path in [r"C:\Windows\Fonts\LeelawUI.ttf", r"C:\Windows\Fonts\tahoma.ttf"] {
        if let Ok(bytes) = std::fs::read(path) {
            let fams = [FontFamily::Proportional, FontFamily::Monospace]
                .into_iter()
                .map(|family| InsertFontFamily { family, priority: FontPriority::Lowest })
                .collect();
            ctx.add_font(FontInsert::new("thai", FontData::from_owned(bytes), fams));
            break;
        }
    }
}

fn main() -> eframe::Result {
    let mut paths = DriverPaths::default();
    let mut tune: Option<f64> = None;
    let mut tab = Tab::Station;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--tab" => {
                tab = match it.next().as_deref() {
                    Some("rds") => Tab::RdsPlus,
                    Some("traffic") => Tab::Traffic,
                    Some("audio") => Tab::Audio,
                    Some("record") => Tab::Record,
                    _ => Tab::Station,
                }
            }
            "--uhd-dll" => paths.uhd_dll = it.next().map(PathBuf::from).unwrap_or(paths.uhd_dll),
            "--rtl-dll" => paths.rtl_dll = it.next().map(PathBuf::from).unwrap_or(paths.rtl_dll),
            "tune" => tune = it.next().and_then(|v| v.parse::<f64>().ok()),
            _ => {}
        }
    }
    if std::env::var_os("UHD_IMAGES_DIR").is_none() && std::path::Path::new(DEFAULT_IMAGES).exists() {
        std::env::set_var("UHD_IMAGES_DIR", DEFAULT_IMAGES);
    }

    let mut a = App::new(Mode::Scan, 50.0, "TX/RX".into());
    a.load_settings_file();
    a.load_logbook();
    let locations = App::data_dir().join("tmc_locations.csv");
    if locations.is_file() {
        match load_location_names(&locations) {
            Ok(names) => a.tmc_locations = names,
            Err(e) => a.push_log(format!("Cannot load TMC locations: {e}")),
        }
    }
    for p in [
        rds_fm::tmc::default_event_csv(),
        PathBuf::from("tmc_events.csv"),
        std::env::current_exe().unwrap_or_default().with_file_name("tmc_events.csv"),
    ] {
        let names = rds_fm::tmc::load_event_names(&p);
        if !names.is_empty() {
            a.tmc_names = names;
            break;
        }
    }
    if let Some(f) = tune {
        a.listen(f * 1e6);
    } else if a.band != Band::Fm {
        a.mode = Mode::Idle;
    }
    let app = Arc::new(Mutex::new(a));
    {
        let app = app.clone();
        let save_path = rds_fm::app::App::data_dir().join("stations.json");
        std::thread::spawn(move || worker::run(app, WorkerCfg { paths, save_path }));
    }
    {
        let app = app.clone();
        std::thread::spawn(move || worker::run_stream_server(app));
    }

    let mut viewport = egui::ViewportBuilder::default();
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../../assets/icon.png")) {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport: viewport
            .with_title("RDS FM Monitor")
            .with_inner_size([1200.0, 720.0])
            .with_min_inner_size([960.0, 600.0])
            .with_maximized(true),
        ..Default::default()
    };
    eframe::run_native(
        "RDS FM Monitor",
        options,
        Box::new(move |cc| {
            setup_fonts(&cc.egui_ctx);
            let mut g = Gui::new(app);
            g.tab = tab;
            Ok(Box::new(g))
        }),
    )
}
