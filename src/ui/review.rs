//! Review window, opened after a video is saved (and already in the
//! clipboard): play it back, trim the ends, or throw it away. Trimming is a
//! stream copy onto keyframes (recordings have one every second), so it is
//! instant and lossless; the handles snap to keyframes to show what you get.

use crate::controller::{copy_file_to_clipboard, Controller};
use crate::recorder;
use crate::x11;
use gtk::{cairo, gdk, gdk_pixbuf::Pixbuf, gio, glib, prelude::*};
use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;

const PAD: f64 = 7.0;
const THUMBS: u32 = 12;
/// Shortest clip a trim may leave.
const MIN_LEN: f64 = 0.3;

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    None,
    Start,
    End,
    Seek,
}

pub struct Review {
    ctl: Rc<Controller>,
    window: gtk::Window,
    picture: gtk::Picture,
    media: RefCell<Option<gtk::MediaFile>>,
    path: RefCell<PathBuf>,
    timeline: gtk::DrawingArea,
    info: gtk::Label,
    play: gtk::Button,
    primary: gtk::Button,
    duration: Cell<f64>,
    sel: Cell<(f64, f64)>,
    keyframes: RefCell<Vec<f64>>,
    thumbs: RefCell<Option<Pixbuf>>,
    drag: Cell<Drag>,
    busy: Cell<bool>,
    /// Last seek target; the stream's timestamp lags behind until it lands.
    target: Cell<f64>,
}

thread_local! {
    static CURRENT: RefCell<Option<Rc<Review>>> = const { RefCell::new(None) };
}

/// Show `path` in the review window, reusing an open one.
pub fn open(ctl: &Rc<Controller>, path: &Path) {
    let r = CURRENT.with(|c| c.borrow().clone()).unwrap_or_else(|| {
        let r = Review::new(ctl);
        CURRENT.with(|c| *c.borrow_mut() = Some(r.clone()));
        r
    });
    r.load(path);
    r.window.present();
}

fn secs(us: i64) -> f64 {
    us as f64 / 1e6
}

fn clock(t: f64) -> String {
    let t = t.max(0.0);
    format!("{:02}:{:04.1}", (t / 60.0) as u64, t % 60.0)
}

impl Review {
    fn new(ctl: &Rc<Controller>) -> Rc<Review> {
        let window = gtk::Window::builder()
            .title("peek2 review")
            .application(&ctl.app)
            .default_width(760)
            .default_height(520)
            .build();
        window.connect_realize(|w| {
            if let (Some(x), Some(id)) = (x11::get(), x11::xid(w)) {
                x.set_dialog_type(id);
            }
        });

        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .vexpand(true)
            .css_classes(["peek2-video"])
            .build();
        let timeline = gtk::DrawingArea::builder().content_height(46).margin_start(12).margin_end(12).margin_top(12).build();

        let play = gtk::Button::from_icon_name("media-playback-start-symbolic");
        play.add_css_class("flat");
        play.set_tooltip_text(Some("Play / pause (space)"));
        let info = gtk::Label::builder().xalign(0.0).hexpand(true).css_classes(["peek2-time"]).build();
        let discard = gtk::Button::builder()
            .label("Discard")
            .css_classes(["flat", "peek2-danger"])
            .tooltip_text("Move to trash and clear the clipboard (Shift+Delete)")
            .build();
        let folder = gtk::Button::builder().label("Show in folder").css_classes(["flat"]).build();
        let fit_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
        let fit_pop = gtk::Popover::builder().child(&fit_box).has_arrow(false).position(gtk::PositionType::Top).build();
        let fit = gtk::MenuButton::builder()
            .label("Fit size")
            .popover(&fit_pop)
            .css_classes(["flat"])
            .tooltip_text("Re-encode the selection to fit an upload limit")
            .build();
        let primary = gtk::Button::builder().label("Done").css_classes(["suggested-action"]).build();
        let hint = gtk::Label::builder()
            .label("space play · i / o set in / out · f fit size · enter save · shift+del discard · esc close")
            .xalign(0.0)
            .css_classes(["peek2-dim"])
            .margin_start(12)
            .margin_bottom(10)
            .build();

        let bar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .margin_start(8)
            .margin_end(12)
            .margin_top(8)
            .margin_bottom(6)
            .build();
        for w in [play.upcast_ref::<gtk::Widget>(), info.upcast_ref(), discard.upcast_ref(), folder.upcast_ref(), fit.upcast_ref(), primary.upcast_ref()] {
            bar.append(w);
        }
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&picture);
        root.append(&timeline);
        root.append(&bar);
        root.append(&hint);
        window.set_child(Some(&root));

