//! Fit a clip under a size limit (Discord's 10 MB and the like). The user's
//! `fit_strategy` decides what gives way: resolution, framerate or quality.
//!
//! Video: the bitrate follows from size and duration; resolution and
//! framerate are then chosen so each pixel keeps enough bits to look clean
//! (`GOOD_BPP`). The result is measured and re-encoded smaller if needed.
//! GIF/APNG size can't be predicted, so those are encoded, measured and
//! adjusted until they fit.
//!
//! Everything here blocks; it runs on worker threads.

use crate::config::{FitStrategy, Format};
use crate::proc::{self, temp_path};
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;

/// Bits per pixel per frame at which VP9/H.264 screen content stays clean.
const GOOD_BPP: f64 = 0.07;
/// Never shrink below a quarter of the size, or below 10 fps.
const MIN_SCALE: f64 = 0.25;
const MIN_FPS: f64 = 10.0;
/// Below this the video is mush whatever the shape.
const MIN_VIDEO_BPS: f64 = 60_000.0;

#[derive(Clone, Copy, Debug)]
pub struct Info {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub duration: f64,
    pub audio: bool,
}

pub fn probe(path: &Path) -> Result<Info> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_type,width,height,avg_frame_rate:format=duration", "-of", "default=nw=1"])
        .arg(path)
        .output()
        .context("failed to run ffprobe")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut info = Info { width: 0, height: 0, fps: 0.0, duration: 0.0, audio: false };
    let mut video = false;
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k {
            "codec_type" => {
                video = v == "video";
                info.audio |= v == "audio";
            }
            "width" if video && info.width == 0 => info.width = v.parse().unwrap_or(0),
            "height" if video && info.height == 0 => info.height = v.parse().unwrap_or(0),
            "avg_frame_rate" if video && info.fps == 0.0 => {
                info.fps = match v.split_once('/') {
                    Some((n, d)) => n.parse::<f64>().unwrap_or(0.0) / d.parse::<f64>().unwrap_or(1.0).max(1.0),
                    None => v.parse().unwrap_or(0.0),
                };
            }
            "duration" => info.duration = v.parse().unwrap_or(0.0),
            _ => {}
        }
    }
    if info.width == 0 || info.height == 0 {
        bail!("{} has no video", path.display());
    }
    if !info.fps.is_finite() || info.fps <= 0.0 {
        info.fps = 30.0;
    }
    Ok(info)
}

/// Output resolution and framerate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shape {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

impl Shape {
    pub fn of(info: &Info) -> Shape {
        Shape { width: info.width, height: info.height, fps: info.fps }
    }

    fn scaled(info: &Info, s: f64, fps: f64) -> Shape {
        let even = |v: f64| (((v / 2.0).round() as u32) * 2).max(2);
        Shape { width: even(info.width as f64 * s), height: even(info.height as f64 * s), fps }
    }

    fn scale(&self, info: &Info) -> f64 {
        self.width as f64 / info.width as f64
    }

    /// ffmpeg filters turning the source into this shape ("null" if none).
    pub fn filter(&self, info: &Info) -> String {
        let mut f = Vec::new();
        if (self.fps - info.fps).abs() > 0.01 {
            f.push(format!("fps={:.3}", self.fps));
        }
        if (self.width, self.height) != (info.width, info.height) {
            f.push(format!("scale={}:{}:flags=lanczos", self.width, self.height));
        }
        if f.is_empty() { "null".into() } else { f.join(",") }
    }
}

/// Resolution and framerate for a video bitrate (bits/s).
pub fn shape_for(info: &Info, vbps: f64, strategy: FitStrategy) -> Shape {
    let (w, h, f0) = (info.width as f64, info.height as f64, info.fps.max(1.0));
    // Within 1 % of GOOD_BPP counts as enough (float noise after a step).
    let bpp = |s: f64, f: f64| {
        let b = vbps / (w * s * h * s * f);
        if b >= GOOD_BPP * 0.99 { b.max(GOOD_BPP) } else { b }
    };
    // Scale that brings bpp up to GOOD_BPP (never above 1).
    let scale_for = |s: f64, f: f64, min: f64| (s * (bpp(s, f) / GOOD_BPP).sqrt()).clamp(min.min(s), s);
    // Whole frames per second when lowered.
    let fps_for = |s: f64, f: f64, min: f64| {
        let n = (f * bpp(s, f) / GOOD_BPP).clamp(min.min(f), f);
        if n < f { n.floor().max(min.min(f)) } else { f }
    };
    let (s, f) = match strategy {
        FitStrategy::Quality => (1.0, f0),
        FitStrategy::Scale => (scale_for(1.0, f0, MIN_SCALE), f0),
        FitStrategy::Framerate => {
            let f = fps_for(1.0, f0, MIN_FPS);
            (scale_for(1.0, f, MIN_SCALE), f)
        }
        FitStrategy::Auto => {
            let f = f0.min(30.0);
            let s = scale_for(1.0, f, 0.5);
            let f = fps_for(s, f, 15.0);
            (scale_for(s, f, MIN_SCALE), f)
        }
    };
    Shape::scaled(info, s, f)
}

