mod audio;
mod config;
mod controller;
mod draw;
mod fit;
mod frame;
mod hw;
mod ipc;
mod meter;
mod postprocess;
mod proc;
mod recorder;
mod replay;
mod rt;
mod selector;
mod status;
mod tray;
mod tray_menu;
mod util;
mod x11;

use controller::{Controller, Phase};
use std::cell::RefCell;
use std::process::ExitCode;
use std::rc::Rc;

const HELP: &str = "\
peek2: record a screen region with audio, straight to the clipboard

usage: peek2 [command]

  -s, --select        select a region or window and record (default)
  -t, --toggle        start/stop recording (bind this to a key)
  -x, --stop          stop and save
  -c, --cancel        discard the recording
  -P, --pause         pause or resume
  -r, --replay        turn the replay buffer on or off
  -R, --save-replay   save the last seconds of the replay buffer
  -o, --open-folder   open the recordings folder
  -p, --preferences   open the settings file
  -b, --background    start resident (hotkey + tray icon), open nothing
  -q, --quit          quit the running instance
      --fit MB FILE   shrink a clip to at most MB megabytes (fit_strategy
                      from the settings decides what gives way)
      --status-follow print a polybar status line on every change
  -h, --help          this help

Settings: ~/.config/peek2/config.toml";

/// Commands, as the long option names without dashes.
const COMMANDS: [(&str, char); 11] = [
    ("select", 's'),
    ("toggle", 't'),
    ("stop", 'x'),
    ("cancel", 'c'),
    ("pause", 'P'),
    ("replay", 'r'),
    ("save-replay", 'R'),
    ("open-folder", 'o'),
    ("preferences", 'p'),
    ("background", 'b'),
    ("quit", 'q'),
];

fn command_of(arg: &str) -> Option<&'static str> {
    if let Some(long) = arg.strip_prefix("--") {
        return COMMANDS.iter().find(|(n, _)| *n == long).map(|(n, _)| *n);
    }
    let mut chars = arg.strip_prefix('-')?.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => COMMANDS.iter().find(|(_, s)| *s == c).map(|(n, _)| *n),
        _ => None,
    }
}

struct App {
    ctl: Rc<Controller>,
    frame: RefCell<Option<Rc<frame::RecordFrame>>>,
}

thread_local! {
    static APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
}

fn app() -> Rc<App> {
    APP.with(|a| a.borrow().clone()).expect("app not started")
}

impl App {
    fn open_selector(&self) {
        if self.ctl.is_busy() {
            return;
        }
        selector::open(&self.ctl, |res| {
            let Some((area, mon)) = res else { return };
            let app = app();
            let f = frame::RecordFrame::start(&app.ctl, area, mon, || {
                app_frame_closed();
            });
            *app.frame.borrow_mut() = Some(f);
        });
    }

    /// Hotkey / `--toggle`: stop a running take, record the current
    /// selection, or open the selector.
    fn toggle(&self) {
        match self.ctl.phase() {
            Phase::Countdown(_) | Phase::Recording(_) | Phase::Paused(_) => return self.ctl.stop(),
            Phase::Processing => return,
            Phase::Idle => {}
        }
        match selector::current() {
            Some(s) => s.record(),
            None => self.open_selector(),
        }
    }

    fn run(&self, cmd: &str) {
        let ctl = &self.ctl;
        match cmd {
            "select" => self.open_selector(),
            "toggle" => self.toggle(),
            "stop" => ctl.stop(),
            "cancel" => {
                ctl.cancel();
                if let Some(s) = selector::current() {
                    s.close();
                }
            }
            "pause" => ctl.toggle_pause(),
            "replay" => ctl.toggle_replay(),
            "save-replay" => ctl.save_replay(),
            "open-folder" => {
                let dir = ctl.config.borrow().save_dir();
                let _ = std::fs::create_dir_all(&dir);
                util::open(&dir);
            }
            "preferences" => {
                let path = config::Config::path();
                if !path.exists() {
                    ctl.config.borrow().save();
                }
                util::open(&path);
            }
            "background" => {}
            "quit" => {
                ctl.cancel();
                ctl.stop_replay();
                rt::quit();
            }
            "tray-menu" => {
                tray_menu::show(ctl, |id| {
                    let app = app();
                    match id.strip_prefix("open:") {
                        Some(path) => util::open(std::path::Path::new(path)),
                        None => app.run(id),
                    }
                });
            }
            other => eprintln!("peek2: unknown command {other}"),
        }
    }
}

