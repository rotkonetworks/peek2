//! Audio device discovery through `pactl`, which works on both PulseAudio and
//! PipeWire (pipewire-pulse). ffmpeg records from these sources via `-f pulse`.
//! pactl runs with LC_ALL=C so its text output is parseable.

use crate::config::{AudioMode, Config};
use std::process::{Command, Stdio};

fn pactl(args: &[&str]) -> Option<String> {
    let out = Command::new("pactl").args(args).env("LC_ALL", "C").stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn pactl_get(what: &str) -> Option<String> {
    let s = pactl(&[what])?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

struct Sink {
    index: u64,
    name: String,
    running: bool,
}

fn sinks() -> Vec<Sink> {
    // index \t name \t driver \t sample spec \t state
    pactl(&["list", "short", "sinks"])
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            Some(Sink { index: f.first()?.parse().ok()?, name: f.get(1)?.to_string(), running: f.get(4) == Some(&"RUNNING") })
        })
        .collect()
}

/// (sink index, corked) for each playback stream.
fn sink_inputs() -> Vec<(u64, bool)> {
    let text = pactl(&["list", "sink-inputs"]).unwrap_or_default();
    let mut out = Vec::new();
    let mut cur: Option<(Option<u64>, bool)> = None;
    for line in text.lines() {
        if line.starts_with("Sink Input #") {
            if let Some((Some(s), c)) = cur.take() {
                out.push((s, c));
            }
            cur = Some((None, false));
        } else if let Some(c) = cur.as_mut() {
            let l = line.trim();
            if let Some(v) = l.strip_prefix("Sink:") {
                c.0 = v.trim().parse().ok();
            } else if let Some(v) = l.strip_prefix("Corked:") {
                c.1 = v.trim() == "yes";
            }
        }
    }
    if let Some((Some(s), c)) = cur {
        out.push((s, c));
    }
    out
}

/// Monitor of the output that is actually playing. The default sink is
/// often not where audio goes (streams moved by a toggle script, HDMI set as
/// default while a headset plays), so prefer the sink with the most
/// uncorked streams, then any running sink, then the default.
fn active_monitor() -> String {
    let inputs = sink_inputs();
    let default = pactl_get("get-default-sink");
    let mut best: Option<(usize, bool, bool, String)> = None;
    for sink in sinks() {
        let streams = inputs.iter().filter(|(s, corked)| *s == sink.index && !corked).count();
        let is_default = default.as_deref() == Some(sink.name.as_str());
        let key = (streams, sink.running, is_default, sink.name);
        if best.as_ref().is_none_or(|b| (key.0, key.1, key.2) > (b.0, b.1, b.2)) {
            best = Some(key);
        }
    }
    match best {
        Some((_, _, _, name)) => format!("{name}.monitor"),
        None => default.map(|s| format!("{s}.monitor")).unwrap_or_else(|| "@DEFAULT_MONITOR@".into()),
    }
}

fn default_mic() -> String {
    pactl_get("get-default-source").unwrap_or_else(|| "default".into())
}

/// Resolve the configured audio mode into concrete pulse source names.
pub fn resolve(cfg: &Config) -> Vec<String> {
    if !cfg.format.has_audio() {
        return Vec::new();
    }
    let desktop = || if cfg.desktop_source.is_empty() { active_monitor() } else { cfg.desktop_source.clone() };
    let mic = || if cfg.mic_source.is_empty() { default_mic() } else { cfg.mic_source.clone() };
    match cfg.audio {
        AudioMode::Off => Vec::new(),
        AudioMode::Desktop => vec![desktop()],
        AudioMode::Mic => vec![mic()],
        AudioMode::Both => vec![desktop(), mic()],
    }
}
