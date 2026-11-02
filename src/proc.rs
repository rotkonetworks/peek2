//! Child processes (ffmpeg, mostly). A helper thread waits for each child
//! and reports its exit on the main loop; stderr goes to a log file, or is
//! read line by line for live audio meters.

use crate::rt;
use anyhow::{bail, Context, Result};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Unique path in `~/.cache/peek2`.
pub fn temp_path(ext: &str) -> Result<PathBuf> {
    let dir = crate::util::cache_dir().join("peek2");
    std::fs::create_dir_all(&dir).context("creating cache dir")?;
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    Ok(dir.join(format!("rec-{t}-{:08x}.{ext}", std::process::id() ^ crate::util::random_u32())))
}

pub enum Stderr {
    /// Kept in a temp file; its tail is reported on failure.
    Log,
    /// Each line is handed to the closure on the main thread; the last
    /// lines are kept for error reports.
    Lines(Box<dyn FnMut(&str)>),
}

pub struct Proc {
    child: Arc<Mutex<Child>>,
    stdin: RefCell<Option<ChildStdin>>,
    exited: Rc<Cell<bool>>,
}

/// Outcome of a child: Ok when it exited with status 0.
pub type Exit = Result<()>;

fn describe(status: std::io::Result<ExitStatus>, tail: &[String]) -> Exit {
    let status = status.context("waiting for process")?;
    if status.success() {
        return Ok(());
    }
    let tail = tail.join("\n");
    match status.code() {
        Some(c) => bail!("exited with status {c}:\n{tail}"),
        None => bail!("process was terminated"),
    }
}

impl Proc {
    pub fn spawn(args: &[OsString], stdin: bool, stderr: Stderr, on_exit: impl FnOnce(Exit) + 'static) -> Result<Proc> {
        let mut cmd = Command::new(&args[0]);
        cmd.args(&args[1..]).stdout(Stdio::null());
        cmd.stdin(if stdin { Stdio::piped() } else { Stdio::null() });
        let log = match &stderr {
            Stderr::Log => {
                let log = temp_path("log")?;
                cmd.stderr(std::fs::File::create(&log).context("creating log file")?);
                Some(log)
            }
            Stderr::Lines(_) => {
                cmd.stderr(Stdio::piped());
                None
            }
        };
        let mut child = cmd.spawn().with_context(|| format!("failed to run {:?}", args[0]))?;
        let stdin_pipe = child.stdin.take();
        let err_pipe = child.stderr.take();
        let child = Arc::new(Mutex::new(child));
        let exited = Rc::new(Cell::new(false));

        // Lines and the exit are posted through the same queue, so every
        // line arrives before the exit.
        let tail: Rc<RefCell<VecDeque<String>>> = Rc::default();
        let lines = match stderr {
            Stderr::Lines(mut f) => {
                let tail = tail.clone();
                Some(rt::channel(move |line: String| {
                    f(&line);
                    let mut t = tail.borrow_mut();
                    t.push_back(line);
                    if t.len() > 12 {
                        t.pop_front();
                    }
                }))
            }
            Stderr::Log => None,
        };
        let mut on_exit = Some(on_exit);
        let ex = exited.clone();
        let done = rt::channel(move |status: std::io::Result<ExitStatus>| {
            ex.set(true);
            let mut t: Vec<String> = match &log {
                Some(log) => {
                    let s = std::fs::read_to_string(log).unwrap_or_default();
                    let _ = std::fs::remove_file(log);
                    s.lines().rev().take(6).map(str::to_string).collect::<Vec<_>>().into_iter().rev().collect()
                }
                None => tail.borrow().iter().cloned().collect(),
            };
            // Meter lines are noise in an error report.
            t.retain(|l| !l.contains("lavfi.astats") && !l.contains("Parsed_a"));
            let start = t.len().saturating_sub(6);
            if let Some(f) = on_exit.take() {
                f(describe(status, &t[start..]));
            }
        });

        let waiter = child.clone();
        std::thread::spawn(move || {
            let reader = err_pipe.zip(lines).map(|(pipe, lines)| {
                std::thread::spawn(move || {
                    for line in BufReader::new(pipe).lines() {
                        match line {
                            Ok(l) => lines.send(l),
                            Err(_) => break,
                        }
                    }
                })
            });
            let status = loop {
                match waiter.lock().map(|mut c| c.try_wait()) {
                    Ok(Ok(Some(s))) => break Ok(s),
                    Ok(Ok(None)) => {}
                    Ok(Err(e)) => break Err(e),
                    Err(_) => break Err(std::io::Error::other("poisoned")),
                }
                std::thread::sleep(Duration::from_millis(40));
            };
            if let Some(r) = reader {
                let _ = r.join();
            }
            done.send(status);
        });

        Ok(Proc { child, stdin: RefCell::new(stdin_pipe), exited })
    }

    /// Ask ffmpeg to finish writing its output ('q' on stdin).
    pub fn quit(&self) {
        let ok = match self.stdin.borrow_mut().as_mut() {
            Some(s) => s.write_all(b"q").and_then(|_| s.flush()).is_ok(),
            None => false,
        };
        if !ok {
            self.kill();
        }
    }

    pub fn kill(&self) {
        if !self.exited.get()
            && let Ok(mut c) = self.child.lock() {
                let _ = c.kill();
            }
    }
}

/// Blocking: run a command to completion (for worker threads). Stderr's
/// tail goes into the error. Checks `cancel` while it runs.
pub fn run(args: &[OsString], cancel: Option<&AtomicBool>) -> Result<()> {
    let log = temp_path("log")?;
    let mut child = Command::new(&args[0])
        .args(&args[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log)?)
        .spawn()
        .with_context(|| format!("failed to run {:?}", args[0]))?;
    let status = loop {
        if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&log);
            bail!("cancelled");
        }
        match child.try_wait() {
            Ok(Some(s)) => break Ok(s),
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => break Err(e),
        }
    };
    let msg = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    let tail: Vec<String> = msg.lines().rev().take(6).map(str::to_string).collect::<Vec<_>>().into_iter().rev().collect();
    describe(status, &tail)
}

pub fn ffmpeg(rest: &[&str]) -> Vec<OsString> {
    let mut a: Vec<OsString> = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-nostdin", "-y"].map(OsString::from).to_vec();
    a.extend(rest.iter().map(OsString::from));
    a
}

pub fn concat_list(files: &[impl AsRef<Path>]) -> String {
    files.iter().map(|p| format!("file '{}'\n", p.as_ref().to_string_lossy().replace('\'', "'\\''"))).collect()
}
