//! Flameshot-style region picker: a dimmed overlay over the monitor under
//! the pointer. Drag to select, click to pick a window, then record from the
//! toolbar under the selection.
//!
//! With a compositor the overlay is a see-through ARGB window; without one
//! it shows a frozen screenshot of the monitor, dimmed outside the selection.

use crate::config::{Config, FIT_PRESETS};
use crate::controller::Controller;
use crate::draw::{self, Item, Style, Surface};
use crate::meter::{AudioOpts, Levels, Preview};
use crate::recorder::Area;
use crate::x11::{self, cursor, keysym, Rect, X};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _, EventMask, ModMask};
use x11rb::protocol::Event;

const SCRIM: u32 = 0x060807;
const SCRIM_ALPHA: f64 = 0.62;

#[derive(Clone, Copy, Debug)]
enum Drag {
    None,
    /// Pressed, not yet moved far enough to count as a drag.
    Pending { x0: i32, y0: i32 },
    New { x0: i32, y0: i32 },
    Move { orig: Rect, x0: i32, y0: i32 },
    Resize { orig: Rect, fx: u8, fy: u8, x0: i32, y0: i32 },
}

/// Handle positions as (fx, fy) in halves: 0 = left/top, 1 = middle, 2 = right/bottom.
const HANDLES: [(u8, u8); 8] = [(0, 0), (1, 0), (2, 0), (2, 1), (2, 2), (1, 2), (0, 2), (0, 1)];

fn handle_pos(r: Rect, fx: u8, fy: u8) -> (i32, i32) {
    (r.x + r.width * fx as i32 / 2, r.y + r.height * fy as i32 / 2)
}

fn from_points(x0: i32, y0: i32, x1: i32, y1: i32) -> Rect {
    Rect::new(x0.min(x1), y0.min(y1), (x1 - x0).abs(), (y1 - y0).abs())
}

fn clamp_to(r: Rect, w: i32, h: i32) -> Rect {
    let x0 = r.x.clamp(0, w);
    let y0 = r.y.clamp(0, h);
    let x1 = (r.x + r.width).clamp(0, w);
    let y1 = (r.y + r.height).clamp(0, h);
    from_points(x0, y0, x1, y1)
}

