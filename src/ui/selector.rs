//! Flameshot-style region picker: one dimmed overlay per monitor. Drag to
//! select, click to pick a window, then record from the floating toolbar.

use super::{after, capture_controls, rec_dot, Meters};
use crate::meter::Preview;
use crate::controller::Controller;
use crate::recorder::Area;
use crate::ui::frame::RecordFrame;
use crate::x11;
use gtk::{cairo, gdk, gdk_pixbuf::Pixbuf, glib, prelude::*};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

// zk.poker palette
const CREAM: (f64, f64, f64) = (0.949, 0.925, 0.875); // fg-high #f2ecdf
const GOLD: (f64, f64, f64) = (0.851, 0.643, 0.255); // #d9a441
const RED: (f64, f64, f64) = (0.780, 0.243, 0.227); // #c73e3a
const ELEV1: (f64, f64, f64) = (0.059, 0.082, 0.075); // #0f1513
const LINE: (f64, f64, f64) = (0.165, 0.220, 0.200); // #2a3833
const MUTED: (f64, f64, f64) = (0.580, 0.541, 0.475); // #948a79
const FONT: &str = "Iosevka Nerd Font";
const HANDLE: f64 = 8.0;

/// Rectangle in an overlay's local (logical) coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
struct R {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl R {
    fn from_points(x0: f64, y0: f64, x1: f64, y1: f64) -> R {
        R { x: x0.min(x1), y: y0.min(y1), w: (x1 - x0).abs(), h: (y1 - y0).abs() }
    }

    fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && y >= self.y && x <= self.x + self.w && y <= self.y + self.h
    }

    fn clamp_to(&self, w: f64, h: f64) -> R {
        let x0 = self.x.clamp(0.0, w);
        let y0 = self.y.clamp(0.0, h);
        let x1 = (self.x + self.w).clamp(0.0, w);
        let y1 = (self.y + self.h).clamp(0.0, h);
        R::from_points(x0, y0, x1, y1)
    }

    /// Handle anchors as fractions of width/height.
    fn handles(&self) -> [(f64, f64, f64, f64); 8] {
        let pts = [(0.0, 0.0), (0.5, 0.0), (1.0, 0.0), (1.0, 0.5), (1.0, 1.0), (0.5, 1.0), (0.0, 1.0), (0.0, 0.5)];
        pts.map(|(fx, fy)| (self.x + self.w * fx, self.y + self.h * fy, fx, fy))
    }

    fn handle_at(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        self.handles()
            .into_iter()
            .find(|(hx, hy, _, _)| (x - hx).abs() <= HANDLE && (y - hy).abs() <= HANDLE)
            .map(|(_, _, fx, fy)| (fx, fy))
    }
}

#[derive(Clone, Copy, Debug)]
enum Drag {
    None,
    New { x0: f64, y0: f64 },
    Move { orig: R },
    Resize { orig: R, fx: f64, fy: f64 },
}

struct Overlay {
    window: gtk::Window,
    area: gtk::DrawingArea,
    overlay: gtk::Overlay,
    /// Monitor geometry in logical pixels.
    geo: gdk::Rectangle,
    scale: i32,
}

impl Overlay {
    fn root_rect(&self) -> x11::Rect {
        let s = self.scale;
        x11::Rect {
            x: self.geo.x() * s,
            y: self.geo.y() * s,
            width: self.geo.width() * s,
            height: self.geo.height() * s,
        }
    }

    fn size(&self) -> (f64, f64) {
        (self.geo.width() as f64, self.geo.height() as f64)
    }
}

pub struct Selector {
    ctl: Rc<Controller>,
    overlays: RefCell<Vec<Overlay>>,
    toolbar: gtk::Box,
    size_label: gtk::Label,
    sel: Cell<Option<(usize, R)>>,
    hover: Cell<Option<(usize, R)>>,
    drag: Cell<Drag>,
    /// Last pointer position (overlay index, x, y) for the guide lines.
    pointer: Cell<Option<(usize, f64, f64)>>,
    /// Client windows in root pixels, topmost first, for click-to-pick.
    windows: Vec<x11::Rect>,
    /// Frozen screenshot used when no compositor can make overlays translucent.
    shot: Option<Pixbuf>,
    closed: Cell<bool>,
    meters: Rc<Meters>,
    /// Live input levels before recording, to check the audio source.
    preview: RefCell<Option<Preview>>,
}

