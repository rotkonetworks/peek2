use gtk::glib;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
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

    pub fn has_audio(self) -> bool {
        matches!(self, Format::Mp4 | Format::Webm)
    }

    /// Animations are recorded to a lossless intermediate and converted afterwards.
    pub fn is_animation(self) -> bool {
        matches!(self, Format::Gif | Format::Apng)
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
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
            AudioMode::Off => "No audio",
            AudioMode::Desktop => "Desktop audio",
            AudioMode::Mic => "Microphone",
            AudioMode::Both => "Desktop + mic",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            AudioMode::Off => "audio-volume-muted-symbolic",
            AudioMode::Desktop => "audio-speakers-symbolic",
            AudioMode::Mic => "audio-input-microphone-symbolic",
            AudioMode::Both => "audio-volume-high-symbolic",
        }
    }

    /// (icon, name) of each source in recording order; matches the channel
    /// layout of level reports (two channels per source).
    pub fn sources(self) -> &'static [(&'static str, &'static str)] {
        const DESKTOP: (&str, &str) = ("audio-speakers-symbolic", "Desktop audio");
        const MIC: (&str, &str) = ("audio-input-microphone-symbolic", "Microphone");
        match self {
            AudioMode::Off => &[],
            AudioMode::Desktop => &[DESKTOP],
            AudioMode::Mic => &[MIC],
            AudioMode::Both => &[DESKTOP, MIC],
        }
    }

    pub fn next(self) -> AudioMode {
        let i = AudioMode::ALL.iter().position(|m| *m == self).unwrap_or(0);
        AudioMode::ALL[(i + 1) % AudioMode::ALL.len()]
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub format: Format,
    pub framerate: u32,
    /// Divide output resolution by this factor.
    pub downsample: u32,
    pub capture_mouse: bool,
    pub audio: AudioMode,
    /// Pulse/PipeWire source for desktop audio; empty = monitor of the output
    /// that is currently playing.
    pub desktop_source: String,
    /// Pulse/PipeWire source for the microphone; empty = default source.
    pub mic_source: String,
    /// x264 CRF / VP9 CRF-ish quality knob, lower is better.
    pub video_quality: u32,
    pub gif_quality: u8,
    pub start_delay: u32,
    /// Empty = ~/Videos/Peek2
    pub save_folder: String,
    /// strftime-style pattern, expanded with GLib's DateTime::format.
    pub filename_format: String,
    pub ask_save_location: bool,
    pub copy_to_clipboard: bool,
    pub notify: bool,
    /// Open the review window (playback, trim, discard) after saving a video.
    pub review_after_save: bool,
    /// Encode MP4 on the GPU (VAAPI) when it works.
    pub hardware_encoding: bool,
    /// Replay buffer length; saving keeps the last this many seconds.
    pub replay_seconds: u32,
    /// Adaptive noise reduction on the microphone.
    pub mic_noise_suppression: bool,
    /// With desktop + mic, also store each as its own audio track.
    pub separate_audio_tracks: bool,
    /// Start the replay buffer with peek2.
    pub replay_on_start: bool,
    /// Global hotkey that saves the replay buffer.
    pub replay_hotkey: String,
    /// GTK accelerator syntax, grabbed globally on X11. Empty disables.
    pub hotkey: String,
    pub window_size: (i32, i32),
}

impl Default for Config {
    fn default() -> Self {
        Config {
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
            review_after_save: true,
            hardware_encoding: true,
            replay_seconds: 60,
            mic_noise_suppression: true,
            separate_audio_tracks: false,
            replay_on_start: false,
            replay_hotkey: "<Ctrl><Alt>S".into(),
            hotkey: "<Ctrl><Alt>R".into(),
            window_size: (640, 400),
        }
    }
}

impl Config {
    fn path() -> PathBuf {
        glib::user_config_dir().join("peek2").join("config.toml")
    }

    pub fn load() -> Config {
        match std::fs::read_to_string(Self::path()) {
            Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
                eprintln!("peek2: ignoring invalid config: {e}");
                Config::default()
            }),
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match toml::to_string_pretty(self) {
            Ok(s) => {
                if let Err(e) = std::fs::write(&path, s) {
                    eprintln!("peek2: failed to save config: {e}");
                }
            }
            Err(e) => eprintln!("peek2: failed to serialize config: {e}"),
        }
    }

    pub fn save_dir(&self) -> PathBuf {
        if !self.save_folder.is_empty() {
            return PathBuf::from(&self.save_folder);
        }
        glib::user_special_dir(glib::UserDirectory::Videos)
            .unwrap_or_else(glib::home_dir)
            .join("Peek2")
    }
}
