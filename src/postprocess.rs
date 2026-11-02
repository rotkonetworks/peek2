//! Turns the raw recording into the final file. Video formats are written
//! directly by the recorder; GIF goes through the gifski library and APNG
//! through ffmpeg.

use crate::config::{Config, Format};
use crate::recorder::{self, temp_path};
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

pub async fn run(input: PathBuf, cfg: &Config, job: &Job) -> Result<PathBuf> {
    let result = match cfg.format {
        Format::Mp4 | Format::Webm => return Ok(input),
        Format::Gif => gifski(&input, cfg, job).await,
        Format::Apng => apng(&input, cfg, job).await,
    };
    let _ = std::fs::remove_file(&input);
    result
}

async fn apng(input: &Path, cfg: &Config, job: &Job) -> Result<PathBuf> {
    let output = temp_path("apng")?;
    let args: Vec<std::ffi::OsString> = vec![
        "ffmpeg".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-y".into(),
        "-i".into(),
        input.into(),
        "-vf".into(),
        format!("fps={}", cfg.framerate.clamp(1, 50)).into(),
        "-plays".into(),
        "0".into(),
        "-f".into(),
        "apng".into(),
        output.clone().into(),
    ];
    let (proc, log) = recorder::spawn(&args, false)?;
    let watcher = {
        let job = job.clone();
        let proc = proc.clone();
        gtk::glib::timeout_add_local(std::time::Duration::from_millis(200), move || {
            if job.is_cancelled() {
                proc.force_exit();
                return gtk::glib::ControlFlow::Break;
            }
            gtk::glib::ControlFlow::Continue
        })
    };
    let res = recorder::wait(&proc, &log).await;
    if !job.is_cancelled() {
        watcher.remove();
    }
    if let Err(e) = res {
        let _ = std::fs::remove_file(&output);
        return Err(e.context("APNG conversion failed"));
    }
    if job.is_cancelled() {
        let _ = std::fs::remove_file(&output);
        bail!("cancelled");
    }
    Ok(output)
}

struct Progress(Job);

impl gifski::progress::ProgressReporter for Progress {
    fn increase(&mut self) -> bool {
        !self.0.is_cancelled()
    }
}

/// Decode the intermediate with ffmpeg into raw RGBA frames and feed them to
/// gifski on worker threads, so the GTK main loop stays responsive.
async fn gifski(input: &Path, cfg: &Config, job: &Job) -> Result<PathBuf> {
    let output = temp_path("gif")?;
    let (w, h) = probe_size(input)?;
    let fps = cfg.framerate.clamp(1, 50);
    let quality = cfg.gif_quality.clamp(1, 100);
    let (tx, rx) = async_channel::bounded::<Result<()>>(1);

    let input = input.to_path_buf();
    let out = output.clone();
    let job2 = job.clone();
    std::thread::spawn(move || {
        let res = (|| -> Result<()> {
            let (collector, writer) = gifski::new(gifski::Settings {
                width: None,
                height: None,
                quality,
                fast: false,
                repeat: gifski::Repeat::Infinite,
            })?;

            let file = std::fs::File::create(&out)?;
            let job3 = job2.clone();
            let writer_thread = std::thread::spawn(move || writer.write(file, &mut Progress(job3)));

            let mut child = Command::new("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-i"])
                .arg(&input)
                .args(["-vf", &format!("fps={fps}"), "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .context("failed to run ffmpeg")?;
            let mut stdout = child.stdout.take().unwrap();
            let mut buf = vec![0u8; w * h * 4];
            let mut index = 0usize;
            let decode = (|| -> Result<()> {
                loop {
                    if job2.is_cancelled() {
                        bail!("cancelled");
                    }
                    match stdout.read_exact(&mut buf) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                        Err(e) => return Err(e.into()),
                    }
                    let pixels: Vec<rgb::RGBA8> = buf
                        .chunks_exact(4)
                        .map(|p| rgb::RGBA8::new(p[0], p[1], p[2], p[3]))
                        .collect();
                    let img = imgref::ImgVec::new(pixels, w, h);
                    collector.add_frame_rgba(index, img, index as f64 / fps as f64)?;
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
        })();
        let _ = tx.send_blocking(res);
    });

    let res = rx.recv().await.unwrap_or_else(|_| Err(anyhow!("gifski worker died")));
    if res.is_err() || job.is_cancelled() {
        let _ = std::fs::remove_file(&output);
    }
    res?;
    if job.is_cancelled() {
        bail!("cancelled");
    }
    Ok(output)
}

fn probe_size(input: &Path) -> Result<(usize, usize)> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height", "-of", "csv=p=0"])
        .arg(input)
        .output()
        .context("failed to run ffprobe")?;
    let s = String::from_utf8_lossy(&out.stdout);
    let mut it = s.trim().split(',').map(|v| v.trim().parse::<usize>());
    match (it.next(), it.next()) {
        (Some(Ok(w)), Some(Ok(h))) if w > 0 && h > 0 => Ok((w, h)),
        _ => bail!("could not determine recording size"),
    }
}
