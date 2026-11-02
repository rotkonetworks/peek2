use crate::audio;
use crate::config::Config;
use crate::controller::Controller;
use gtk::{gio, glib, prelude::*};
use std::rc::Rc;

fn row(grid: &gtk::Grid, y: &mut i32, label: &str, widget: &impl IsA<gtk::Widget>) {
    let l = gtk::Label::builder().label(label).xalign(0.0).hexpand(true).build();
    grid.attach(&l, 0, *y, 1, 1);
    widget.set_halign(gtk::Align::End);
    grid.attach(widget, 1, *y, 1, 1);
    *y += 1;
}

fn heading(grid: &gtk::Grid, y: &mut i32, text: &str) {
    let l = gtk::Label::builder()
        .label(text.to_uppercase())
        .xalign(0.0)
        .css_classes(["heading"])
        .margin_top(if *y == 0 { 0 } else { 12 })
        .build();
    grid.attach(&l, 0, *y, 2, 1);
    *y += 1;
}

fn spin(ctl: &Rc<Controller>, min: f64, max: f64, value: u32, set: fn(&mut Config, u32)) -> gtk::SpinButton {
    let s = gtk::SpinButton::with_range(min, max, 1.0);
    s.set_value(value as f64);
    let ctl = ctl.clone();
    s.connect_value_changed(move |s| ctl.update_config(|c| set(c, s.value() as u32)));
    s
}

fn switch(ctl: &Rc<Controller>, value: bool, set: fn(&mut Config, bool)) -> gtk::Switch {
    let s = gtk::Switch::new();
    s.set_active(value);
    let ctl = ctl.clone();
    s.connect_active_notify(move |s| ctl.update_config(|c| set(c, s.is_active())));
    s
}

fn device_dropdown(ctl: &Rc<Controller>, monitors: bool, current: &str, set: fn(&mut Config, String)) -> gtk::DropDown {
    let sources: Vec<audio::Source> = audio::list_sources().into_iter().filter(|s| s.is_monitor == monitors).collect();
    let mut labels = vec![if monitors { "Auto (where audio is playing)".to_string() } else { "Default input".to_string() }];
    labels.extend(sources.iter().map(|s| s.description.clone()));
    let refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
    let d = gtk::DropDown::from_strings(&refs);
    d.set_size_request(260, -1);
    let selected = sources.iter().position(|s| s.name == current).map(|i| i + 1).unwrap_or(0);
    d.set_selected(selected as u32);
    let ctl = ctl.clone();
    d.connect_selected_notify(move |d| {
        let name = match d.selected() {
            0 => String::new(),
            i => sources.get(i as usize - 1).map(|s| s.name.clone()).unwrap_or_default(),
        };
        ctl.update_config(|c| set(c, name));
    });
    d
}

