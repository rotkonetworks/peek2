//! Recording session state machine shared by the Peek window and the
//! Flameshot-style selector: countdown → record → post-process → save.

use crate::config::Config;
use crate::postprocess::{self, Job};
use crate::recorder::{Area, Recording};
use crate::replay::Replay;
use gtk::{gdk, gio, glib, prelude::*};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Phase {
    Idle,
    Countdown(u32),
    Recording(Duration),
    /// Total recorded so far.
    Paused(Duration),
    Processing,
}

#[derive(Clone, Debug)]
pub enum Event {
    Phase(Phase),
    Saved(PathBuf),
    Error(String),
    /// Peak dBFS per channel; two channels per audio source.
    Levels(Vec<f32>),
}

type AreaFn = Rc<dyn Fn() -> Option<Area>>;

pub struct Controller {
    pub app: gtk::Application,
    pub config: RefCell<Config>,
    phase: Cell<Phase>,
    rec: RefCell<Option<Rc<Recording>>>,
    job: RefCell<Option<Job>>,
    timer: RefCell<Option<glib::SourceId>>,
    cancelled: Cell<bool>,
    listener: RefCell<Option<Rc<dyn Fn(&Event)>>>,
    hold: RefCell<Option<gio::ApplicationHoldGuard>>,
    pub last_saved: RefCell<Option<PathBuf>>,
    last_status: RefCell<String>,
    /// Finished parts of a paused take, and their total length.
    parts: RefCell<Vec<PathBuf>>,
    offset: Cell<Duration>,
    resume_pending: Cell<bool>,
    area_fn: RefCell<Option<AreaFn>>,
    replay: RefCell<Option<Rc<Replay>>>,
}

impl Controller {
    pub fn new(app: &gtk::Application) -> Rc<Controller> {
        Rc::new(Controller {
            app: app.clone(),
            config: RefCell::new(Config::load()),
            phase: Cell::new(Phase::Idle),
            rec: RefCell::new(None),
            job: RefCell::new(None),
            timer: RefCell::new(None),
            cancelled: Cell::new(false),
            listener: RefCell::new(None),
            hold: RefCell::new(None),
            last_saved: RefCell::new(None),
            last_status: RefCell::new(String::new()),
            parts: RefCell::new(Vec::new()),
            offset: Cell::new(Duration::ZERO),
            resume_pending: Cell::new(false),
            area_fn: RefCell::new(None),
            replay: RefCell::new(None),
        })
    }

    pub fn phase(&self) -> Phase {
        self.phase.get()
    }

    pub fn is_busy(&self) -> bool {
        self.phase.get() != Phase::Idle
    }

    /// The active UI receives all session events.
    pub fn set_listener(&self, f: impl Fn(&Event) + 'static) {
        *self.listener.borrow_mut() = Some(Rc::new(f));
    }

    fn emit(&self, ev: Event) {
        let l = self.listener.borrow().clone();
        match (l, &ev) {
            (Some(l), _) => l(&ev),
            // No UI has been open yet (e.g. replay buffer from the tray).
            (None, Event::Error(msg)) => crate::ui::show_error(None, msg),
            _ => {}
        }
    }

    fn set_phase(&self, p: Phase) {
        self.phase.set(p);
        if p == Phase::Idle {
            self.hold.borrow_mut().take();
        } else if self.hold.borrow().is_none() {
            *self.hold.borrow_mut() = Some(self.app.hold());
        }
        match p {
            Phase::Idle => self.status("idle", ""),
            Phase::Countdown(0) => self.status("countdown", ""),
            Phase::Countdown(n) => self.status("countdown", &n.to_string()),
            Phase::Recording(d) => self.status("recording", &crate::ui::format_time(d.as_secs())),
            Phase::Paused(d) => self.status("paused", &crate::ui::format_time(d.as_secs())),
            Phase::Processing => {
                let fmt = self.config.borrow().format;
                self.status("processing", &format!("{}…", fmt.label()))
            }
        }
        self.emit(Event::Phase(p));
    }