fn mb(bytes: u64) -> String {
    format!("{} MB", (bytes as f64 / 1e6 * 10.0).round() / 10.0)
}

fn size_of(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(u64::MAX)
}

fn encode_video(input: &Path, output: &Path, fmt: Format, info: &Info, shape: Shape, vbps: f64, abps: u32, cancel: &AtomicBool) -> Result<()> {
    let mut a: Vec<OsString> = proc::ffmpeg(&["-i"]);
    a.push(input.into());
    let v = (vbps / 1000.0).round() as u64;
    let mut push = |s: &str| a.push(s.into());
    for s in ["-map", "0:v:0", "-map", "0:a:0?", "-vf"] {
        push(s);
    }
    push(&format!("{},format=yuv420p", shape.filter(info)));
    let rate = [format!("{v}k"), format!("{}k", v * 3 / 2), format!("{}k", v * 2)];
    match fmt {
        Format::Mp4 => {
            for s in ["-c:v", "libx264", "-preset", "medium"] {
                push(s);
            }
            for (k, r) in ["-b:v", "-maxrate", "-bufsize"].iter().zip(&rate) {
                push(k);
                push(r);
            }
            for s in ["-c:a", "aac", "-movflags", "+faststart"] {
                push(s);
            }
        }
        _ => {
            for s in ["-c:v", "libvpx-vp9", "-deadline", "good", "-cpu-used", "4", "-row-mt", "1"] {
                push(s);
            }
            for (k, r) in ["-b:v", "-maxrate", "-bufsize"].iter().zip(&rate) {
                push(k);
                push(r);
            }
            for s in ["-c:a", "libopus"] {
                push(s);
            }
        }
    }
    push("-b:a");
    push(&format!("{}k", abps / 1000));
    a.push(output.into());
    proc::run(&a, Some(cancel))
}

/// Re-encode a WebM/MP4 to at most `target` bytes. Returns a new temp file.
pub fn fit_video(input: &Path, fmt: Format, target: u64, strategy: FitStrategy, cancel: &AtomicBool) -> Result<PathBuf> {
    let info = probe(input)?;
    if info.duration <= 0.0 {
        bail!("could not determine the clip length");
    }
    // Container overhead and rate-control slack.
    let mut budget = 0.92;
    for _ in 0..4 {
        let total = target as f64 * 8.0 * budget / info.duration;
        let abps: u32 = match (info.audio, total) {
            (false, _) => 0,
            (_, t) if t > 1.5e6 => 128_000,
            (_, t) if t > 400e3 => 96_000,
            _ => 48_000,
        };
        let vbps = total - abps as f64;
        if vbps < MIN_VIDEO_BPS {
            bail!("{:.0} s is too long to fit in {}; pick a bigger size or record less", info.duration, mb(target));
        }
        let shape = shape_for(&info, vbps, strategy);
        if std::env::var_os("PEEK2_DEBUG").is_some() {
            eprintln!("peek2: fit: {shape:?} at {:.0} kb/s video, {} kb/s audio", vbps / 1000.0, abps / 1000);
        }
        let out = temp_path(fmt.ext())?;
        if let Err(e) = encode_video(input, &out, fmt, &info, shape, vbps, abps, cancel) {
            let _ = std::fs::remove_file(&out);
            return Err(e.context("re-encoding to fit failed"));
        }
        let size = size_of(&out);
        if size <= target {
            return Ok(out);
        }
        let _ = std::fs::remove_file(&out);
        budget *= target as f64 / size as f64 * 0.97;
    }
    bail!("could not get the clip under {}", mb(target))
}

/// Next shape (and GIF quality) after an animation came out `size` bytes
/// against `target`; None when nothing is left to give.
pub fn shrink_animation(info: &Info, shape: Shape, quality: u8, size: u64, target: u64, strategy: FitStrategy) -> Option<(Shape, u8)> {
    // Size roughly follows pixels × frames.
    let ratio = (target as f64 / size as f64 * 0.93).min(0.95);
    let s = shape.scale(info);
    let scale = |shape: Shape| {
        let ns = (s * ratio.sqrt()).max(MIN_SCALE);
        (ns < s - 0.005).then(|| Shape::scaled(info, ns, shape.fps))
    };
    let fewer_frames = |min: f64| {
        let f = (shape.fps * ratio).floor().max(min);
        (f < shape.fps - 0.01).then_some(Shape { fps: f, ..shape })
    };
    match strategy {
        FitStrategy::Scale => scale(shape).map(|s| (s, quality)),
        FitStrategy::Framerate => fewer_frames(MIN_FPS).or_else(|| scale(shape)).map(|s| (s, quality)),
        // Lower quality only goes so far for GIF; then scale.
        FitStrategy::Quality if quality > 40 => Some((shape, quality.saturating_sub(25).max(40))),
        FitStrategy::Quality => scale(shape).map(|s| (s, quality)),
        FitStrategy::Auto => {
            if shape.fps > 15.0 {
                fewer_frames(15.0).map(|s| (s, quality))
            } else if s > 0.5 {
                let ns = (s * ratio.sqrt()).max(0.5);
                Some((Shape::scaled(info, ns, shape.fps), quality))
            } else if quality > 60 {
                Some((shape, 60))
            } else {
                scale(shape).map(|s| (s, quality))
            }
        }
    }
}

