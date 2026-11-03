//! Right-click menu for the tray icon: a small override-redirect window at
//! the pointer (the XEmbed tray has no menu protocol of its own). Settings
//! rows cycle in place and keep the menu open; actions close it. Closes on
//! Escape or a click outside.

use crate::config::FIT_PRESETS;
use crate::controller::{Controller, Phase};
use crate::draw::{self, Surface};
use crate::x11::{self, keysym, Rect, X};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use x11rb::protocol::xproto::EventMask;
use x11rb::protocol::Event;

enum Row {
    Heading(String),
    Item { id: String, label: String, hint: String },
    Sep,
}

struct Menu {
    x: Rc<X>,
    ctl: Rc<Controller>,
    win: u32,
    depth: u8,
    surf: RefCell<Option<Surface>>,
    rows: RefCell<Vec<Row>>,
    rects: RefCell<Vec<(String, Rect)>>,
    hover: RefCell<Option<String>>,
    origin: (i32, i32),
    on_action: Rc<dyn Fn(&str)>,
    closed: Cell<bool>,
}

thread_local! {
    static OPEN: RefCell<Option<Rc<Menu>>> = const { RefCell::new(None) };
}

/// Newest clips in the save folder.
fn recent(ctl: &Controller, n: usize) -> Vec<PathBuf> {
    let dir = ctl.config.borrow().save_dir();
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| matches!(x.to_str(), Some("webm" | "mp4" | "gif" | "apng"))))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files.into_iter().take(n).map(|(_, p)| p).collect()
}

fn ellipsize(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let head: String = s.chars().take(keep / 2).collect();
    let tail: String = s.chars().skip(n - (keep - keep / 2)).collect();
    format!("{head}~{tail}")
}

fn rows(ctl: &Controller) -> Vec<Row> {
    let cfg = ctl.config.borrow().clone();
    let item = |id: &str, label: &str, hint: &str| Row::Item { id: id.into(), label: label.into(), hint: hint.into() };
    let mut v = vec![Row::Heading("RECORD".into())];
    match ctl.phase() {
        Phase::Idle => {
            v.push(item("select", "Select area…", ""));
            v.push(item("screenshot", "Screenshot…", ""));
        }
        Phase::Recording(_) | Phase::Countdown(_) => {
            v.push(item("stop", "Stop and save", ""));
            v.push(item("pause", "Pause", ""));
            v.push(item("cancel", "Discard", ""));
        }
        Phase::Paused(_) => {
            v.push(item("pause", "Resume", ""));
            v.push(item("stop", "Stop and save", ""));
            v.push(item("cancel", "Discard", ""));
        }
        Phase::Processing => v.push(item("cancel", "Cancel saving", "")),
    }
    if ctl.replay_active() {
        v.push(item("save-replay", "Save replay", &format!("last {}s", cfg.replay_seconds)));
        v.push(item("replay", "Stop replay buffer", ""));
    } else {
        v.push(item("replay", "Start replay buffer", ""));
    }
    v.push(Row::Sep);
    v.push(Row::Heading("SETTINGS".into()));
    v.push(item("set:format", "Format", cfg.format.label()));
    let audio = if cfg.format.has_audio() { cfg.audio.label() } else { "n/a" };
    v.push(item("set:audio", "Audio", audio));
    v.push(item("set:mouse", "Mouse pointer", if cfg.capture_mouse { "shown" } else { "hidden" }));
    let fit = if cfg.fit_size_mb == 0 { "any".to_string() } else { format!("{} MB", cfg.fit_size_mb) };
    v.push(item("set:fit", "Max file size", &fit));
    if cfg.fit_size_mb > 0 {
        v.push(item("set:strategy", "Shrink by", cfg.fit_strategy.key()));
    }
    let recent = recent(ctl, 5);
    if !recent.is_empty() {
        v.push(Row::Sep);
        v.push(Row::Heading("RECENT".into()));
        for p in recent {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            v.push(Row::Item { id: format!("open:{}", p.display()), label: ellipsize(&name, 34), hint: String::new() });
        }
    }
    v.push(Row::Sep);
    v.push(item("open-folder", "Open folder", ""));
    v.push(item("preferences", "Edit settings file", ""));
    v.push(item("quit", "Quit", ""));
    v
}

