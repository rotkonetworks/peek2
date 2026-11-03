//! Recording session state machine: countdown → record (→ pause/resume) →
//! join parts → post-process (convert, fit to size) → save, clipboard,
//! notify. Slow steps run on worker threads; results come back here.

use crate::config::Config;
use crate::postprocess::{self, Job};
use crate::recorder::{Area, Recording};
use crate::replay::Replay;
use crate::rt::{self, TimerId};
use crate::shot::Image;
use crate::util;
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime};

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
    Saved,
    Error(String),
    /// Peak dBFS per channel; two channels per audio source.
    Levels(Vec<f32>),
}

pub struct Controller {
    pub config: RefCell<Config>,
    config_mtime: Cell<Option<SystemTime>>,
    phase: Cell<Phase>,
    rec: RefCell<Option<Rc<Recording>>>,
    job: RefCell<Option<Job>>,
    timer: Cell<Option<TimerId>>,
    cancelled: Cell<bool>,
    listener: RefCell<Option<Rc<dyn Fn(&Event)>>>,
    pub last_saved: RefCell<Option<PathBuf>>,
    last_status: RefCell<String>,
    /// Finished parts of a paused take, and their total length.
    parts: RefCell<Vec<PathBuf>>,
    offset: Cell<Duration>,
    resume_pending: Cell<bool>,
    area: Cell<Option<Area>>,
    replay: RefCell<Option<Rc<Replay>>>,
    /// Settings of the running take, fixed at its start: changing the
    /// format or audio mid-take (tray menu) must not mix containers.
    take: RefCell<Config>,
}

impl Controller {
    pub fn new() -> Rc<Controller> {
        Rc::new(Controller {
            config: RefCell::new(Config::load()),
            config_mtime: Cell::new(Config::mtime()),
            phase: Cell::new(Phase::Idle),
            rec: RefCell::new(None),
            job: RefCell::new(None),
            timer: Cell::new(None),
            cancelled: Cell::new(false),
            listener: RefCell::new(None),
            last_saved: RefCell::new(None),
            last_status: RefCell::new(String::new()),
            parts: RefCell::new(Vec::new()),
            offset: Cell::new(Duration::ZERO),
            resume_pending: Cell::new(false),
            area: Cell::new(None),
            replay: RefCell::new(None),
            take: RefCell::new(Config::default()),
        })
    }

    pub fn phase(&self) -> Phase {
        self.phase.get()
    }

    pub fn is_busy(&self) -> bool {
        self.phase.get() != Phase::Idle
    }

    /// Pick up hand edits of config.toml.
    pub fn reload_config(&self) {
        let m = Config::mtime();
        if m != self.config_mtime.get() {
            *self.config.borrow_mut() = Config::load();
            self.config_mtime.set(m);
        }
    }

    pub fn update_config(&self, f: impl FnOnce(&mut Config)) {
        self.reload_config();
        let mut c = self.config.borrow_mut();
        f(&mut c);
        c.save();
        self.config_mtime.set(Config::mtime());
    }

    /// The active UI receives all session events.
    pub fn set_listener(&self, f: impl Fn(&Event) + 'static) {
        *self.listener.borrow_mut() = Some(Rc::new(f));
    }

    fn emit(&self, ev: Event) {
        let l = self.listener.borrow().clone();
        match (l, &ev) {
            (Some(l), _) => l(&ev),
            (None, Event::Error(msg)) => util::notify_error(msg),
            _ => {}
        }
    }

    fn set_phase(&self, p: Phase) {
        self.phase.set(p);
        match p {
            Phase::Idle => self.status("idle", ""),
            Phase::Countdown(0) => self.status("countdown", ""),
            Phase::Countdown(n) => self.status("countdown", &n.to_string()),
            Phase::Recording(d) => self.status("recording", &util::format_time(d.as_secs())),
            Phase::Paused(d) => self.status("paused", &util::format_time(d.as_secs())),
            Phase::Processing => {
                let fmt = self.take.borrow().format;
                self.status("processing", &format!("{}…", fmt.label()))
            }
        }
        self.emit(Event::Phase(p));
    }

    /// Publish state for the tray and status bars, skipping unchanged writes.
    pub fn status(&self, state: &str, detail: &str) {
        // Idle with the replay buffer running reads differently in bars.
        let state = if state == "idle" && self.replay_active() { "replay" } else { state };
        let key = format!("{state}\t{detail}");
        if *self.last_status.borrow() != key {
            crate::status::publish(state, detail);
            crate::tray::set_state(state);
            *self.last_status.borrow_mut() = key;
        }
    }

