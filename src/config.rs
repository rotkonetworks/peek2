//! `~/.config/peek2/config.toml`. Flat `key = value` TOML, read and written
//! by hand (no serde): strings, integers and booleans are all it needs.
//! Unknown keys are ignored, so configs from older versions still load.

use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Format {
    Mp4,
    #[default]
    Webm,
    Gif,
    Apng,
}

impl Format {
    pub const ALL: [Format; 4] = [Format::Webm, Format::Mp4, Format::Gif, Format::Apng];

    pub fn label(self) -> &'static str {
        match self {
            Format::Mp4 => "MP4",
            Format::Webm => "WebM",
            Format::Gif => "GIF",
            Format::Apng => "APNG",
        }
    }

    pub fn ext(self) -> &'static str {
        match self {
            Format::Mp4 => "mp4",
            Format::Webm => "webm",
            Format::Gif => "gif",
            Format::Apng => "apng",
        }
    }

    fn parse(s: &str) -> Option<Format> {
        Format::ALL.into_iter().find(|f| f.ext() == s)
    }

    pub fn has_audio(self) -> bool {
        matches!(self, Format::Mp4 | Format::Webm)
    }

    /// Animations are recorded to a lossless intermediate and converted afterwards.
    pub fn is_animation(self) -> bool {
        matches!(self, Format::Gif | Format::Apng)
    }

    pub fn next(self) -> Format {
        let i = Format::ALL.iter().position(|f| *f == self).unwrap_or(0);
        Format::ALL[(i + 1) % Format::ALL.len()]
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum AudioMode {
    Off,
    #[default]
    Desktop,
    Mic,
    Both,
}

impl AudioMode {
    pub const ALL: [AudioMode; 4] = [AudioMode::Off, AudioMode::Desktop, AudioMode::Mic, AudioMode::Both];

    pub fn label(self) -> &'static str {
        match self {
            AudioMode::Off => "no audio",
            AudioMode::Desktop => "desktop",
            AudioMode::Mic => "mic",
            AudioMode::Both => "desktop+mic",
        }
    }

    fn key(self) -> &'static str {
        match self {
            AudioMode::Off => "off",
            AudioMode::Desktop => "desktop",
            AudioMode::Mic => "mic",
            AudioMode::Both => "both",
        }
    }

    fn parse(s: &str) -> Option<AudioMode> {
        AudioMode::ALL.into_iter().find(|m| m.key() == s)
    }

    /// Short name of each source in recording order; matches the channel
    /// layout of level reports (two channels per source).
    pub fn sources(self) -> &'static [&'static str] {
        match self {
            AudioMode::Off => &[],
            AudioMode::Desktop => &["spk"],
            AudioMode::Mic => &["mic"],
            AudioMode::Both => &["spk", "mic"],
        }
    }

    pub fn next(self) -> AudioMode {
        let i = AudioMode::ALL.iter().position(|m| *m == self).unwrap_or(0);
        AudioMode::ALL[(i + 1) % AudioMode::ALL.len()]
    }
}

/// What gives way when a clip has to fit a size limit.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum FitStrategy {
    /// Cap at 30 fps, shrink to half size, then drop frames, then quality.
    #[default]
    Auto,
    /// Keep framerate and quality, shrink the resolution.
    Scale,
    /// Drop frames first (down to 10 fps), then shrink.
    Framerate,
    /// Keep size and framerate, lower the bitrate only.
    Quality,
}

impl FitStrategy {
    pub const ALL: [FitStrategy; 4] = [FitStrategy::Auto, FitStrategy::Scale, FitStrategy::Framerate, FitStrategy::Quality];

    pub fn key(self) -> &'static str {
        match self {
            FitStrategy::Auto => "auto",
            FitStrategy::Scale => "scale",
            FitStrategy::Framerate => "framerate",
            FitStrategy::Quality => "quality",
        }
    }

    pub fn parse(s: &str) -> Option<FitStrategy> {
        FitStrategy::ALL.into_iter().find(|m| m.key() == s)
    }

    pub fn next(self) -> FitStrategy {
        let i = FitStrategy::ALL.iter().position(|m| *m == self).unwrap_or(0);
        FitStrategy::ALL[(i + 1) % FitStrategy::ALL.len()]
    }
}

/// Size limits offered in the toolbar and tray (MB; 0 = off).
pub const FIT_PRESETS: [u32; 4] = [0, 10, 25, 50];