        let this = Rc::new(Review {
            ctl: ctl.clone(),
            window,
            picture,
            media: RefCell::new(None),
            path: RefCell::new(PathBuf::new()),
            timeline,
            info,
            play,
            primary,
            duration: Cell::new(0.0),
            sel: Cell::new((0.0, 0.0)),
            keyframes: RefCell::new(Vec::new()),
            thumbs: RefCell::new(None),
            drag: Cell::new(Drag::None),
            busy: Cell::new(false),
            target: Cell::new(0.0),
        });

        let weak = Rc::downgrade(&this);
        this.timeline.set_draw_func(move |_, cr, w, h| {
            if let Some(t) = weak.upgrade() {
                t.draw(cr, w as f64, h as f64);
            }
        });
        this.connect_timeline();

        let weak = Rc::downgrade(&this);
        this.play.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.toggle_play();
            }
        });
        let weak = Rc::downgrade(&this);
        this.primary.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.accept();
            }
        });
        let weak = Rc::downgrade(&this);
        discard.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.discard();
            }
        });
        for (label, mb) in [("Under 10 MB · Discord", 10u64), ("Under 25 MB", 25), ("Under 50 MB", 50)] {
            let b = gtk::Button::builder().label(label).css_classes(["flat", "peek2-menu-item"]).build();
            let weak = Rc::downgrade(&this);
            let pop = fit_pop.clone();
            b.connect_clicked(move |_| {
                pop.popdown();
                if let Some(t) = weak.upgrade() {
                    if !t.busy.get() {
                        glib::spawn_future_local(async move { t.fit(mb * 1_000_000).await });
                    }
                }
            });
            fit_box.append(&b);
        }
        let weak = Rc::downgrade(&this);
        folder.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                let p = t.path.borrow().to_string_lossy().into_owned();
                t.ctl.app.activate_action("show-in-folder", Some(&p.to_variant()));
            }
        });

        // Capture phase: a focused button would otherwise eat space/Enter.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&this);
        let fit_key = fit.clone();
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(t) = weak.upgrade() else { return glib::Propagation::Proceed };
            let now = t.position();
            let (s, e) = t.sel.get();
            match key {
                gdk::Key::space => t.toggle_play(),
                gdk::Key::Return | gdk::Key::KP_Enter => t.accept(),
                gdk::Key::Escape => t.window.close(),
                // Shift: one stray key must not trash the clip.
                gdk::Key::Delete if state.contains(gdk::ModifierType::SHIFT_MASK) => t.discard(),
                gdk::Key::i | gdk::Key::bracketleft => t.set_sel(t.snap(now.min(e - MIN_LEN)), e),
                gdk::Key::o | gdk::Key::bracketright => t.set_sel(s, now.max(s + MIN_LEN)),
                gdk::Key::Left => t.seek(now - 1.0),
                gdk::Key::Right => t.seek(now + 1.0),
                gdk::Key::Home => t.seek(s),
                gdk::Key::f => fit_key.popup(),
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        this.window.add_controller(keys);

        this.window.connect_close_request(|_| {
            if let Some(t) = CURRENT.with(|c| c.borrow_mut().take()) {
                t.unload();
            }
            glib::Propagation::Proceed
        });
        this
    }

    fn connect_timeline(self: &Rc<Self>) {
        let drag = gtk::GestureDrag::new();
        let weak = Rc::downgrade(self);
        drag.connect_drag_begin(move |_, x, _| {
            let Some(t) = weak.upgrade() else { return };
            let (s, e) = t.sel.get();
            let (xs, xe) = (t.x_of(s), t.x_of(e));
            let mode = if (x - xs).abs() <= 8.0 && (x - xs).abs() <= (x - xe).abs() {
                Drag::Start
            } else if (x - xe).abs() <= 8.0 {
                Drag::End
            } else {
                Drag::Seek
            };
            t.drag.set(mode);
            t.drag_to(x);
        });
        let weak = Rc::downgrade(self);
        drag.connect_drag_update(move |g, dx, _| {
            let Some(t) = weak.upgrade() else { return };
            if let Some((x, _)) = g.start_point() {
                t.drag_to(x + dx);
            }
        });
        let weak = Rc::downgrade(self);
        drag.connect_drag_end(move |_, _, _| {
            if let Some(t) = weak.upgrade() {
                t.drag.set(Drag::None);
            }
        });
        self.timeline.add_controller(drag);

        let motion = gtk::EventControllerMotion::new();
        let weak = Rc::downgrade(self);
        motion.connect_motion(move |_, x, _| {
            let Some(t) = weak.upgrade() else { return };
            let (s, e) = t.sel.get();
            let near = (x - t.x_of(s)).abs() <= 8.0 || (x - t.x_of(e)).abs() <= 8.0;
            t.timeline.set_cursor_from_name(Some(if near { "col-resize" } else { "pointer" }));
        });
        self.timeline.add_controller(motion);
    }

    fn x_of(&self, t: f64) -> f64 {
        let w = self.timeline.width() as f64;
        let d = self.duration.get().max(0.001);
        PAD + (t / d).clamp(0.0, 1.0) * (w - 2.0 * PAD)
    }

    fn t_of(&self, x: f64) -> f64 {
        let w = self.timeline.width() as f64;
        ((x - PAD) / (w - 2.0 * PAD)).clamp(0.0, 1.0) * self.duration.get()
    }

    /// Nearest keyframe: a copy-trim can only start on one.
    fn snap(&self, t: f64) -> f64 {
        let keys = self.keyframes.borrow();
        keys.iter().copied().min_by(|a, b| (a - t).abs().total_cmp(&(b - t).abs())).unwrap_or(t)
    }

    fn drag_to(&self, x: f64) {
        let t = self.t_of(x);
        let (s, e) = self.sel.get();
        match self.drag.get() {
            Drag::Start => {
                let mut ns = self.snap(t);
                if ns > e - MIN_LEN {
                    // Keep the last keyframe that still leaves a clip.
                    ns = self.keyframes.borrow().iter().copied().filter(|k| *k <= e - MIN_LEN).fold(0.0, f64::max);
                }
                self.set_sel(ns, e);
                self.seek(ns);
            }
            Drag::End => {
                let ne = t.max(s + MIN_LEN).min(self.duration.get());
                self.set_sel(s, ne);
                self.seek(ne);
            }
            Drag::Seek => self.seek(t),
            Drag::None => {}
        }
    }

    fn set_sel(&self, s: f64, e: f64) {
        self.sel.set((s.max(0.0), e.min(self.duration.get())));
        self.update_info();
        self.timeline.queue_draw();
    }

    fn trimmed(&self) -> bool {
        let (s, e) = self.sel.get();
        s > 0.05 || e < self.duration.get() - 0.05
    }

    fn position(&self) -> f64 {
        match self.media.borrow().as_ref() {
            Some(m) if m.is_seeking() => self.target.get(),
            Some(m) => secs(m.timestamp()),
            None => 0.0,
        }
    }

    fn seek(&self, t: f64) {
        let t = t.clamp(0.0, self.duration.get());
        self.target.set(t);
        if let Some(m) = self.media.borrow().as_ref() {
            m.seek((t * 1e6) as i64);
        }
        self.timeline.queue_draw();
    }

    fn toggle_play(&self) {
        let Some(m) = self.media.borrow().clone() else { return };
        if m.is_playing() {
            m.pause();
        } else {
            let (s, e) = self.sel.get();
            let now = secs(m.timestamp());
            if now < s || now >= e - 0.05 {
                m.seek((s * 1e6) as i64);
            }
            m.play();
        }
    }

    fn update_info(&self) {
        let (s, e) = self.sel.get();
        let size = std::fs::metadata(&*self.path.borrow()).map(|m| m.len()).unwrap_or(0);
        let d = self.duration.get();
        let text = if self.trimmed() && d > 0.0 {
            let est = (size as f64 * (e - s) / d) as u64;
            format!("{} – {} · {:.1}s of {:.1}s · ~{}", clock(s), clock(e), e - s, d, glib::format_size(est))
        } else {
            format!("{:.1}s · {}", d, glib::format_size(size))
        };
        self.info.set_label(&text);
        if !self.busy.get() {
            self.primary.set_label(if self.trimmed() { "Save trim" } else { "Done" });
        }
    }

    fn load(self: &Rc<Self>, path: &Path) {
        self.unload();
        *self.path.borrow_mut() = path.to_path_buf();
        self.window.set_title(Some(&format!(
            "{} · peek2",
            path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
        )));
        self.duration.set(0.0);
        self.sel.set((0.0, 0.0));
        self.keyframes.borrow_mut().clear();
        self.thumbs.borrow_mut().take();

        let media = gtk::MediaFile::for_filename(path);
        let weak = Rc::downgrade(self);
        media.connect_prepared_notify(move |m| {
            let Some(t) = weak.upgrade() else { return };
            if m.is_prepared() {
                let d = secs(m.duration());
                t.duration.set(d);
                t.set_sel(0.0, d);
                t.load_strip(d);
            }
        });
        let weak = Rc::downgrade(self);
        media.connect_timestamp_notify(move |m| {
            let Some(t) = weak.upgrade() else { return };
            let (s, e) = t.sel.get();
            // Loop inside the selection, like an editor's preview.
            if m.is_playing() && t.drag.get() == Drag::None && t.trimmed() && secs(m.timestamp()) >= e {
                m.seek((s * 1e6) as i64);
            }
            t.timeline.queue_draw();
        });
        let weak = Rc::downgrade(self);
        media.connect_playing_notify(move |m| {
            if let Some(t) = weak.upgrade() {
                t.play.set_icon_name(if m.is_playing() { "media-playback-pause-symbolic" } else { "media-playback-start-symbolic" });
            }
        });
        let weak = Rc::downgrade(self);
        media.connect_error_notify(move |m| {
            if let (Some(t), Some(e)) = (weak.upgrade(), m.error()) {
                t.info.set_label(&format!("cannot play: {e}"));
            }
        });
        self.picture.set_paintable(Some(&media));
        *self.media.borrow_mut() = Some(media.clone());
        media.play();
        self.load_keyframes();
        self.update_info();
    }

    fn unload(&self) {
        if let Some(m) = self.media.borrow_mut().take() {
            m.pause();
            m.clear();
        }
        self.picture.set_paintable(gdk::Paintable::NONE);
    }

    /// Keyframe times, so the start handle only lands where a copy can cut.
    fn load_keyframes(self: &Rc<Self>) {
        let path = self.path.borrow().clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(proc) = gio::Subprocess::newv(
                &[
                    "ffprobe".as_ref(),
                    "-v".as_ref(),
                    "error".as_ref(),
                    "-select_streams".as_ref(),
                    "v:0".as_ref(),
                    // Packet flags: no decoding needed.
                    "-show_entries".as_ref(),
                    "packet=pts_time,flags".as_ref(),
                    "-of".as_ref(),
                    "csv=p=0".as_ref(),
                    path.as_os_str(),
                ],
                gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_SILENCE,
            ) else {
                return;
            };
            let Ok((Some(out), _)) = proc.communicate_future(None).await else { return };
            let Some(t) = weak.upgrade() else { return };
            if *t.path.borrow() != path {
                return;
            }
            let mut keys: Vec<f64> =
                String::from_utf8_lossy(&out)
                    .lines()
                    .filter_map(|l| l.trim().split_once(','))
                    .filter(|(_, flags)| flags.starts_with('K'))
                    .filter_map(|(t, _)| t.parse().ok())
                    .collect();
            keys.sort_by(f64::total_cmp);
            if keys.first().is_none_or(|k| *k > 0.01) {
                keys.insert(0, 0.0);
            }
            *t.keyframes.borrow_mut() = keys;
            t.timeline.queue_draw();
        });
    }

    /// A filmstrip of the clip behind the timeline.
    fn load_strip(self: &Rc<Self>, duration: f64) {
        if duration <= 0.0 {
            return;
        }
        let path = self.path.borrow().clone();
        let Ok(out) = recorder::temp_path("png") else { return };
        // One quick seek per thumbnail: cost stays flat however long or
        // heavy the clip is.
        let mut args: Vec<OsString> = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y"].map(OsString::from).to_vec();
        let mut graph = String::new();
        for i in 0..THUMBS {
            let t = duration * (i as f64 + 0.5) / THUMBS as f64;
            for a in ["-skip_frame".into(), "nokey".into(), "-ss".into(), format!("{t:.3}"), "-i".into()] {
                args.push(a.into());
            }
            args.push(path.clone().into());
            graph.push_str(&format!("[{i}:v]scale=-2:92,setsar=1[t{i}];"));
        }
        for i in 0..THUMBS {
            graph.push_str(&format!("[t{i}]"));
        }
        graph.push_str(&format!("hstack=inputs={THUMBS}"));
        for a in ["-filter_complex".into(), graph, "-frames:v".into(), "1".into()] {
            args.push(a.into());
        }
        args.push(out.clone().into());
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok((proc, log)) = recorder::spawn(&args, false) else { return };
            let ok = recorder::wait(&proc, &log).await.is_ok();
            let pb = ok.then(|| Pixbuf::from_file(&out).ok()).flatten();
            let _ = std::fs::remove_file(&out);
            if let (Some(t), Some(pb)) = (weak.upgrade(), pb) {
                if *t.path.borrow() == path {
                    *t.thumbs.borrow_mut() = Some(pb);
                    t.timeline.queue_draw();
                }
            }
        });
    }

    fn draw(&self, cr: &cairo::Context, w: f64, h: f64) {
        let rgb = |c: u32| (((c >> 16) & 0xff) as f64 / 255.0, ((c >> 8) & 0xff) as f64 / 255.0, (c & 0xff) as f64 / 255.0);
        let set = |c: u32, a: f64| {
            let (r, g, b) = rgb(c);
            cr.set_source_rgba(r, g, b, a);
        };
        let (x0, x1) = (PAD, w - PAD);
        set(0x151d1a, 1.0);
        cr.rectangle(x0, 0.0, x1 - x0, h);
        cr.fill().ok();
        if let Some(pb) = self.thumbs.borrow().as_ref() {
            cr.save().ok();
            cr.rectangle(x0, 0.0, x1 - x0, h);
            cr.clip();
            cr.translate(x0, 0.0);
            cr.scale((x1 - x0) / pb.width() as f64, h / pb.height() as f64);
            cr.set_source_pixbuf(pb, 0.0, 0.0);
            cr.paint().ok();
            cr.restore().ok();
        }
        if self.duration.get() <= 0.0 {
            return;
        }
        let (s, e) = self.sel.get();
        let (xs, xe) = (self.x_of(s), self.x_of(e));
        // Dim what will be cut.
        set(0x090c0b, 0.78);
        cr.rectangle(x0, 0.0, xs - x0, h);
        cr.rectangle(xe, 0.0, x1 - xe, h);
        cr.fill().ok();
        // Keyframe ticks: where the start can land.
        set(0x948a79, 0.8);
        for k in self.keyframes.borrow().iter() {
            let x = self.x_of(*k).round() + 0.5;
            cr.move_to(x, h - 4.0);
            cr.line_to(x, h);
        }
        cr.set_line_width(1.0);
        cr.stroke().ok();
        // Selection: 1px gold keyline with solid grips.
        set(0xd9a441, 1.0);
        cr.rectangle(xs.round() + 0.5, 0.5, (xe - xs).round() - 1.0, h - 1.0);
        cr.stroke().ok();
        cr.rectangle(xs.round() - 3.0, 0.0, 6.0, h);
        cr.rectangle(xe.round() - 3.0, 0.0, 6.0, h);
        cr.fill().ok();
        set(0x141008, 1.0);
        for x in [xs.round(), xe.round()] {
            cr.rectangle(x - 0.5, h / 2.0 - 6.0, 1.0, 12.0);
        }
        cr.fill().ok();
        // Playhead.
        let xp = self.x_of(self.position()).round() + 0.5;
        set(0xf2ecdf, 1.0);
        cr.move_to(xp, 0.0);
        cr.line_to(xp, h);
        cr.stroke().ok();
    }

    fn accept(self: &Rc<Self>) {
        if self.busy.get() {
            return;
        }
        if !self.trimmed() {
            self.window.close();
            return;
        }
        let this = self.clone();
        glib::spawn_future_local(async move { this.apply_trim().await });
    }

    /// Re-encode the selection to land under `target` bytes (upload limits).
    /// Bitrate is aimed at the target; when that bitrate is too thin for the
    /// resolution the video is scaled down instead of turning to mush.
    async fn fit(self: &Rc<Self>, target: u64) {
        let (s, e) = self.sel.get();
        let dur = (e - s).max(0.1);
        let path = self.path.borrow().clone();
        let ext = path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
        let Some(info) = probe(&path).await else {
            return super::show_error(Some(&self.window), "Could not read the clip.");
        };
        let audio_k = if info.audio { 96.0 } else { 0.0 };
        self.busy.set(true);
        self.primary.set_sensitive(false);
        if let Some(m) = self.media.borrow().as_ref() {
            m.pause();
        }
        let tmp = path.with_extension(format!("fit.{ext}"));
        let mut factor = 0.93;
        let mut result = Err(anyhow::anyhow!("nothing to do"));
        for attempt in 0..2 {
            let video_k = target as f64 * 8.0 / 1000.0 / dur * factor - audio_k;
            if video_k < 80.0 {
                result = Err(anyhow::anyhow!("{dur:.0}s is too long to fit in {} MB; trim it first", target / 1_000_000));
                break;
            }
            // Bits per pixel below ~0.06 looks bad; trade resolution instead.
            let bpp = video_k * 1000.0 / (info.width as f64 * info.height as f64 * info.fps.max(1.0));
            let height = if bpp < 0.06 {
                let h = (info.height as f64 * (bpp / 0.06).sqrt()).max(360.0).min(info.height as f64);
                Some((h as i64 / 2 * 2).max(2))
            } else {
                None
            };
            let mut args: Vec<OsString> =
                ["ffmpeg", "-hide_banner", "-loglevel", "error", "-nostats", "-progress", "pipe:1", "-y", "-ss"].map(OsString::from).to_vec();
            args.push(format!("{s:.3}").into());
            args.push("-to".into());
            args.push(format!("{e:.3}").into());
            args.push("-i".into());
            args.push(path.clone().into());
            if let Some(h) = height {
                args.push("-vf".into());
                args.push(format!("scale=-2:{h}:flags=lanczos").into());
            }
            let v = format!("{}k", video_k as u64);
            let max = format!("{}k", (video_k * 1.4) as u64);
            let buf = format!("{}k", (video_k * 2.0) as u64);
            let codec: &[&str] = if ext == "mp4" {
                &["-c:v", "libx264", "-preset", "medium", "-c:a", "aac", "-movflags", "+faststart"]
            } else {
                &["-c:v", "libvpx-vp9", "-deadline", "good", "-cpu-used", "4", "-row-mt", "1", "-c:a", "libopus"]
            };
            for a in codec.iter().copied().chain(["-b:v", &v, "-maxrate", &max, "-bufsize", &buf, "-b:a", "96k"]) {
                args.push(a.into());
            }
            args.push(tmp.clone().into());
            let weak = Rc::downgrade(self);
            let pass = if attempt == 0 { String::new() } else { " (again)".into() };
            result = run_with_progress(&args, dur, move |f| {
                if let Some(t) = weak.upgrade() {
                    t.primary.set_label(&format!("Fitting{pass} {:.0}%", f * 100.0));
                }
            })
            .await;
            let size = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(u64::MAX);
            if result.is_err() || size <= target {
                break;
            }
            factor *= 0.85 * target as f64 / size as f64;
            result = Err(anyhow::anyhow!("came out at {} after two tries", glib::format_size(size)));
        }
        self.busy.set(false);
        self.primary.set_sensitive(true);
        let result = result.and_then(|_| std::fs::rename(&tmp, &path).map_err(Into::into));
        if let Err(e) = result {
            let _ = std::fs::remove_file(&tmp);
            self.update_info();
            return super::show_error(Some(&self.window), &format!("Could not fit the clip: {e:#}"));
        }
        self.after_rewrite(&path);
        // Stay open on the result so it can be checked.
        self.load(&path);
    }

    /// The file on disk changed: re-offer it and update the bar status.
    fn after_rewrite(&self, path: &Path) {
        if self.ctl.config.borrow().copy_to_clipboard {
            copy_file_to_clipboard(path);
        }
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if !self.ctl.is_busy() {
            self.ctl.status("saved", &glib::format_size(size));
        }
    }

    async fn apply_trim(self: &Rc<Self>) {
        let (s, e) = self.sel.get();
        let path = self.path.borrow().clone();
        let ext = path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
        let tmp = path.with_extension(format!("trim.{ext}"));
        self.busy.set(true);
        self.primary.set_label("Trimming…");
        self.primary.set_sensitive(false);
        if let Some(m) = self.media.borrow().as_ref() {
            m.pause();
        }
        let mut args: Vec<OsString> = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-ss"].map(OsString::from).to_vec();
        args.push(format!("{s:.3}").into());
        args.push("-to".into());
        args.push(format!("{e:.3}").into());
        args.push("-i".into());
        args.push(path.clone().into());
        for a in ["-map", "0", "-c", "copy", "-avoid_negative_ts", "make_zero"] {
            args.push(a.into());
        }
        if ext == "mp4" {
            args.push("-movflags".into());
            args.push("+faststart".into());
        }
        args.push(tmp.clone().into());
        let res = match recorder::spawn(&args, false) {
            Ok((proc, log)) => recorder::wait(&proc, &log).await,
            Err(e) => Err(e),
        };
        self.busy.set(false);
        self.primary.set_sensitive(true);
        let res = res.and_then(|_| std::fs::rename(&tmp, &path).map_err(Into::into));
        if let Err(e) = res {
            let _ = std::fs::remove_file(&tmp);
            super::show_error(Some(&self.window), &format!("Trim failed: {e:#}"));
            self.update_info();
            return;
        }
        self.after_rewrite(&path);
        self.window.close();
    }

    fn discard(&self) {
        if self.busy.get() {
            return;
        }
        let path = self.path.borrow().clone();
        self.unload();
        let file = gio::File::for_path(&path);
        if file.trash(gio::Cancellable::NONE).is_err() {
            let _ = std::fs::remove_file(&path);
        }
        if self.ctl.config.borrow().copy_to_clipboard {
            if let Some(d) = gdk::Display::default() {
                let _ = d.clipboard().set_content(None::<&gdk::ContentProvider>);
            }
        }
        if self.ctl.last_saved.borrow().as_deref() == Some(path.as_path()) {
            self.ctl.last_saved.borrow_mut().take();
        }
        if !self.ctl.is_busy() {
            self.ctl.status("idle", "");
        }
        self.window.close();
    }
}

