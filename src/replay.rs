//! Replay buffer, as in OBS: the monitor under the pointer is recorded
//! continuously into a ring of 2-second MPEG-TS segments in
//! `$XDG_RUNTIME_DIR` (RAM, no SSD wear), so "save replay" can join the last
//! N seconds instantly with a stream copy. H.264 on the GPU when VAAPI works,
//! x264 otherwise; always saved as MP4.

use crate::audio;
use crate::config::Config;
use crate::meter;
use crate::proc::{self, Proc, Stderr};
use crate::recorder::Area;
use anyhow::{bail, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const SEGMENT: u32 = 2;

pub struct Replay {
    proc: Proc,
    dir: PathBuf,
    seconds: u32,
}

fn dir() -> PathBuf {
    crate::util::runtime_dir().join("peek2").join("replay")
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
    /// `on_exit` gets ffmpeg's error if the buffer dies on its own.
    pub fn start(cfg: &Config, area: Area, on_exit: impl FnOnce(String) + 'static) -> Result<Replay> {
        let dir = dir();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        // Desktop/mic per the audio setting, whatever the clip format is.
        let mut audio_cfg = cfg.clone();
        audio_cfg.format = crate::config::Format::Mp4;
        let sources = audio::resolve(&audio_cfg);
        let args = args(area, cfg, &sources, &dir);
        if std::env::var_os("PEEK2_DEBUG").is_some() {
            eprintln!("peek2: replay: {}", args.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" "));
        }
        let proc = Proc::spawn(&args, false, Stderr::Log, move |res| on_exit(res.err().map(|e| format!("{e:#}")).unwrap_or_default()))?;
        Ok(Replay { proc, dir, seconds: cfg.replay_seconds.max(SEGMENT) })
    }

    /// Blocking: join the newest segments covering the buffer length into an MP4.
    pub fn save_job(&self) -> impl FnOnce() -> Result<PathBuf> + Send + 'static {
        let (dir, seconds) = (self.dir.clone(), self.seconds);
        move || save(&dir, seconds)
    }
}

fn save(dir: &Path, seconds: u32) -> Result<PathBuf> {
    let mut segs: Vec<(SystemTime, PathBuf)> = std::fs::read_dir(dir)?
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
    let keep = (seconds.div_ceil(SEGMENT) + 1) as usize;
    let segs: Vec<PathBuf> = segs[segs.len().saturating_sub(keep)..].iter().map(|(_, p)| p.clone()).collect();
    let list = proc::temp_path("txt")?;
    std::fs::write(&list, proc::concat_list(&segs))?;
    let out = proc::temp_path("mp4")?;
    let mut a = proc::ffmpeg(&["-f", "concat", "-safe", "0", "-i"]);
    a.push(list.clone().into());
    for x in ["-map", "0", "-c", "copy", "-bsf:a", "aac_adtstoasc", "-movflags", "+faststart"] {
        a.push(x.into());
    }
    a.push(out.clone().into());
    let res = proc::run(&a, None);
    let _ = std::fs::remove_file(&list);
    if let Err(e) = res {
        let _ = std::fs::remove_file(&out);
        return Err(e.context("saving the replay failed"));
    }
    Ok(out)
}

impl Drop for Replay {
    fn drop(&mut self) {
        self.proc.kill();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