fn app_frame_closed() {
    let f = app().frame.borrow_mut().take();
    drop(f);
}

fn setup_hotkeys(ctl: &Controller) {
    let cfg = ctl.config.borrow().clone();
    let x = x11::get();
    let grab = |accel: &str, what: &str, cmd: &'static str| {
        if accel.is_empty() {
            return;
        }
        if let Err(e) = x.grab_hotkey(accel, move || app().run(cmd)) {
            eprintln!("peek2: {what} hotkey disabled: {e:#}");
        }
    };
    grab(&cfg.hotkey, "record", "toggle");
    grab(&cfg.replay_hotkey, "save replay", "save-replay");
}

fn fit_command(args: &[String]) -> ExitCode {
    let (Some(mb), Some(file)) = (args.get(2).and_then(|s| s.trim_end_matches(['M', 'B', 'm', 'b']).parse::<u32>().ok()), args.get(3)) else {
        eprintln!("usage: peek2 --fit MB FILE");
        return ExitCode::FAILURE;
    };
    let cfg = config::Config::load();
    match fit::fit_file(std::path::Path::new(file), mb.max(1), cfg.fit_strategy, cfg.gif_quality) {
        Ok(dest) => {
            println!("{}", dest.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("peek2: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--status-follow") => status::follow(),
        Some("-h" | "--help") => {
            println!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Some("--version" | "-V") => {
            println!("peek2 {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some("--fit") => return fit_command(&args),
        Some("--window" | "-w" | "--review" | "-v") => {
            eprintln!("peek2: {} is gone in this version; see peek2 --help", args[1]);
            return ExitCode::FAILURE;
        }
        _ => {}
    }
    let cmd = match args.get(1) {
        None => "select",
        Some(a) => match command_of(a) {
            Some(c) => c,
            None => {
                eprintln!("peek2: unknown option {a}\n\n{HELP}");
                return ExitCode::FAILURE;
            }
        },
    };

    if ipc::forward(&[cmd.to_string()]) {
        return ExitCode::SUCCESS;
    }
    if matches!(cmd, "quit" | "stop" | "cancel" | "pause" | "save-replay") {
        eprintln!("peek2: not running");
        return ExitCode::FAILURE;
    }

    let rx = rt::init();
    let x = match x11::init() {
        Ok(x) => x,
        Err(e) => {
            eprintln!("peek2: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = ipc::listen() {
        // Lost a start-up race: hand over to the winner.
        if ipc::forward(&[cmd.to_string()]) {
            return ExitCode::SUCCESS;
        }
        eprintln!("peek2: {e:#}");
        return ExitCode::FAILURE;
    }

    let ctl = Controller::new();
    APP.with(|a| *a.borrow_mut() = Some(Rc::new(App { ctl: ctl.clone(), frame: RefCell::new(None) })));
    setup_hotkeys(&ctl);
    tray::start(|button, state| {
        let cmd = match button {
            1 => "toggle",
            2 if state == "replay" => "save-replay",
            2 => "open-folder",
            3 => "tray-menu",
            _ => return,
        };
        app().run(cmd);
    });
    hw::probe();
    ctl.status("idle", "");
    if ctl.config.borrow().replay_on_start {
        ctl.toggle_replay();
    }
    app().run(cmd);
    x.flush();

    let x2 = x.clone();
    rt::run(
        rx,
        move |ev| x2.dispatch(ev),
        |args, stream| match args.first().map(String::as_str) {
            Some("--status-follow") => status::add_follower(stream),
            Some(c) => app().run(c),
            None => app().run("select"),
        },
        || x.flush(),
    );

    ctl.cancel();
    ctl.stop_replay();
    ipc::cleanup();
    ExitCode::SUCCESS
}
