//! Screen capture backend. On X11 (and XWayland) this drives ffmpeg's
//! `x11grab` + `pulse` inputs, the same building blocks OBS uses there.
//! Stopping is done by sending `q` on stdin so ffmpeg finalizes the container.

use crate::audio;
use crate::meter;
use crate::config::{Config, Format};
use anyhow::{bail, Context, Result};
use gtk::{gio, glib, prelude::*};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::rc::Rc;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A capture rectangle in X root-window pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Area {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Area {
    pub fn is_valid(&self) -> bool {
        self.width >= 2 && self.height >= 2
    }
}

pub fn temp_path(ext: &str) -> Result<PathBuf> {
    let dir = glib::user_cache_dir().join("peek2");
    std::fs::create_dir_all(&dir).context("creating cache dir")?;
    let stamp = glib::DateTime::now_local()
        .ok()
        .and_then(|d| d.format("%Y%m%d-%H%M%S").ok())
        .map(|s| s.to_string())
        .unwrap_or_default();
    Ok(dir.join(format!("rec-{stamp}-{}.{ext}", std::process::id() ^ glib::random_int())))
}

/// GPU encoder device for this recording, if enabled and available.
/// Only MP4 (H.264) is encoded on the GPU: AMD's VCN has no VP9 encoder.
pub fn hw_device(cfg: &Config) -> Option<&'static str> {
    (cfg.hardware_encoding && cfg.format == Format::Mp4).then(crate::hw::vaapi_device).flatten()
}

/// Video filter: optional downscale, with even dimensions for 4:2:0 encoders,
/// then upload to the GPU for VAAPI.
fn video_filter(cfg: &Config, hw: bool) -> String {
    let d = cfg.downsample.max(1);
    if cfg.format.is_animation() {
        if d == 1 { "null".into() } else { format!("scale=iw/{d}:-1:flags=lanczos") }
    } else if hw {
        format!("scale=trunc(iw/{d}/2)*2:trunc(ih/{d}/2)*2:flags=lanczos,format=nv12,hwupload")
    } else {
        format!("scale=trunc(iw/{d}/2)*2:trunc(ih/{d}/2)*2:flags=lanczos,format=yuv420p")
    }
}

