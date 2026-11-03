//! While recording a selected region: a thin red border drawn *outside* the
//! captured area (four solid X windows painted by the server, so it shows
//! reliably with or without a compositor) and a small control bar next to it.

use crate::controller::{Controller, Event, Phase};
use crate::draw::{self, Item, Style, Surface};
use crate::meter::Levels;
use crate::recorder::Area;
use crate::util;
use crate::x11::{self, Rect, X};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use x11rb::protocol::xproto::EventMask;
use x11rb::protocol::Event as XEvent;

const BORDER: i32 = 2;
const GAP: i32 = 4;

pub struct RecordFrame {
    x: Rc<X>,
    ctl: Rc<Controller>,
    strips: Vec<u32>,
    bar: u32,
    depth: u8,
    surf: RefCell<Option<Surface>>,
    label: RefCell<String>,
    phase: Cell<Phase>,
    levels: RefCell<Levels>,
    hits: RefCell<Vec<(&'static str, Rect)>>,
    hover: Cell<Option<&'static str>>,
    area: Area,
    mon: Rect,
    visible: Cell<bool>,
    closed: Cell<bool>,
    on_close: RefCell<Option<Box<dyn FnOnce()>>>,
}

impl RecordFrame {
    /// Show the frame and start recording `area` (on monitor `mon`).
    pub fn start(ctl: &Rc<Controller>, area: Area, mon: Rect, on_close: impl FnOnce() + 'static) -> Rc<RecordFrame> {
        let x = x11::get();
        let b = BORDER * x.ui_scale();
        let rects = [
            Rect::new(area.x - b, area.y - b, area.width + 2 * b, b),
            Rect::new(area.x - b, area.y + area.height, area.width + 2 * b, b),
            Rect::new(area.x - b, area.y, b, area.height),
            Rect::new(area.x + area.width, area.y, b, area.height),
        ];
        let strips: Vec<u32> = rects.iter().filter_map(|r| x.create_solid(*r, draw::RED)).collect();
        let events = EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION | EventMask::LEAVE_WINDOW;
        let (bar, depth) = x.popup(Rect::new(area.x, area.y, 10, 10), x.can_blend(), events).unwrap_or((0, 24));
        let mut levels = Levels::default();
        levels.configure(&ctl.config.borrow());
        let this = Rc::new(RecordFrame {
            x: x.clone(),
            ctl: ctl.clone(),
            strips,
            bar,
            depth,
            surf: RefCell::new(None),
            label: RefCell::new("starting".into()),
            phase: Cell::new(Phase::Countdown(0)),
            levels: RefCell::new(levels),
            hits: RefCell::new(Vec::new()),
            hover: Cell::new(None),
            area,
            mon,
            visible: Cell::new(false),
            closed: Cell::new(false),
            on_close: RefCell::new(Some(Box::new(on_close))),
        });
        if bar != 0 {
            let weak = Rc::downgrade(&this);
            x.on(bar, move |ev| {
                if let Some(t) = weak.upgrade() {
                    t.x_event(ev);
                }
            });
            x.set_cursor(bar, x11::cursor::LEFT_PTR);
        }
        this.layout();

        let weak = Rc::downgrade(&this);
        ctl.set_listener(move |ev| match weak.upgrade() {
            Some(t) => t.on_event(ev),
            // The bar is gone once a video stops; errors still surface.
            None => {
                if let Event::Error(msg) = ev {
                    util::notify_error(msg);
                }
            }
        });
        ctl.start(area);
        this
    }

    fn items(&self) -> Vec<Item> {
        let phase = self.phase.get();
        let processing = phase == Phase::Processing;
        let mut v = vec![Item::Dot(matches!(phase, Phase::Recording(_))), Item::Text(format!("{:<10}", self.label.borrow()), draw::FG_HIGH)];
        if !processing {
            if let Some(m) = self.levels.borrow().item() {
                v.push(m);
            }
            let pause = if matches!(phase, Phase::Paused(_)) { "resume" } else { "pause" };
            v.push(Item::Button { id: "pause", label: pause.into(), style: Style::Flat });
            v.push(Item::Button { id: "stop", label: "stop".into(), style: Style::Danger });
        }
        v.push(Item::Button { id: "discard", label: "discard".into(), style: Style::Flat });
        v
    }

