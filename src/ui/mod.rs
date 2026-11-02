pub mod frame;
pub mod peek_window;
pub mod prefs;
pub mod review;
pub mod tray_menu;
pub mod selector;

use crate::config::{AudioMode, Config, Format};
use crate::controller::Controller;
use gtk::{gdk, glib, prelude::*};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// zk.poker look: ink-black surfaces, square corners, 1px lines, no
/// shadows, Iosevka, one gold primary per screen, red only for recording.
const CSS: &str = r#"
@define-color zf_canvas #090c0b;
@define-color zf_elev1 #0f1513;
@define-color zf_elev2 #151d1a;
@define-color zf_line_soft #1c2724;
@define-color zf_line #2a3833;
@define-color zf_fg_high #f2ecdf;
@define-color zf_fg #ded5c4;
@define-color zf_fg_muted #948a79;
@define-color zf_fg_dim #867d6e;
@define-color zf_red #c73e3a;
@define-color zf_red_light #d95b4e;
@define-color zf_red_tint #2a1210;
@define-color zf_gold #d9a441;
@define-color zf_gold_light #e6b85c;
@define-color zf_gold_fg #141008;
@define-color zf_ok #8fbf73;

* {
    font-family: "Iosevka", "Iosevka Nerd Font", "IosevkaTerm Nerd Font", monospace;
    font-size: 13px;
    -gtk-icon-shadow: none;
    text-shadow: none;
}

window, dialog, popover > contents, tooltip {
    background: @zf_elev1;
    color: @zf_fg;
}
window.csd, window.solid-csd, window.csd:backdrop {
    border-radius: 0;
    box-shadow: none;
    outline: 1px solid @zf_line;
}
window.peek2-transparent,
window.peek2-transparent > contents {
    background: transparent;
    box-shadow: none;
    border: none;
    outline: none;
}
window.peek2-window {
    background: transparent;
}

headerbar {
    background: @zf_elev1;
    color: @zf_fg;
    border-bottom: 1px solid @zf_line_soft;
    box-shadow: none;
    min-height: 34px;
    padding: 0 6px;
}
headerbar:backdrop { background: @zf_elev1; }