#[derive(Clone, Debug)]
pub struct Config {
    /// Shrink saved clips to at most this many MB (10^6 bytes); 0 = off.
    pub fit_size_mb: u32,
    pub fit_strategy: FitStrategy,
    pub format: Format,
    pub framerate: u32,
    pub downsample: u32,
    pub capture_mouse: bool,
    pub audio: AudioMode,
    pub desktop_source: String,
    pub mic_source: String,
    pub video_quality: u32,
    pub gif_quality: u8,
    pub start_delay: u32,
    pub save_folder: String,
    pub filename_format: String,
    pub ask_save_location: bool,
    pub copy_to_clipboard: bool,
    pub notify: bool,
    pub hardware_encoding: bool,
    pub replay_seconds: u32,
    pub mic_noise_suppression: bool,
    pub separate_audio_tracks: bool,
    pub replay_on_start: bool,
    pub replay_hotkey: String,
    pub hotkey: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            fit_size_mb: 0,
            fit_strategy: FitStrategy::Auto,
            format: Format::Webm,
            framerate: 30,
            downsample: 1,
            capture_mouse: true,
            audio: AudioMode::Desktop,
            desktop_source: String::new(),
            mic_source: String::new(),
            video_quality: 23,
            gif_quality: 90,
            start_delay: 0,
            save_folder: String::new(),
            filename_format: "Peek %Y-%m-%d %H-%M-%S".into(),
            ask_save_location: false,
            copy_to_clipboard: true,
            notify: true,
            hardware_encoding: true,
            replay_seconds: 60,
            mic_noise_suppression: true,
            separate_audio_tracks: false,
            replay_on_start: false,
            replay_hotkey: "<Ctrl><Alt>S".into(),
            hotkey: "<Ctrl><Alt>R".into(),
        }
    }
}

enum Value {
    Str(String),
    Int(i64),
    Bool(bool),
}

fn parse_string(s: &str) -> Option<String> {
    if let Some(inner) = s.strip_prefix('\'') {
        return inner.strip_suffix('\'').map(str::to_string);
    }
    let inner = s.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            'u' => {
                let hex: String = chars.by_ref().take(4).collect();
                out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
            }
            _ => return None,
        }
    }
    Some(out)
}

fn strip_comment(v: &str) -> &str {
    // A '#' outside quotes starts a comment.
    let mut quote = None;
    for (i, c) in v.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '#') => return v[..i].trim_end(),
            _ => {}
        }
    }
    v
}

fn parse_value(v: &str) -> Option<Value> {
    let v = strip_comment(v.trim());
    match v {
        "true" => Some(Value::Bool(true)),
        "false" => Some(Value::Bool(false)),
        _ if v.starts_with('"') || v.starts_with('\'') => parse_string(v).map(Value::Str),
        _ => v.replace('_', "").parse().ok().map(Value::Int),
    }
}

fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

impl Config {
    pub fn path() -> PathBuf {
        crate::util::config_dir().join("peek2").join("config.toml")
    }

    pub fn mtime() -> Option<SystemTime> {
        std::fs::metadata(Self::path()).and_then(|m| m.modified()).ok()
    }

    pub fn load() -> Config {
        match std::fs::read_to_string(Self::path()) {
            Ok(s) => Self::parse(&s),
            Err(_) => Config::default(),
        }
    }

