mod audio;
mod config;
mod meter;
mod controller;
mod postprocess;
mod recorder;
mod hw;
mod replay;
mod status;
mod tray;
mod ui;
mod x11;

use controller::{Controller, Phase};
use gtk::{gio, glib, prelude::*};
use std::cell::RefCell;
use std::rc::Rc;

const APP_ID: &str = "net.rotko.Peek2";

pub struct App {
    pub ctl: Rc<Controller>,
    /// Stay resident: keeps the global hotkey alive and, on X11, the
    /// clipboard contents (they die with the owning process).
    _resident: gio::ApplicationHoldGuard,
    pub peek: RefCell<Option<Rc<ui::peek_window::PeekWindow>>>,
    pub selector: RefCell<Option<Rc<ui::selector::Selector>>>,
    pub frame: RefCell<Option<Rc<ui::frame::RecordFrame>>>,
}

thread_local! {
    static APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
}

pub fn app() -> Rc<App> {
    APP.with(|a| a.borrow().clone()).expect("app not started")
}

impl App {
    pub fn show_peek_window(&self) {
        let existing = self.peek.borrow().clone();
        let w = existing.unwrap_or_else(|| {
            let w = ui::peek_window::PeekWindow::new(&self.ctl);
            *self.peek.borrow_mut() = Some(w.clone());
            w
        });
        w.present();
    }

    pub fn open_selector(&self) {
        if self.ctl.is_busy() || self.selector.borrow().is_some() {
            return;
        }
        let s = ui::selector::Selector::open(&self.ctl);
        *self.selector.borrow_mut() = Some(s);
    }

    /// Hotkey / `--toggle`: stop a running take, otherwise start one from
    /// whichever UI is in front.
    pub fn toggle(&self) {
        match self.ctl.phase() {
            Phase::Countdown(_) | Phase::Recording(_) | Phase::Paused(_) => return self.ctl.stop(),
            Phase::Processing => return,
            Phase::Idle => {}
        }
        if let Some(s) = self.selector.borrow().clone() {
            return s.record();
        }
        let peek = self.peek.borrow().clone();
        match peek {
            Some(p) if p.is_visible() => p.record(),
            _ => self.open_selector(),
        }
    }
}

fn add_actions(app: &gtk::Application) {
    let simple = |name: &str, f: fn(&App)| {
        let a = gio::SimpleAction::new(name, None);
        a.connect_activate(move |_, _| f(&self::app()));
        app.add_action(&a);
    };
    simple("select", App::open_selector);
    simple("window", App::show_peek_window);
    simple("toggle", App::toggle);
    simple("stop", |a| a.ctl.stop());
    simple("pause", |a| a.ctl.toggle_pause());
    simple("replay", |a| a.ctl.toggle_replay());
    simple("tray-menu", |a| ui::tray_menu::show(&a.ctl));
    simple("save-replay", |a| a.ctl.save_replay());
    simple("cancel", |a| a.ctl.cancel());
    simple("preferences", |a| ui::prefs::show(&a.ctl));
    simple("open-folder", |a| {
        let dir = a.ctl.config.borrow().save_dir();
        let _ = std::fs::create_dir_all(&dir);
        launch_uri(&gio::File::for_path(dir).uri());
    });
    simple("quit", |a| {
        a.ctl.cancel();
        a.ctl.stop_replay();
        a.ctl.app.quit();
    });

    let open = gio::SimpleAction::new("open-file", Some(glib::VariantTy::STRING));
    open.connect_activate(|_, p| {
        if let Some(path) = p.and_then(|p| p.get::<String>()) {
            launch_uri(&gio::File::for_path(path).uri());
        }
    });
    app.add_action(&open);

    let show = gio::SimpleAction::new("show-in-folder", Some(glib::VariantTy::STRING));
    show.connect_activate(|_, p| {
        if let Some(path) = p.and_then(|p| p.get::<String>()) {
            show_in_folder(&path);
        }
    });
    app.add_action(&show);
}

fn launch_uri(uri: &str) {
    if let Err(e) = gio::AppInfo::launch_default_for_uri(uri, gio::AppLaunchContext::NONE) {
        eprintln!("peek2: could not open {uri}: {e}");
    }
}

