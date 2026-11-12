# Peek2

A Rust rewrite of [Peek](https://github.com/phw/peek) built for quick clips:
pick an area, record WebM/MP4 **with audio** (or GIF/APNG), and the file lands
in your clipboard ready to paste into chat.

No GTK, no GLib: peek2 speaks X11 directly and links only against libc. It
needs `ffmpeg` and `pactl` at runtime, nothing else.

## Usage

Launch `peek2` (or press **Ctrl+Alt+R**). The monitor under the pointer darkens:

- **drag** to select a region, or **click** a window to record exactly it
- drag the handles to adjust; arrows nudge (Shift ×10, Ctrl resizes)
- **Space**: whole screen · **Enter** / double-click: record · **Esc**: cancel
- with a region chosen, **Space** records *and* presses Space in the app under
  it, so one key starts both the recording and the (paused) video
- **Ctrl+C**: copy the selection to the clipboard as a PNG · **Ctrl+S**: save it
- the toolbar under the selection cycles format, audio source, mouse capture
  and the size limit (right-click the size limit to change what gives way)

While recording, a red border sits *outside* the captured area with a small
stop bar (pause, stop, discard). A full-screen recording has no room outside,
so the bar stays hidden; use the hotkey or the tray. Press **Ctrl+Alt+R**
again (or Stop) to finish. The clip is saved to `~/Videos/Peek2` and copied to
the clipboard as a file (paste it into Discord, Slack, a browser or a file
manager). A notification offers to open it or show it in its folder.

**Drawing on a recording**: press **draw** on the stop bar (or **Ctrl+Alt+D**,
or `peek2 --draw`) and draw on the screen while it records. Pen, arrow, box
and marker, C for colour, the wheel for size, Ctrl+Z / Ctrl+Shift+Z, Delete
clears, Esc or a click outside puts the pen down (the marks stay until you
clear them). The marks are a window shaped to their pixels, so they are in
the clip and need no compositor.

Pausing ends the current part and resuming starts a new one; when the take
ends, the parts are joined without re-encoding.

## Screenshots

**Ctrl+Alt+P** (or `peek2 --screenshot`, or the tray menu) opens the same
selector for a still: **Enter** saves a PNG to `~/Pictures/Peek2` and copies
it, **Ctrl+C** only copies it. The picture is the frame frozen when the
selector opened, so menus and tooltips stay in it. Ctrl+C and Ctrl+S (or the
copy / save png buttons) work in the recording selector too.

### Drawing on screenshots

A second toolbar under the selection has **pen** (P), **arrow** (A), **box**
(R), **marker** (M, a highlighter), numbered **steps** (N) and **blur** (B, pixelates, for hiding
secrets). C or the colour chip cycles colours, the scroll wheel changes the
thickness, **Ctrl+Z** undoes and **Ctrl+Shift+Z** / **Ctrl+Y** redoes, Esc
puts the tool down. Once you've drawn
something the selection is a screenshot: Enter saves it with the marks, and
the record button goes away until you undo them all.

## Fit to size

Set a size limit (toolbar, tray menu, or `fit_size_mb` in the settings) and
clips that come out bigger are shrunk to fit: 10 MB for Discord, 25, 50, or
any number. `fit_strategy` decides what gives way:

| strategy | what happens |
|---|---|
| `auto` (default) | cap at 30 fps, shrink down to half size, then drop frames (to 15 fps), then lower quality |
| `scale` | keep framerate and quality, shrink the resolution |
| `framerate` | drop frames first (down to 10 fps), then shrink |
| `quality` | keep resolution and framerate, lower the bitrate only |

Videos get a bitrate computed from the limit and the clip length, with the
resolution and framerate picked so each pixel keeps enough bits to stay clean;
the result is measured and re-encoded smaller if it's still over. GIF and APNG
sizes can't be predicted, so they're encoded, measured and adjusted until they
fit.

Existing clips too: `peek2 --fit 10 clip.mp4` writes `clip (10 MB).mp4`.

## Replay buffer

Like OBS: turn it on from the tray menu (or `peek2 --replay`, or
`replay_on_start = true`), and the monitor under the pointer is recorded
continuously into 2-second chunks in RAM (`$XDG_RUNTIME_DIR`). **Ctrl+Alt+S**
(or middle-click the tray icon, or `peek2 --save-replay`) saves the last 60 s
(configurable) as an MP4, joined without re-encoding (and fitted to the size
limit, if set), into the clipboard. The tray icon shows a small red dot while
it runs.

peek2 stays resident after the first launch, which keeps the hotkey working.
On X11, clipboard contents also disappear when the app that owns them exits.
`peek2 --quit` stops it.

| Command | |
|---|---|
| `peek2` / `--select` | region selector |
| `--screenshot` | selector for a PNG screenshot |
| `--toggle` | start/stop (for binding in sxhkd etc.) |
| `--background` | start resident with the tray icon and hotkey (autostart, e.g. in bspwmrc) |
| `--pause` | pause / resume |
| `--replay`, `--save-replay` | replay buffer on/off, save it |
| `--fit MB FILE` | shrink an existing clip |
| `--status-follow` | optional text status line for polybar (`tail = true`) |
| `--preferences` | open the settings file |
| `--stop`, `--cancel`, `--open-folder`, `--quit` | |

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
| right | menu: record / pause / stop, replay buffer, format, audio, size limit, recent clips, folder, settings |

### Optional text module

For a timer in the bar instead, `--status-follow` prints a polybar line on every
state change. It's pushed over the instance's socket, not polled:

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

While you select an area, the toolbar shows live peak meters per source
(`spk`, `mic`), each with stacked **L/R** bars. The same meters run in the stop
bar during recording. They come straight from the recording ffmpeg, so they
show what actually goes into the file. A source that stays silent for ~2 s
greys out, which shows a wrong or dead device right away.

## Formats

| | Video | Audio |
|---|---|---|
| WebM (default) | VP9 | Opus |
| MP4 | H.264, on the GPU via VAAPI when available | AAC |
| GIF | ffmpeg, two-pass palette | — |
| APNG | ffmpeg | — |

Audio modes are desktop audio (the monitor of whichever output is actually
playing, not just the default one), microphone, or both mixed. Specific
devices can be set in the settings file (`pactl list short sources`). The
microphone gets adaptive noise suppression (ffmpeg `afftdn`, on by default).
With both sources, `separate_audio_tracks` also stores each source as its own
named track after the mix, for editing later.

**Hardware encoding:** at startup peek2 test-encodes a few frames with
`h264_vaapi` and uses the GPU for MP4 and the replay buffer if that works
(about 0.3 CPU-seconds per 10 s of 1440p, against about 22 with x264). VP9 stays
on the CPU, because AMD GPUs have no VP9 encoder.

## Settings

`~/.config/peek2/config.toml`, documented inline; `peek2 --preferences` opens
it. Changes apply to the next recording; hotkeys after a restart. The toolbar
and tray menu write the common ones for you.

## Requirements

- X11 session, or XWayland
- `ffmpeg` (with x11grab, pulse, libvpx, libopus, libx264) and `ffprobe`
- `pactl` (PulseAudio or PipeWire)
- optional: `notify-send` for notifications, `xdg-open` to open clips,
  `zenity` or `kdialog` for `ask_save_location`
- optional: a compositor (e.g. picom) for the translucent record bar. The
  selector always shows a dimmed frozen screenshot, which works under any
  compositor setup; `PEEK2_LIVE_SELECT=1` makes it see-through instead
  (this breaks with picom's `unredir-if-possible`).

Text is drawn with the X server's built-in bitmap fonts (misc-fixed), so
there are no font dependencies. On HiDPI screens the UI scales with `Xft.dpi`
(or `PEEK2_SCALE=2`); `PEEK2_FONT` picks another core font.

**Wayland:** Wayland gives apps no global coordinates or screen access, so
under XWayland only X11 windows show up in recordings. Native Wayland capture
would need the xdg-desktop-portal ScreenCast + PipeWire path that OBS uses,
which isn't implemented yet.

Tiling WMs: the selector, stop bar and menu are override-redirect windows, so
the WM leaves them alone.

## Build

```sh
cargo build --release
install -Dm755 target/release/peek2 ~/.local/bin/peek2
install -Dm644 data/net.rotko.Peek2.desktop ~/.local/share/applications/net.rotko.Peek2.desktop
```

Set `PEEK2_DEBUG=1` to print ffmpeg command lines and size-fitting decisions.

License: MIT.