impl Selector {
    pub fn open(ctl: &Rc<Controller>) -> Rc<Selector> {
        let x = x11::get();
        let windows = x.as_ref().map(|x| x.client_rects(&[])).unwrap_or_default();
        let shot = match &x {
            Some(x) if !x.compositor_running() => screenshot(x),
            _ => None,
        };

        let size_label = gtk::Label::new(None);
        size_label.add_css_class("peek2-dim");
        size_label.set_width_chars(11);
        let toolbar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .css_classes(["peek2-toolbar"])
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Start)
            .visible(false)
            .build();

        let this = Rc::new(Selector {
            ctl: ctl.clone(),
            overlays: RefCell::new(Vec::new()),
            toolbar,
            size_label,
            sel: Cell::new(None),
            hover: Cell::new(None),
            drag: Cell::new(Drag::None),
            pointer: Cell::new(None),
            windows,
            shot,
            closed: Cell::new(false),
            meters: Meters::new(),
            preview: RefCell::new(None),
        });
        this.build_toolbar();
        this.restart_preview();

        let Some(display) = gdk::Display::default() else { return this };
        // Open on the monitor under the pointer, like Flameshot's per-screen mode.
        let monitors = display.monitors();
        let pointer = x.as_ref().and_then(|x| x.pointer());
        let all: Vec<gdk::Monitor> =
            (0..monitors.n_items()).filter_map(|i| monitors.item(i).and_downcast::<gdk::Monitor>()).collect();
        let current = all
            .iter()
            .find(|m| {
                let (g, s) = (m.geometry(), m.scale_factor().max(1));
                pointer.is_some_and(|(px, py)| {
                    x11::Rect { x: g.x() * s, y: g.y() * s, width: g.width() * s, height: g.height() * s }
                        .contains(px, py)
                })
            })
            .or(all.first());
        if let Some(mon) = current {
            let ov = this.build_overlay(mon, 0);
            this.overlays.borrow_mut().push(ov);
        }
        for ov in this.overlays.borrow().iter() {
            ov.window.present();
        }
        this
    }

    fn build_toolbar(self: &Rc<Self>) {
        let record = gtk::Button::with_label("Record");
        record.add_css_class("suggested-action");
        record.set_focusable(false);
        let weak = Rc::downgrade(self);
        record.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.record();
            }
        });
        let weak = Rc::downgrade(self);
        let controls = capture_controls(&self.ctl, move || {
            if let Some(t) = weak.upgrade() {
                t.restart_preview();
            }
        });

        let cancel = gtk::Button::from_icon_name("window-close-symbolic");
        cancel.add_css_class("flat");
        cancel.set_tooltip_text(Some("Cancel (Esc)"));
        cancel.set_focusable(false);
        let weak = Rc::downgrade(self);
        cancel.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.close();
            }
        });

        self.toolbar.append(&rec_dot());
        self.toolbar.append(&self.size_label);
        self.toolbar.append(&controls);
        self.toolbar.append(&self.meters.root);
        self.toolbar.append(&record);
        self.toolbar.append(&cancel);
    }

    fn restart_preview(self: &Rc<Self>) {
        self.preview.borrow_mut().take();
        let cfg = self.ctl.config.borrow().clone();
        self.meters.configure(&cfg);
        let weak = Rc::downgrade(&self.meters);
        let sources = crate::audio::resolve(&cfg);
        let opts = crate::meter::AudioOpts::new(&cfg, sources.len());
        let p = Preview::start(&sources, opts, move |levels| {
            if let Some(m) = weak.upgrade() {
                m.set(&levels);
            }
        });
        *self.preview.borrow_mut() = p;
    }

    fn build_overlay(self: &Rc<Self>, mon: &gdk::Monitor, idx: usize) -> Overlay {
        let geo = mon.geometry();
        let scale = mon.scale_factor().max(1);
        let window = gtk::Window::builder()
            .application(&self.ctl.app)
            .decorated(false)
            .resizable(false)
            .default_width(geo.width())
            .default_height(geo.height())
            .css_classes(["peek2-transparent"])
            .title("Peek2 selection")
            .build();

        let area = gtk::DrawingArea::new();
        area.set_hexpand(true);
        area.set_vexpand(true);
        area.set_cursor_from_name(Some("crosshair"));
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&area));
        window.set_child(Some(&overlay));

        let root = x11::Rect {
            x: geo.x() * scale,
            y: geo.y() * scale,
            width: geo.width() * scale,
            height: geo.height() * scale,
        };
        window.connect_realize(|w| {
            if let (Some(x), Some(id)) = (x11::get(), x11::xid(w)) {
                x.set_override_redirect(id);
            }
        });
        window.connect_map(move |w| {
            let w = w.clone();
            after(1, move || {
                if let (Some(x), Some(id)) = (x11::get(), x11::xid(&w)) {
                    x.move_resize(id, root);
                    x.focus(id);
                }
            });
        });

        let weak = Rc::downgrade(self);
        area.set_draw_func(move |_, cr, w, h| {
            if let Some(t) = weak.upgrade() {
                t.draw(idx, cr, w as f64, h as f64);
            }
        });

        self.connect_input(&window, &area, idx);
        Overlay { window, area, overlay, geo, scale }
    }

    fn connect_input(self: &Rc<Self>, window: &gtk::Window, area: &gtk::DrawingArea, idx: usize) {
        let drag = gtk::GestureDrag::new();
        drag.set_button(gdk::BUTTON_PRIMARY);
        let weak = Rc::downgrade(self);
        drag.connect_drag_begin(move |_, x, y| {
            if let Some(t) = weak.upgrade() {
                t.drag_begin(idx, x, y);
            }
        });
        let weak = Rc::downgrade(self);
        drag.connect_drag_update(move |g, dx, dy| {
            if let (Some(t), Some((x0, y0))) = (weak.upgrade(), g.start_point()) {
                t.drag_update(idx, x0, y0, dx, dy);
            }
        });
        let weak = Rc::downgrade(self);
        drag.connect_drag_end(move |g, dx, dy| {
            if let (Some(t), Some((x0, y0))) = (weak.upgrade(), g.start_point()) {
                t.drag_end(idx, x0, y0, dx, dy);
            }
        });
        area.add_controller(drag);

        let click = gtk::GestureClick::new();
        click.set_button(0);
        let weak = Rc::downgrade(self);
        click.connect_pressed(move |g, n, x, y| {
            let Some(t) = weak.upgrade() else { return };
            match g.current_button() {
                gdk::BUTTON_SECONDARY => {
                    t.sel.set(None);
                    t.refresh();
                }
                gdk::BUTTON_PRIMARY if n == 2 => {
                    if matches!(t.sel.get(), Some((i, r)) if i == idx && r.contains(x, y)) {
                        t.record();
                    }
                }
                _ => {}
            }
        });
        area.add_controller(click);

        let motion = gtk::EventControllerMotion::new();
        let weak = Rc::downgrade(self);
        motion.connect_motion(move |_, x, y| {
            if let Some(t) = weak.upgrade() {
                t.motion(idx, x, y);
            }
        });
        area.add_controller(motion);

        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, mods| {
            let Some(t) = weak.upgrade() else { return glib::Propagation::Proceed };
            t.key(idx, key, mods)
        });
        window.add_controller(keys);
    }

    fn overlay_size(&self, idx: usize) -> (f64, f64) {
        self.overlays.borrow().get(idx).map(|o| o.size()).unwrap_or((0.0, 0.0))
    }

    fn drag_begin(&self, idx: usize, x: f64, y: f64) {
        let d = match self.sel.get() {
            Some((i, r)) if i == idx => {
                if let Some((fx, fy)) = r.handle_at(x, y) {
                    Drag::Resize { orig: r, fx, fy }
                } else if r.contains(x, y) {
                    Drag::Move { orig: r }
                } else {
                    Drag::New { x0: x, y0: y }
                }
            }
            _ => Drag::New { x0: x, y0: y },
        };
        self.drag.set(d);
        self.toolbar.set_visible(false);
    }

    fn drag_update(&self, idx: usize, _x0: f64, _y0: f64, dx: f64, dy: f64) {
        let (w, h) = self.overlay_size(idx);
        let r = match self.drag.get() {
            Drag::None => return,
            Drag::New { x0, y0 } => {
                if dx.abs() + dy.abs() < 4.0 {
                    return;
                }
                R::from_points(x0, y0, x0 + dx, y0 + dy).clamp_to(w, h)
            }
            Drag::Move { orig } => R {
                x: (orig.x + dx).clamp(0.0, (w - orig.w).max(0.0)),
                y: (orig.y + dy).clamp(0.0, (h - orig.h).max(0.0)),
                ..orig
            },
            Drag::Resize { orig, fx, fy } => {
                let (mut l, mut t, mut r, mut b) = (orig.x, orig.y, orig.x + orig.w, orig.y + orig.h);
                if fx == 0.0 { l += dx } else if fx == 1.0 { r += dx }
                if fy == 0.0 { t += dy } else if fy == 1.0 { b += dy }
                R::from_points(l, t, r, b).clamp_to(w, h)
            }
        };
        self.sel.set(Some((idx, r)));
        self.hover.set(None);
        self.refresh();
    }

    fn drag_end(&self, idx: usize, _x0: f64, _y0: f64, dx: f64, dy: f64) {
        let d = self.drag.replace(Drag::None);
        if let Drag::New { .. } = d {
            if dx.abs() + dy.abs() < 4.0 {
                // A click: pick the hovered window, if any.
                if let Some((i, r)) = self.hover.get() {
                    if i == idx {
                        self.sel.set(Some((i, r)));
                    }
                }
            }
        }
        self.refresh();
    }

    fn motion(&self, idx: usize, x: f64, y: f64) {
        self.pointer.set(Some((idx, x, y)));
        if self.sel.get().is_none() {
            if let Some(ov) = self.overlays.borrow().get(idx) {
                ov.area.queue_draw();
            }
        }
        if !matches!(self.drag.get(), Drag::None) || self.sel.get().is_some() {
            let cursor = match self.sel.get() {
                Some((i, r)) if i == idx => match r.handle_at(x, y) {
                    Some((fx, fy)) => match (fx, fy) {
                        (0.0, 0.0) | (1.0, 1.0) => "nwse-resize",
                        (1.0, 0.0) | (0.0, 1.0) => "nesw-resize",
                        (0.5, _) => "ns-resize",
                        _ => "ew-resize",
                    },
                    None if r.contains(x, y) => "move",
                    None => "crosshair",
                },
                _ => "crosshair",
            };
            if let Some(ov) = self.overlays.borrow().get(idx) {
                ov.area.set_cursor_from_name(Some(cursor));
            }
            return;
        }
        let hover = self.window_at(idx, x, y).map(|r| (idx, r));
        if hover != self.hover.get() {
            self.hover.set(hover);
            self.refresh();
        }
    }

    /// Topmost client window under the pointer, clipped to this monitor.
    fn window_at(&self, idx: usize, x: f64, y: f64) -> Option<R> {
        let ovs = self.overlays.borrow();
        let ov = ovs.get(idx)?;
        let s = ov.scale as f64;
        let root = ov.root_rect();
        let (rx, ry) = (root.x + (x * s) as i32, root.y + (y * s) as i32);
        let win = self.windows.iter().find(|w| w.contains(rx, ry))?;
        let r = R {
            x: (win.x - root.x) as f64 / s,
            y: (win.y - root.y) as f64 / s,
            w: win.width as f64 / s,
            h: win.height as f64 / s,
        };
        let (w, h) = ov.size();
        Some(r.clamp_to(w, h))
    }

    fn key(self: &Rc<Self>, idx: usize, key: gdk::Key, mods: gdk::ModifierType) -> glib::Propagation {
        use gdk::Key;
        let step = if mods.contains(gdk::ModifierType::SHIFT_MASK) { 10.0 } else { 1.0 };
        let resize = mods.contains(gdk::ModifierType::CONTROL_MASK);
        match key {
            Key::Escape => self.close(),
            Key::Return | Key::KP_Enter => self.record(),
            Key::space | Key::f | Key::F => {
                // Whole monitor under the pointer (or the focused overlay).
                let (w, h) = self.overlay_size(idx);
                self.sel.set(Some((idx, R { x: 0.0, y: 0.0, w, h })));
                self.refresh();
            }
            Key::Left | Key::Right | Key::Up | Key::Down => {
                let Some((i, mut r)) = self.sel.get() else { return glib::Propagation::Proceed };
                let (dx, dy) = match key {
                    Key::Left => (-step, 0.0),
                    Key::Right => (step, 0.0),
                    Key::Up => (0.0, -step),
                    _ => (0.0, step),
                };
                let (w, h) = self.overlay_size(i);
                if resize {
                    r.w = (r.w + dx).max(2.0);
                    r.h = (r.h + dy).max(2.0);
                } else {
                    r.x += dx;
                    r.y += dy;
                }
                self.sel.set(Some((i, r.clamp_to(w, h))));
                self.refresh();
            }
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    }

    fn refresh(&self) {
        let ovs = self.overlays.borrow();
        for ov in ovs.iter() {
            ov.area.queue_draw();
        }
        let Some((idx, r)) = self.sel.get() else {
            self.toolbar.set_visible(false);
            return;
        };
        let Some(ov) = ovs.get(idx) else { return };
        let s = ov.scale as f64;
        self.size_label
            .set_label(&format!("{} × {}", (r.w * s).round() as i32, (r.h * s).round() as i32));

        // Move the toolbar to the overlay that holds the selection.
        if self.toolbar.parent().as_ref() != Some(ov.overlay.upcast_ref()) {
            if let Some(p) = self.toolbar.parent().and_downcast::<gtk::Overlay>() {
                p.remove_overlay(&self.toolbar);
            }
            ov.overlay.add_overlay(&self.toolbar);
        }
        let dragging = !matches!(self.drag.get(), Drag::None);
        self.toolbar.set_visible(!dragging && r.w >= 2.0 && r.h >= 2.0);

        // measure() includes margins, so clear the previous position first.
        self.toolbar.set_margin_start(0);
        self.toolbar.set_margin_top(0);
        let (_, tw, _, _) = self.toolbar.measure(gtk::Orientation::Horizontal, -1);
        let (_, th, _, _) = self.toolbar.measure(gtk::Orientation::Vertical, tw);
        let (tw, th) = (tw as f64, th as f64);
        let (w, h) = ov.size();
        let x = (r.x + r.w / 2.0 - tw / 2.0).clamp(0.0, (w - tw).max(0.0));
        let y = if r.y + r.h + th <= h {
            r.y + r.h
        } else if r.y - th >= 0.0 {
            r.y - th
        } else {
            (r.y + r.h - th).max(0.0)
        };
        self.toolbar.set_margin_start(x as i32);
        self.toolbar.set_margin_top(y as i32);
    }

    fn draw(&self, idx: usize, cr: &cairo::Context, w: f64, h: f64) {
        let ovs = self.overlays.borrow();
        let Some(ov) = ovs.get(idx) else { return };
        let s = ov.scale as f64;

        let paint_shot = |cr: &cairo::Context| {
            if let Some(shot) = &self.shot {
                cr.save().ok();
                cr.scale(1.0 / s, 1.0 / s);
                let root = ov.root_rect();
                cr.set_source_pixbuf(shot, -root.x as f64, -root.y as f64);
                cr.paint().ok();
                cr.restore().ok();
            }
        };

        cr.set_operator(cairo::Operator::Source);
        if self.shot.is_some() {
            paint_shot(cr);
            cr.set_operator(cairo::Operator::Over);
        }
        // ink scrim
        cr.set_source_rgba(0.024, 0.031, 0.027, 0.62);
        cr.paint().ok();

        let sel = self.sel.get().filter(|(i, _)| *i == idx).map(|(_, r)| r);
        let hover = self.hover.get().filter(|(i, _)| *i == idx).map(|(_, r)| r);
        let dragging = !matches!(self.drag.get(), Drag::None);

        // Hairline guides through the pointer until something is selected.
        if self.sel.get().is_none() && !dragging {
            if let Some((i, px, py)) = self.pointer.get() {
                if i == idx {
                    let (cr_, cg, cb) = CREAM;
                    cr.set_source_rgba(cr_, cg, cb, 0.22);
                    cr.set_line_width(1.0);
                    cr.move_to(0.0, py.floor() + 0.5);
                    cr.line_to(w, py.floor() + 0.5);
                    cr.move_to(px.floor() + 0.5, 0.0);
                    cr.line_to(px.floor() + 0.5, h);
                    cr.stroke().ok();
                    let label = format!("{} {}", (px * s) as i32, (py * s) as i32);
                    draw_tag(cr, px + 12.0, py + 12.0, &label, MUTED);
                }
            }
        }

        let Some(r) = sel.or(hover) else {
            if self.sel.get().is_none() {
                draw_hint(cr, w, h);
            }
            return;
        };

        cr.rectangle(r.x, r.y, r.w, r.h);
        if self.shot.is_some() {
            cr.save().ok();
            cr.clip();
            cr.set_operator(cairo::Operator::Source);
            paint_shot(cr);
            cr.restore().ok();
        } else {
            cr.set_operator(cairo::Operator::Clear);
            cr.fill().ok();
        }
        cr.set_operator(cairo::Operator::Over);
        cr.set_line_width(1.0);

        if sel.is_some() {
            // 1px cream keyline just outside the capture, square handles.
            let (cr_, cg, cb) = CREAM;
            cr.set_source_rgb(cr_, cg, cb);
            cr.rectangle(r.x - 0.5, r.y - 0.5, r.w + 1.0, r.h + 1.0);
            cr.stroke().ok();
            for (hx, hy, _, _) in r.handles() {
                cr.rectangle(hx.round() - 3.0, hy.round() - 3.0, 6.0, 6.0);
                cr.fill().ok();
            }
        } else {
            let (gr, gg, gb) = GOLD;
            cr.set_source_rgb(gr, gg, gb);
            cr.rectangle(r.x + 0.5, r.y + 0.5, (r.w - 1.0).max(0.0), (r.h - 1.0).max(0.0));
            cr.stroke().ok();
        }

        let label = format!("{} × {}", (r.w * s).round() as i32, (r.h * s).round() as i32);
        let (lx, ly) = if r.y >= 28.0 { (r.x - 0.5, r.y - 25.0) } else { (r.x + 6.0, r.y + 6.0) };
        draw_tag(cr, lx.max(0.0), ly, &label, CREAM);
    }

    pub fn record(self: &Rc<Self>) {
        let Some((idx, r)) = self.sel.get() else { return };
        let (area, mon) = {
            let ovs = self.overlays.borrow();
            let Some(ov) = ovs.get(idx) else { return };
            let s = ov.scale as f64;
            let root = ov.root_rect();
            let area = Area {
                x: root.x + (r.x * s).round() as i32,
                y: root.y + (r.y * s).round() as i32,
                width: (r.w * s).round() as i32,
                height: (r.h * s).round() as i32,
            };
            (area, root)
        };
        if !area.is_valid() {
            return;
        }
        let scale = self.overlays.borrow()[idx].scale;
        self.close();
        let frame = RecordFrame::start(&self.ctl, area, mon, scale);
        *crate::app().frame.borrow_mut() = Some(frame);
    }

    pub fn close(&self) {
        if self.closed.replace(true) {
            return;
        }
        self.preview.borrow_mut().take();
        for ov in self.overlays.borrow().iter() {
            ov.window.destroy();
        }
        let app = crate::app();
        let mine = app.selector.borrow().as_ref().is_some_and(|s| std::ptr::eq(Rc::as_ptr(s), self));
        if mine {
            // Drop after this call returns; we may be inside one of our own handlers.
            let s = app.selector.borrow_mut().take();
            glib::idle_add_local_once(move || drop(s));
        }
    }
}

