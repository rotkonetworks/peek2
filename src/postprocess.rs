//! Turns the raw recording into the final file. Video formats are written
//! directly by the recorder (and re-encoded only to fit a size limit); GIF
//! goes through the gifski library and APNG through ffmpeg.
//! Blocking: runs on a worker thread.

use crate::config::{Config, FitStrategy, Format};
use crate::fit::{self, Shape};
use crate::proc;
use anyhow::{anyhow, bail, Context, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
    let _ = std::fs::remove_file(&input);
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

struct Progress(Job);

impl gifski::progress::ProgressReporter for Progress {
    fn increase(&mut self) -> bool {
        !self.0.is_cancelled()
    }
}

/// Decode with ffmpeg into raw RGBA frames of the target shape and feed
/// them to gifski, which encodes on its own writer thread.
fn encode_gif(input: &Path, output: &Path, shape: Shape, quality: u8, job: &Job) -> Result<()> {
    let (w, h) = (shape.width as usize, shape.height as usize);
    // Explicit size: left unset, gifski quietly shrinks anything above
    // ~800×600; resizing is the fit strategy's (and `downsample`'s) call.
    let (collector, writer) = gifski::new(gifski::Settings {
        width: Some(shape.width),
        height: Some(shape.height),
        quality,
        fast: false,
        repeat: gifski::Repeat::Infinite,
    })?;

    let file = std::fs::File::create(output)?;
    let job2 = job.clone();
    let writer_thread = std::thread::spawn(move || writer.write(file, &mut Progress(job2)));

    // Always resample to a fixed rate so frame timestamps are exact.
    let filter = format!("fps={:.3},scale={w}:{h}:flags=lanczos", shape.fps);
    let mut child = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"])
        .arg(input)
        .args(["-vf", &filter, "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to run ffmpeg")?;
    let mut stdout = child.stdout.take().context("no ffmpeg stdout")?;
    let mut buf = vec![0u8; w * h * 4];
    let mut index = 0usize;
    let decode = (|| -> Result<()> {
        loop {
            if job.is_cancelled() {
                bail!("cancelled");
            }
            match stdout.read_exact(&mut buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(e.into()),
            }
            let pixels: Vec<rgb::RGBA8> = buf.as_chunks::<4>().0.iter().map(|p| rgb::RGBA8::new(p[0], p[1], p[2], p[3])).collect();
            collector.add_frame_rgba(index, imgref::ImgVec::new(pixels, w, h), index as f64 / shape.fps)?;
            index += 1;
        }
    })();
    let _ = child.kill();
    let _ = child.wait();
    drop(collector);
    let written = writer_thread.join().map_err(|_| anyhow!("gifski panicked"))?;
    decode?;
    if index == 0 {
        bail!("no frames were recorded");
    }
    written.map_err(|e| anyhow!("gifski: {e}"))
}
