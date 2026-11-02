//! Replay buffer, as in OBS: the monitor under the pointer is recorded
//! continuously into a ring of 2-second MPEG-TS segments in
//! `$XDG_RUNTIME_DIR` (RAM, no SSD wear), so "save replay" can join the last
//! N seconds instantly with a stream copy. H.264 on the GPU when VAAPI works,
//! x264 otherwise; always saved as MP4.

use crate::audio;
use crate::config::Config;
use crate::meter;
use crate::recorder::{self, Area};
use crate::x11;
use anyhow::{bail, Context, Result};
use gtk::{gdk, gio, glib, prelude::*};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const SEGMENT: u32 = 2;

pub struct Replay {
    proc: gio::Subprocess,
    log: PathBuf,
    dir: PathBuf,
    seconds: u32,
}

fn dir() -> PathBuf {
    glib::user_runtime_dir().join("peek2").join("replay")
}

/// The monitor under the pointer, in root pixels.
pub fn current_monitor() -> Option<Area> {
    let display = gdk::Display::default()?;
    let monitors = display.monitors();
    let pointer = x11::get().and_then(|x| x.pointer());
    let rects: Vec<Area> = (0..monitors.n_items())
        .filter_map(|i| monitors.item(i).and_downcast::<gdk::Monitor>())
        .map(|m| {
            let (g, s) = (m.geometry(), m.scale_factor().max(1));
            Area { x: g.x() * s, y: g.y() * s, width: g.width() * s, height: g.height() * s }
        })
        .collect();
    rects
        .iter()
        .find(|r| pointer.is_some_and(|(px, py)| px >= r.x && py >= r.y && px < r.x + r.width && py < r.y + r.height))
        .or(rects.first())
        .copied()
}

fn args(area: Area, cfg: &Config, sources: &[String], dir: &Path) -> Vec<OsString> {
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
    let hw = cfg.hardware_encoding.then(crate::hw::vaapi_device_blocking).flatten();
    let fps = cfg.framerate.clamp(1, 120);
    let d = cfg.downsample.max(1);
    let wrap = cfg.replay_seconds.div_ceil(SEGMENT) + 2;
    let mut a: Vec<String> = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-nostats", "-nostdin", "-y"].map(String::from).to_vec();
    if let Some(dev) = hw {
        a.extend(["-vaapi_device".into(), dev.into()]);
    }
    a.extend(["-thread_queue_size", "1024", "-f", "x11grab", "-show_region", "0", "-framerate"].map(String::from));
    a.push(fps.to_string());
    a.push("-video_size".into());
    a.push(format!("{}x{}", area.width, area.height));
    if !cfg.capture_mouse {
        a.extend(["-draw_mouse".into(), "0".into()]);
    }
    a.push("-i".into());
    a.push(format!("{display}+{},{}", area.x, area.y));
    for src in sources {
        a.extend(["-thread_queue_size", "1024", "-f", "pulse", "-fragment_size", "4096", "-i"].map(String::from));
        a.push(src.clone());
    }
    let upload = if hw.is_some() { "format=nv12,hwupload" } else { "format=yuv420p" };
    let mut graph = format!("[0:v]scale=trunc(iw/{d}/2)*2:trunc(ih/{d}/2)*2:flags=lanczos,{upload}[v]");
    if !sources.is_empty() {
        graph.push(';');
        // One mixed track: replays are for sharing.
        let opts = meter::AudioOpts { separate: false, ..meter::AudioOpts::new(cfg, sources.len()) };
        graph.push_str(&meter::audio_graph(1, sources.len(), true, opts));
    }
    a.extend(["-filter_complex".into(), graph, "-map".into(), "[v]".into()]);
    if !sources.is_empty() {
        a.extend(["-map".into(), "[a]".into()]);
    }
    let q = cfg.video_quality.to_string();
    if hw.is_some() {
        a.extend(["-c:v", "h264_vaapi", "-rc_mode", "CQP", "-qp"].map(String::from));
    } else {
        a.extend(["-c:v", "libx264", "-preset", "veryfast", "-crf"].map(String::from));
    }
    a.push(q);
    // Every segment must start on a keyframe to be joinable on its own.
    a.extend(["-g".into(), (fps * SEGMENT).to_string(), "-force_key_frames".into(), format!("expr:gte(t,n_forced*{SEGMENT})")]);
    if !sources.is_empty() {
        a.extend(["-c:a", "aac", "-b:a", "160k"].map(String::from));
    }
    a.extend(["-f", "segment", "-segment_format", "mpegts", "-reset_timestamps", "1", "-segment_time"].map(String::from));
    a.push(SEGMENT.to_string());
    a.push("-segment_wrap".into());
    a.push(wrap.to_string());
    a.push(dir.join("seg%03d.ts").to_string_lossy().into_owned());
    a.into_iter().map(OsString::from).collect()
}