pub fn ffmpeg_args(area: Area, cfg: &Config, sources: &[String], output: &Path) -> Vec<OsString> {
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
    let mut a: Vec<OsString> = Vec::new();
    let mut push = |s: &str| a.push(s.into());

    // With audio, meter lines are logged at info level and read live from stderr.
    let level = if sources.is_empty() { "error" } else { "info" };
    for s in ["ffmpeg", "-hide_banner", "-loglevel", level, "-nostats", "-y"] {
        push(s);
    }
    let hw = hw_device(cfg);
    if let Some(dev) = hw {
        push("-vaapi_device");
        push(dev);
    }

    // Video input.
    for s in ["-thread_queue_size", "1024", "-f", "x11grab", "-show_region", "0"] {
        push(s);
    }
    push("-framerate");
    push(&cfg.framerate.clamp(1, 120).to_string());
    push("-video_size");
    push(&format!("{}x{}", area.width, area.height));
    if !cfg.capture_mouse {
        push("-draw_mouse");
        push("0");
    }
    push("-i");
    push(&format!("{display}+{},{}", area.x, area.y));

    // Audio inputs.
    for src in sources {
        for s in ["-thread_queue_size", "1024", "-f", "pulse", "-fragment_size", "4096", "-i"] {
            push(s);
        }
        push(src);
    }

    let audio_opts = meter::AudioOpts::new(cfg, sources.len());
    let mut graph = format!("[0:v]{}[v]", video_filter(cfg, hw.is_some()));
    if !sources.is_empty() {
        graph.push(';');
        graph.push_str(&crate::meter::audio_graph(1, sources.len(), true, audio_opts));
    }
    push("-filter_complex");
    push(&graph);
    push("-map");
    push("[v]");
    if !sources.is_empty() {
        push("-map");
        push("[a]");
    }
    if audio_opts.separate && cfg.format.has_audio() {
        // Track 1 is the mix (what players play); then each source alone.
        push("-metadata:s:a:0");
        push("title=Mix");
        for (i, (_, name)) in cfg.audio.sources().iter().enumerate().take(sources.len()) {
            push("-map");
            push(&format!("[t{i}]"));
            push(&format!("-metadata:s:a:{}", i + 1));
            push(&format!("title={name}"));
        }
    }

    let q = cfg.video_quality.to_string();
    match cfg.format {
        Format::Mp4 => {
            if hw.is_some() {
                // Constant QP is the closest VAAPI analogue to x264's CRF.
                for s in ["-c:v", "h264_vaapi", "-rc_mode", "CQP", "-qp"] {
                    push(s);
                }
            } else {
                for s in ["-c:v", "libx264", "-preset", "veryfast", "-crf"] {
                    push(s);
                }
            }
            push(&q);
            // A keyframe every second: the review window trims by stream copy,
            // which can only cut on keyframes.
            for s in ["-g", &cfg.framerate.to_string(), "-movflags", "+faststart"] {
                push(s);
            }
            if !sources.is_empty() {
                for s in ["-c:a", "aac", "-b:a", "192k"] {
                    push(s);
                }
            }
        }
        Format::Webm => {
            for s in [
                "-c:v", "libvpx-vp9", "-deadline", "realtime", "-cpu-used", "8", "-row-mt", "1", "-b:v", "0",
                "-crf",
            ] {
                push(s);
            }
            push(&(cfg.video_quality + 8).to_string());
            push("-g");
            push(&cfg.framerate.to_string());
            if !sources.is_empty() {
                for s in ["-c:a", "libopus", "-b:a", "160k"] {
                    push(s);
                }
            }
        }
        Format::Gif | Format::Apng => {
            // Lossless intermediate, converted after recording.
            for s in ["-c:v", "libx264rgb", "-preset", "ultrafast", "-qp", "0"] {
                push(s);
            }
        }
    }
    a.push(output.as_os_str().to_owned());
    a
}

/// Spawn a command with stderr captured to a log file, so the pipe can never
/// fill up during long jobs while errors are still reportable.
pub fn spawn(args: &[OsString], stdin: bool) -> Result<(gio::Subprocess, PathBuf)> {
    let log = temp_path("log")?;
    let mut flags = gio::SubprocessFlags::STDOUT_SILENCE;
    if stdin {
        flags |= gio::SubprocessFlags::STDIN_PIPE;
    }
    let launcher = gio::SubprocessLauncher::new(flags);
    launcher.set_stderr_file_path(Some(&log));
    let argv: Vec<&std::ffi::OsStr> = args.iter().map(|s| s.as_os_str()).collect();
    let proc = launcher.spawn(&argv).with_context(|| format!("failed to run {:?}", args[0]))?;
    Ok((proc, log))
}

pub async fn wait(proc: &gio::Subprocess, log: &Path) -> Result<()> {
    let res = proc.wait_future().await;
    let msg = std::fs::read_to_string(log).unwrap_or_default();
    let _ = std::fs::remove_file(log);
    res?;
    check_exit(proc, &msg)
}

fn check_exit(proc: &gio::Subprocess, msg: &str) -> Result<()> {
    if !proc.has_exited() {
        bail!("process was terminated");
    }
    if proc.exit_status() == 0 {
        return Ok(());
    }
    let tail: Vec<&str> = msg.lines().rev().take(6).collect();
    let tail: Vec<&str> = tail.into_iter().rev().collect();
    bail!("exited with status {}:\n{}", proc.exit_status(), tail.join("\n"))
}

