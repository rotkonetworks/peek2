//! Audio level metering. Each pulse source is normalised to 48 kHz stereo,
//! split off and merged into one multichannel stream (source 0 → channels
//! 1-2, source 1 → 3-4, …) that `astats` measures 10× per second. ffmpeg
//! logs the peaks via `ametadata` on stderr (unbuffered, unlike its avio
//! outputs), so the meter shows exactly what is being recorded, live.

use gtk::{gio, glib, prelude::*};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::rc::Rc;

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

pub type Sink = Rc<RefCell<Option<Box<dyn Fn(Vec<f32>)>>>>;

/// Drain ffmpeg's stderr: meter lines go to `sink` as per-channel peak dBFS
/// (-inf for silence) once per window; anything else is kept in `other`
/// (bounded) for error reporting. Must run for the process' whole life so
/// the pipe never fills.
pub fn read_stderr(stream: gio::InputStream, sink: Sink, other: Rc<RefCell<VecDeque<String>>>) {
    let data = gio::DataInputStream::new(&stream);
    glib::spawn_future_local(async move {
        let mut cur: Vec<f32> = Vec::new();
        let flush = |cur: &mut Vec<f32>| {
            if !cur.is_empty() {
                let levels = std::mem::take(cur);
                if let Some(f) = sink.borrow().as_ref() {
                    f(levels);
                }
            }
        };
        while let Ok(Some(line)) = data.read_line_utf8_future(glib::Priority::DEFAULT).await {
            if let Some(i) = line.find("lavfi.astats.") {
                let rest = &line[i + "lavfi.astats.".len()..];
                let Some((ch, val)) = rest.split_once(".Peak_level=") else { continue };
                let Ok(ch) = ch.parse::<usize>() else { continue };
                let v = val.trim().parse::<f32>().unwrap_or(f32::NEG_INFINITY);
                if cur.len() < ch {
                    cur.resize(ch, f32::NEG_INFINITY);
                }
                cur[ch - 1] = v;
            } else if line.contains("] frame:") {
                flush(&mut cur);
            } else if !line.contains("Parsed_astats") {
                let mut o = other.borrow_mut();
                o.push_back(line.to_string());
                if o.len() > 12 {
                    o.pop_front();
                }
            }
        }
        flush(&mut cur);
    });
}

/// Meter-only ffmpeg used before recording starts (e.g. in the selector),
/// so the user can check the right source is live.
pub struct Preview {
    proc: gio::Subprocess,
}

impl Preview {
    pub fn start(sources: &[String], opts: AudioOpts, f: impl Fn(Vec<f32>) + 'static) -> Option<Preview> {
        if sources.is_empty() {
            return None;
        }
        let mut a: Vec<OsString> = ["ffmpeg", "-hide_banner", "-loglevel", "info", "-nostats", "-nostdin"]
            .iter()
            .map(OsString::from)
            .collect();
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
        let argv: Vec<&std::ffi::OsStr> = a.iter().map(|s| s.as_os_str()).collect();
        let proc = gio::Subprocess::newv(
            &argv,
            gio::SubprocessFlags::STDOUT_SILENCE | gio::SubprocessFlags::STDERR_PIPE,
        )
        .map_err(|e| eprintln!("peek2: meter: {e}"))
        .ok()?;
        let sink: Sink = Rc::new(RefCell::new(Some(Box::new(f))));
        read_stderr(proc.stderr_pipe()?, sink, Rc::default());
        Some(Preview { proc })
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.proc.force_exit();
    }
}