pub fn show(ctl: &Rc<Controller>) {
    let parent = ctl.app.active_window();
    let cfg = ctl.config.borrow().clone();
    let grid = gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(24)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();
    let mut y = 0;

    heading(&grid, &mut y, "Recording");
    row(&grid, &mut y, "Frames per second", &spin(ctl, 1.0, 60.0, cfg.framerate, |c, v| c.framerate = v));
    let down = gtk::DropDown::from_strings(&["Full size", "1/2", "1/3", "1/4"]);
    down.set_selected(cfg.downsample.clamp(1, 4) - 1);
    {
        let ctl = ctl.clone();
        down.connect_selected_notify(move |d| ctl.update_config(|c| c.downsample = d.selected() + 1));
    }
    row(&grid, &mut y, "Resolution", &down);
    let q = spin(ctl, 10.0, 40.0, cfg.video_quality, |c, v| c.video_quality = v);
    q.set_tooltip_text(Some("CRF: lower is better quality and bigger files (WebM/MP4)"));
    row(&grid, &mut y, "Video quality (CRF, lower = better)", &q);
    row(&grid, &mut y, "GIF quality", &spin(ctl, 1.0, 100.0, cfg.gif_quality as u32, |c, v| c.gif_quality = v as u8));
    row(&grid, &mut y, "Start delay (seconds)", &spin(ctl, 0.0, 10.0, cfg.start_delay, |c, v| c.start_delay = v));
    let hw = switch(ctl, cfg.hardware_encoding, |c, v| c.hardware_encoding = v);
    hw.set_tooltip_text(Some(match crate::hw::vaapi_device() {
        Some(_) => "H.264 on the GPU via VAAPI: near-zero CPU, slightly bigger files",
        None => "No working VAAPI H.264 encoder found; MP4 uses the CPU",
    }));
    row(&grid, &mut y, "Hardware encoding (MP4)", &hw);
    row(&grid, &mut y, "Capture mouse pointer", &switch(ctl, cfg.capture_mouse, |c, v| c.capture_mouse = v));

    heading(&grid, &mut y, "Audio (WebM and MP4)");
    row(&grid, &mut y, "Desktop audio from", &device_dropdown(ctl, true, &cfg.desktop_source, |c, v| c.desktop_source = v));
    row(&grid, &mut y, "Microphone", &device_dropdown(ctl, false, &cfg.mic_source, |c, v| c.mic_source = v));
    row(&grid, &mut y, "Microphone noise suppression", &switch(ctl, cfg.mic_noise_suppression, |c, v| c.mic_noise_suppression = v));
    let sep = switch(ctl, cfg.separate_audio_tracks, |c, v| c.separate_audio_tracks = v);
    sep.set_tooltip_text(Some("Desktop + mic: track 1 is the mix, then desktop and mic alone, for editing"));
    row(&grid, &mut y, "Separate desktop and mic tracks", &sep);

    heading(&grid, &mut y, "Saving");
    let folder_label = gtk::Label::builder()
        .label(cfg.save_dir().to_string_lossy())
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .max_width_chars(30)
        .build();
    let folder = gtk::Button::builder().child(&folder_label).width_request(260).build();
    {
        let ctl = ctl.clone();
        let folder_label = folder_label.clone();
        folder.connect_clicked(move |b| {
            let ctl = ctl.clone();
            let b = b.clone();
            let folder_label = folder_label.clone();
            glib::spawn_future_local(async move {
                let dir = ctl.config.borrow().save_dir();
                let dialog = gtk::FileDialog::builder()
                    .title("Save recordings to")
                    .initial_folder(&gio::File::for_path(&dir))
                    .build();
                let parent = b.root().and_downcast::<gtk::Window>();
                if let Ok(f) = dialog.select_folder_future(parent.as_ref()).await {
                    if let Some(p) = f.path() {
                        folder_label.set_label(&p.to_string_lossy());
                        ctl.update_config(|c| c.save_folder = p.to_string_lossy().into_owned());
                    }
                }
            });
        });
    }
    row(&grid, &mut y, "Folder", &folder);
    let name = gtk::Entry::builder().text(&cfg.filename_format).width_chars(24).build();
    name.set_tooltip_text(Some("strftime format, e.g. %Y-%m-%d %H-%M-%S"));
    {
        let ctl = ctl.clone();
        name.connect_changed(move |e| ctl.update_config(|c| c.filename_format = e.text().to_string()));
    }
    row(&grid, &mut y, "File name", &name);
    row(&grid, &mut y, "Ask where to save", &switch(ctl, cfg.ask_save_location, |c, v| c.ask_save_location = v));
    row(&grid, &mut y, "Copy file to clipboard", &switch(ctl, cfg.copy_to_clipboard, |c, v| c.copy_to_clipboard = v));
    row(&grid, &mut y, "Review and trim after saving", &switch(ctl, cfg.review_after_save, |c, v| c.review_after_save = v));
    row(&grid, &mut y, "Show notification", &switch(ctl, cfg.notify, |c, v| c.notify = v));

    heading(&grid, &mut y, "Replay buffer");
    let secs = spin(ctl, 10.0, 600.0, cfg.replay_seconds, |c, v| c.replay_seconds = v);
    secs.set_tooltip_text(Some("Kept in RAM; applies when the buffer is next turned on"));
    row(&grid, &mut y, "Keep the last (seconds)", &secs);
    row(&grid, &mut y, "Start with peek2", &switch(ctl, cfg.replay_on_start, |c, v| c.replay_on_start = v));

    heading(&grid, &mut y, "Shortcut");
    let hotkey = gtk::Entry::builder().text(&cfg.hotkey).width_chars(16).build();
    hotkey.set_tooltip_text(Some("GTK accelerator, e.g. <Ctrl><Alt>R or <Super>Print. Applies after restart."));
    {
        let ctl = ctl.clone();
        hotkey.connect_changed(move |e| {
            let t = e.text().to_string();
            if t.is_empty() || gtk::accelerator_parse(&t).is_some() {
                e.remove_css_class("error");
                ctl.update_config(|c| c.hotkey = t);
            } else {
                e.add_css_class("error");
            }
        });
    }
    row(&grid, &mut y, "Global record hotkey (restart to apply)", &hotkey);
    let replay_key = gtk::Entry::builder().text(&cfg.replay_hotkey).width_chars(16).build();
    replay_key.set_tooltip_text(Some("Saves the replay buffer. Applies after restart."));
    {
        let ctl = ctl.clone();
        replay_key.connect_changed(move |e| {
            let t = e.text().to_string();
            if t.is_empty() || gtk::accelerator_parse(&t).is_some() {
                e.remove_css_class("error");
                ctl.update_config(|c| c.replay_hotkey = t);
            } else {
                e.add_css_class("error");
            }
        });
    }
    row(&grid, &mut y, "Save replay hotkey (restart to apply)", &replay_key);

    let win = gtk::Window::builder()
        .title("peek2 preferences")
        .application(&ctl.app)
        .child(
            &gtk::ScrolledWindow::builder()
                .child(&grid)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .propagate_natural_height(true)
                .build(),
        )
        .default_width(560)
        .build();
    if let Some(parent) = parent {
        win.set_transient_for(Some(&parent));
    }
    win.present();
}
