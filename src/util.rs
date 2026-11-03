//! Small replacements for what GLib used to provide: XDG directories, sizes,
//! local time formatting, opening files and desktop notifications.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var).map(PathBuf::from) {
        Some(p) if p.is_absolute() => p,
        _ => home().join(fallback),
    }
}

pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config")
}

pub fn cache_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache")
}

/// `$XDG_RUNTIME_DIR` (RAM-backed, per user), else the cache dir.
pub fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        Some(p) if p.is_absolute() => p,
        _ => cache_dir(),
    }
}

/// `XDG_VIDEOS_DIR` from user-dirs.dirs, else ~/Videos.
pub fn videos_dir() -> PathBuf {
    user_dir("XDG_VIDEOS_DIR", "Videos")
}

/// `XDG_PICTURES_DIR` from user-dirs.dirs, else ~/Pictures.
pub fn pictures_dir() -> PathBuf {
    user_dir("XDG_PICTURES_DIR", "Pictures")
}

fn user_dir(key: &str, fallback: &str) -> PathBuf {
    let file = config_dir().join("user-dirs.dirs");
    if let Ok(s) = std::fs::read_to_string(file) {
        for line in s.lines() {
            if let Some(v) = line.trim().strip_prefix(key).and_then(|v| v.strip_prefix('=')) {
                let v = v.trim().trim_matches('"');
                let v = v.replace("$HOME", &home().to_string_lossy());
                if v.starts_with('/') && Path::new(&v) != home() {
                    return PathBuf::from(v);
                }
            }
        }
    }
    home().join(fallback)
}

/// Decimal units, like GLib's format_size: "834 kB", "12.3 MB".
pub fn format_size(n: u64) -> String {
    const UNITS: [&str; 4] = ["kB", "MB", "GB", "TB"];
    if n < 1000 {
        return format!("{n} bytes");
    }
    let mut v = n as f64 / 1000.0;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    format!("{v:.1} {}", UNITS[u])
}

pub fn format_time(secs: u64) -> String {
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

/// Local time formatted with a strftime pattern (via date(1), which knows
/// the time zone and locale).
pub fn strftime(pattern: &str) -> Option<String> {
    let out = Command::new("date").arg(format!("+{pattern}")).stderr(Stdio::null()).output().ok()?;
    let s = String::from_utf8(out.stdout).ok()?.trim_end_matches('\n').to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

pub fn file_uri(path: &Path) -> String {
    let mut s = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

fn spawn_detached(cmd: &mut Command) -> bool {
    let child = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    match child {
        Ok(mut c) => {
            // Reap it so it doesn't linger as a zombie.
            std::thread::spawn(move || c.wait());
            true
        }
        Err(_) => false,
    }
}

pub fn open(path: &Path) {
    if !spawn_detached(Command::new("xdg-open").arg(path)) {
        eprintln!("peek2: could not run xdg-open for {}", path.display());
    }
}

/// Reveal a file in the file manager (FileManager1 over D-Bus), else open
/// its folder.
pub fn show_in_folder(path: &Path) {
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        let ok = Command::new("dbus-send")
            .args([
                "--session",
                "--print-reply",
                "--reply-timeout=2000",
                "--dest=org.freedesktop.FileManager1",
                "/org/freedesktop/FileManager1",
                "org.freedesktop.FileManager1.ShowItems",
            ])
            .arg(format!("array:string:{}", file_uri(&path)))
            .arg("string:")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok
            && let Some(dir) = path.parent() {
                open(dir);
            }
    });
}

/// A desktop notification via notify-send. With actions, waits on a helper
/// thread and runs `on_action(name)` on the main thread when one is picked.
pub fn notify(id: &str, summary: &str, body: &str, actions: &[(&str, &str)], on_action: impl FnOnce(String) + 'static) {
    let mut cmd = Command::new("notify-send");
    cmd.args(["-a", "Peek2", "-i", "media-record", "-h", &format!("string:x-canonical-private-synchronous:peek2-{id}")]);
    for (name, label) in actions {
        cmd.arg(format!("--action={name}={label}"));
    }
    cmd.arg(summary).arg(body).stdin(Stdio::null()).stderr(Stdio::null());
    if actions.is_empty() {
        cmd.stdout(Stdio::null());
        if !spawn_detached(&mut cmd) {
            eprintln!("peek2: {summary}: {body}");
        }
        return;
    }
    cmd.stdout(Stdio::piped());
    crate::rt::background(
        move || cmd.output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()),
        move |picked| match picked {
            Some(a) if !a.is_empty() => on_action(a),
            None => eprintln!("peek2: notify-send is not available"),
            _ => {}
        },
    );
}

pub fn notify_error(msg: &str) {
    eprintln!("peek2: {msg}");
    let mut cmd = Command::new("notify-send");
    cmd.args(["-a", "Peek2", "-u", "critical", "-i", "dialog-error", "Peek2", msg]);
    spawn_detached(&mut cmd);
}

/// Four random bytes, for unique temp names.
pub fn random_u32() -> u32 {
    use std::io::Read;
    let mut b = [0u8; 4];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_ok() {
        return u32::from_ne_bytes(b);
    }
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0)
}