    /// Publish state for status bars, skipping unchanged writes.
    pub fn status(&self, state: &str, detail: &str) {
        // Idle with the replay buffer running reads differently in bars.
        let state = if state == "idle" && self.replay_active() { "replay" } else { state };
        let key = format!("{state}\t{detail}");
        if *self.last_status.borrow() != key {
            crate::status::write(state, detail);
            crate::tray::set_state(state);
            *self.last_status.borrow_mut() = key;
        }
    }

    fn fail(&self, msg: String) {
        eprintln!("peek2: {msg}");
        self.stop_timer();
        self.rec.borrow_mut().take();
        self.job.borrow_mut().take();
        self.set_phase(Phase::Idle);
        self.emit(Event::Error(msg));
    }

    fn stop_timer(&self) {
        if let Some(id) = self.timer.borrow_mut().take() {
            id.remove();
        }
    }

    pub fn update_config(&self, f: impl FnOnce(&mut Config)) {
        let mut c = self.config.borrow_mut();
        f(&mut c);
        c.save();
    }

    /// Start a session. `area` is evaluated when capture begins and re-checked
    /// while recording; if it changes (window moved/resized) the take is
    /// cancelled, since ffmpeg can't follow a moving region.
    pub fn start(self: &Rc<Self>, area: impl Fn() -> Option<Area> + 'static) {
        if self.is_busy() {
            return;
        }
        self.cancelled.set(false);
        self.discard_parts();
        let area: AreaFn = Rc::new(area);
        let delay = self.config.borrow().start_delay;
        if delay == 0 {
            self.set_phase(Phase::Countdown(0));
            self.begin_after_repaint(area);
            return;
        }
        let remaining = Cell::new(delay);
        self.set_phase(Phase::Countdown(delay));
        let this = Rc::downgrade(self);
        let id = glib::timeout_add_local(Duration::from_secs(1), move || {
            let Some(this) = this.upgrade() else { return glib::ControlFlow::Break };
            let n = remaining.get() - 1;
            remaining.set(n);
            this.set_phase(Phase::Countdown(n));
            if n == 0 {
                this.timer.borrow_mut().take();
                this.begin_after_repaint(area.clone());
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
        *self.timer.borrow_mut() = Some(id);
    }

    /// Give the compositor a moment to drop overlays (countdown labels,
    /// selection dimming) before the first frame is grabbed.
    fn begin_after_repaint(self: &Rc<Self>, area: AreaFn) {
        let this = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(Duration::from_millis(250), move || {
            if let Some(this) = this.upgrade() {
                this.timer.borrow_mut().take();
                this.begin(area);
            }
        });
        *self.timer.borrow_mut() = Some(id);
    }

    /// Record one part. A take is one part, or several when paused and
    /// resumed; they are joined losslessly when the take ends.
    fn begin(self: &Rc<Self>, area_fn: AreaFn) {
        let Some(area) = area_fn() else {
            return self.fail("Could not determine the recording area.".into());
        };
        let rec = match Recording::start(area, &self.config.borrow()) {
            Ok(r) => Rc::new(r),
            Err(e) => return self.fail(format!("{e:#}")),
        };
        *self.area_fn.borrow_mut() = Some(area_fn.clone());
        *self.rec.borrow_mut() = Some(rec.clone());
        let offset = self.offset.get();
        self.set_phase(Phase::Recording(offset));
        let this = Rc::downgrade(self);
        *rec.levels.borrow_mut() = Some(Box::new(move |levels| {
            if let Some(this) = this.upgrade() {
                if matches!(this.phase(), Phase::Recording(_)) {
                    this.emit(Event::Levels(levels));
                }
            }
        }));

        let this = Rc::downgrade(self);
        let r2 = rec.clone();
        let id = glib::timeout_add_local(Duration::from_millis(250), move || {
            let Some(this) = this.upgrade() else { return glib::ControlFlow::Break };
            if area_fn() != Some(r2.area) {
                this.timer.borrow_mut().take();
                this.cancel();
                this.emit(Event::Error("The recording area moved, so the recording was cancelled.".into()));
                return glib::ControlFlow::Break;
            }
            this.set_phase(Phase::Recording(offset + r2.elapsed()));
            glib::ControlFlow::Continue
        });
        *self.timer.borrow_mut() = Some(id);

        let this = self.clone();
        glib::spawn_future_local(async move {
            let res = rec.finished().await;
            this.rec.borrow_mut().take();
            if this.cancelled.get() {
                let _ = std::fs::remove_file(&rec.output);
                this.discard_parts();
                this.set_phase(Phase::Idle);
                return;
            }
            if let Err(e) = res {
                let _ = std::fs::remove_file(&rec.output);
                this.discard_parts();
                return this.fail(format!("{e:#}"));
            }
            this.parts.borrow_mut().push(rec.output.clone());
            match this.phase() {
                // Paused: wait for resume or stop. A resume pressed while
                // this part was still finalizing starts the next one now.
                Phase::Paused(_) if this.resume_pending.replace(false) => this.resume(),
                Phase::Paused(_) => {}
                _ => this.join_and_finish().await,
            }
        });
    }

    async fn join_and_finish(self: &Rc<Self>) {
        self.set_phase(Phase::Processing);
        let parts = std::mem::take(&mut *self.parts.borrow_mut());
        self.offset.set(Duration::ZERO);
        match crate::recorder::concat(&parts).await {
            Ok(raw) => self.finish(raw).await,
            Err(e) => {
                for p in &parts {
                    let _ = std::fs::remove_file(p);
                }
                self.fail(format!("Joining the paused parts failed: {e:#}"));
            }
        }
    }

    fn discard_parts(&self) {
        for p in self.parts.borrow_mut().drain(..) {
            let _ = std::fs::remove_file(p);
        }
        self.offset.set(Duration::ZERO);
        self.resume_pending.set(false);
    }

    pub fn stop(self: &Rc<Self>) {
        match self.phase.get() {
            Phase::Countdown(_) => self.cancel(),
            Phase::Recording(_) => {
                let rec = self.rec.borrow().clone();
                if let Some(rec) = rec {
                    if rec.elapsed() < Duration::from_millis(500) && self.parts.borrow().is_empty() {
                        self.cancel();
                    } else {
                        self.stop_timer();
                        rec.stop();
                        // Respond now; ffmpeg still needs a moment to finalize.
                        self.set_phase(Phase::Processing);
                    }
                }
            }
            Phase::Paused(_) => {
                self.resume_pending.set(false);
                self.set_phase(Phase::Processing);
                // Otherwise the part still finalizing finishes the take.
                if self.rec.borrow().is_none() {
                    let this = self.clone();
                    glib::spawn_future_local(async move { this.join_and_finish().await });
                }
            }
            _ => {}
        }
    }

    /// Pause or resume the running take.
    pub fn toggle_pause(self: &Rc<Self>) {
        match self.phase.get() {
            Phase::Recording(_) => {
                let Some(rec) = self.rec.borrow().clone() else { return };
                if rec.elapsed() < Duration::from_millis(500) {
                    return;
                }
                self.stop_timer();
                self.offset.set(self.offset.get() + rec.elapsed());
                rec.stop();
                self.set_phase(Phase::Paused(self.offset.get()));
            }
            Phase::Paused(_) => self.resume(),
            _ => {}
        }
    }

    fn resume(self: &Rc<Self>) {
        if self.rec.borrow().is_some() {
            // The previous part is still being finalized.
            self.resume_pending.set(true);
            return;
        }
        let Some(area_fn) = self.area_fn.borrow().clone() else { return };
        self.begin(area_fn);
    }

    pub fn cancel(&self) {
        self.cancelled.set(true);
        self.stop_timer();
        match self.phase.get() {
            Phase::Countdown(_) => self.set_phase(Phase::Idle),
            Phase::Recording(_) | Phase::Paused(_) => match self.rec.borrow().as_ref() {
                Some(rec) => rec.kill(),
                None => {
                    self.discard_parts();
                    self.set_phase(Phase::Idle);
                }
            },
            Phase::Processing => {
                if let Some(job) = self.job.borrow().as_ref() {
                    job.cancel();
                }
            }
            Phase::Idle => {}
        }
    }

    async fn finish(self: &Rc<Self>, raw: PathBuf) {
        self.set_phase(Phase::Processing);
        let job = Job::default();
        *self.job.borrow_mut() = Some(job.clone());
        let cfg = self.config.borrow().clone();
        let res = postprocess::run(raw, &cfg, &job).await;
        self.job.borrow_mut().take();
        if self.cancelled.get() {
            if let Ok(p) = res {
                let _ = std::fs::remove_file(p);
            }
            return self.set_phase(Phase::Idle);
        }
        let file = match res {
            Ok(f) => f,
            Err(e) => return self.fail(format!("{e:#}")),
        };

        let Some(dest) = self.deliver(file, &cfg, cfg.format.ext(), "").await else {
            return self.set_phase(Phase::Idle);
        };
        self.set_phase(Phase::Idle);
        let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
        self.status("saved", &glib::format_size(size));
        self.idle_status_later();
        self.emit(Event::Saved(dest));
    }

    /// Save a finished file: pick the destination, move it there, copy it
    /// to the clipboard and open the review window (or notify).
    async fn deliver(self: &Rc<Self>, file: PathBuf, cfg: &Config, ext: &str, suffix: &str) -> Option<PathBuf> {
        let Some(dest) = self.destination(cfg, ext, suffix).await else {
            let _ = std::fs::remove_file(&file);
            return None;
        };
        if let Err(e) = move_file(&file, &dest) {
            self.emit(Event::Error(format!("Could not save {}: {e}", dest.display())));
            return None;
        }
        if cfg.copy_to_clipboard {
            copy_file_to_clipboard(&dest);
        }
        if cfg.review_after_save && matches!(ext, "webm" | "mp4") {
            crate::ui::review::open(self, &dest);
        } else if cfg.notify {
            self.notify_saved(&dest);
        }
        *self.last_saved.borrow_mut() = Some(dest.clone());
        Some(dest)
    }

    fn idle_status_later(self: &Rc<Self>) {
        let this = Rc::downgrade(self);
        glib::timeout_add_seconds_local_once(4, move || {
            if let Some(this) = this.upgrade() {
                if !this.is_busy() {
                    this.status("idle", "");
                }
            }
        });
    }

    pub fn replay_active(&self) -> bool {
        self.replay.borrow().is_some()
    }

    pub fn stop_replay(&self) {
        if let Some(r) = self.replay.borrow_mut().take() {
            r.stop();
        }
    }

    pub fn toggle_replay(self: &Rc<Self>) {
        if self.replay_active() {
            self.stop_replay();
            return self.refresh_status();
        }
        let replay = match Replay::start(&self.config.borrow()) {
            Ok(r) => Rc::new(r),
            Err(e) => return self.emit(Event::Error(format!("Replay buffer: {e:#}"))),
        };
        *self.replay.borrow_mut() = Some(replay.clone());
        self.refresh_status();
        let this = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let msg = replay.exited().await;
            let Some(this) = this.upgrade() else { return };
            let current = this.replay.borrow().as_ref().is_some_and(|r| Rc::ptr_eq(r, &replay));
            if current {
                this.replay.borrow_mut().take();
                this.refresh_status();
                this.emit(Event::Error(format!("The replay buffer stopped:\n{msg}")));
            }
        });
    }

    /// Save the last `replay_seconds` from the replay buffer.
    pub fn save_replay(self: &Rc<Self>) {
        let Some(replay) = self.replay.borrow().clone() else {
            let n = gio::Notification::new("Replay buffer is off");
            n.set_body(Some("Turn it on from the tray menu or with peek2 --replay."));
            self.app.send_notification(Some("replay"), &n);
            return;
        };
        let this = self.clone();
        glib::spawn_future_local(async move {
            let cfg = this.config.borrow().clone();
            let file = match replay.save().await {
                Ok(f) => f,
                Err(e) => return this.emit(Event::Error(format!("{e:#}"))),
            };
            if let Some(dest) = this.deliver(file, &cfg, "mp4", " replay").await {
                if !this.is_busy() {
                    let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
                    this.status("saved", &glib::format_size(size));
                    this.idle_status_later();
                }
            }
        });
    }

    fn refresh_status(&self) {
        if !self.is_busy() {
            self.status("idle", "");
        }
    }

    async fn destination(&self, cfg: &Config, ext: &str, suffix: &str) -> Option<PathBuf> {
        let dir = cfg.save_dir();
        let _ = std::fs::create_dir_all(&dir);
        let name = glib::DateTime::now_local()
            .ok()
            .and_then(|d| d.format(&cfg.filename_format).ok())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Peek".into());
        let name = format!("{}{suffix}.{ext}", name.replace('/', "-"));

        if !cfg.ask_save_location {
            return Some(unique_path(&dir.join(name)));
        }
        let dialog = gtk::FileDialog::builder()
            .title("Save recording")
            .initial_folder(&gio::File::for_path(&dir))
            .initial_name(name.as_str())
            .modal(true)
            .build();
        let parent = self.app.active_window();
        let file = dialog.save_future(parent.as_ref()).await.ok()?;
        let path = file.path()?;
        if let Some(parent) = path.parent() {
            if parent != dir {
                self.update_config(|c| c.save_folder = parent.to_string_lossy().into_owned());
            }
        }
        Some(path)
    }

    fn notify_saved(&self, path: &Path) {
        let n = gio::Notification::new("Recording saved");
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let size = glib::format_size(size);
        n.set_body(Some(&if self.config.borrow().copy_to_clipboard {
            format!("{name} ({size}) — copied to clipboard")
        } else {
            format!("{name} ({size})")
        }));
        let target = path.to_string_lossy().to_variant();
        n.set_default_action_and_target_value("app.open-file", Some(&target));
        n.add_button_with_target_value("Open", "app.open-file", Some(&target));
        n.add_button_with_target_value("Show in folder", "app.show-in-folder", Some(&target));
        self.app.send_notification(Some("saved"), &n);
    }
}

fn unique_path(p: &Path) -> PathBuf {
    if !p.exists() {
        return p.to_path_buf();
    }
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = p.extension().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    for i in 2.. {
        let c = p.with_file_name(format!("{stem} ({i}).{ext}"));
        if !c.exists() {
            return c;
        }
    }
    unreachable!()
}

fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::rename(from, to).is_err() {
        std::fs::copy(from, to)?;
        let _ = std::fs::remove_file(from);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o644));
    }
    Ok(())
}