button, dropdown > button, menubutton > button {
    border-radius: 0;
    box-shadow: none;
    background: @zf_elev2;
    border: 1px solid @zf_line_soft;
    color: @zf_fg_high;
    min-height: 26px;
    min-width: 26px;
    padding: 0 8px;
    outline: none;
}
button:hover, dropdown > button:hover, menubutton > button:hover { background: @zf_line_soft; }
button:active, button:checked, menubutton > button:checked {
    background: @zf_line;
    color: @zf_fg_high;
}
button:disabled { background: @zf_elev2; color: @zf_fg_dim; border-color: @zf_line_soft; }
button.flat, windowcontrols button {
    background: transparent;
    border-color: transparent;
    color: @zf_fg_muted;
}
button.flat:hover, windowcontrols button:hover { background: @zf_elev2; color: @zf_fg_high; }
windowcontrols button { min-height: 22px; min-width: 22px; padding: 0; }
windowcontrols button image { background: none; box-shadow: none; }
button.suggested-action {
    background: @zf_gold;
    color: @zf_gold_fg;
    border-color: @zf_gold;
    font-weight: 600;
}
button.suggested-action:hover { background: @zf_gold_light; border-color: @zf_gold_light; }
button.destructive-action {
    background: transparent;
    color: @zf_red_light;
    border-color: @zf_red;
}
button.destructive-action:hover { background: alpha(@zf_red, 0.12); }
window.peek2-menu, window.peek2-menu > contents {
    background: @zf_elev1;
}
window.peek2-menu > box {
    border: 1px solid @zf_line;
    padding: 6px;
    min-width: 280px;
}
window.peek2-menu separator { margin: 4px 2px; background: @zf_line_soft; min-height: 1px; }
button.peek2-menu-item { padding: 5px 10px; min-height: 0; }
button.peek2-menu-item label { color: @zf_fg; }
button.peek2-menu-item:hover label { color: @zf_fg_high; }
.peek2-menu-heading { margin: 6px 10px 2px 10px; }
.peek2-seg { margin: 2px 8px 4px 8px; }
.peek2-seg button { padding: 3px 0; min-height: 0; }
.peek2-seg button:checked { border-color: @zf_gold; color: @zf_gold; background: alpha(@zf_gold, 0.08); }
button.flat.peek2-danger:hover { color: @zf_red_light; background: @zf_red_tint; }
.peek2-video { background: #000; }

popover > contents {
    border-radius: 0;
    border: 1px solid @zf_line;
    box-shadow: none;
    padding: 6px;
}
popover > arrow { background: @zf_elev1; border: 1px solid @zf_line; }
popover modelbutton, popover listview row, popover row {
    border-radius: 0;
    min-height: 26px;
    padding: 0 8px;
}
popover modelbutton:hover, popover row:hover, popover row:selected {
    background: @zf_elev2;
    color: @zf_fg_high;
}
check, radio {
    border-radius: 0;
    background: @zf_canvas;
    border: 1px solid @zf_line;
    box-shadow: none;
    color: @zf_gold_fg;
}
check:checked, radio:checked { background: @zf_gold; border-color: @zf_gold; }

entry, spinbutton {
    border-radius: 0;
    background: @zf_canvas;
    border: 1px solid @zf_line_soft;
    box-shadow: none;
    color: @zf_fg_high;
    outline: none;
}
entry:focus-within, spinbutton:focus-within { border-color: @zf_gold; }
entry.error { border-color: @zf_red; color: @zf_red_light; }
spinbutton > button { border-width: 0 0 0 1px; }

switch {
    border-radius: 0;
    background: @zf_line_soft;
    border: 1px solid @zf_line;
    box-shadow: none;
}
switch:checked { background: @zf_gold; border-color: @zf_gold; }
switch > slider {
    border-radius: 0;
    background: @zf_fg;
    box-shadow: none;
    border: none;
}

tooltip {
    border-radius: 0;
    border: 1px solid @zf_line;
    box-shadow: none;
    padding: 4px 8px;
}
tooltip > box { background: transparent; }

scrollbar slider { border-radius: 0; background: @zf_line; min-width: 4px; }

.heading {
    color: @zf_gold;
    font-size: 11px;
    letter-spacing: 1.3px;
    font-weight: normal;
}
.dim-label { color: @zf_fg_muted; }
selection { background: alpha(@zf_gold, 0.35); color: @zf_fg_high; }

/* Peek-style window: a 1px keyline around the hole, vermillion while recording. */
.peek2-frame {
    border: 1px solid @zf_fg_dim;
    border-top-width: 0;
}
.recording .peek2-frame {
    border: 2px solid @zf_red;
    border-top-width: 0;
}
.peek2-osd {
    background: alpha(@zf_elev1, 0.92);
    color: @zf_fg_high;
    border: 1px solid @zf_line;
    border-radius: 0;
    padding: 8px 16px;
    font-size: 15px;
}
.peek2-osd.countdown {
    font-size: 56px;
    padding: 4px 28px;
    color: @zf_gold;
}

.peek2-toolbar {
    background: @zf_elev1;
    color: @zf_fg;
    border: 1px solid @zf_line;
    border-radius: 0;
    padding: 5px;
    margin: 6px;
    box-shadow: none;
}
/* record dot: a ring when idle, filled red while recording */
.peek2-dot {
    min-width: 8px;
    min-height: 8px;
    border-radius: 50%;
    background: @zf_red;
    border: 1px solid @zf_red;
}
.peek2-dot.idle {
    background: transparent;
    border-color: @zf_fg_muted;
}
.peek2-time {
    font-feature-settings: "tnum";
    color: @zf_fg_high;
}
.peek2-dim { color: @zf_fg_muted; }

levelbar.peek2-meter trough {
    min-height: 3px;
    min-width: 56px;
    padding: 0;
    border: none;
    border-radius: 0;
    background: @zf_line_soft;
}
levelbar.peek2-meter block {
    min-height: 3px;
    border-radius: 0;
    border: none;
}
levelbar.peek2-meter block.empty { background: transparent; }
levelbar.peek2-meter block.filled.meter-ok { background: @zf_fg; }
levelbar.peek2-meter block.filled.meter-hot { background: @zf_gold; }
levelbar.peek2-meter block.filled.meter-clip { background: @zf_red; }
.peek2-meter-silent { opacity: 0.3; }
.peek2-meter-row image { color: @zf_fg_muted; }
"#;

pub fn load_css() {
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_application_prefer_dark_theme(true);
    }
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
}