pub struct Selector {
    x: Rc<X>,
    ctl: Rc<Controller>,
    win: u32,
    /// Monitor in root pixels; local (0,0) is its corner.
    mon: Rect,
    blend: bool,
    /// Screenshot mode: (bright, dimmed) pixmaps of the monitor.
    shot: Option<(u32, u32)>,
    surf: RefCell<Option<Surface>>,
    sel: Cell<Option<Rect>>,
    hover: Cell<Option<Rect>>,
    drag: Cell<Drag>,
    pointer: Cell<Option<(i32, i32)>>,
    /// Client windows in root pixels, topmost first, for click-to-pick.
    windows: Vec<Rect>,
    hits: RefCell<Vec<(&'static str, Rect)>>,
    hover_btn: Cell<Option<&'static str>>,
    pressed_btn: Cell<Option<&'static str>>,
    last_click: Cell<(u32, i32, i32)>,
    cursor: Cell<u16>,
    levels: RefCell<Levels>,
    preview: RefCell<Option<Preview>>,
    closed: Cell<bool>,
    on_done: RefCell<Option<Box<dyn FnOnce(Option<(Area, Rect)>)>>>,
}

impl Selector {
    /// `on_done` gets the chosen area and its monitor, or None if cancelled.
    pub fn open(ctl: &Rc<Controller>, on_done: impl FnOnce(Option<(Area, Rect)>) + 'static) -> Option<Rc<Selector>> {
        let x = x11::get();
        ctl.reload_config();
        let windows = x.client_rects();
        let mon = x.current_monitor();
        let blend = x.can_blend();
        let events = EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION | EventMask::KEY_PRESS;
        let (win, depth) = match x.popup(mon, blend, events) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("peek2: selector: {e:#}");
                return None;
            }
        };
        let shot = if blend { None } else { screenshot(&x, win, mon) };
        let this = Rc::new(Selector {
            x: x.clone(),
            ctl: ctl.clone(),
            win,
            mon,
            blend,
            shot,
            surf: RefCell::new(Surface::new(&x, win, depth, mon.width, mon.height)),
            sel: Cell::new(None),
            hover: Cell::new(None),
            drag: Cell::new(Drag::None),
            pointer: Cell::new(x.pointer().map(|(px, py)| (px - mon.x, py - mon.y))),
            windows,
            hits: RefCell::new(Vec::new()),
            hover_btn: Cell::new(None),
            pressed_btn: Cell::new(None),
            last_click: Cell::new((0, 0, 0)),
            cursor: Cell::new(0),
            levels: RefCell::new(Levels::default()),
            preview: RefCell::new(None),
            closed: Cell::new(false),
            on_done: RefCell::new(Some(Box::new(on_done))),
        });
        SELF.with(|s| *s.borrow_mut() = Some(this.clone()));
        let weak = Rc::downgrade(&this);
        x.on(win, move |ev| {
            if let Some(t) = weak.upgrade() {
                t.event(ev);
            }
        });
        this.set_cursor(cursor::CROSSHAIR);
        x.map(win);
        x.raise(win);
        this.restart_preview();
        this.hover_at_pointer();
        this.redraw();
        // The keyboard can only be grabbed once the window is viewable, and
        // not while another client (e.g. the hotkey daemon) holds a grab.
        let weak = Rc::downgrade(&this);
        let tries = Cell::new(0);
        crate::rt::timeout(std::time::Duration::from_millis(20), move || {
            let Some(t) = weak.upgrade() else { return false };
            if t.closed.get() {
                return false;
            }
            tries.set(tries.get() + 1);
            if t.x.grab_input(t.win, true) {
                return false;
            }
            if tries.get() > 50 {
                eprintln!("peek2: could not grab the keyboard; Esc and Enter won't work");
                return false;
            }
            true
        });
        Some(this)
    }

    fn size(&self) -> (i32, i32) {
        (self.mon.width, self.mon.height)
    }

    fn set_cursor(&self, shape: u16) {
        if self.cursor.replace(shape) != shape {
            self.x.set_cursor(self.win, shape);
        }
    }

    fn restart_preview(self: &Rc<Self>) {
        self.preview.borrow_mut().take();
        let cfg = self.ctl.config.borrow().clone();
        self.levels.borrow_mut().configure(&cfg);
        let sources = crate::audio::resolve(&cfg);
        let opts = AudioOpts::new(&cfg, sources.len());
        let levels = Rc::downgrade(self);
        *self.preview.borrow_mut() = Preview::start(&sources, opts, move |l| {
            if let Some(t) = levels.upgrade() {
                t.levels.borrow_mut().set(&l);
                if t.sel.get().is_some() {
                    t.redraw();
                }
            }
        });
    }

    // ---- input -------------------------------------------------------------

    fn event(self: &Rc<Self>, ev: &Event) {
        match ev {
            Event::Expose(e) if e.count == 0 => self.present(),
            Event::MotionNotify(e) => self.motion(e.event_x as i32, e.event_y as i32),
            Event::ButtonPress(e) => self.press(e.detail, e.event_x as i32, e.event_y as i32, e.time),
            Event::ButtonRelease(e) if e.detail == 1 => self.release(e.event_x as i32, e.event_y as i32),
            Event::KeyPress(e) => self.key(e.detail, e.state.into()),
            _ => {}
        }
    }

    fn press(self: &Rc<Self>, button: u8, x: i32, y: i32, time: u32) {
        let btn = draw::hit(&self.hits.borrow(), x, y);
        match button {
            1 if btn.is_some() => self.pressed_btn.set(btn),
            1 => {
                let (t0, x0, y0) = self.last_click.replace((time, x, y));
                let double = time.wrapping_sub(t0) < 400 && (x - x0).abs() < 5 && (y - y0).abs() < 5;
                if double && self.sel.get().is_some_and(|r| r.contains(x, y)) {
                    return self.record();
                }
                let d = match self.sel.get() {
                    Some(r) => match self.handle_at(r, x, y) {
                        Some((fx, fy)) => Drag::Resize { orig: r, fx, fy, x0: x, y0: y },
                        None if r.contains(x, y) => Drag::Move { orig: r, x0: x, y0: y },
                        None => Drag::Pending { x0: x, y0: y },
                    },
                    None => Drag::Pending { x0: x, y0: y },
                };
                self.drag.set(d);
            }
            3 if btn == Some("fit") => {
                self.ctl.update_config(|c| c.fit_strategy = c.fit_strategy.next());
                self.redraw();
            }
            3 => {
                self.sel.set(None);
                self.hover_at(x, y);
                self.redraw();
            }
            _ => {}
        }
    }

    fn handle_at(&self, r: Rect, x: i32, y: i32) -> Option<(u8, u8)> {
        let reach = 8 * self.x.ui_scale();
        HANDLES.into_iter().find(|&(fx, fy)| {
            let (hx, hy) = handle_pos(r, fx, fy);
            (x - hx).abs() <= reach && (y - hy).abs() <= reach
        })
    }

    fn motion(&self, x: i32, y: i32) {
        self.pointer.set(Some((x, y)));
        let (w, h) = self.size();
        match self.drag.get() {
            Drag::Pending { x0, y0 } => {
                if (x - x0).abs() + (y - y0).abs() >= 4 {
                    self.drag.set(Drag::New { x0, y0 });
                    return self.motion(x, y);
                }
            }
            Drag::New { x0, y0 } => {
                self.sel.set(Some(clamp_to(from_points(x0, y0, x, y), w, h)));
                self.hover.set(None);
            }
            Drag::Move { orig, x0, y0 } => {
                let nx = (orig.x + x - x0).clamp(0, (w - orig.width).max(0));
                let ny = (orig.y + y - y0).clamp(0, (h - orig.height).max(0));
                self.sel.set(Some(Rect { x: nx, y: ny, ..orig }));
            }
            Drag::Resize { orig, fx, fy, x0, y0 } => {
                let (dx, dy) = (x - x0, y - y0);
                let (mut l, mut t, mut r, mut b) = (orig.x, orig.y, orig.x + orig.width, orig.y + orig.height);
                match fx {
                    0 => l += dx,
                    2 => r += dx,
                    _ => {}
                }
                match fy {
                    0 => t += dy,
                    2 => b += dy,
                    _ => {}
                }
                self.sel.set(Some(clamp_to(from_points(l, t, r, b), w, h)));
            }
            Drag::None => {
                let btn = draw::hit(&self.hits.borrow(), x, y);
                self.hover_btn.set(btn);
                let shape = match (btn, self.sel.get()) {
                    (Some(_), _) => cursor::LEFT_PTR,
                    (None, Some(r)) => match self.handle_at(r, x, y) {
                        Some((0, 0)) => cursor::TOP_LEFT,
                        Some((2, 2)) => cursor::BOTTOM_RIGHT,
                        Some((2, 0)) => cursor::TOP_RIGHT,
                        Some((0, 2)) => cursor::BOTTOM_LEFT,
                        Some((1, _)) => cursor::V_DOUBLE,
                        Some(_) => cursor::H_DOUBLE,
                        None if r.contains(x, y) => cursor::FLEUR,
                        None => cursor::CROSSHAIR,
                    },
                    (None, None) => {
                        self.hover_at(x, y);
                        cursor::CROSSHAIR
                    }
                };
                self.set_cursor(shape);
            }
        }
        self.redraw();
    }

    fn release(self: &Rc<Self>, x: i32, y: i32) {
        if let Some(b) = self.pressed_btn.take() {
            if draw::hit(&self.hits.borrow(), x, y) == Some(b) {
                self.button(b);
            }
            return;
        }
        match self.drag.replace(Drag::None) {
            // A click: pick the hovered window, if any.
            Drag::Pending { .. } => {
                if self.sel.get().is_none() {
                    self.hover_at(x, y);
                    if let Some(r) = self.hover.get() {
                        self.sel.set(Some(r));
                    }
                } else {
                    // Click outside the selection starts over.
                    self.sel.set(None);
                    self.hover_at(x, y);
                }
            }
            Drag::New { .. }
                if self.sel.get().is_some_and(|r| r.width < 2 || r.height < 2) => {
                    self.sel.set(None);
                }
            _ => {}
        }
        self.redraw();
    }

    fn button(self: &Rc<Self>, id: &str) {
        match id {
            "record" => return self.record(),
            "cancel" => return self.close(),
            "fmt" => self.ctl.update_config(|c| c.format = c.format.next()),
            "audio" => self.ctl.update_config(|c| c.audio = c.audio.next()),
            "mouse" => self.ctl.update_config(|c| c.capture_mouse = !c.capture_mouse),
            "fit" => self.ctl.update_config(|c| c.fit_size_mb = next_fit(c.fit_size_mb)),
            _ => {}
        }
        if matches!(id, "fmt" | "audio") {
            self.restart_preview();
        }
        self.redraw();
    }

    fn key(self: &Rc<Self>, code: u8, state: u16) {
        if self.x.is_hotkey(code, state).is_some() {
            return self.record();
        }
        let sym = self.x.keysym(code, 0);
        let shift = state & u16::from(ModMask::SHIFT) != 0;
        let ctrl = state & u16::from(ModMask::CONTROL) != 0;
        let step = if shift { 10 } else { 1 };
        let (w, h) = self.size();
        match sym {
            keysym::ESCAPE => return self.close(),
            keysym::RETURN | keysym::KP_ENTER => return self.record(),
            s if s == keysym::SPACE || s == 'f' as u32 => {
                // Whole monitor.
                self.sel.set(Some(Rect::new(0, 0, w, h)));
            }
            keysym::LEFT | keysym::RIGHT | keysym::UP | keysym::DOWN => {
                let Some(mut r) = self.sel.get() else { return };
                let (dx, dy) = match sym {
                    keysym::LEFT => (-step, 0),
                    keysym::RIGHT => (step, 0),
                    keysym::UP => (0, -step),
                    _ => (0, step),
                };
                if ctrl {
                    r.width = (r.width + dx).max(2);
                    r.height = (r.height + dy).max(2);
                } else {
                    r.x = (r.x + dx).clamp(0, (w - r.width).max(0));
                    r.y = (r.y + dy).clamp(0, (h - r.height).max(0));
                }
                self.sel.set(Some(clamp_to(r, w, h)));
            }
            _ => return,
        }
        self.redraw();
    }

    fn hover_at_pointer(&self) {
        if let Some((x, y)) = self.pointer.get() {
            self.hover_at(x, y);
        }
    }

    /// Topmost client window under the pointer, clipped to this monitor.
    fn hover_at(&self, x: i32, y: i32) {
        let (rx, ry) = (self.mon.x + x, self.mon.y + y);
        let r = self.windows.iter().find(|w| w.contains(rx, ry)).map(|w| {
            let (mw, mh) = self.size();
            clamp_to(Rect::new(w.x - self.mon.x, w.y - self.mon.y, w.width, w.height), mw, mh)
        });
        self.hover.set(r);
    }

    // ---- drawing -------------------------------------------------------

    fn toolbar_items(&self, r: Rect) -> Vec<Item> {
        let cfg: Config = self.ctl.config.borrow().clone();
        let mut v = vec![
            Item::Dot(false),
            Item::Text(format!("{:>4} x {:<4}", r.width, r.height), draw::MUTED),
            Item::Button { id: "fmt", label: cfg.format.label().into(), style: Style::Normal },
        ];
        let audio = if cfg.format.has_audio() { cfg.audio.label() } else { "no audio" };
        v.push(Item::Button { id: "audio", label: audio.into(), style: Style::Normal });
        v.push(Item::Button { id: "mouse", label: if cfg.capture_mouse { "mouse" } else { "no mouse" }.into(), style: Style::Normal });
        let fit = match cfg.fit_size_mb {
            0 => "any size".to_string(),
            n => format!("max {n}MB {}", cfg.fit_strategy.key()),
        };
        v.push(Item::Button { id: "fit", label: fit, style: Style::Normal });
        if let Some(m) = self.levels.borrow().item() {
            v.push(m);
        }
        v.push(Item::Button { id: "record", label: "record".into(), style: Style::Primary });
        v.push(Item::Button { id: "cancel", label: "x".into(), style: Style::Flat });
        v
    }

    fn redraw(&self) {
        if self.closed.get() {
            return;
        }
        let surf = self.surf.borrow();
        let Some(s) = surf.as_ref() else { return };
        let (w, h) = self.size();
        let full = Rect::new(0, 0, w, h);
        let sc = self.x.ui_scale();
        let f = draw::font();

        // Backdrop: translucent scrim, or the dimmed screenshot.
        match self.shot {
            Some((_, dim)) => s.copy_from(dim, 0, 0, full),
            None if self.blend => {
                s.clear(full);
                s.fill(full, SCRIM, SCRIM_ALPHA);
            }
            None => s.fill(full, SCRIM, 1.0),
        }

        let sel = self.sel.get();
        let dragging = !matches!(self.drag.get(), Drag::None | Drag::Pending { .. });

        // Hairline guides through the pointer until something is selected.
        if sel.is_none() && !dragging
            && let Some((px, py)) = self.pointer.get() {
                let (ink, a) = if self.blend { (draw::FG_HIGH, 0.22) } else { (0x4a4640, 1.0) };
                s.line(0, py, w, py, ink, a);
                s.line(px, 0, px, h, ink, a);
                tag(s, px + 12 * sc, py + 12 * sc, &format!("{} {}", self.mon.x + px, self.mon.y + py), draw::MUTED);
            }

        let target = sel.or(self.hover.get());
        if let Some(r) = target {
            // The hole shows the live screen (or the bright screenshot).
            match self.shot {
                Some((bright, _)) => s.copy_from(bright, r.x, r.y, r),
                None if self.blend => s.clear(r),
                None => {}
            }
            if sel.is_some() {
                // 1px cream keyline just outside the capture, square handles.
                s.stroke(Rect::new(r.x - 1, r.y - 1, r.width + 2, r.height + 2), draw::FG_HIGH);
                let hs = 3 * sc;
                for (fx, fy) in HANDLES {
                    let (hx, hy) = handle_pos(r, fx, fy);
                    s.fill(Rect::new(hx - hs, hy - hs, 2 * hs, 2 * hs), draw::FG_HIGH, 1.0);
                }
            } else {
                s.stroke(r, draw::GOLD);
            }
            let label = format!("{} x {}", r.width, r.height);
            let th = f.height() + 6 * sc;
            let (lx, ly) = if r.y >= th + 4 * sc { (r.x, r.y - th - 4 * sc) } else { (r.x + 6 * sc, r.y + 6 * sc) };
            tag(s, lx.max(0), ly, &label, draw::FG_HIGH);
        }

        if sel.is_none() {
            hint(s, w);
        }

        let mut hits = Vec::new();
        if let (Some(r), false) = (sel, dragging)
            && r.width >= 2 && r.height >= 2 {
                let items = self.toolbar_items(r);
                let (tw, th) = draw::bar_size(&items);
                let gap = 6 * sc;
                let tx = (r.x + r.width / 2 - tw / 2).clamp(0, (w - tw).max(0));
                let ty = if r.y + r.height + gap + th <= h {
                    r.y + r.height + gap
                } else if r.y - gap - th >= 0 {
                    r.y - gap - th
                } else {
                    (r.y + r.height - th - gap).max(0)
                };
                hits = draw::draw_bar(s, tx, ty, &items, self.hover_btn.get());
            }
        *self.hits.borrow_mut() = hits;
        s.present(self.win);
    }

    fn present(&self) {
        if let Some(s) = self.surf.borrow().as_ref() {
            s.present(self.win);
        }
    }

    // ---- result --------------------------------------------------------

    pub fn record(self: &Rc<Self>) {
        let Some(r) = self.sel.get() else { return };
        let area = Area { x: self.mon.x + r.x, y: self.mon.y + r.y, width: r.width, height: r.height };
        if !area.is_valid() {
            return;
        }
        self.finish(Some((area, self.mon)));
    }

    pub fn close(&self) {
        self.finish(None);
    }

    fn finish(&self, result: Option<(Area, Rect)>) {
        if self.closed.replace(true) {
            return;
        }
        self.preview.borrow_mut().take();
        self.x.ungrab();
        self.x.destroy(self.win);
        self.surf.borrow_mut().take();
        if let Some((a, b)) = self.shot {
            let _ = self.x.conn.free_pixmap(a);
            let _ = self.x.conn.free_pixmap(b);
        }
        self.x.flush();
        SELF.with(|s| s.borrow_mut().take());
        if let Some(f) = self.on_done.borrow_mut().take() {
            // Let the server unmap the overlay before anything is captured.
            crate::rt::idle(move || f(result));
        }
    }
}

