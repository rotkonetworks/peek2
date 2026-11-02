//! Status for bars (polybar `tail = true` modules). `peek2 --status-follow`
//! connects to the running instance's socket and prints one line per state
//! change, pushed, not polled; when no instance runs it shows an off ring
//! and reconnects.

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

// zk.poker palette; a record dot like the tray icon.
const IDLE: &str = "#948a79";
const BUSY: &str = "#d9a441";
const RED: &str = "#c73e3a";
const SAVED: &str = "#8fbf73";
const OFF: &str = "#4a4238";
const DOT: &str = "●";
const RING: &str = "○";

fn render(state: &str, detail: &str) -> String {
    let text = |d: &str| if d.is_empty() { DOT.to_string() } else { format!("{DOT} {d}") };
    match state {
        "recording" => format!("%{{F{RED}}}{}%{{F-}}", text(detail)),
        "paused" => format!("%{{F{RED}}}{RING} {detail}%{{F-}}"),
        "countdown" | "processing" => format!("%{{F{BUSY}}}{}%{{F-}}", text(detail)),
        "saved" => format!("%{{F{SAVED}}}{}%{{F-}}", text(detail)),
        "replay" => format!("%{{F{IDLE}}}{RING}%{{F-}}%{{F{RED}}}·%{{F-}}"),
        _ => format!("%{{F{IDLE}}}{RING}%{{F-}}"),
    }
}

thread_local! {
    static FOLLOWERS: RefCell<Vec<UnixStream>> = const { RefCell::new(Vec::new()) };
    static LAST: RefCell<String> = RefCell::new(render("idle", ""));
}

/// `state` is one of idle, replay, countdown, recording, paused,
/// processing, saved; `detail` is shown next to the dot.
pub fn publish(state: &str, detail: &str) {
    let line = render(state, detail);
    LAST.with(|l| *l.borrow_mut() = line.clone());
    FOLLOWERS.with(|f| f.borrow_mut().retain_mut(|s| writeln!(s, "{line}").is_ok()));
}

pub fn add_follower(mut s: UnixStream) {
    // A stuck reader must never block the UI: drop it instead.
    let _ = s.set_nonblocking(true);
    let line = LAST.with(|l| l.borrow().clone());
    if writeln!(s, "{line}").is_ok() {
        FOLLOWERS.with(|f| f.borrow_mut().push(s));
    }
}

/// Client side of `--status-follow`; runs forever.
pub fn follow() -> ! {
    let off = format!("%{{F{OFF}}}{RING}%{{F-}}");
    let mut last = String::new();
    let mut print = |line: &str| {
        if line != last {
            println!("{line}");
            let _ = std::io::stdout().flush();
            last = line.to_string();
        }
    };
    loop {
        if let Ok(mut s) = UnixStream::connect(crate::ipc::socket_path())
            && writeln!(s, "--status-follow").is_ok() {
                for line in BufReader::new(s).lines() {
                    match line {
                        Ok(l) => print(&l),
                        Err(_) => break,
                    }
                }
            }
        print(&off);
        std::thread::sleep(Duration::from_secs(2));
    }
}