    fn fail(&self, msg: String) {
        self.stop_timer();
        self.rec.borrow_mut().take();
        self.job.borrow_mut().take();
        self.set_phase(Phase::Idle);
        self.emit(Event::Error(msg));
    }

    fn stop_timer(&self) {
        if let Some(id) = self.timer.take() {
            rt::cancel(id);
        }
    }

    /// Start a session on a fixed area.
    pub fn start(self: &Rc<Self>, area: Area) {
        if self.is_busy() {
            return;
        }
        self.reload_config();
        *self.take.borrow_mut() = self.config.borrow().clone();
        self.cancelled.set(false);
        self.discard_parts();
        self.area.set(Some(area));
        let delay = self.config.borrow().start_delay;
        if delay == 0 {
            self.set_phase(Phase::Countdown(0));
            self.begin_after_repaint();
            return;
        }
        let remaining = Cell::new(delay);
        self.set_phase(Phase::Countdown(delay));
        let this = Rc::downgrade(self);
        let id = rt::timeout(Duration::from_secs(1), move || {
            let Some(this) = this.upgrade() else { return false };
            let n = remaining.get() - 1;
            remaining.set(n);
            this.set_phase(Phase::Countdown(n));
            if n == 0 {
                this.timer.take();
                this.begin_after_repaint();
                return false;
            }
            true
        });
        self.timer.set(Some(id));
    }

    /// Give the compositor a moment to drop overlays (countdown labels,
    /// selection dimming) before the first frame is grabbed.
    fn begin_after_repaint(self: &Rc<Self>) {
        let this = Rc::downgrade(self);
        let id = rt::once_ms(250, move || {
            if let Some(this) = this.upgrade() {
                this.timer.take();
                this.begin();
            }
        });
        self.timer.set(Some(id));
    }

    /// Record one part. A take is one part, or several when paused and
    /// resumed; they are joined losslessly when the take ends.
    fn begin(self: &Rc<Self>) {
        let Some(area) = self.area.get() else {
            return self.fail("Could not determine the recording area.".into());
        };
        let levels = {
            let this = Rc::downgrade(self);
            move |levels: Vec<f32>| {
                if let Some(this) = this.upgrade()
                    && matches!(this.phase(), Phase::Recording(_)) {
                        this.emit(Event::Levels(levels));
                    }
            }
        };
        let on_exit = {
            let this = Rc::downgrade(self);
            move |res: anyhow::Result<()>| {
                if let Some(this) = this.upgrade() {
                    this.part_finished(res);
                }
            }
        };
        let started = Recording::start(area, &self.take.borrow(), levels, on_exit);
        let rec = match started {
            Ok(r) => Rc::new(r),
            Err(e) => return self.fail(format!("{e:#}")),
        };
        *self.rec.borrow_mut() = Some(rec.clone());
        let offset = self.offset.get();
        self.set_phase(Phase::Recording(offset));
        let this = Rc::downgrade(self);
        let id = rt::timeout(Duration::from_millis(250), move || {
            let Some(this) = this.upgrade() else { return false };
            if !matches!(this.phase(), Phase::Recording(_)) {
                return false;
            }
            this.set_phase(Phase::Recording(offset + rec.elapsed()));
            true
        });
        self.timer.set(Some(id));
    }

    fn part_finished(self: &Rc<Self>, res: anyhow::Result<()>) {
        let Some(rec) = self.rec.borrow_mut().take() else { return };
        self.stop_timer();
        if self.cancelled.get() {
            let _ = std::fs::remove_file(&rec.output);
            self.discard_parts();
            return self.set_phase(Phase::Idle);
        }
        if let Err(e) = res {
            let mut keep = std::mem::take(&mut *self.parts.borrow_mut());
            keep.push(rec.output.clone());
            self.discard_parts();
            return self.fail(format!("{e:#}{}", self.rescue(&keep)));
        }
        self.parts.borrow_mut().push(rec.output.clone());
        match self.phase() {
            // Paused: wait for resume or stop. A resume pressed while this
            // part was still finalizing starts the next one now.
            Phase::Paused(_) if self.resume_pending.replace(false) => self.resume(),
            Phase::Paused(_) => {}
            _ => self.join_and_finish(),
        }
    }