thread_local! {
    /// The open selector (there is at most one).
    static SELF: RefCell<Option<Rc<Selector>>> = const { RefCell::new(None) };
}

/// Open the selector unless one is open already.
pub fn open(ctl: &Rc<Controller>, on_done: impl FnOnce(Option<(Area, Rect)>) + 'static) {
    if current().is_none() {
        Selector::open(ctl, on_done);
    }
}

pub fn current() -> Option<Rc<Selector>> {
    SELF.with(|s| s.borrow().clone())
}

fn next_fit(cur: u32) -> u32 {
    let i = FIT_PRESETS.iter().position(|p| *p == cur);
    match i {
        Some(i) => FIT_PRESETS[(i + 1) % FIT_PRESETS.len()],
        None => 0,
    }
}

/// A square label box: elev-1 fill, 1px line, monospace text.
fn tag(s: &Surface, x: i32, y: i32, text: &str, ink: u32) {
    let f = draw::font();
    let sc = crate::x11::get().ui_scale();
    let (pw, ph) = (7 * sc, 3 * sc);
    let r = Rect::new(x, y, f.text_width(text) + 2 * pw, f.height() + 2 * ph);
    s.fill(r, draw::ELEV1, 0.94);
    s.stroke(r, draw::LINE);
    s.text(x + pw, y + ph, text, ink);
}

