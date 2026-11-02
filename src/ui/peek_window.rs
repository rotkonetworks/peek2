//! The classic Peek UI: a see-through window; whatever shows through the
//! hole gets recorded.

use super::{capture_controls, format_time, rec_dot, set_rec_active, Meters};
use crate::controller::{Controller, Event, Phase};
use crate::recorder::Area;
use crate::x11;
use gtk::{cairo, gio, glib, graphene, prelude::*};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

pub struct PeekWindow {
    ctl: Rc<Controller>,
    window: gtk::ApplicationWindow,
    hole: gtk::Box,
    osd: gtk::Label,
    title: gtk::Label,
    record: gtk::Button,
    stop: gtk::Button,
    pause: gtk::Button,
    cancel: gtk::Button,
    controls: gtk::Box,
    meters: Rc<Meters>,
    dot: gtk::Widget,
    last_hole: Cell<(i32, i32)>,
    osd_timeout: Rc<RefCell<Option<glib::SourceId>>>,
}

impl PeekWindow {
    pub fn new(ctl: &Rc<Controller>) -> Rc<PeekWindow> {
        let (w, h) = ctl.config.borrow().window_size;
        let window = gtk::ApplicationWindow::builder()
            .application(&ctl.app)
            .title("Peek2")
            .default_width(w.max(160))
            .default_height(h.max(60) + 46)
            .css_classes(["peek2-window"])
            .build();

        let header = gtk::HeaderBar::new();
        let title = gtk::Label::new(Some("peek2"));
        title.add_css_class("peek2-time");
        let meters = Meters::new();
        meters.root.set_visible(false);
        let title_box = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let dot = rec_dot();
        title_box.append(&dot);
        title_box.append(&title);
        title_box.append(&meters.root);
        header.set_title_widget(Some(&title_box));

        let record = gtk::Button::with_label("Record");
        record.add_css_class("suggested-action");
        let stop = gtk::Button::with_label("Stop");
        stop.add_css_class("destructive-action");
        stop.set_visible(false);
        let pause = gtk::Button::from_icon_name("media-playback-pause-symbolic");
        pause.set_tooltip_text(Some("Pause / resume"));
        pause.set_visible(false);
        let cancel = gtk::Button::from_icon_name("process-stop-symbolic");
        cancel.set_tooltip_text(Some("Cancel"));
        cancel.set_visible(false);
        header.pack_start(&record);
        header.pack_start(&stop);
        header.pack_start(&pause);
        header.pack_start(&cancel);

        let menu = gio::Menu::new();
        let section = gio::Menu::new();
        section.append(Some("Select region… (Ctrl+Alt+R)"), Some("app.select"));
        section.append(Some("Open recordings folder"), Some("app.open-folder"));
        menu.append_section(None, &section);
        let sizes = gio::Menu::new();
        for (label, size) in [
            ("640 × 360", "640x360"),
            ("854 × 480", "854x480"),
            ("1280 × 720", "1280x720"),
            ("1920 × 1080", "1920x1080"),
            ("800 × 800", "800x800"),
        ] {
            sizes.append(Some(label), Some(&format!("win.size('{size}')")));
        }
        menu.append_submenu(Some("Recording size"), &sizes);
        let section = gio::Menu::new();
        section.append(Some("Preferences"), Some("app.preferences"));
        section.append(Some("Quit"), Some("app.quit"));
        menu.append_section(None, &section);
        let menu_button = gtk::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).build();
        header.pack_end(&menu_button);

        window.set_titlebar(Some(&header));