    fn join_and_finish(self: &Rc<Self>) {
        self.set_phase(Phase::Processing);
        let parts = std::mem::take(&mut *self.parts.borrow_mut());
        self.offset.set(Duration::ZERO);
        let this = self.clone();
        let p2 = parts.clone();
        rt::background(
            move || crate::recorder::concat(&p2),
            move |res| match res {
                Ok(raw) => this.finish(raw),
                Err(e) => {
                    let kept = this.rescue(&parts);
                    this.fail(format!("Joining the paused parts failed: {e:#}{kept}"));
                }
            },
        );
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
                        if self.take.borrow().notify {
                            util::notify("short", "Recording discarded", "It was shorter than half a second.", &[], |_| {});
                        }
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
                    self.join_and_finish();
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
        self.begin();
    }

    pub fn cancel(&self) {
        self.cancelled.set(true);
        self.stop_timer();
        match self.phase.get() {
            Phase::Countdown(_) => self.set_phase(Phase::Idle),
            Phase::Recording(_) | Phase::Paused(_) => match self.rec.borrow().clone() {
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

    fn finish(self: &Rc<Self>, raw: PathBuf) {
        self.set_phase(Phase::Processing);
        let job = Job::default();
        *self.job.borrow_mut() = Some(job.clone());
        let cfg = self.take.borrow().clone();
        let raw_kept = raw.clone();
        let this = self.clone();
        let cfg2 = cfg.clone();
        rt::background(
            move || postprocess::run(raw, &cfg2, &job),
            move |res| {
                this.job.borrow_mut().take();
                if this.cancelled.get() {
                    if let Ok(p) = res {
                        let _ = std::fs::remove_file(p);
                    }
                    return this.set_phase(Phase::Idle);
                }
                let file = match res {
                    Ok(f) => f,
                    Err(e) => {
                        let kept = this.rescue(std::slice::from_ref(&raw_kept));
                        return this.fail(format!("{e:#}{kept}"));
                    }
                };
                let t2 = this.clone();
                this.deliver(file, &cfg, cfg.save_dir(), cfg.format.ext(), "", move |dest| {
                    t2.set_phase(Phase::Idle);
                    if let Some(dest) = dest {
                        t2.saved_status(&dest);
                        t2.emit(Event::Saved);
                    }
                });
            },
        );
    }

    /// After a failure: move whatever was recorded into the save folder
    /// instead of deleting it. Returns a note for the error message.
    fn rescue(&self, files: &[PathBuf]) -> String {
        let dir = self.take.borrow().save_dir();
        let kept = rescue_files(files, &dir, "unprocessed");
        match kept.as_slice() {
            [] => String::new(),
            [one] => format!("\n\nThe recording was kept: {}", one.display()),
            many => format!("\n\nThe recorded parts were kept in {} ({} files)", dir.display(), many.len()),
        }
    }

    fn saved_status(self: &Rc<Self>, dest: &Path) {
        let size = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
        self.status("saved", &util::format_size(size));
        let this = Rc::downgrade(self);
        rt::once(Duration::from_secs(4), move || {
            if let Some(this) = this.upgrade()
                && !this.is_busy() {
                    this.status("idle", "");
                }
        });
    }

    /// Save a finished file: pick the destination, move it there, copy it
    /// to the clipboard and notify.
    fn deliver(self: &Rc<Self>, file: PathBuf, cfg: &Config, dir: PathBuf, ext: &str, suffix: &str, done: impl FnOnce(Option<PathBuf>) + 'static) {
        let this = self.clone();
        let cfg2 = cfg.clone();
        self.destination(cfg, dir, ext, suffix, move |dest| {
            let Some(dest) = dest else {
                let _ = std::fs::remove_file(&file);
                return done(None);
            };
            if let Err(e) = move_file(&file, &dest) {
                this.emit(Event::Error(format!("Could not save {}: {e}", dest.display())));
                return done(None);
            }
            if cfg2.copy_to_clipboard {
                copy_file_to_clipboard(&dest);
            }
            if cfg2.notify {
                notify_saved(&dest, cfg2.copy_to_clipboard);
            }
            *this.last_saved.borrow_mut() = Some(dest.clone());
            done(Some(dest));
        });
    }

    fn destination(&self, cfg: &Config, dir: PathBuf, ext: &str, suffix: &str, done: impl FnOnce(Option<PathBuf>) + 'static) {
        let _ = std::fs::create_dir_all(&dir);
        let name = util::strftime(&cfg.filename_format).unwrap_or_else(|| "Peek".into());
        let name = format!("{}{suffix}.{ext}", name.replace('/', "-"));
        let default = unique_path(&dir.join(&name));
        if !cfg.ask_save_location {
            return done(Some(default));
        }
        rt::background(
            move || match ask_save_path(&default) {
                Some(choice) => choice,
                // No dialog tool available: save where we would have anyway.
                None => Some(default),
            },
            done,
        );
    }

    /// Encode a screenshot to PNG, then save it (to the clipboard too, per
    /// the settings) or only put it on the clipboard.
    pub fn screenshot(self: &Rc<Self>, img: Image, save: bool) {
        self.reload_config();
        let cfg = self.config.borrow().clone();
        let (w, h) = (img.width, img.height);
        let this = self.clone();
        rt::background(
            move || img.into_png(),
            move |res| {
                let file = match res {
                    Ok(f) => f,
                    Err(e) => return this.emit(Event::Error(format!("Screenshot: {e:#}"))),
                };
                if save {
                    let t2 = this.clone();
                    let dir = cfg.screenshot_dir();
                    return this.deliver(file, &cfg, dir, "png", "", move |dest| {
                        if let (Some(dest), false) = (dest, t2.is_busy()) {
                            t2.saved_status(&dest);
                        }
                    });
                }
                let data = std::fs::read(&file);
                let _ = std::fs::remove_file(&file);
                match data {
                    Ok(png) => crate::x11::get().set_clipboard(vec![("image/png", png)]),
                    Err(e) => return this.emit(Event::Error(format!("Screenshot: {e}"))),
                }
                if cfg.notify {
                    util::notify("shot", "Screenshot copied", &format!("{w} x {h}, on the clipboard"), &[], |_| {});
                }
            },
        );
    }

    pub fn replay_active(&self) -> bool {
        self.replay.borrow().is_some()
    }

    pub fn stop_replay(&self) {
        self.replay.borrow_mut().take();
    }

    pub fn toggle_replay(self: &Rc<Self>) {
        if self.replay_active() {
            self.stop_replay();
            return self.refresh_status();
        }
        self.reload_config();
        let area = crate::x11::get().current_monitor();
        let area = Area { x: area.x, y: area.y, width: area.width, height: area.height };
        let this = Rc::downgrade(self);
        let started = Replay::start(&self.config.borrow(), area, move |msg| {
            let Some(this) = this.upgrade() else { return };
            // Only report if this buffer is still the current one.
            if this.replay.borrow_mut().take().is_some() {
                this.refresh_status();
                this.emit(Event::Error(format!("The replay buffer stopped:\n{msg}")));
            }
        });
        let replay = match started {
            Ok(r) => Rc::new(r),
            Err(e) => return self.emit(Event::Error(format!("Replay buffer: {e:#}"))),
        };
        *self.replay.borrow_mut() = Some(replay);
        self.refresh_status();
    }

    /// Save the last `replay_seconds` from the replay buffer.
    pub fn save_replay(self: &Rc<Self>) {
        let Some(replay) = self.replay.borrow().clone() else {
            util::notify("replay", "Replay buffer is off", "Turn it on from the tray menu or with peek2 --replay.", &[], |_| {});
            return;
        };
        self.reload_config();
        let cfg = self.config.borrow().clone();
        let save = replay.save_job();
        let (limit, strategy) = (cfg.fit_size_mb as u64 * 1_000_000, cfg.fit_strategy);
        let this = self.clone();
        rt::background(
            move || {
                let f = save()?;
                if limit > 0 && std::fs::metadata(&f).map(|m| m.len()).unwrap_or(0) > limit {
                    let fitted = crate::fit::fit_video(&f, crate::config::Format::Mp4, limit, strategy, &Default::default());
                    let _ = std::fs::remove_file(&f);
                    return fitted;
                }
                Ok(f)
            },
            move |res: anyhow::Result<PathBuf>| {
                let file = match res {
                    Ok(f) => f,
                    Err(e) => return this.emit(Event::Error(format!("{e:#}"))),
                };
                let t2 = this.clone();
                this.deliver(file, &cfg, cfg.save_dir(), "mp4", " replay", move |dest| {
                    if let (Some(dest), false) = (dest, t2.is_busy()) {
                        t2.saved_status(&dest);
                    }
                });
            },
        );
    }

    fn refresh_status(&self) {
        if !self.is_busy() {
            self.status("idle", "");
        }
    }
}

/// Ask for a file name with zenity or kdialog. None: no tool available;
/// Some(None): the user cancelled.
fn ask_save_path(default: &Path) -> Option<Option<PathBuf>> {
    use std::process::{Command, Stdio};
    let tries: [(&str, Vec<String>); 2] = [
        ("zenity", vec!["--file-selection".into(), "--save".into(), "--confirm-overwrite".into(), format!("--filename={}", default.display())]),
        ("kdialog", vec!["--getsavefilename".into(), default.display().to_string()]),
    ];
    for (tool, args) in tries {
        let Ok(out) = Command::new(tool).args(&args).stdin(Stdio::null()).stderr(Stdio::null()).output() else { continue };
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        return Some((out.status.success() && !s.is_empty()).then(|| PathBuf::from(s)));
    }
    None
}

fn unique_path(p: &Path) -> PathBuf {
    if !p.exists() {
        return p.to_path_buf();
    }
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = p.extension().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    (2..).map(|i| p.with_file_name(format!("{stem} ({i}).{ext}"))).find(|c| !c.exists()).unwrap_or_else(|| p.to_path_buf())
}

pub fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::rename(from, to).is_err() {
        std::fs::copy(from, to)?;
        let _ = std::fs::remove_file(from);
    }
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o644));
    Ok(())
}