/// Join recorded parts (same encoder settings) without re-encoding.
/// Consumes the parts; a single part is returned as is.
pub async fn concat(parts: &[PathBuf]) -> Result<PathBuf> {
    match parts {
        [] => bail!("nothing was recorded"),
        [one] => return Ok(one.clone()),
        _ => {}
    }
    let ext = parts[0].extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_else(|| "mkv".into());
    let list = temp_path("txt")?;
    let body: String = parts
        .iter()
        .map(|p| format!("file '{}'\n", p.to_string_lossy().replace('\'', "'\\''")))
        .collect();
    std::fs::write(&list, body).context("writing concat list")?;
    let out = temp_path(&ext)?;
    let mut args: Vec<OsString> =
        ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-f", "concat", "-safe", "0", "-i"].map(OsString::from).to_vec();
    args.push(list.clone().into());
    for a in ["-map", "0", "-c", "copy"] {
        args.push(a.into());
    }
    if ext == "mp4" {
        args.push("-movflags".into());
        args.push("+faststart".into());
    }
    args.push(out.clone().into());
    let (proc, log) = spawn(&args, false)?;
    let res = wait(&proc, &log).await;
    let _ = std::fs::remove_file(&list);
    if let Err(e) = res {
        let _ = std::fs::remove_file(&out);
        return Err(e.context("ffmpeg concat failed"));
    }
    for p in parts {
        let _ = std::fs::remove_file(p);
    }
    Ok(out)
}

enum Stderr {
    Log(PathBuf),
    /// Live stderr: meter output plus the last non-meter lines.
    Piped(Rc<RefCell<VecDeque<String>>>),
}

pub struct Recording {
    proc: gio::Subprocess,
    stderr: Stderr,
    /// Receives audio levels while recording; set by the UI side.
    pub levels: meter::Sink,
    pub output: PathBuf,
    pub area: Area,
    started: Instant,
}

impl Recording {
    pub fn start(area: Area, cfg: &Config) -> Result<Recording> {
        if !area.is_valid() {
            bail!("recording area is too small");
        }
        let ext = if cfg.format.is_animation() { "mkv" } else { cfg.format.ext() };
        let output = temp_path(ext)?;
        let sources = audio::resolve(cfg);
        let args = ffmpeg_args(area, cfg, &sources, &output);
        if std::env::var_os("PEEK2_DEBUG").is_some() {
            eprintln!("peek2: {}", args.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" "));
        }
        let levels: meter::Sink = Rc::default();
        let (proc, stderr) = if sources.is_empty() {
            let (proc, log) = spawn(&args, true)?;
            (proc, Stderr::Log(log))
        } else {
            let argv: Vec<&std::ffi::OsStr> = args.iter().map(|s| s.as_os_str()).collect();
            let proc = gio::Subprocess::newv(
                &argv,
                gio::SubprocessFlags::STDIN_PIPE | gio::SubprocessFlags::STDOUT_SILENCE | gio::SubprocessFlags::STDERR_PIPE,
            )
            .context("failed to run ffmpeg")?;
            let other: Rc<RefCell<VecDeque<String>>> = Rc::default();
            if let Some(err) = proc.stderr_pipe() {
                meter::read_stderr(err, levels.clone(), other.clone());
            }
            (proc, Stderr::Piped(other))
        };
        Ok(Recording { proc, stderr, levels, output, area, started: Instant::now() })
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Ask ffmpeg to finish writing the file.
    pub fn stop(&self) {
        if let Some(stdin) = self.proc.stdin_pipe() {
            if stdin.write_all(b"q", gio::Cancellable::NONE).is_err() {
                self.proc.send_signal(2);
            }
            let _ = stdin.flush(gio::Cancellable::NONE);
        } else {
            self.proc.send_signal(2);
        }
    }

    pub fn kill(&self) {
        self.proc.force_exit();
        let _ = std::fs::remove_file(&self.output);
    }

    /// Resolves once ffmpeg exits; Ok means the output file is complete.
    pub async fn finished(&self) -> Result<()> {
        let res = match &self.stderr {
            Stderr::Log(log) => wait(&self.proc, log).await,
            Stderr::Piped(other) => match self.proc.wait_future().await {
                Ok(()) => {
                    let msg = other.borrow().iter().cloned().collect::<Vec<_>>().join("\n");
                    check_exit(&self.proc, &msg)
                }
                Err(e) => Err(e.into()),
            },
        };
        res.context("ffmpeg recording failed")
    }
}