fn hint(s: &Surface, w: i32) {
    let keys = [("drag", "region"), ("click", "window"), ("space", "screen"), ("enter", "record"), ("esc", "cancel")];
    let f = draw::font();
    let sc = crate::x11::get().ui_scale();
    let gap = 3 * f.width;
    let lead = 20 * sc;
    let content: i32 = keys.iter().map(|(k, v)| f.text_width(k) + f.width + f.text_width(v)).sum::<i32>() + gap * (keys.len() as i32 - 1);
    let (bw, bh) = (lead + content + 28 * sc, f.height() + 24 * sc);
    let (bx, by) = ((w - bw) / 2, 40 * sc);
    let r = Rect::new(bx, by, bw, bh);
    s.fill(r, draw::ELEV1, 0.96);
    s.stroke(r, draw::LINE);
    s.circle(bx + 18 * sc, by + bh / 2, 4 * sc, draw::RED, true);
    let ty = by + (bh - f.height()) / 2;
    let mut x = bx + 14 * sc + lead;
    for (k, v) in keys {
        s.text(x, ty, k, draw::GOLD);
        x += f.text_width(k) + f.width;
        s.text(x, ty, v, draw::MUTED);
        x += f.text_width(v) + gap;
    }
}

/// Grab the monitor once (compositor-less setups) into two server-side
/// pixmaps: as is, and dimmed like the scrim.
fn screenshot(x: &Rc<X>, win: u32, mon: Rect) -> Option<(u32, u32)> {
    let data = x.screenshot(mon)?;
    let a = SCRIM_ALPHA;
    let keep = 1.0 - a;
    let sc = [(SCRIM & 0xff) as f64 * a, ((SCRIM >> 8) & 0xff) as f64 * a, ((SCRIM >> 16) & 0xff) as f64 * a];
    let mut dim = data.clone();
    for px in dim.as_chunks_mut::<4>().0 {
        for k in 0..3 {
            px[k] = (px[k] as f64 * keep + sc[k]).round() as u8;
        }
    }
    let (w, h) = (mon.width as u16, mon.height as u16);
    let make = |bytes: &[u8]| -> Option<u32> {
        let p = x.conn.generate_id().ok()?;
        x.conn.create_pixmap(x.root_depth, p, win, w, h).ok()?;
        let gc = x.conn.generate_id().ok()?;
        x.conn.create_gc(gc, p, &xproto::CreateGCAux::new()).ok()?;
        x.put_image(p, gc, w, h, x.root_depth, bytes);
        let _ = x.conn.free_gc(gc);
        Some(p)
    };
    let shot = (make(&data)?, make(&dim)?);
    let _ = x.conn.flush();
    Some(shot)
}