fn show_in_folder(path: &str) {
    let uri = gio::File::for_path(path).uri().to_string();
    let parent = std::path::Path::new(path)
        .parent()
        .map(|p| gio::File::for_path(p).uri().to_string());
    glib::spawn_future_local(async move {
        let ok = match gio::bus_get_future(gio::BusType::Session).await {
            Ok(bus) => bus
                .call_future(
                    Some("org.freedesktop.FileManager1"),
                    "/org/freedesktop/FileManager1",
                    "org.freedesktop.FileManager1",
                    "ShowItems",
                    Some(&(vec![uri], "").to_variant()),
                    None,
                    gio::DBusCallFlags::NONE,
                    2000,
                )
                .await
                .is_ok(),
            Err(_) => false,
        };
        if !ok {
            if let Some(p) = parent {
                launch_uri(&p);
            }
        }
    });
}

fn setup_hotkeys(ctl: &Controller) {
    let cfg = ctl.config.borrow().clone();
    let grab = |accel: &str, what: &str, f: fn(&App)| {
        if accel.is_empty() {
            return;
        }
        match x11::grab_hotkey(accel) {
            Ok(rx) => {
                glib::spawn_future_local(async move {
                    while rx.recv().await.is_ok() {
                        f(&app());
                    }
                });
            }
            Err(e) => eprintln!("peek2: {what} hotkey disabled: {e:#}"),
        }
    };
    grab(&cfg.hotkey, "record", App::toggle);
    grab(&cfg.replay_hotkey, "save replay", |a| a.ctl.save_replay());
}

fn main() -> glib::ExitCode {
    // Wayland gives clients no global coordinates or screen access, so Peek2
    // always runs as an X11 client (natively or through XWayland).
    if std::env::var_os("GDK_BACKEND").is_none() {
        // SAFETY: no other threads exist yet.
        unsafe { std::env::set_var("GDK_BACKEND", "x11") };
    }

    // Bar integration runs standalone, without touching the display.
    if std::env::args().nth(1).as_deref() == Some("--status-follow") {
        return status::follow();
    }

    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();

    let opt = |long: &str, short: char, desc: &str| {
        app.add_main_option(long, glib::Char::from(short as u8), glib::OptionFlags::NONE, glib::OptionArg::None, desc, None);
    };
    opt("select", 's', "Select a region or window to record (default)");
    opt("window", 'w', "Show the Peek-style see-through recording window");
    opt("toggle", 't', "Start/stop recording in the running instance");
    opt("stop", 'x', "Stop recording");
    opt("cancel", 'c', "Cancel recording");
    opt("pause", 'P', "Pause or resume recording");
    opt("replay", 'r', "Turn the replay buffer on or off");
    opt("save-replay", 'R', "Save the last seconds of the replay buffer");
    opt("preferences", 'p', "Open preferences");
    opt("open-folder", 'o', "Open the recordings folder");
    opt("quit", 'q', "Quit the running instance");
    app.add_main_option(
        "review",
        glib::Char::from(b'v'),
        glib::OptionFlags::NONE,
        glib::OptionArg::Filename,
        "Open a clip in the review window (play, trim, fit size)",
        Some("FILE"),
    );
    opt("background", 'b', "Start resident (hotkey + tray icon) without opening anything");

    app.connect_startup(|app| {
        ui::load_css();
        let ctl = Controller::new(app);
        let state = Rc::new(App {
            ctl: ctl.clone(),
            _resident: app.hold(),
            peek: RefCell::new(None),
            selector: RefCell::new(None),
            frame: RefCell::new(None),
        });
        APP.with(|a| *a.borrow_mut() = Some(state));
        add_actions(app);
        app.set_accels_for_action("app.quit", &["<Ctrl>Q"]);
        setup_hotkeys(&ctl);
        tray::start(app);
        hw::probe();
        ctl.status("idle", "");
        if ctl.config.borrow().replay_on_start {
            ctl.toggle_replay();
        }
    });
    app.connect_shutdown(|_| {
        APP.with(|a| {
            if let Some(a) = a.borrow().as_ref() {
                a.ctl.stop_replay();
            }
        });
        status::clear();
    });

    app.connect_command_line(|app, cmd| {
        let opts = cmd.options_dict();
        if opts.contains("background") {
            return glib::ExitCode::SUCCESS;
        }
        if let Some(file) = opts.lookup::<std::path::PathBuf>("review").ok().flatten() {
            // Relative to the caller's directory, not the running instance's.
            let file = cmd.create_file_for_arg(&file);
            if let Some(path) = file.path() {
                ui::review::open(&self::app().ctl, &path);
                return glib::ExitCode::SUCCESS;
            }
        }
        let first = ["select", "window", "toggle", "stop", "pause", "replay", "save-replay", "cancel", "preferences", "open-folder", "quit"]
            .into_iter()
            .find(|o| opts.contains(o));
        app.activate_action(first.unwrap_or("select"), None);
        glib::ExitCode::SUCCESS
    });

    app.connect_activate(|_| self::app().open_selector());

    app.run()
}