fn set_font(cr: &cairo::Context, size: f64) {
    cr.select_font_face(FONT, cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    cr.set_font_size(size);
}

/// A square label box: elev-1 fill, 1px line, monospace text.
fn draw_tag(cr: &cairo::Context, x: f64, y: f64, text: &str, ink: (f64, f64, f64)) {
    set_font(cr, 12.0);
    let Ok(ext) = cr.text_extents(text) else { return };
    let (x, y) = (x.round(), y.round());
    let (bw, bh) = ((ext.x_advance() + 14.0).round(), 20.0);
    let (er, eg, eb) = ELEV1;
    cr.set_source_rgba(er, eg, eb, 0.94);
    cr.rectangle(x, y, bw, bh);
    cr.fill().ok();
    let (lr, lg, lb) = LINE;
    cr.set_source_rgb(lr, lg, lb);
    cr.set_line_width(1.0);
    cr.rectangle(x + 0.5, y + 0.5, bw - 1.0, bh - 1.0);
    cr.stroke().ok();
    let (ir, ig, ib) = ink;
    cr.set_source_rgb(ir, ig, ib);
    cr.move_to(x + 7.0, y + 14.0);
    cr.show_text(text).ok();
}

fn draw_hint(cr: &cairo::Context, w: f64, _h: f64) {
    let keys = [
        ("drag", "region"),
        ("click", "window"),
        ("space", "screen"),
        ("enter", "record"),
        ("esc", "cancel"),
    ];
    set_font(cr, 13.0);
    let gap = 22.0;
    let parts: Vec<(f64, f64)> = keys
        .iter()
        .map(|(k, v)| {
            let kw = cr.text_extents(k).map(|e| e.x_advance()).unwrap_or(0.0);
            let vw = cr.text_extents(v).map(|e| e.x_advance()).unwrap_or(0.0);
            (kw, vw)
        })
        .collect();
    let lead = 20.0;
    let content: f64 = parts.iter().map(|(k, v)| k + 8.0 + v).sum::<f64>() + gap * (keys.len() - 1) as f64;
    let (bw, bh) = ((lead + content + 28.0).round(), 40.0);
    let (bx, by) = (((w - bw) / 2.0).round(), 40.0);

    let (er, eg, eb) = ELEV1;
    cr.set_source_rgba(er, eg, eb, 0.96);
    cr.rectangle(bx, by, bw, bh);
    cr.fill().ok();
    let (lr, lg, lb) = LINE;
    cr.set_source_rgb(lr, lg, lb);
    cr.set_line_width(1.0);
    cr.rectangle(bx + 0.5, by + 0.5, bw - 1.0, bh - 1.0);
    cr.stroke().ok();

    // record dot
    let (rr, rg, rb) = RED;
    cr.set_source_rgb(rr, rg, rb);
    cr.arc(bx + 18.0, by + bh / 2.0, 4.0, 0.0, std::f64::consts::TAU);
    cr.fill().ok();

    set_font(cr, 13.0);
    let mut x = bx + 14.0 + lead;
    for ((k, v), (kw, vw)) in keys.iter().zip(&parts) {
        let (gr, gg, gb) = GOLD;
        cr.set_source_rgb(gr, gg, gb);
        cr.move_to(x, by + 25.0);
        cr.show_text(k).ok();
        x += kw + 8.0;
        let (mr, mg, mb) = MUTED;
        cr.set_source_rgb(mr, mg, mb);
        cr.move_to(x, by + 25.0);
        cr.show_text(v).ok();
        x += vw + gap;
    }
}

/// Grab the whole root window once, for compositor-less setups.
fn screenshot(x: &x11::X) -> Option<Pixbuf> {
    let (w, h) = x.root_size();
    let path = crate::recorder::temp_path("png").ok()?;
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
    let ok = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "x11grab", "-video_size"])
        .arg(format!("{w}x{h}"))
        .arg("-i")
        .arg(format!("{display}+0,0"))
        .args(["-frames:v", "1"])
        .arg(&path)
        .status()
        .is_ok_and(|s| s.success());
    let pb = ok.then(|| Pixbuf::from_file(&path).ok()).flatten();
    let _ = std::fs::remove_file(&path);
    pb
}
