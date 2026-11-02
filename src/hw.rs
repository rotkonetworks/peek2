//! VAAPI hardware encoding (AMD/Intel GPUs). Probed once in the background
//! at startup with a tiny test encode, so recording never waits on it and a
//! driver that advertises an encoder it can't run is never used.

use std::process::{Command, Stdio};
use std::sync::OnceLock;

static DEVICE: OnceLock<Option<String>> = OnceLock::new();

fn works(device: &str) -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-vaapi_device", device])
        .args(["-f", "lavfi", "-i", "testsrc2=s=256x144:r=30", "-frames:v", "5"])
        .args(["-vf", "format=nv12,hwupload", "-c:v", "h264_vaapi", "-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn detect() -> Option<String> {
    let mut nodes: Vec<String> = std::fs::read_dir("/dev/dri")
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path().to_string_lossy().into_owned())
        .filter(|p| p.contains("renderD"))
        .collect();
    nodes.sort();
    nodes.into_iter().find(|n| works(n))
}

/// Start probing; call early so the answer is ready by the first recording.
pub fn probe() {
    std::thread::spawn(|| {
        let d = DEVICE.get_or_init(detect);
        if std::env::var_os("PEEK2_DEBUG").is_some() {
            eprintln!("peek2: vaapi h264: {}", d.as_deref().unwrap_or("unavailable"));
        }
    });
}

/// Render node with a working H.264 encoder, probing now if needed (for
/// long-running encodes where a short wait beats falling back to the CPU).
pub fn vaapi_device_blocking() -> Option<&'static str> {
    DEVICE.get_or_init(detect).as_deref()
}

/// Render node with a working H.264 encoder, if the probe has finished and
/// found one.
pub fn vaapi_device() -> Option<&'static str> {
    DEVICE.get().and_then(|d| d.as_deref())
}
