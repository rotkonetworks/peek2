//! Turns the raw recording into the final file. Video formats are written
//! directly by the recorder (and re-encoded only to fit a size limit); GIF
//! and APNG are converted by ffmpeg.
//! Blocking: runs on a worker thread.

use crate::config::{Config, FitStrategy, Format};
use crate::fit::{self, Shape};
use crate::proc;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Cancellation handle for a running post-processing job.
#[derive(Clone, Default)]
pub struct Job {
    cancelled: Arc<AtomicBool>,
}

impl Job {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

fn target(cfg: &Config) -> u64 {
    cfg.fit_size_mb as u64 * 1_000_000
}

pub fn run(input: PathBuf, cfg: &Config, job: &Job) -> Result<PathBuf> {
    let fps = cfg.framerate.clamp(1, 50) as f64;
    let result = match cfg.format {
        Format::Mp4 | Format::Webm => {
            let limit = target(cfg);
            if limit == 0 || std::fs::metadata(&input).map(|m| m.len()).unwrap_or(0) <= limit {
                return Ok(input);
            }
            fit::fit_video(&input, cfg.format, limit, cfg.fit_strategy, &job.cancelled)
        }
        Format::Gif => gif(&input, Some(fps), cfg.gif_quality, target(cfg), cfg.fit_strategy, job),
        Format::Apng => apng(&input, Some(fps), target(cfg), cfg.fit_strategy, job),
    };
    // On failure the raw recording stays, so the caller can rescue it.
    if result.is_ok() || job.is_cancelled() {
        let _ = std::fs::remove_file(&input);
    }
    if job.is_cancelled() {
        if let Ok(p) = &result {
            let _ = std::fs::remove_file(p);
        }
        bail!("cancelled");
    }
    result
}

fn first_shape(info: &fit::Info, fps: Option<f64>) -> Shape {
    let mut s = Shape::of(info);
    if let Some(f) = fps {
        s.fps = f.min(info.fps.max(1.0));
    }
    s
}

/// `input` to GIF at `fps` (None: the source's), under `target` bytes if non-zero.
pub fn gif(input: &Path, fps: Option<f64>, quality: u8, target: u64, strategy: FitStrategy, job: &Job) -> Result<PathBuf> {
    let info = fit::probe(input)?;
    let first = first_shape(&info, fps);
    fit::fit_animation(&info, first, quality.clamp(1, 100), target, strategy, "gif", |shape, q, out| {
        encode_gif(input, out, shape, q, job)
    })
}

pub fn apng(input: &Path, fps: Option<f64>, target: u64, strategy: FitStrategy, job: &Job) -> Result<PathBuf> {
    let info = fit::probe(input)?;
    let first = first_shape(&info, fps);
    fit::fit_animation(&info, first, 100, target, strategy, "apng", |shape, _, out| {
        let mut args = proc::ffmpeg(&["-i"]);
        args.push(input.into());
        for a in ["-vf", &shape.filter(&info), "-plays", "0", "-f", "apng"] {
            args.push(a.into());
        }
        args.push(out.into());
        proc::run(&args, Some(&job.cancelled)).context("APNG conversion failed")
    })
}

/// Two-pass palette GIF, tuned for screen content: only moving parts
/// drive the palette, only changed rectangles are re-dithered. `quality`
/// (1-100) sets palette size and dithering, so lowering it shrinks files.
fn encode_gif(input: &Path, output: &Path, shape: Shape, quality: u8, job: &Job) -> Result<()> {
    let q = quality.clamp(1, 100) as u32;
    let colors = (32 + q * 224 / 100).min(256);
    let dither = match q {
        80.. => "sierra2_4a",
        50..80 => "bayer:bayer_scale=2",
        _ => "bayer:bayer_scale=4",
    };
    let filter = format!(
        "fps={:.3},scale={}:{}:flags=lanczos,split[a][b];[a]palettegen=max_colors={colors}:stats_mode=diff[p];[b][p]paletteuse=dither={dither}:diff_mode=rectangle",
        shape.fps, shape.width, shape.height
    );
    let mut args = proc::ffmpeg(&["-i"]);
    args.push(input.into());
    for a in ["-filter_complex", &filter, "-loop", "0", "-f", "gif"] {
        args.push(a.into());
    }
    args.push(output.into());
    proc::run(&args, Some(&job.cancelled)).context("GIF conversion failed")
}
