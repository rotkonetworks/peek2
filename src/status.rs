//! Status for bars (polybar `tail = true` modules). The running instance
//! writes its state to `$XDG_RUNTIME_DIR/peek2/status`; `peek2 --status-follow`
//! watches that file with inotify (via GIO) and prints one line per change.

use gtk::{gio, glib, prelude::*};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

fn path() -> PathBuf {
    glib::user_runtime_dir().join("peek2").join("status")
}

/// `state` is one of idle, countdown, recording, processing, saved;
/// `detail` is shown next to the icon (time, countdown, file name).
pub fn write(state: &str, detail: &str) {
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = p.with_extension("tmp");
    let line = format!("{}\t{state}\t{detail}\n", std::process::id());
    if std::fs::write(&tmp, line).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

pub fn clear() {
    let _ = std::fs::remove_file(path());
}

// zk.poker palette; a record dot like the tray icon.
const IDLE: &str = "#948a79";
const BUSY: &str = "#d9a441";
const RED: &str = "#c73e3a";
const SAVED: &str = "#8fbf73";
const OFF: &str = "#4a4238";
const DOT: &str = "●";
const RING: &str = "○";

fn render() -> String {
    let content = std::fs::read_to_string(path()).unwrap_or_default();
    let mut parts = content.trim_end().splitn(3, '\t');
    let (pid, state, detail) = (parts.next(), parts.next(), parts.next().unwrap_or(""));
    let alive = pid
        .and_then(|p| p.parse::<u32>().ok())
        .is_some_and(|p| std::path::Path::new(&format!("/proc/{p}")).exists());
    let text = |d: &str| if d.is_empty() { DOT.to_string() } else { format!("{DOT} {d}") };
    match (alive, state) {
        (false, _) | (_, None) => format!("%{{F{OFF}}}{RING}%{{F-}}"),
        (_, Some("recording")) => format!("%{{F{RED}}}{}%{{F-}}", text(detail)),
        (_, Some("paused")) => format!("%{{F{RED}}}{RING} {detail}%{{F-}}"),
        (_, Some("countdown" | "processing")) => format!("%{{F{BUSY}}}{}%{{F-}}", text(detail)),
        (_, Some("saved")) => format!("%{{F{SAVED}}}{}%{{F-}}", text(detail)),
        _ => format!("%{{F{IDLE}}}{RING}%{{F-}}"),
    }
}

pub fn follow() -> glib::ExitCode {
    let file = gio::File::for_path(path());
    if let Some(dir) = path().parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let last = Rc::new(RefCell::new(String::new()));
    let emit = {
        let last = last.clone();
        move || {
            let line = render();
            if *last.borrow() != line {
                println!("{line}");
                *last.borrow_mut() = line;
            }
        }
    };
    emit();
    // Watch the directory: the status file is replaced by rename.
    let monitor = match file.parent().map(|d| d.monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)) {
        Some(Ok(m)) => m,
        _ => {
            eprintln!("peek2: cannot watch status file");
            return glib::ExitCode::FAILURE;
        }
    };
    let e = emit.clone();
    monitor.connect_changed(move |_, _, _, _| e());
    // Notice a crashed instance (no file change happens then).
    glib::timeout_add_seconds_local(5, move || {
        emit();
        glib::ControlFlow::Continue
    });
    glib::MainLoop::new(None, false).run();
    drop(monitor);
    glib::ExitCode::SUCCESS
}
