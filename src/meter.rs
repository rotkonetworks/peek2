//! Audio level metering. Each pulse source is normalised to 48 kHz stereo,
//! split off and merged into one multichannel stream (source 0 → channels
//! 1-2, source 1 → 3-4, …) that `astats` measures 10× per second. ffmpeg
//! logs the peaks via `ametadata` on stderr (unbuffered, unlike its avio
//! outputs), so the meter shows exactly what is being recorded, live.

use crate::proc::{Proc, Stderr};
use std::ffi::OsString;

const METER_CHAIN: &str = "asetnsamples=n=4800:p=0,astats=metadata=1:reset=1:measure_perchannel=Peak_level:measure_overall=none,ametadata=mode=print";

/// Per-take audio processing choices.
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioOpts {
    /// Index of the microphone among the sources.
    pub mic: Option<usize>,
    /// Adaptive FFT noise reduction on the microphone.
    pub denoise: bool,
    /// Also write each source as its own track (`[t0]`, `[t1]`, …).
    pub separate: bool,
}

impl AudioOpts {
    pub fn new(cfg: &crate::config::Config, n: usize) -> AudioOpts {
        use crate::config::AudioMode;
        let mic = match cfg.audio {
            AudioMode::Mic => Some(0),
            AudioMode::Both => Some(1),
            _ => None,
        }
        .filter(|i| *i < n);
        AudioOpts { mic, denoise: cfg.mic_noise_suppression, separate: cfg.separate_audio_tracks && n > 1 }
    }
}

/// Audio part of a `-filter_complex` graph for pulse inputs starting at
/// input index `first`. When `record`, produces `[a]` for the encoder (and
/// `[t<i>]` per source with `separate`) and the meter branch ends in a sink;
/// otherwise only `[meter]` is produced, for mapping to a null output.
pub fn audio_graph(first: usize, n: usize, record: bool, opts: AudioOpts) -> String {
    let mut g = Vec::new();
    for i in 0..n {
        let denoise = if opts.denoise && opts.mic == Some(i) { ",afftdn=nr=12:nf=-40:tn=1" } else { "" };
        let tracks = if record && opts.separate { format!("asplit=3[r{i}][m{i}][t{i}]") } else { format!("asplit[r{i}][m{i}]") };
        g.push(format!("[{}:a]aformat=sample_rates=48000:channel_layouts=stereo{denoise},{tracks}", first + i));
    }
    let rec: String = (0..n).map(|i| format!("[r{i}]")).collect();
    let met: String = (0..n).map(|i| format!("[m{i}]")).collect();
    if record {
        if n == 1 {
            g.push("[r0]aresample=async=1000[a]".into());
        } else {
            g.push(format!("{rec}amix=inputs={n}:duration=longest:normalize=0,aresample=async=1000[a]"));
        }
    } else {
        for i in 0..n {
            g.push(format!("[r{i}]anullsink"));
        }
    }
    let merge = if n == 1 { format!("{met}anull") } else { format!("{met}amerge=inputs={n}") };
    let sink = if record { ",anullsink" } else { "[meter]" };
    g.push(format!("{merge},{METER_CHAIN}{sink}"));
    g.join(";")
}

/// A stderr line handler that turns meter lines into per-channel peak dBFS
/// (-inf for silence), reported once per measuring window.
pub fn parser(sink: impl Fn(Vec<f32>) + 'static) -> Box<dyn FnMut(&str)> {
    let mut cur: Vec<f32> = Vec::new();
    Box::new(move |line: &str| {
        if let Some(i) = line.find("lavfi.astats.") {
            let rest = &line[i + "lavfi.astats.".len()..];
            let Some((ch, val)) = rest.split_once(".Peak_level=") else { return };
            let Ok(ch) = ch.parse::<usize>() else { return };
            if ch == 0 {
                return;
            }
            let v = val.trim().parse::<f32>().unwrap_or(f32::NEG_INFINITY);
            if cur.len() < ch {
                cur.resize(ch, f32::NEG_INFINITY);
            }
            cur[ch - 1] = v;
        } else if line.contains("] frame:") && !cur.is_empty() {
            sink(std::mem::take(&mut cur));
        }
    })
}

/// Latest levels per source, plus how long each has been silent, ready to
/// draw. A source silent for ~2 s is greyed out, so a wrong or dead device
/// shows at a glance.
#[derive(Default)]
pub struct Levels {
    names: &'static [&'static str],
    values: Vec<f32>,
    silent_for: Vec<u32>,
}

impl Levels {
    pub fn configure(&mut self, cfg: &crate::config::Config) {
        self.names = if cfg.format.has_audio() { cfg.audio.sources() } else { &[] };
        self.reset();
    }

    pub fn reset(&mut self) {
        self.values = vec![f32::NEG_INFINITY; self.names.len() * 2];
        self.silent_for = vec![0; self.names.len()];
    }

    pub fn set(&mut self, levels: &[f32]) {
        for i in 0..self.names.len() {
            let l = levels.get(2 * i).copied().unwrap_or(f32::NEG_INFINITY);
            let r = levels.get(2 * i + 1).copied().unwrap_or(l);
            self.values[2 * i] = l;
            self.values[2 * i + 1] = r;
            self.silent_for[i] = if l.max(r) < -70.0 { self.silent_for[i].saturating_add(1) } else { 0 };
        }
    }

    /// None without audio.
    pub fn item(&self) -> Option<crate::draw::Item> {
        if self.names.is_empty() {
            return None;
        }
        Some(crate::draw::Item::Meters(
            self.names
                .iter()
                .enumerate()
                .map(|(i, n)| (*n, self.values[2 * i], self.values[2 * i + 1], self.silent_for[i] >= 20))
                .collect(),
        ))
    }
}

/// Meter-only ffmpeg used before recording starts (in the selector), so the
/// user can check the right source is live.
pub struct Preview {
    proc: Proc,
}

impl Preview {
    pub fn start(sources: &[String], opts: AudioOpts, f: impl Fn(Vec<f32>) + 'static) -> Option<Preview> {
        if sources.is_empty() {
            return None;
        }
        let mut a: Vec<OsString> =
            ["ffmpeg", "-hide_banner", "-loglevel", "info", "-nostats", "-nostdin"].iter().map(OsString::from).collect();
        for s in sources {
            for x in ["-f", "pulse", "-fragment_size", "4096", "-i", s] {
                a.push(x.into());
            }
        }
        a.push("-filter_complex".into());
        a.push(audio_graph(0, sources.len(), false, opts).into());
        for x in ["-map", "[meter]", "-f", "null", "/dev/null"] {
            a.push(x.into());
        }
        Proc::spawn(&a, false, Stderr::Lines(parser(f)), |_| {}).map_err(|e| eprintln!("peek2: meter: {e:#}")).ok().map(|proc| Preview { proc })
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.proc.kill();
    }
}