impl Replay {
    pub fn start(cfg: &Config) -> Result<Replay> {
        let area = current_monitor().context("no monitor found")?;
        let dir = dir();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).context("creating replay buffer dir")?;
        // Desktop/mic per the audio setting, whatever the clip format is.
        let mut audio_cfg = cfg.clone();
        audio_cfg.format = crate::config::Format::Mp4;
        let sources = audio::resolve(&audio_cfg);
        let args = args(area, cfg, &sources, &dir);
        if std::env::var_os("PEEK2_DEBUG").is_some() {
            eprintln!("peek2: replay: {}", args.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" "));
        }
        let (proc, log) = recorder::spawn(&args, false)?;
        Ok(Replay { proc, log, dir, seconds: cfg.replay_seconds.max(SEGMENT) })
    }

    /// End the buffer and drop its segments.
    pub fn stop(&self) {
        self.proc.force_exit();
        let _ = std::fs::remove_file(&self.log);
        let _ = std::fs::remove_dir_all(&self.dir);
    }

    /// Resolves with ffmpeg's error output if the buffer dies.
    pub async fn exited(&self) -> String {
        let _ = self.proc.wait_future().await;
        let msg = std::fs::read_to_string(&self.log).unwrap_or_default();
        msg.lines().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")
    }

    /// Join the newest segments covering the buffer length into an MP4.
    pub async fn save(&self) -> Result<PathBuf> {
        let mut segs: Vec<(SystemTime, PathBuf)> = std::fs::read_dir(&self.dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "ts"))
            .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
            .filter(|(_, p)| std::fs::metadata(p).is_ok_and(|m| m.len() > 0))
            .collect();
        if segs.is_empty() {
            bail!("the replay buffer is still empty");
        }
        segs.sort();
        // The newest segment is still being written; MPEG-TS reads fine cut off.
        let keep = (self.seconds.div_ceil(SEGMENT) + 1) as usize;
        let segs = &segs[segs.len().saturating_sub(keep)..];
        let list = recorder::temp_path("txt")?;
        let body: String =
            segs.iter().map(|(_, p)| format!("file '{}'\n", p.to_string_lossy().replace('\'', "'\\''"))).collect();
        std::fs::write(&list, body)?;
        let out = recorder::temp_path("mp4")?;
        let mut a: Vec<OsString> =
            ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-f", "concat", "-safe", "0", "-i"].map(OsString::from).to_vec();
        a.push(list.clone().into());
        for x in ["-map", "0", "-c", "copy", "-bsf:a", "aac_adtstoasc", "-movflags", "+faststart"] {
            a.push(x.into());
        }
        a.push(out.clone().into());
        let (proc, log) = recorder::spawn(&a, false)?;
        let res = recorder::wait(&proc, &log).await;
        let _ = std::fs::remove_file(&list);
        if let Err(e) = res {
            let _ = std::fs::remove_file(&out);
            return Err(e.context("saving the replay failed"));
        }
        Ok(out)
    }
}

impl Drop for Replay {
    fn drop(&mut self) {
        self.proc.force_exit();
        let _ = std::fs::remove_file(&self.log);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