/// Encode an animation with `encode(shape, quality, output)` until it is at
/// most `target` bytes (one pass when `target` is 0).
pub fn fit_animation(
    info: &Info,
    first: Shape,
    quality: u8,
    target: u64,
    strategy: FitStrategy,
    ext: &str,
    mut encode: impl FnMut(Shape, u8, &Path) -> Result<()>,
) -> Result<PathBuf> {
    let (mut shape, mut quality) = (first, quality);
    for _ in 0..8 {
        let out = temp_path(ext)?;
        if let Err(e) = encode(shape, quality, &out) {
            let _ = std::fs::remove_file(&out);
            return Err(e);
        }
        let size = size_of(&out);
        if target == 0 || size <= target {
            return Ok(out);
        }
        let _ = std::fs::remove_file(&out);
        match shrink_animation(info, shape, quality, size, target, strategy) {
            Some((s, q)) => {
                if std::env::var_os("PEEK2_DEBUG").is_some() {
                    eprintln!("peek2: fit: {} too big, trying {s:?} q{q}", mb(size));
                }
                shape = s;
                quality = q;
            }
            None => break,
        }
    }
    bail!("could not get the {} under {}; try fit_strategy = \"scale\" or a shorter clip", ext.to_uppercase(), mb(target))
}

/// Shrink an existing clip next to itself: `name (10 MB).ext`.
pub fn fit_file(path: &Path, target_mb: u32, strategy: FitStrategy, gif_quality: u8) -> Result<PathBuf> {
    let target = target_mb as u64 * 1_000_000;
    if size_of(path) <= target {
        bail!("{} is already under {target_mb} MB", path.display());
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let cancel = AtomicBool::new(false);
    let out = match ext.as_str() {
        "mp4" | "m4v" | "mov" => fit_video(path, Format::Mp4, target, strategy, &cancel)?,
        "webm" | "mkv" => fit_video(path, Format::Webm, target, strategy, &cancel)?,
        "gif" => crate::postprocess::gif(path, None, gif_quality, target, strategy, &crate::postprocess::Job::default())?,
        "apng" | "png" => crate::postprocess::apng(path, None, target, strategy, &crate::postprocess::Job::default())?,
        _ => bail!("don't know how to shrink .{ext} files"),
    };
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = if ext == "mkv" { "webm".to_string() } else { ext };
    let dest = path.with_file_name(format!("{stem} ({target_mb} MB).{ext}"));
    crate::controller::move_file(&out, &dest)?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> Info {
        Info { width: 2560, height: 1440, fps: 60.0, duration: 30.0, audio: true }
    }

    #[test]
    fn quality_keeps_shape() {
        let s = shape_for(&info(), 500_000.0, FitStrategy::Quality);
        assert_eq!((s.width, s.height, s.fps), (2560, 1440, 60.0));
    }

    #[test]
    fn plenty_of_bits_keeps_shape() {
        let s = shape_for(&info(), 50e6, FitStrategy::Scale);
        assert_eq!((s.width, s.height), (2560, 1440));
        // Auto still caps at 30 fps.
        assert_eq!(shape_for(&info(), 50e6, FitStrategy::Auto).fps, 30.0);
    }

    #[test]
    fn scale_shrinks_resolution_only() {
        let s = shape_for(&info(), 2e6, FitStrategy::Scale);
        assert_eq!(s.fps, 60.0);
        assert!(s.width < 2560 && s.width % 2 == 0 && s.height % 2 == 0);
        assert!(s.width >= 640);
    }

    #[test]
    fn framerate_drops_frames_first() {
        let s = shape_for(&info(), 8e6, FitStrategy::Framerate);
        assert!(s.fps < 60.0);
        assert!(s.fps >= MIN_FPS);
    }

    #[test]
    fn auto_halves_before_dropping_frames() {
        let i = info();
        let s = shape_for(&i, 3e6, FitStrategy::Auto);
        assert_eq!(s.fps, 30.0);
        assert!(s.width < 2560 && s.width >= 1280);
        let tight = shape_for(&i, 0.5e6, FitStrategy::Auto);
        assert!(tight.fps < 30.0 && tight.fps >= 15.0);
    }
}