pub fn format_time(secs: u64) -> String {
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

pub fn show_error(parent: Option<&gtk::Window>, msg: &str) {
    let d = gtk::AlertDialog::builder()
        .message("Recording failed")
        .detail(msg)
        .modal(true)
        .build();
    d.show(parent);
}

/// Format picker, audio source and mouse toggle, shared by both UIs.
/// `on_change` runs after the config changed (e.g. to update a label).
pub fn capture_controls(ctl: &Rc<Controller>, on_change: impl Fn() + 'static) -> gtk::Box {
    let on_change = Rc::new(on_change);
    let bx = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let cfg = ctl.config.borrow().clone();

    let labels: Vec<&str> = Format::ALL.iter().map(|f| f.label()).collect();
    let fmt = gtk::DropDown::from_strings(&labels);
    fmt.set_tooltip_text(Some("Output format"));
    fmt.set_selected(Format::ALL.iter().position(|f| *f == cfg.format).unwrap_or(0) as u32);
    fmt.set_focusable(false);

    let audio = gtk::MenuButton::new();
    audio.set_focusable(false);
    let audio_list = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let mut group: Option<gtk::CheckButton> = None;
    let update_audio = {
        let audio = audio.clone();
        let ctl = ctl.clone();
        move || {
            let c = ctl.config.borrow();
            let mode = if c.format.has_audio() { c.audio } else { AudioMode::Off };
            audio.set_icon_name(mode.icon());
            audio.set_sensitive(c.format.has_audio());
            audio.set_tooltip_text(Some(if c.format.has_audio() {
                mode.label()
            } else {
                "Audio is only recorded for WebM and MP4"
            }));
        }
    };
    for mode in AudioMode::ALL {
        let b = gtk::CheckButton::with_label(mode.label());
        if let Some(g) = &group {
            b.set_group(Some(g));
        } else {
            group = Some(b.clone());
        }
        b.set_active(cfg.audio == mode);
        let ctl = ctl.clone();
        let update = update_audio.clone();
        let on_change = on_change.clone();
        b.connect_toggled(move |b| {
            if b.is_active() {
                ctl.update_config(|c| c.audio = mode);
                update();
                on_change();
            }
        });
        audio_list.append(&b);
    }
    let pop = gtk::Popover::new();
    pop.set_has_arrow(false);
    pop.set_child(Some(&audio_list));
    audio.set_popover(Some(&pop));
    update_audio();

    {
        let ctl = ctl.clone();
        let update = update_audio.clone();
        let on_change = on_change.clone();
        fmt.connect_selected_notify(move |d| {
            let f = Format::ALL[d.selected() as usize % Format::ALL.len()];
            ctl.update_config(|c| c.format = f);
            update();
            on_change();
        });
    }

    let mouse = gtk::ToggleButton::new();
    mouse.set_icon_name("input-mouse-symbolic");
    mouse.set_tooltip_text(Some("Capture mouse pointer"));
    mouse.set_active(cfg.capture_mouse);
    mouse.set_focusable(false);
    {
        let ctl = ctl.clone();
        mouse.connect_toggled(move |b| ctl.update_config(|c| c.capture_mouse = b.is_active()));
    }

    bx.append(&fmt);
    bx.append(&audio);
    bx.append(&mouse);
    bx
}

/// Run `f` once after `ms` milliseconds on the main loop.
pub fn after(ms: u64, f: impl FnOnce() + 'static) {
    glib::timeout_add_local_once(std::time::Duration::from_millis(ms), f);
}

/// Per-source stereo peak meters: an icon and stacked L/R bars for each
/// audio source. A source that stays silent fades out, so a wrong or dead
/// device is obvious at a glance.
pub struct Meters {
    pub root: gtk::Box,
    rows: RefCell<Vec<(gtk::Box, gtk::LevelBar, gtk::LevelBar)>>,
    silent_for: RefCell<Vec<u32>>,
    names: Cell<&'static [(&'static str, &'static str)]>,
}

impl Meters {
    pub fn new() -> Rc<Meters> {
        Rc::new(Meters {
            root: gtk::Box::new(gtk::Orientation::Horizontal, 8),
            rows: RefCell::new(Vec::new()),
            silent_for: RefCell::new(Vec::new()),
            names: Cell::new(&[]),
        })
    }

    /// Rebuild for the audio mode the given config will record with.
    pub fn configure(&self, cfg: &Config) {
        let mode = if cfg.format.has_audio() { cfg.audio } else { AudioMode::Off };
        while let Some(c) = self.root.first_child() {
            self.root.remove(&c);
        }
        let mut rows = Vec::new();
        for (icon, name) in mode.sources() {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            row.set_valign(gtk::Align::Center);
            row.add_css_class("peek2-meter-row");
            row.append(&gtk::Image::from_icon_name(icon));
            let bars = gtk::Box::new(gtk::Orientation::Vertical, 3);
            bars.set_valign(gtk::Align::Center);
            let mk = || {
                let b = gtk::LevelBar::new();
                b.add_css_class("peek2-meter");
                for o in ["low", "high", "full"] {
                    b.remove_offset_value(Some(o));
                }
                b.add_offset_value("meter-ok", 0.80);
                b.add_offset_value("meter-hot", 0.97);
                b.add_offset_value("meter-clip", 1.0);
                bars.append(&b);
                b
            };
            let (l, r) = (mk(), mk());
            row.append(&bars);
            row.set_tooltip_text(Some(&format!("{name}: left / right")));
            self.root.append(&row);
            rows.push((row, l, r));
        }
        self.root.set_visible(!rows.is_empty());
        *self.silent_for.borrow_mut() = vec![0; rows.len()];
        *self.rows.borrow_mut() = rows;
        self.names.set(mode.sources());
        self.reset();
    }

    pub fn reset(&self) {
        for (row, l, r) in self.rows.borrow().iter() {
            l.set_value(0.0);
            r.set_value(0.0);
            row.remove_css_class("peek2-meter-silent");
        }
    }

    pub fn set(&self, levels: &[f32]) {
        // -60 dBFS .. 0 dBFS → 0 .. 1
        let norm = |db: f32| if db.is_finite() { ((db + 60.0) / 60.0).clamp(0.0, 1.0) as f64 } else { 0.0 };
        let mut silent = self.silent_for.borrow_mut();
        for (i, (row, l, r)) in self.rows.borrow().iter().enumerate() {
            let (lv, rv) = (
                levels.get(2 * i).copied().unwrap_or(f32::NEG_INFINITY),
                levels.get(2 * i + 1).copied().unwrap_or(f32::NEG_INFINITY),
            );
            l.set_value(norm(lv));
            r.set_value(norm(rv));
            // ~2 s below -70 dBFS on both channels counts as silent.
            if lv.max(rv) < -70.0 {
                silent[i] = silent[i].saturating_add(1);
            } else {
                silent[i] = 0;
            }
            if silent[i] >= 20 {
                row.add_css_class("peek2-meter-silent");
            } else {
                row.remove_css_class("peek2-meter-silent");
            }
            let (_, name) = self.names.get().get(i).copied().unwrap_or(("", ""));
            let state = if silent[i] >= 20 { " (silent)" } else { "" };
            row.set_tooltip_text(Some(&format!("{name}: {lv:.0} / {rv:.0} dBFS{state}")));
        }
    }
}

/// Record indicator: a muted ring when idle, a red dot while recording.
pub fn rec_dot() -> gtk::Widget {
    let d = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    d.add_css_class("peek2-dot");
    d.add_css_class("idle");
    d.set_valign(gtk::Align::Center);
    d.set_halign(gtk::Align::Center);
    d.set_margin_start(4);
    d.set_margin_end(2);
    d.upcast()
}

pub fn set_rec_active(dot: &gtk::Widget, active: bool) {
    if active {
        dot.remove_css_class("idle");
    } else {
        dot.add_css_class("idle");
    }
}