/// Move non-empty `files` into `dir` as "Peek <label> <n>.<ext>"; empty
/// ones are deleted. Returns where they went.
pub fn rescue_files(files: &[PathBuf], dir: &Path, label: &str) -> Vec<PathBuf> {
    let _ = std::fs::create_dir_all(dir);
    let stamp = util::strftime("%Y-%m-%d %H-%M-%S").unwrap_or_default();
    let mut kept = Vec::new();
    for (i, f) in files.iter().enumerate() {
        if std::fs::metadata(f).map(|m| m.len()).unwrap_or(0) == 0 {
            let _ = std::fs::remove_file(f);
            continue;
        }
        let ext = f.extension().and_then(|e| e.to_str()).unwrap_or("mkv");
        let n = if files.len() > 1 { format!(" part {}", i + 1) } else { String::new() };
        let dest = unique_path(&dir.join(format!("Peek {label} {stamp}{n}.{ext}")));
        if move_file(f, &dest).is_ok() {
            kept.push(dest);
        }
    }
    kept
}

/// Start-up: recordings left in the cache by a crash or power loss are
/// moved to the save folder; other leftovers (logs, lists) are deleted.
pub fn recover_leftovers(save_dir: &Path) -> Vec<PathBuf> {
    let dir = util::cache_dir().join("peek2");
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut videos = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if !name.starts_with("rec-") {
            continue;
        }
        match p.extension().and_then(|x| x.to_str()) {
            Some("mkv" | "mp4" | "webm" | "ts") => videos.push(p),
            _ => {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    videos.sort();
    rescue_files(&videos, save_dir, "recovered")
}

fn notify_saved(path: &Path, copied: bool) {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let size = util::format_size(std::fs::metadata(path).map(|m| m.len()).unwrap_or(0));
    let body = if copied { format!("{name} ({size}), copied to clipboard") } else { format!("{name} ({size})") };
    let p = path.to_path_buf();
    let title = if path.extension().is_some_and(|e| e == "png") { "Screenshot saved" } else { "Recording saved" };
    util::notify("saved", title, &body, &[("default", "Open"), ("open", "Open"), ("folder", "Show in folder")], move |a| {
        match a.as_str() {
            "folder" => util::show_in_folder(&p),
            _ => util::open(&p),
        }
    });
}

/// Offer the file in every flavour paste targets commonly understand
/// (file managers, browsers, chat apps).
pub fn copy_file_to_clipboard(path: &Path) {
    let uri = util::file_uri(path);
    let text = path.to_string_lossy().into_owned();
    let mut items: Vec<(&str, Vec<u8>)> = vec![
        ("text/uri-list", format!("{uri}\r\n").into_bytes()),
        ("x-special/gnome-copied-files", format!("copy\n{uri}").into_bytes()),
        ("application/x-kde-cutselection", b"0".to_vec()),
    ];
    // Raw media for apps that accept pasted image/video data directly.
    let mime = match path.extension().and_then(|e| e.to_str()) {
        Some("png") => Some("image/png"),
        Some("gif") => Some("image/gif"),
        Some("apng") => Some("image/apng"),
        Some("webm") => Some("video/webm"),
        Some("mp4") => Some("video/mp4"),
        _ => None,
    };
    let small = std::fs::metadata(path).is_ok_and(|m| m.len() <= 64 << 20);
    if let (Some(mime), true) = (mime, small)
        && let Ok(data) = std::fs::read(path) {
            items.push((mime, data));
        }
    for t in ["UTF8_STRING", "text/plain;charset=utf-8", "text/plain", "STRING", "TEXT"] {
        items.push((t, text.clone().into_bytes()));
    }
    crate::x11::get().set_clipboard(items);
}
