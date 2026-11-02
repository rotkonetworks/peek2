//! Screen capture backend. On X11 (and XWayland) this drives ffmpeg's
//! `x11grab` + `pulse` inputs, the same building blocks OBS uses there.
//! Stopping is done by sending `q` on stdin so ffmpeg finalizes the container.

use crate::audio;
use crate::config::{Config, Format};
use crate::meter;
use crate::proc::{self, Proc, Stderr};
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
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
        graph.push_str(&meter::audio_graph(1, sources.len(), true, audio_opts));
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
        for (i, name) in cfg.audio.sources().iter().enumerate().take(sources.len()) {
            push("-map");
            push(&format!("[t{i}]"));
            push(&format!("-metadata:s:a:{}", i + 1));
            push(&format!("title={}", if *name == "mic" { "Microphone" } else { "Desktop audio" }));
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
                "-c:v", "libvpx-vp9", "-deadline", "realtime", "-cpu-used", "8", "-row-mt", "1", "-b:v", "0", "-crf",
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

/// Join recorded parts (same encoder settings) without re-encoding.
/// Blocking. Consumes the parts; a single part is returned as is.
pub fn concat(parts: &[PathBuf]) -> Result<PathBuf> {
    match parts {
        [] => bail!("nothing was recorded"),
        [one] => return Ok(one.clone()),
        _ => {}
    }
    let ext = parts[0].extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_else(|| "mkv".into());
    let list = proc::temp_path("txt")?;
    std::fs::write(&list, proc::concat_list(parts)).context("writing concat list")?;
    let out = proc::temp_path(&ext)?;
    let mut args = proc::ffmpeg(&["-f", "concat", "-safe", "0", "-i"]);
    args.push(list.clone().into());
    for a in ["-map", "0", "-c", "copy"] {
        args.push(a.into());
    }
    if ext == "mp4" {
        args.push("-movflags".into());
        args.push("+faststart".into());
    }
    args.push(out.clone().into());
    let res = proc::run(&args, None::<&AtomicBool>);
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

pub struct Recording {
    proc: Proc,
    pub output: PathBuf,
    started: Instant,
}

impl Recording {
    /// `levels` receives per-channel peak dBFS while recording with audio;
    /// `on_exit` runs once ffmpeg is gone (Ok: the output is complete).
    pub fn start(
        area: Area,
        cfg: &Config,
        levels: impl Fn(Vec<f32>) + 'static,
        on_exit: impl FnOnce(Result<()>) + 'static,
    ) -> Result<Recording> {
        if !area.is_valid() {
            bail!("recording area is too small");
        }
        let ext = if cfg.format.is_animation() { "mkv" } else { cfg.format.ext() };
        let output = proc::temp_path(ext)?;
        let sources = audio::resolve(cfg);
        let args = ffmpeg_args(area, cfg, &sources, &output);
        if std::env::var_os("PEEK2_DEBUG").is_some() {
            eprintln!("peek2: {}", args.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" "));
        }
        let stderr = if sources.is_empty() { Stderr::Log } else { Stderr::Lines(meter::parser(levels)) };
        let proc = Proc::spawn(&args, true, stderr, move |res| on_exit(res.context("ffmpeg recording failed")))?;
        Ok(Recording { proc, output, started: Instant::now() })
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn stop(&self) {
        self.proc.quit();
    }

    pub fn kill(&self) {
        self.proc.kill();
    }
}
