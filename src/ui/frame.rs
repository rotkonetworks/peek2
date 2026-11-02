//! While recording a selected region: a thin red border drawn *outside* the
//! captured area (four solid X windows painted by the server, so it shows
//! reliably with or without a compositor) and a small control bar next to it.

use super::{after, format_time, rec_dot, set_rec_active, Meters};
use crate::controller::{Controller, Event, Phase};
use crate::recorder::Area;
use crate::x11::{self, Rect};
use gtk::{glib, prelude::*};
use std::rc::Rc;

const BORDER: i32 = 2;
const GAP: i32 = 4;

pub struct RecordFrame {
    ctl: Rc<Controller>,
    strips: Vec<u32>,
    bar: gtk::Window,
    label: gtk::Label,
    dot: gtk::Widget,
    stop: gtk::Button,
    pause: gtk::Button,
    meters: Rc<Meters>,
    closed: std::cell::Cell<bool>,
}

pub(crate) fn popup_window(ctl: &Controller, classes: &[&str]) -> gtk::Window {
    let w = gtk::Window::builder()
        .application(&ctl.app)
        .decorated(false)
        .resizable(false)
        .css_classes(classes.iter().copied().map(String::from).collect::<Vec<_>>())
        .title("Peek2")
        .build();
    w.connect_realize(|w| {
        if let (Some(x), Some(id)) = (x11::get(), x11::xid(w)) {
            x.set_override_redirect(id);
        }
    });
    w
}

impl RecordFrame {
    pub fn start(ctl: &Rc<Controller>, area: Area, mon: Rect, scale: i32) -> Rc<RecordFrame> {
        let b = BORDER * scale;
        let rects = [
            Rect { x: area.x - b, y: area.y - b, width: area.width + 2 * b, height: b },
            Rect { x: area.x - b, y: area.y + area.height, width: area.width + 2 * b, height: b },
            Rect { x: area.x - b, y: area.y, width: b, height: area.height },
            Rect { x: area.x + area.width, y: area.y, width: b, height: area.height },
        ];
        let strips: Vec<u32> = match x11::get() {
            Some(x) => rects.iter().filter_map(|r| x.create_solid(*r, 0xc73e3a)).collect(),
            None => Vec::new(),
        };

        let bar = popup_window(ctl, &["peek2-transparent"]);
        let bx = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .css_classes(["peek2-toolbar"])
            .build();
        let label = gtk::Label::new(Some("Starting…"));
        label.add_css_class("peek2-time");
        label.set_width_chars(10);
        label.set_xalign(0.0);
        let dot = rec_dot();
        let stop = gtk::Button::with_label("Stop");
        stop.add_css_class("destructive-action");
        stop.set_tooltip_text(Some("Stop and save (Ctrl+Alt+R)"));
        let pause = gtk::Button::from_icon_name("media-playback-pause-symbolic");
        pause.add_css_class("flat");
        pause.set_tooltip_text(Some("Pause / resume"));
        let cancel = gtk::Button::from_icon_name("user-trash-symbolic");
        cancel.add_css_class("flat");
        cancel.set_tooltip_text(Some("Discard"));
        let meters = Meters::new();
        meters.configure(&ctl.config.borrow());
        bx.append(&dot);
        bx.append(&label);
        bx.append(&meters.root);
        bx.append(&pause);
        bx.append(&stop);
        bx.append(&cancel);
        bar.set_child(Some(&bx));
        let ctl2 = ctl.clone();
        stop.connect_clicked(move |_| ctl2.stop());
        let ctl2 = ctl.clone();
        pause.connect_clicked(move |_| ctl2.toggle_pause());
        let ctl2 = ctl.clone();
        cancel.connect_clicked(move |_| ctl2.cancel());

        // Place the bar below the area, else above, else beside it. With no
        // room outside (full screen) it stays hidden rather than being
        // recorded; the tray icon and hotkey still stop the recording.
        bar.connect_map(move |w| {
            let w = w.clone();
            after(1, move || {
                let (Some(x), Some(id)) = (x11::get(), x11::xid(&w)) else { return };
                let (_, nw, _, _) = w.measure(gtk::Orientation::Horizontal, -1);
                let (_, nh, _, _) = w.measure(gtk::Orientation::Vertical, nw);
                let (bw, bh) = (nw * scale, nh * scale);
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
                    Some((px, py)) => x.move_to(id, px, py),
                    None => w.set_visible(false),
                }
            });
        });

        let this = Rc::new(RecordFrame { ctl: ctl.clone(), strips, bar, label, dot, stop, pause, meters, closed: Default::default() });
        this.bar.present();

        let weak = Rc::downgrade(&this);
        ctl.set_listener(move |ev| {
            if let Some(t) = weak.upgrade() {
                t.on_event(ev);
            } else if let Event::Error(msg) = ev {
                // The bar is gone once a video stops; errors still surface.
                super::show_error(None, msg);
            }
        });
        ctl.start(move || Some(area));
        this
    }

    fn on_event(&self, ev: &Event) {
        match ev {
            Event::Phase(Phase::Countdown(n)) if *n > 0 => self.label.set_label(&format!("in {n}")),
            Event::Phase(Phase::Countdown(_)) => self.label.set_label("…"),
            Event::Phase(Phase::Paused(d)) => {
                set_rec_active(&self.dot, false);
                self.meters.reset();
                self.pause.set_icon_name("media-playback-start-symbolic");
                self.label.set_label(&format!("paused {}", format_time(d.as_secs())));
            }
            Event::Phase(Phase::Recording(d)) => {
                self.pause.set_icon_name("media-playback-pause-symbolic");
                set_rec_active(&self.dot, true);
                self.label.set_label(&format_time(d.as_secs()));
            }
            Event::Levels(l) => self.meters.set(l),
            // Video is final as soon as ffmpeg exits: get out of the way.
            Event::Phase(Phase::Processing) if !self.ctl.config.borrow().format.is_animation() => self.close(),
            Event::Phase(Phase::Processing) => {
                self.meters.root.set_visible(false);
                if let Some(x) = x11::get() {
                    for &s in &self.strips {
                        x.set_mapped(s, false);
                    }
                }
                self.stop.set_visible(false);
                self.pause.set_visible(false);
                set_rec_active(&self.dot, false);
                let fmt = self.ctl.config.borrow().format.label();
                self.label.set_label(&format!("{fmt}…"));
            }
            Event::Phase(Phase::Idle) => self.close(),
            Event::Saved(_) => {}
            Event::Error(msg) => {
                self.close();
                super::show_error(None, msg);
            }
        }
    }

    fn close(&self) {
        if self.closed.replace(true) {
            return;
        }
        if let Some(x) = x11::get() {
            for &s in &self.strips {
                x.destroy(s);
            }
        }
        self.bar.destroy();
        let app = crate::app();
        let f = app.frame.borrow_mut().take();
        glib::idle_add_local_once(move || drop(f));
    }
}
