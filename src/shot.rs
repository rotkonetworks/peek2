//! Still screenshots: a region of the selector's frozen frame (what the
//! user saw when the overlay opened) encoded to PNG by ffmpeg.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// BGRX pixels of an image.
pub struct Image {
    pub width: i32,
    pub height: i32,
    pub data: Vec<u8>,
}

impl Image {
    /// Cut `(x, y, w, h)` out of a BGRX frame `stride_px` pixels wide.
    pub fn crop(frame: &[u8], stride_px: i32, x: i32, y: i32, width: i32, height: i32) -> Image {
        let stride = stride_px as usize * 4;
        let row = width as usize * 4;
        let mut data = Vec::with_capacity(row * height as usize);
        for r in 0..height as usize {
            let start = (y as usize + r) * stride + x as usize * 4;
            data.extend_from_slice(&frame[start..start + row]);
        }
        Image { width, height, data }
    }

    /// Blocking: write a PNG to a temp file.
    pub fn into_png(self) -> Result<PathBuf> {
        let out = crate::proc::temp_path("png")?;
        let size = format!("{}x{}", self.width, self.height);
        let mut child = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-f", "rawvideo", "-pix_fmt", "bgr0", "-s", &size, "-i", "-"])
            .args(["-frames:v", "1", "-pix_fmt", "rgb24", "-compression_level", "9", "-f", "image2", "-c:v", "png"])
            .arg(&out)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("running ffmpeg")?;
        let mut stdin = child.stdin.take().context("ffmpeg stdin")?;
        let data = self.data;
        let writer = std::thread::spawn(move || stdin.write_all(&data));
        let res = child.wait_with_output()?;
        let _ = writer.join();
        if !res.status.success() || !out.exists() {
            let _ = std::fs::remove_file(&out);
            bail!("PNG encoding failed: {}", String::from_utf8_lossy(&res.stderr).trim());
        }
        Ok(out)
    }
}