pub fn close() {
    if let Some(m) = OPEN.with(|o| o.borrow_mut().take()) {
        m.close();
    }
}

/// Toggle the menu at the pointer. `on_action(id)` runs the picked action.
pub fn show(ctl: &Rc<Controller>, on_action: impl Fn(&str) + 'static) {
    if OPEN.with(|o| o.borrow().is_some()) {
        return close();
    }
    let x = x11::get();
    ctl.reload_config();
    let origin = x.pointer().unwrap_or((0, 0));
    let events = EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION | EventMask::KEY_PRESS;
    let Ok((win, depth)) = x.popup(Rect::new(origin.0, origin.1, 10, 10), false, events) else { return };
    let menu = Rc::new(Menu {
        x: x.clone(),
        ctl: ctl.clone(),
        win,
        depth,
        surf: RefCell::new(None),
        rows: RefCell::new(rows(ctl)),
        rects: RefCell::new(Vec::new()),
        hover: RefCell::new(None),
        origin,
        on_action: Rc::new(on_action),
        closed: Cell::new(false),
    });
    let weak = Rc::downgrade(&menu);
    x.on(win, move |ev| {
        if let Some(m) = weak.upgrade() {
            m.event(ev);
        }
    });
    x.set_cursor(win, x11::cursor::LEFT_PTR);
    menu.layout(true);
    x.map(win);
    let weak = Rc::downgrade(&menu);
    let tries = Cell::new(0);
    crate::rt::timeout(std::time::Duration::from_millis(20), move || {
        let Some(m) = weak.upgrade() else { return false };
        tries.set(tries.get() + 1);
        !m.closed.get() && !m.x.grab_input(m.win, true) && tries.get() < 25
    });
    OPEN.with(|o| *o.borrow_mut() = Some(menu));
}

impl Menu {
    fn metrics(&self) -> (i32, i32, i32) {
        let f = draw::font();
        let s = self.x.ui_scale();
        (f.height() + 8 * s, 10 * s, 7 * s) // row height, side padding, separator height
    }

    fn size(&self) -> (i32, i32) {
        let f = draw::font();
        let (rh, pad, sep) = self.metrics();
        let mut w = 0;
        let mut h = 2 * self.x.ui_scale();
        for r in self.rows.borrow().iter() {
            match r {
                Row::Heading(t) => {
                    w = w.max(f.text_width(t));
                    h += rh;
                }
                Row::Item { label, hint, .. } => {
                    w = w.max(f.text_width(label) + if hint.is_empty() { 0 } else { 3 * f.width + f.text_width(hint) });
                    h += rh;
                }
                Row::Sep => h += sep,
            }
        }
        (w.max(22 * f.width) + 2 * pad, h)
    }

    /// Size the window to its rows; `place` positions it at the pointer.
    fn layout(&self, place: bool) {
        let (w, h) = self.size();
        if self.surf.borrow().as_ref().is_none_or(|s| (s.w, s.h) != (w, h)) {
            *self.surf.borrow_mut() = Surface::new(&self.x, self.win, self.depth, w, h);
        }
        let mon = self.x.monitors().into_iter().find(|m| m.contains(self.origin.0, self.origin.1)).unwrap_or_else(|| self.x.root_rect());
        // Open towards the inside of the screen (trays sit at the edges).
        let (px, py) = self.origin;
        let x = if px + w > mon.x + mon.width { px - w } else { px };
        let y = if py + h > mon.y + mon.height { py - h } else { py };
        let x = x.clamp(mon.x, (mon.x + mon.width - w).max(mon.x));
        let y = y.clamp(mon.y, (mon.y + mon.height - h).max(mon.y));
        let _ = place;
        self.x.move_resize(self.win, Rect::new(x, y, w, h));
        self.redraw();
    }