struct ClipInfo {
    width: i64,
    height: i64,
    fps: f64,
    audio: bool,
}

async fn probe(path: &Path) -> Option<ClipInfo> {
    let proc = gio::Subprocess::newv(
        &[
            "ffprobe".as_ref(),
            "-v".as_ref(),
            "error".as_ref(),
            "-show_entries".as_ref(),
            "stream=codec_type,width,height,avg_frame_rate".as_ref(),
            "-of".as_ref(),
            "json".as_ref(),
            path.as_os_str(),
        ],
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_SILENCE,
    )
    .ok()?;
    let (Some(out), _) = proc.communicate_future(None).await.ok()? else { return None };
    let v: serde_json::Value = serde_json::from_slice(&out).ok()?;
    let streams = v.get("streams")?.as_array()?;
    let video = streams.iter().find(|s| s.get("codec_type").and_then(|t| t.as_str()) == Some("video"))?;
    let fps = video
        .get("avg_frame_rate")
        .and_then(|r| r.as_str())
        .and_then(|r| r.split_once('/'))
        .and_then(|(n, d)| Some(n.parse::<f64>().ok()? / d.parse::<f64>().ok().filter(|d| *d > 0.0)?))
        .unwrap_or(30.0);
    Some(ClipInfo {
        width: video.get("width")?.as_i64()?,
        height: video.get("height")?.as_i64()?,
        fps,
        audio: streams.iter().any(|s| s.get("codec_type").and_then(|t| t.as_str()) == Some("audio")),
    })
}

/// Run ffmpeg with `-progress pipe:1`, reporting the done fraction.
async fn run_with_progress(args: &[OsString], total: f64, progress: impl Fn(f64)) -> anyhow::Result<()> {
    let log = recorder::temp_path("log")?;
    let launcher = gio::SubprocessLauncher::new(gio::SubprocessFlags::STDOUT_PIPE);
    launcher.set_stderr_file_path(Some(&log));
    let argv: Vec<&std::ffi::OsStr> = args.iter().map(|s| s.as_os_str()).collect();
    let proc = launcher.spawn(&argv)?;
    if let Some(out) = proc.stdout_pipe() {
        let lines = gio::DataInputStream::new(&out);
        while let Ok(Some(line)) = lines.read_line_utf8_future(glib::Priority::DEFAULT).await {
            if let Some(us) = line.strip_prefix("out_time_us=").and_then(|v| v.trim().parse::<f64>().ok()) {
                progress((us / 1e6 / total).clamp(0.0, 1.0));
            }
        }
    }
    recorder::wait(&proc, &log).await
}