    pub fn parse(text: &str) -> Config {
        let mut c = Config::default();
        let mut section = false;
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with('[') {
                // Tables aren't used; ignore their contents.
                section = true;
                continue;
            }
            if section {
                continue;
            }
            let Some((key, val)) = line.split_once('=') else {
                eprintln!("peek2: config line {}: expected key = value", n + 1);
                continue;
            };
            let key = key.trim().trim_matches('"');
            let Some(val) = parse_value(val) else {
                // e.g. arrays from older versions (window_size); not used now.
                continue;
            };
            if !c.set(key, val) {
                eprintln!("peek2: config line {}: bad value for {key}", n + 1);
            }
        }
        c
    }

    fn set(&mut self, key: &str, v: Value) -> bool {
        let uint = |v: &Value| match v {
            Value::Int(i) if *i >= 0 && *i <= u32::MAX as i64 => Some(*i as u32),
            _ => None,
        };
        let s = |v: Value| match v {
            Value::Str(s) => Some(s),
            _ => None,
        };
        let b = |v: &Value| match v {
            Value::Bool(b) => Some(*b),
            _ => None,
        };
        macro_rules! put {
            ($field:expr, $val:expr) => {
                match $val {
                    Some(x) => {
                        $field = x;
                        true
                    }
                    None => false,
                }
            };
        }
        match key {
            "format" => put!(self.format, s(v).and_then(|s| Format::parse(&s))),
            "framerate" => put!(self.framerate, uint(&v)),
            "downsample" => put!(self.downsample, uint(&v)),
            "capture_mouse" => put!(self.capture_mouse, b(&v)),
            "audio" => put!(self.audio, s(v).and_then(|s| AudioMode::parse(&s))),
            "desktop_source" => put!(self.desktop_source, s(v)),
            "mic_source" => put!(self.mic_source, s(v)),
            "video_quality" => put!(self.video_quality, uint(&v)),
            "gif_quality" => put!(self.gif_quality, uint(&v).filter(|q| *q <= 100).map(|q| q as u8)),
            "start_delay" => put!(self.start_delay, uint(&v)),
            "save_folder" => put!(self.save_folder, s(v)),
            "filename_format" => put!(self.filename_format, s(v)),
            "ask_save_location" => put!(self.ask_save_location, b(&v)),
            "copy_to_clipboard" => put!(self.copy_to_clipboard, b(&v)),
            "notify" => put!(self.notify, b(&v)),
            "hardware_encoding" => put!(self.hardware_encoding, b(&v)),
            "replay_seconds" => put!(self.replay_seconds, uint(&v)),
            "mic_noise_suppression" => put!(self.mic_noise_suppression, b(&v)),
            "separate_audio_tracks" => put!(self.separate_audio_tracks, b(&v)),
            "replay_on_start" => put!(self.replay_on_start, b(&v)),
            "replay_hotkey" => put!(self.replay_hotkey, s(v)),
            "hotkey" => put!(self.hotkey, s(v)),
            "fit_size_mb" => put!(self.fit_size_mb, uint(&v)),
            "fit_strategy" => put!(self.fit_strategy, s(v).and_then(|s| FitStrategy::parse(&s))),
            // Gone with the GTK UI; accepted silently.
            "review_after_save" | "window_size" => true,
            _ => true,
        }
    }

    pub fn to_toml(&self) -> String {
        let mut o = String::from("# Peek2 settings. Edited by hand or from the tray menu; changes apply\n# to the next recording (hotkeys after a restart: peek2 --quit).\n\n");
        let mut line = |doc: &str, key: &str, val: String| {
            if !doc.is_empty() {
                for d in doc.lines() {
                    o.push_str("# ");
                    o.push_str(d);
                    o.push('\n');
                }
            }
            o.push_str(&format!("{key} = {val}\n\n"));
        };
        line("webm, mp4, gif or apng", "format", quote(self.format.ext()));
        line("", "framerate", self.framerate.to_string());
        line(
            "Shrink saved clips to fit this many MB (e.g. 10 for Discord); 0 = off.",
            "fit_size_mb",
            self.fit_size_mb.to_string(),
        );
        line(
            "What gives way to fit: auto (30 fps cap, then down to half size, then\nfewer frames, then quality), scale (resolution only), framerate (fewer\nframes first, then resolution) or quality (bitrate only).",
            "fit_strategy",
            quote(self.fit_strategy.key()),
        );
        line("Divide the output resolution by this factor.", "downsample", self.downsample.to_string());
        line("", "capture_mouse", self.capture_mouse.to_string());
        line("off, desktop, mic or both", "audio", quote(self.audio.key()));
        line(
            "Pulse/PipeWire source names (pactl list short sources). Empty: the\nmonitor of the output that is playing / the default input.",
            "desktop_source",
            quote(&self.desktop_source),
        );
        line("", "mic_source", quote(&self.mic_source));
        line("x264/VP9 quality, lower is better.", "video_quality", self.video_quality.to_string());
        line("gifski quality, 1-100.", "gif_quality", self.gif_quality.to_string());
        line("Seconds of countdown before recording.", "start_delay", self.start_delay.to_string());
        line("Empty: ~/Videos/Peek2", "save_folder", quote(&self.save_folder));
        line("strftime pattern, as date(1) understands it.", "filename_format", quote(&self.filename_format));
        line("Ask for a file name (needs zenity or kdialog).", "ask_save_location", self.ask_save_location.to_string());
        line("", "copy_to_clipboard", self.copy_to_clipboard.to_string());
        line("Desktop notification after saving (notify-send).", "notify", self.notify.to_string());
        line("Encode MP4 on the GPU (VAAPI) when it works.", "hardware_encoding", self.hardware_encoding.to_string());
        line("Replay buffer length; saving keeps the last this many seconds.", "replay_seconds", self.replay_seconds.to_string());
        line("", "replay_on_start", self.replay_on_start.to_string());
        line("Adaptive noise reduction on the microphone.", "mic_noise_suppression", self.mic_noise_suppression.to_string());
        line("With desktop + mic, also store each as its own audio track.", "separate_audio_tracks", self.separate_audio_tracks.to_string());
        line("Global hotkeys, e.g. \"<Ctrl><Alt>R\", \"<Super>F9\". Empty disables.", "hotkey", quote(&self.hotkey));
        line("", "replay_hotkey", quote(&self.replay_hotkey));
        o
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("toml.tmp");
        if let Err(e) = std::fs::write(&tmp, self.to_toml()).and_then(|_| std::fs::rename(&tmp, &path)) {
            eprintln!("peek2: failed to save config: {e}");
        }
    }

    pub fn save_dir(&self) -> PathBuf {
        if !self.save_folder.is_empty() {
            return PathBuf::from(&self.save_folder);
        }
        crate::util::videos_dir().join("Peek2")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut c = Config::default();
        c.format = Format::Gif;
        c.audio = AudioMode::Both;
        c.save_folder = "/tmp/a \"b\"\\c".into();
        c.framerate = 60;
        let d = Config::parse(&c.to_toml());
        assert_eq!(d.format, Format::Gif);
        assert_eq!(d.audio, AudioMode::Both);
        assert_eq!(d.save_folder, c.save_folder);
        assert_eq!(d.framerate, 60);
    }

    #[test]
    fn old_config_loads() {
        let old = "format = \"mp4\"\nwindow_size = [640, 400]\ngif_quality = 90\nhotkey = \"<Super>R\" # comment\n\n[extra]\nformat = \"gif\"\n";
        let c = Config::parse(old);
        assert_eq!(c.format, Format::Mp4);
        assert_eq!(c.hotkey, "<Super>R");
    }
}