    /// Place the bar below the area, else above, else beside it. With no
    /// room outside (full screen) it stays hidden rather than being
    /// recorded; the tray icon and hotkey still stop the recording.
    fn layout(&self) {
        if self.bar == 0 {
            return;
        }
        let (bw, bh) = draw::bar_size(&self.items());
        let (area, mon) = (self.area, self.mon);
        let b = BORDER * self.x.ui_scale();
        let below = area.y + area.height + b + GAP;
        let above = area.y - b - GAP - bh;
        let right = area.x + area.width + b + GAP;
        let left = area.x - b - GAP - bw;
        let cx = (area.x + area.width / 2 - bw / 2).clamp(mon.x, (mon.x + mon.width - bw).max(mon.x));
        let cy = (area.y + area.height - bh).clamp(mon.y, (mon.y + mon.height - bh).max(mon.y));
        let pos = if below + bh <= mon.y + mon.height {
            Some((cx, below))
        } else if above >= mon.y {
            Some((cx, above))
        } else if right + bw <= mon.x + mon.width {
            Some((right, cy))
        } else if left >= mon.x {
            Some((left, cy))
        } else {
            None
        };
        match pos {
            Some((px, py)) => {
                let resized = self.surf.borrow().as_ref().is_none_or(|s| (s.w, s.h) != (bw, bh));
                if resized {
                    *self.surf.borrow_mut() = Surface::new(&self.x, self.bar, self.depth, bw, bh);
                }
                self.x.move_resize(self.bar, Rect::new(px, py, bw, bh));
                if !self.visible.replace(true) {
                    self.x.map(self.bar);
                }
                self.redraw();
            }
            None => {
                self.visible.set(false);
                self.x.set_mapped(self.bar, false);
            }
        }
    }

    fn redraw(&self) {
        let surf = self.surf.borrow();
        let Some(s) = surf.as_ref() else { return };
        s.clear(Rect::new(0, 0, s.w, s.h));
        *self.hits.borrow_mut() = draw::draw_bar(s, 0, 0, &self.items(), self.hover.get());
        s.present(self.bar);
    }

    fn x_event(&self, ev: &XEvent) {
        match ev {
            XEvent::Expose(e) if e.count == 0 => self.redraw(),
            XEvent::MotionNotify(e) => {
                let h = draw::hit(&self.hits.borrow(), e.event_x as i32, e.event_y as i32);
                if h != self.hover.get() {
                    self.hover.set(h);
                    self.redraw();
                }
            }
            XEvent::LeaveNotify(_) => {
                if self.hover.take().is_some() {
                    self.redraw();
                }
            }
            XEvent::ButtonRelease(e) if e.detail == 1 => {
                // Don't hold `hits` borrowed: the controller calls back into redraw.
                let h = draw::hit(&self.hits.borrow(), e.event_x as i32, e.event_y as i32);
                match h {
                    Some("stop") => self.ctl.stop(),
                    Some("pause") => self.ctl.toggle_pause(),
                    Some("discard") => self.ctl.cancel(),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn on_event(&self, ev: &Event) {
        match ev {
            Event::Phase(p) => {
                let before = self.phase.replace(*p);
                *self.label.borrow_mut() = match p {
                    Phase::Countdown(n) if *n > 0 => format!("in {n}"),
                    Phase::Countdown(_) => "…".into(),
                    Phase::Recording(d) => util::format_time(d.as_secs()),
                    Phase::Paused(d) => format!("paused {}", util::format_time(d.as_secs())),
                    Phase::Processing => format!("{}…", self.ctl.config.borrow().format.label()),
                    Phase::Idle => String::new(),
                };
                match p {
                    Phase::Idle => return self.close(),
                    Phase::Processing => {
                        let cfg = self.ctl.config.borrow();
                        // A video is final as soon as ffmpeg exits, unless it
                        // still has to be shrunk: then show progress.
                        if !cfg.format.is_animation() && cfg.fit_size_mb == 0 {
                            drop(cfg);
                            return self.close();
                        }
                        drop(cfg);
                        for &s in &self.strips {
                            self.x.set_mapped(s, false);
                        }
                    }
                    Phase::Paused(_) => self.levels.borrow_mut().reset(),
                    _ => {}
                }
                // Buttons come and go with the phase: resize.
                if std::mem::discriminant(&before) != std::mem::discriminant(p) {
                    self.layout();
                } else {
                    self.redraw();
                }
            }
            Event::Levels(l) => {
                self.levels.borrow_mut().set(l);
                self.redraw();
            }
            Event::Saved => {}
            Event::Error(msg) => {
                self.close();
                util::notify_error(msg);
            }
        }
    }

    pub fn close(&self) {
        if self.closed.replace(true) {
            return;
        }
        for &s in &self.strips {
            self.x.destroy(s);
        }
        self.surf.borrow_mut().take();
        if self.bar != 0 {
            self.x.destroy(self.bar);
        }
        if let Some(f) = self.on_close.borrow_mut().take() {
            crate::rt::idle(f);
        }
    }
}
