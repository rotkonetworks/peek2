//! Right-click menu for the tray icon: a small override-redirect window at
//! the pointer (the XEmbed tray has no menu protocol of its own). Closes on
//! Escape, on losing focus, or after an item runs.

use super::frame::popup_window;
use crate::config::{AudioMode, Format};
use crate::controller::{Controller, Phase};
use crate::x11;
use gtk::{gdk, glib, prelude::*};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

thread_local! {
    static OPEN: RefCell<Option<gtk::Window>> = const { RefCell::new(None) };
}

fn close() {
    if let Some(w) = OPEN.with(|o| o.borrow_mut().take()) {
        w.destroy();
    }
}

/// Newest clips in the save folder.
fn recent(ctl: &Controller, n: usize) -> Vec<PathBuf> {
    let dir = ctl.config.borrow().save_dir();
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| matches!(x.to_str(), Some("webm" | "mp4" | "gif" | "apng"))))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files.into_iter().take(n).map(|(_, p)| p).collect()
}

fn item(label: &str, accel: Option<&str>, f: impl Fn() + 'static) -> gtk::Button {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    row.append(&gtk::Label::builder().label(label).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::Middle).build());
    if let Some(a) = accel {
        row.append(&gtk::Label::builder().label(a).css_classes(["peek2-dim"]).build());
    }
    let b = gtk::Button::builder().child(&row).css_classes(["flat", "peek2-menu-item"]).build();
    b.connect_clicked(move |_| {
        close();
        f();
    });
    b
}

fn heading(text: &str) -> gtk::Label {
    gtk::Label::builder().label(text.to_uppercase()).xalign(0.0).css_classes(["heading", "peek2-menu-heading"]).build()
}

/// A row of exclusive choices, applied immediately (menu stays open).
fn segmented<T: Copy + PartialEq + 'static>(options: &[(T, &str)], current: T, set: impl Fn(T) + 'static) -> gtk::Box {
    let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).css_classes(["linked", "peek2-seg"]).homogeneous(true).build();
    let set = Rc::new(set);
    let mut first: Option<gtk::ToggleButton> = None;
    for &(value, label) in options {
        let b = gtk::ToggleButton::with_label(label);
        b.set_active(value == current);
        if let Some(f) = &first {
            b.set_group(Some(f));
        } else {
            first = Some(b.clone());
        }
        let set = set.clone();
        b.connect_toggled(move |b| {
            if b.is_active() {
                set(value);
            }
        });
        row.append(&b);
    }
    row
}

fn act(name: &'static str) -> impl Fn() + 'static {
    move || {
        crate::app().ctl.app.activate_action(name, None);
    }
}

pub fn show(ctl: &Rc<Controller>) {
    close();
    let cfg = ctl.config.borrow().clone();
    let win = popup_window(ctl, &["peek2-menu"]);
    let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
    let sep = || gtk::Separator::new(gtk::Orientation::Horizontal);
    let key = |a: &str| gtk::accelerator_parse(a).map(|(k, m)| gtk::accelerator_get_label(k, m).to_string());
    let rec_key = key(&cfg.hotkey);
    let replay_key = key(&cfg.replay_hotkey);

    match ctl.phase() {
        Phase::Idle => {
            col.append(&item("Record area", rec_key.as_deref(), act("select")));
            col.append(&item("Peek window", None, act("window")));
        }
        Phase::Paused(_) => {
            col.append(&item("Resume", None, act("pause")));
            col.append(&item("Stop and save", rec_key.as_deref(), act("stop")));
            col.append(&item("Discard", None, act("cancel")));
        }
        Phase::Recording(_) | Phase::Countdown(_) => {
            col.append(&item("Stop and save", rec_key.as_deref(), act("stop")));
            col.append(&item("Pause", None, act("pause")));
            col.append(&item("Discard", None, act("cancel")));
        }
        Phase::Processing => {
            col.append(&item("Cancel saving", None, act("cancel")));
        }
    }
    col.append(&sep());
    let on = ctl.replay_active();
    col.append(&item(
        &if on { format!("Replay buffer: on, last {}s", cfg.replay_seconds) } else { "Replay buffer: off".into() },
        None,
        act("replay"),
    ));
    if on {
        col.append(&item("Save replay", replay_key.as_deref(), act("save-replay")));
    }
    col.append(&sep());

    col.append(&heading("Format"));
    let c = ctl.clone();
    col.append(&segmented(
        &Format::ALL.map(|f| (f, f.label())),
        cfg.format,
        move |f| c.update_config(|c| c.format = f),
    ));
    col.append(&heading("Audio"));
    let c = ctl.clone();
    col.append(&segmented(
        &[(AudioMode::Off, "Off"), (AudioMode::Desktop, "Desktop"), (AudioMode::Mic, "Mic"), (AudioMode::Both, "Both")],
        cfg.audio,
        move |a| c.update_config(|c| c.audio = a),
    ));
    col.append(&sep());

    let files = recent(ctl, 5);
    if !files.is_empty() {
        col.append(&heading("Recent"));
        for f in files {
            let name = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let size = std::fs::metadata(&f).map(|m| glib::format_size(m.len()).to_string()).unwrap_or_default();
            let c = ctl.clone();
            let video = f.extension().is_some_and(|x| x == "webm" || x == "mp4");
            col.append(&item(&name, Some(&size), move || {
                if video {
                    super::review::open(&c, &f);
                } else {
                    c.app.activate_action("open-file", Some(&f.to_string_lossy().to_variant()));
                }
            }));
        }
    }
    col.append(&item("Open folder", None, act("open-folder")));
    col.append(&sep());
    col.append(&item("Preferences", None, act("preferences")));
    col.append(&item("Quit peek2", None, act("quit")));
    win.set_child(Some(&col));

    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed(|_, k, _, _| {
        if k == gdk::Key::Escape {
            close();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    win.add_controller(keys);
    // Close when focus goes elsewhere (a click outside, the WM, …).
    win.connect_is_active_notify(|w| {
        if !w.is_active() {
            let w = w.clone();
            super::after(80, move || {
                if !w.is_active() {
                    close();
                }
            });
        }
    });

    // Open at the pointer, kept inside its monitor.
    win.connect_map(|w| {
        let w = w.clone();
        super::after(1, move || {
            let (Some(x), Some(id)) = (x11::get(), x11::xid(&w)) else { return };
            let Some((px, py)) = x.pointer() else { return };
            let mon = crate::replay::current_monitor();
            let (_, mw, _, _) = w.measure(gtk::Orientation::Horizontal, -1);
            let (_, mh, _, _) = w.measure(gtk::Orientation::Vertical, mw);
            let s = w.scale_factor().max(1);
            let (mw, mh) = (mw * s, mh * s);
            let (mut nx, mut ny) = (px - mw / 2, py + 4);
            if let Some(m) = mon {
                nx = nx.clamp(m.x, (m.x + m.width - mw).max(m.x));
                if ny + mh > m.y + m.height {
                    ny = (py - mh - 4).max(m.y);
                }
            }
            x.move_to(id, nx, ny);
            x.focus(id);
        });
    });
    OPEN.with(|o| *o.borrow_mut() = Some(win.clone()));
    win.present();
}