    fn redraw(&self) {
        let surf = self.surf.borrow();
        let Some(s) = surf.as_ref() else { return };
        let f = draw::font();
        let (rh, pad, sep) = self.metrics();
        let full = Rect::new(0, 0, s.w, s.h);
        s.fill(full, draw::ELEV1, 1.0);
        s.stroke(full, draw::LINE);
        let hover = self.hover.borrow().clone();
        let mut rects = Vec::new();
        let mut y = self.x.ui_scale();
        for r in self.rows.borrow().iter() {
            match r {
                Row::Heading(t) => {
                    s.text(pad, y + (rh - f.height()) / 2, t, draw::GOLD);
                    y += rh;
                }
                Row::Item { id, label, hint } => {
                    let rect = Rect::new(1, y, s.w - 2, rh);
                    let hot = hover.as_deref() == Some(id.as_str());
                    if hot {
                        s.fill(rect, draw::ELEV2, 1.0);
                        s.fill(Rect::new(1, y, 2 * self.x.ui_scale(), rh), draw::GOLD, 1.0);
                    }
                    let ty = y + (rh - f.height()) / 2;
                    s.text(pad, ty, label, if hot { draw::FG_HIGH } else { draw::FG });
                    if !hint.is_empty() {
                        let ink = if id.starts_with("set:") { draw::GOLD } else { draw::MUTED };
                        s.text(s.w - pad - f.text_width(hint), ty, hint, ink);
                    }
                    rects.push((id.clone(), rect));
                    y += rh;
                }
                Row::Sep => {
                    s.fill(Rect::new(pad, y + sep / 2, s.w - 2 * pad, 1), draw::LINE_SOFT, 1.0);
                    y += sep;
                }
            }
        }
        *self.rects.borrow_mut() = rects;
        s.present(self.win);
    }

    fn row_at(&self, x: i32, y: i32) -> Option<String> {
        self.rects.borrow().iter().find(|(_, r)| r.contains(x, y)).map(|(id, _)| id.clone())
    }

    fn inside(&self, x: i32, y: i32) -> bool {
        self.surf.borrow().as_ref().is_some_and(|s| x >= 0 && y >= 0 && x < s.w && y < s.h)
    }

    fn event(self: &Rc<Self>, ev: &Event) {
        match ev {
            Event::Expose(e) if e.count == 0 => self.redraw(),
            Event::MotionNotify(e) => {
                let h = self.row_at(e.event_x as i32, e.event_y as i32);
                if h != *self.hover.borrow() {
                    *self.hover.borrow_mut() = h;
                    self.redraw();
                }
            }
            Event::ButtonPress(e) if !self.inside(e.event_x as i32, e.event_y as i32) => close(),
            Event::ButtonRelease(e) => {
                if let Some(id) = self.row_at(e.event_x as i32, e.event_y as i32) {
                    self.pick(&id);
                }
            }
            Event::KeyPress(e) if self.x.keysym(e.detail, 0) == keysym::ESCAPE => close(),
            _ => {}
        }
    }

    fn pick(self: &Rc<Self>, id: &str) {
        let ctl = &self.ctl;
        let setting = match id {
            "set:format" => {
                ctl.update_config(|c| c.format = c.format.next());
                true
            }
            "set:audio" => {
                ctl.update_config(|c| c.audio = c.audio.next());
                true
            }
            "set:mouse" => {
                ctl.update_config(|c| c.capture_mouse = !c.capture_mouse);
                true
            }
            "set:fit" => {
                ctl.update_config(|c| {
                    let i = FIT_PRESETS.iter().position(|p| *p == c.fit_size_mb);
                    c.fit_size_mb = i.map(|i| FIT_PRESETS[(i + 1) % FIT_PRESETS.len()]).unwrap_or(0);
                });
                true
            }
            "set:strategy" => {
                ctl.update_config(|c| c.fit_strategy = c.fit_strategy.next());
                true
            }
            _ => false,
        };
        if setting {
            *self.rows.borrow_mut() = rows(ctl);
            self.layout(false);
            return;
        }
        let action = self.on_action.clone();
        close();
        let id = id.to_string();
        // Run after the menu is gone (and the grab released).
        crate::rt::idle(move || action(&id));
    }

    fn close(&self) {
        if self.closed.replace(true) {
            return;
        }
        self.x.ungrab();
        self.surf.borrow_mut().take();
        self.x.destroy(self.win);
    }
}
