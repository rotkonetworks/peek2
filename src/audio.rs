//! Audio device discovery through `pactl`, which works on both PulseAudio and
//! PipeWire (pipewire-pulse). ffmpeg records from these sources via `-f pulse`.

use crate::config::{AudioMode, Config};
use std::process::Command;

#[derive(Clone, Debug)]
pub struct Source {
    pub name: String,
    pub description: String,
    /// Set when this source is the monitor of an output (i.e. desktop audio).
    pub is_monitor: bool,
}

pub fn list_sources() -> Vec<Source> {
    let Ok(out) = Command::new("pactl").args(["-f", "json", "list", "sources"]).output() else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return Vec::new();
    };
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|s| {
                    let name = s.get("name")?.as_str()?.to_string();
                    let description = s.get("description").and_then(|d| d.as_str()).unwrap_or(&name).to_string();
                    let is_monitor = s
                        .get("monitor_source")
                        .and_then(|m| m.as_str())
                        .is_some_and(|m| !m.is_empty())
                        || name.ends_with(".monitor");
                    Some(Source { name, description, is_monitor })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn pactl_get(what: &str) -> Option<String> {
    let out = Command::new("pactl").arg(what).output().ok()?;
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn pactl_json(what: &[&str]) -> Vec<serde_json::Value> {
    Command::new("pactl")
        .args(["-f", "json", "list"])
        .args(what)
        .output()
        .ok()
        .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

/// Monitor of the output that is actually playing. The default sink is
/// often not where audio goes (streams moved by a toggle script, HDMI set as
/// default while a headset plays), so prefer the sink with the most
/// uncorked streams, then any running sink, then the default.
fn active_monitor() -> String {
    let sinks = pactl_json(&["sinks"]);
    let inputs = pactl_json(&["sink-inputs"]);
    let default = pactl_get("get-default-sink");
    let name_of = |s: &serde_json::Value| s.get("name").and_then(|n| n.as_str()).map(str::to_string);

    let mut best: Option<(usize, bool, bool, String)> = None;
    for sink in &sinks {
        let Some(name) = name_of(sink) else { continue };
        let index = sink.get("index").and_then(|i| i.as_u64());
        let streams = inputs
            .iter()
            .filter(|i| i.get("sink").and_then(|s| s.as_u64()) == index)
            .filter(|i| !i.get("corked").and_then(|c| c.as_bool()).unwrap_or(false))
            .count();
        let running = sink.get("state").and_then(|s| s.as_str()) == Some("RUNNING");
        let is_default = default.as_deref() == Some(name.as_str());
        let key = (streams, running, is_default, name);
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
    let desktop = || {
        if cfg.desktop_source.is_empty() { active_monitor() } else { cfg.desktop_source.clone() }
    };
    let mic = || if cfg.mic_source.is_empty() { default_mic() } else { cfg.mic_source.clone() };
    match cfg.audio {
        AudioMode::Off => Vec::new(),
        AudioMode::Desktop => vec![desktop()],
        AudioMode::Mic => vec![mic()],
        AudioMode::Both => vec![desktop(), mic()],
    }
}
