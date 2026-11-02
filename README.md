# Peek2

A Rust rewrite of [Peek](https://github.com/phw/peek) built for quick clips:
pick an area, record WebM/MP4 **with audio** (or GIF/APNG), and the file lands
in your clipboard ready to paste into chat.

## Usage

Launch `peek2` (or press **Ctrl+Alt+R**). The monitor under the pointer darkens:

- **drag** to select a region, or **click** a window to record exactly it
- drag the handles to adjust; arrows nudge (Shift ×10, Ctrl resizes)
- **Space**: whole screen · **Enter** / double-click: record · **Esc**: cancel
- pick format, audio source and mouse capture in the toolbar under the selection

While recording, a red border sits *outside* the captured area with a small
stop bar (pause, stop, discard). A full-screen recording has no room outside,
so the bar stays hidden; use the hotkey or the tray. Press **Ctrl+Alt+R**
again (or Stop) to finish. The clip is saved to `~/Videos/Peek2` and copied to
the clipboard as a file (paste it into Discord, Slack, a browser or a file
manager), then opens in the review window.

Pausing ends the current part and resuming starts a new one; when the take
ends, the parts are joined without re-encoding.

## Review window

After a WebM/MP4 is saved (it's already on disk and in the clipboard), it opens
for checking:

- plays back with sound, looping inside the selection
- drag the gold handles on the filmstrip to trim. The start snaps to keyframes
  (recordings have one per second), because a trim is a lossless stream copy
  and so takes no time
- **Fit size** re-encodes the selection to land under 10 MB (Discord), 25 MB
  or 50 MB. It scales the video down instead of going blocky when the bitrate
  gets thin, and checks the result
- **Discard** moves the file to the trash and clears the clipboard

Keys: space play · `i` / `o` set in and out at the playhead · ←/→ step 1 s ·
`f` fit size · Enter save · Shift+Delete discard · Esc close. Turn it off in
Preferences, or open any clip with `peek2 --review FILE`.

## Replay buffer

Like OBS: turn it on from the tray menu (or `peek2 --replay`, or *Start with
peek2* in Preferences), and the monitor under the pointer is recorded
continuously into 2-second chunks in RAM (`$XDG_RUNTIME_DIR`). **Ctrl+Alt+S**
(or middle-click the tray icon, or `peek2 --save-replay`) saves the last 60 s
(configurable) as an MP4, joined without re-encoding, into the clipboard and
the review window. The tray icon shows a small red dot while it runs.

`peek2 --window` opens the classic Peek see-through window instead.

peek2 stays resident after the first launch, which keeps the hotkey working.
On X11, clipboard contents also disappear when the app that owns them exits.
`peek2 --quit` stops it.

| Command | |
|---|---|
| `peek2` / `--select` | region selector |
| `--window` | Peek-style window |
| `--toggle` | start/stop (for binding in sxhkd etc.) |
| `--background` | start resident with the tray icon and hotkey (autostart, e.g. in bspwmrc) |
| `--status-follow` | optional text status line for polybar (`tail = true`) |
| `--pause` | pause / resume |
| `--replay`, `--save-replay` | replay buffer on/off, save it |
| `--review FILE` | open a clip in the review window |
| `--stop`, `--cancel`, `--open-folder`, `--preferences`, `--quit` | |

## Tray icon

The running instance docks a record icon in any XEmbed system tray (polybar's
`internal/tray`, stalonetray, trayer…), next to your other tray icons. Start it
with `peek2 --background` from your autostart.

It's a cream ring when idle. The ring fills with a dot that's gold during the
countdown and while saving, red while recording, and green just after saving.
Paused is an empty red ring; a running replay buffer adds a small red dot.

| Click | Action |
|---|---|
| left | select an area and record, or stop |
| middle | save the replay buffer when it runs, else open the recordings folder |
| right | menu: record / pause / stop, replay buffer, format, audio, recent clips, folder, preferences |

### Optional text module

For a timer in the bar instead, `--status-follow` prints a polybar line on every
state change. It's pushed through inotify, not polled:

```ini
[module/peek2]
type = custom/script
exec = ~/.local/bin/peek2 --status-follow
tail = true
click-left = ~/.local/bin/peek2 --toggle &
click-right = ~/.local/bin/peek2 --cancel &
click-middle = ~/.local/bin/peek2 --open-folder &
```

## Audio meters

While you select an area, the toolbar shows live peak meters: a speaker
and/or mic icon, each with stacked **L/R** bars. The same meters run in the
stop bar during recording. They come straight from the recording ffmpeg, so
they show what actually goes into the file. A source that stays silent for
~2 s fades out, which shows a wrong or dead device right away. Hover a meter
for exact dBFS.

## Formats

| | Video | Audio |
|---|---|---|
| WebM (default) | VP9 | Opus |
| MP4 | H.264, on the GPU via VAAPI when available | AAC |
| GIF | gifski (built in) | — |
| APNG | ffmpeg | — |

Audio modes are desktop audio (the monitor of whichever output is actually
playing, not just the default one), microphone,
or both mixed. Specific devices can be chosen in Preferences. The microphone
gets adaptive noise suppression (ffmpeg `afftdn`, on by default). With both
sources, *Separate desktop and mic tracks* also stores each source as its own
named track after the mix, for editing later.

**Hardware encoding:** at startup peek2 test-encodes a few frames with
`h264_vaapi` and uses the GPU for MP4 and the replay buffer if that works
(about 0.3 CPU-seconds per 10 s of 1440p, against about 22 with x264). VP9 stays
on the CPU, because AMD GPUs have no VP9 encoder. This uses
PulseAudio/PipeWire through `pactl` and ffmpeg's `pulse` input.

## Requirements

- X11 session, or XWayland (peek2 always uses the X11 backend)
- `ffmpeg` with x11grab, pulse, libvpx, libopus, libx264
- `pactl`
- a compositor (e.g. picom) for the translucent overlay and Peek window.
  Without one, the selector falls back to a frozen screenshot.

**Wayland:** Wayland gives apps no global coordinates or screen access, so
under XWayland only X11 windows show up in recordings. Native Wayland capture
would need the xdg-desktop-portal ScreenCast + PipeWire path that OBS uses,
which isn't implemented yet. The recorder sits behind its own module so it can
be added later.

Tiling WMs: the selector and recording frame are override-redirect windows, so
the WM leaves them alone. The Peek window marks itself as a dialog, so
bspwm/i3 float it.

## Build

```sh
cargo build --release
install -Dm755 target/release/peek2 ~/.local/bin/peek2
install -Dm644 data/net.rotko.Peek2.desktop ~/.local/share/applications/net.rotko.Peek2.desktop
```

Config: `~/.config/peek2/config.toml`. Set `PEEK2_DEBUG=1` to print the ffmpeg
command line.

License: GPL-3.0-or-later (like Peek).