/// Offer the file in every flavour paste targets commonly understand
/// (file managers, browsers, chat apps).
pub fn copy_file_to_clipboard(path: &Path) {
    let Some(display) = gdk::Display::default() else { return };
    let file = gio::File::for_path(path);
    let uri = file.uri().to_string();
    let providers = [
        gdk::ContentProvider::for_value(&gdk::FileList::from_array(&[file]).to_value()),
        gdk::ContentProvider::for_bytes("text/uri-list", &glib::Bytes::from_owned(format!("{uri}\r\n"))),
        gdk::ContentProvider::for_bytes(
            "x-special/gnome-copied-files",
            &glib::Bytes::from_owned(format!("copy\n{uri}")),
        ),
        gdk::ContentProvider::for_value(&path.to_string_lossy().to_value()),
    ];
    let mut providers = providers.to_vec();
    // Raw media for apps that accept pasted image/video data directly.
    let mime = match path.extension().and_then(|e| e.to_str()) {
        Some("gif") => Some("image/gif"),
        Some("apng") => Some("image/apng"),
        Some("webm") => Some("video/webm"),
        Some("mp4") => Some("video/mp4"),
        _ => None,
    };
    let small = std::fs::metadata(path).is_ok_and(|m| m.len() <= 64 << 20);
    if let (Some(mime), true) = (mime, small) {
        if let Ok(data) = std::fs::read(path) {
            providers.push(gdk::ContentProvider::for_bytes(mime, &glib::Bytes::from_owned(data)));
        }
    }
    let union = gdk::ContentProvider::new_union(&providers);
    if let Err(e) = display.clipboard().set_content(Some(&union)) {
        eprintln!("peek2: clipboard: {e}");
    }
}