        let hole = gtk::Box::builder().hexpand(true).vexpand(true).build();
        let osd = gtk::Label::builder()
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .css_classes(["peek2-osd"])
            .visible(false)
            .build();
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&hole));
        overlay.add_overlay(&osd);
        overlay.set_can_target(false);
        let frame = gtk::Box::builder().css_classes(["peek2-frame"]).build();
        frame.append(&overlay);
        window.set_child(Some(&frame));

        let controls = capture_controls(ctl, || {});
        header.pack_end(&controls);

        let this = Rc::new(PeekWindow {
            ctl: ctl.clone(),
            window,
            hole,
            osd,
            title,
            record,
            stop,
            pause,
            cancel,
            controls,
            meters,
            dot,
            last_hole: Cell::new((0, 0)),
            osd_timeout: Rc::new(RefCell::new(None)),
        });

        this.connect_signals();
        this
    }

    fn connect_signals(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.record.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.record();
            }
        });
        let ctl = self.ctl.clone();
        self.stop.connect_clicked(move |_| ctl.stop());
        let ctl = self.ctl.clone();
        self.pause.connect_clicked(move |_| ctl.toggle_pause());
        let ctl = self.ctl.clone();
        self.cancel.connect_clicked(move |_| ctl.cancel());

        let size = gio::SimpleAction::new("size", Some(glib::VariantTy::STRING));
        let weak = Rc::downgrade(self);
        size.connect_activate(move |_, p| {
            let (Some(t), Some(s)) = (weak.upgrade(), p.and_then(|p| p.get::<String>())) else { return };
            if let Some((w, h)) = s.split_once('x') {
                t.resize_hole(w.parse().unwrap_or(640), h.parse().unwrap_or(360));
            }
        });
        self.window.add_action(&size);

        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, mods| {
            let Some(t) = weak.upgrade() else { return glib::Propagation::Proceed };
            let ctrl = mods.contains(gtk::gdk::ModifierType::CONTROL_MASK);
            match key {
                gtk::gdk::Key::Escape if t.ctl.is_busy() => t.ctl.cancel(),
                gtk::gdk::Key::Return | gtk::gdk::Key::r if ctrl => {
                    if t.ctl.is_busy() { t.ctl.stop() } else { t.record() }
                }
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        self.window.add_controller(keys);

        self.window.connect_realize(|w| {
            if let (Some(x), Some(id)) = (x11::get(), x11::xid(w)) {
                x.set_dialog_type(id);
            }
        });
        self.window.connect_map(|w| {
            let w = w.clone();
            super::after(50, move || {
                if let (Some(x), Some(id)) = (x11::get(), x11::xid(&w)) {
                    x.set_above(id);
                    if !x.compositor_running() {
                        eprintln!("peek2: no compositor running; the window cannot be see-through. Start picom or use --select.");
                    }
                }
            });
        });

        // GTK recomputes the CSD input shape on allocation, so re-apply ours
        // after every frame: clicks inside the hole go to the windows below.
        let weak = Rc::downgrade(self);
        self.window.connect_realize(move |w| {
            let Some(clock) = w.frame_clock() else { return };
            let weak = weak.clone();
            clock.connect_after_paint(move |_| {
                if let Some(t) = weak.upgrade() {
                    t.update_input_region();
                }
            });
        });

        let weak = Rc::downgrade(self);
        self.window.connect_close_request(move |w| {
            let Some(t) = weak.upgrade() else { return glib::Propagation::Proceed };
            let size = (t.hole.width(), t.hole.height());
            if size.0 > 0 {
                t.ctl.update_config(|c| c.window_size = size);
            }
            match t.ctl.phase() {
                Phase::Countdown(_) | Phase::Recording(_) | Phase::Paused(_) => t.ctl.cancel(),
                // Keep rendering in the background; the controller holds the app.
                Phase::Processing => {
                    w.set_visible(false);
                    return glib::Propagation::Stop;
                }
                Phase::Idle => {}
            }
            crate::app().peek.borrow_mut().take();
            glib::Propagation::Proceed
        });
    }

    pub fn present(self: &Rc<Self>) {
        self.attach();
        self.window.present();
    }

    pub fn is_visible(&self) -> bool {
        self.window.is_visible()
    }

    fn attach(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.ctl.set_listener(move |ev| {
            if let Some(t) = weak.upgrade() {
                t.on_event(ev);
            }
        });
    }

    pub fn record(self: &Rc<Self>) {
        if self.ctl.is_busy() {
            return;
        }
        self.attach();
        let weak = Rc::downgrade(self);
        self.ctl.start(move || weak.upgrade().and_then(|t| t.area()));
    }

    fn scale(&self) -> i32 {
        self.window.surface().map(|s| s.scale_factor()).unwrap_or(1)
    }

    /// Hole rectangle in surface coordinates.
    fn hole_in_surface(&self) -> Option<(f64, f64, f64, f64)> {
        let (sx, sy) = self.window.surface_transform();
        let p = self.hole.compute_point(&self.window, &graphene::Point::new(0.0, 0.0))?;
        Some((sx + p.x() as f64, sy + p.y() as f64, self.hole.width() as f64, self.hole.height() as f64))
    }

    /// Hole rectangle in X root pixels.
    fn area(&self) -> Option<Area> {
        let x = x11::get()?;
        let (ox, oy) = x.root_origin(x11::xid(&self.window)?)?;
        let (hx, hy, hw, hh) = self.hole_in_surface()?;
        let s = self.scale() as f64;
        Some(Area {
            x: ox + (hx * s).round() as i32,
            y: oy + (hy * s).round() as i32,
            width: (hw * s).round() as i32,
            height: (hh * s).round() as i32,
        })
    }

    fn update_input_region(&self) {
        let Some(surface) = self.window.surface() else { return };
        let Some((hx, hy, hw, hh)) = self.hole_in_surface() else { return };
        let region = cairo::Region::create_rectangle(&cairo::RectangleInt::new(0, 0, surface.width(), surface.height()));
        let _ = region.subtract_rectangle(&cairo::RectangleInt::new(hx as i32, hy as i32, hw as i32, hh as i32));
        surface.set_input_region(Some(&region));

        let size = (self.hole.width(), self.hole.height());
        if size != self.last_hole.get() {
            self.last_hole.set(size);
            if !self.ctl.is_busy() && size.0 > 0 {
                let s = self.scale();
                self.flash_osd(&format!("{} × {}", size.0 * s, size.1 * s));
            }
        }
    }

    fn resize_hole(&self, w: i32, h: i32) {
        if self.ctl.is_busy() {
            return;
        }
        let s = self.scale();
        let dw = self.window.width() - self.hole.width();
        let dh = self.window.height() - self.hole.height();
        self.window.set_default_size(w / s + dw, h / s + dh);
    }

    fn flash_osd(&self, text: &str) {
        self.osd.remove_css_class("countdown");
        self.osd.set_label(text);
        self.osd.set_visible(true);
        if let Some(id) = self.osd_timeout.borrow_mut().take() {
            id.remove();
        }
        let osd = self.osd.clone();
        let pending = self.osd_timeout.clone();
        let slot = glib::timeout_add_local_once(std::time::Duration::from_millis(1200), move || {
            pending.borrow_mut().take();
            osd.set_visible(false);
        });
        *self.osd_timeout.borrow_mut() = Some(slot);
    }

    fn set_busy(&self, busy: bool) {
        self.record.set_visible(!busy);
        self.stop.set_visible(busy);
        self.pause.set_visible(busy);
        self.cancel.set_visible(busy);
        self.controls.set_sensitive(!busy);
        self.window.set_resizable(!busy);
        if busy {
            self.window.add_css_class("recording");
        } else {
            self.window.remove_css_class("recording");
        }
    }

    fn on_event(&self, ev: &Event) {
        match ev {
            Event::Levels(l) => self.meters.set(l),
            Event::Phase(Phase::Countdown(n)) => {
                // First countdown event of this take.
                if self.record.is_visible() {
                    self.meters.configure(&self.ctl.config.borrow());
                }
                self.set_busy(true);
                if let Some(id) = self.osd_timeout.borrow_mut().take() {
                    id.remove();
                }
                self.title.set_label("…");
                self.osd.add_css_class("countdown");
                self.osd.set_label(&n.to_string());
                self.osd.set_visible(*n > 0);
            }
            Event::Phase(Phase::Paused(d)) => {
                set_rec_active(&self.dot, false);
                self.meters.reset();
                self.pause.set_icon_name("media-playback-start-symbolic");
                self.title.set_label(&format!("paused {}", format_time(d.as_secs())));
            }
            Event::Phase(Phase::Recording(d)) => {
                self.pause.set_icon_name("media-playback-pause-symbolic");
                self.osd.set_visible(false);
                self.stop.set_sensitive(true);
                set_rec_active(&self.dot, true);
                self.title.set_label(&format_time(d.as_secs()));
            }
            Event::Phase(Phase::Processing) => {
                set_rec_active(&self.dot, false);
                self.meters.root.set_visible(false);
                self.stop.set_visible(false);
                self.pause.set_visible(false);
                self.title.set_label("rendering…");
            }
            Event::Phase(Phase::Idle) => {
                set_rec_active(&self.dot, false);
                self.meters.root.set_visible(false);
                self.set_busy(false);
                self.title.set_label("peek2");
            }
            Event::Saved(path) => {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.title.set_label(&name);
                self.title.set_tooltip_text(Some(&path.to_string_lossy()));
                if !self.window.is_visible() {
                    self.window.close();
                }
            }
            Event::Error(msg) => {
                self.set_busy(false);
                self.title.set_label("peek2");
                super::show_error(Some(self.window.upcast_ref()), msg);
            }
        }
    }
}
