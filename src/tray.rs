//! System tray icon (freedesktop XEmbed tray, as polybar's `internal/tray`,
//! stalonetray, trayer… implement it). GTK4 dropped tray support, so the
//! icon is a plain X window docked by hand and painted with cairo: a record
//! ring, with a red dot inside while recording.
//!
//! Left click starts a selection or stops the recording, middle saves the
//! replay buffer (or opens the recordings folder), right opens the menu.

use gtk::{cairo, gio, gio::prelude::*, glib};
use std::cell::{Cell, RefCell};
use std::os::fd::AsFd;
use std::rc::Rc;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _, EventMask, PropMode};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

const SYSTEM_TRAY_REQUEST_DOCK: u32 = 0;

struct Atoms {
    selection: u32,
    opcode: u32,
    visual: u32,
    manager: u32,
    xembed_info: u32,
    wm_name: u32,
    utf8: u32,
}

struct Tray {
    conn: RustConnection,
    root: u32,
    root_visual: u32,
    root_depth: u8,
    atoms: Atoms,
    win: Cell<u32>,
    gc: Cell<u32>,
    depth: Cell<u8>,
    size: Cell<(u16, u16)>,
    state: RefCell<String>,
}

fn intern(conn: &RustConnection, name: &str) -> anyhow::Result<u32> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

/// Dock the icon and keep it docked across tray (bar) restarts.
pub fn start(app: &gtk::Application) {
    let tray = match Tray::connect() {
        Ok(t) => Rc::new(t),
        Err(e) => {
            eprintln!("peek2: tray icon disabled: {e:#}");
            return;
        }
    };
    tray.dock();
    // Watch the connection's fd (a dup, so GSocket may close its own copy).
    match tray.conn.stream().as_fd().try_clone_to_owned().map_err(anyhow::Error::from).and_then(|fd| Ok(gio::Socket::from_fd(fd)?)) {
        Ok(sock) => {
            let tray = tray.clone();
            let app = app.clone();
            gio::prelude::SocketExtManual::create_source(&sock, glib::IOCondition::IN, gio::Cancellable::NONE, Some("peek2-tray"), glib::Priority::DEFAULT, move |_, _| {
                tray.pump(&app);
                glib::ControlFlow::Continue
            })
            .attach(None);
        }
        Err(e) => eprintln!("peek2: tray: {e:#}"),
    }
    // Replies can pull events into x11rb's buffer without the fd waking us.
    {
        let tray = tray.clone();
        let app = app.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(500), move || {
            tray.pump(&app);
            glib::ControlFlow::Continue
        });
    }
    TRAY.with(|t| *t.borrow_mut() = Some(tray));
}

thread_local! {
    static TRAY: RefCell<Option<Rc<Tray>>> = const { RefCell::new(None) };
}

/// Called with every bar status change (see `status::write`).
pub fn set_state(state: &str) {
    if let Some(t) = TRAY.with(|t| t.borrow().clone()) {
        if *t.state.borrow() != state {
            *t.state.borrow_mut() = state.to_string();
            t.draw();
        }
    }
}

impl Tray {
    fn connect() -> anyhow::Result<Tray> {
        let (conn, screen) = x11rb::connect(None)?;
        let s = &conn.setup().roots[screen];
        let (root, root_visual, root_depth) = (s.root, s.root_visual, s.root_depth);
        let atoms = Atoms {
            selection: intern(&conn, &format!("_NET_SYSTEM_TRAY_S{screen}"))?,
            opcode: intern(&conn, "_NET_SYSTEM_TRAY_OPCODE")?,
            visual: intern(&conn, "_NET_SYSTEM_TRAY_VISUAL")?,
            manager: intern(&conn, "MANAGER")?,
            xembed_info: intern(&conn, "_XEMBED_INFO")?,
            wm_name: intern(&conn, "_NET_WM_NAME")?,
            utf8: intern(&conn, "UTF8_STRING")?,
        };
        // A (re)started tray announces itself with a MANAGER message on the root.
        conn.change_window_attributes(root, &xproto::ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY))?;
        conn.flush()?;
        Ok(Tray {
            conn,
            root,
            root_visual,
            root_depth,
            atoms,
            win: Cell::new(0),
            gc: Cell::new(0),
            depth: Cell::new(0),
            size: Cell::new((20, 20)),
            state: RefCell::new("idle".into()),
        })
    }

    /// Visual the tray asks icons to use; 32-bit ARGB gives a transparent icon.
    fn tray_visual(&self, owner: u32) -> (u32, u8) {
        let id = self
            .conn
            .get_property(false, owner, self.atoms.visual, xproto::AtomEnum::VISUALID, 0, 1)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().and_then(|mut v| v.next()));
        let depth = id.and_then(|id| {
            self.conn.setup().roots.iter().flat_map(|s| &s.allowed_depths).find_map(|d| {
                d.visuals.iter().any(|v| v.visual_id == id).then_some(d.depth)
            })
        });
        match (id, depth) {
            (Some(id), Some(d)) => (id, d),
            _ => (self.root_visual, self.root_depth),
        }
    }

    fn dock(&self) {
        if let Err(e) = self.try_dock() {
            eprintln!("peek2: tray: {e:#}");
        }
    }

    fn try_dock(&self) -> anyhow::Result<()> {
        let c = &self.conn;
        let owner = c.get_selection_owner(self.atoms.selection)?.reply()?.owner;
        if owner == x11rb::NONE {
            return Ok(()); // no tray yet; MANAGER will tell us
        }
        if self.win.get() != 0 {
            let _ = c.free_gc(self.gc.get());
            let _ = c.destroy_window(self.win.get());
        }
        let (visual, depth) = self.tray_visual(owner);
        let (w, h) = self.size.get();
        let win = c.generate_id()?;
        let mut aux = xproto::CreateWindowAux::new()
            .event_mask(EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY | EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE);
        if depth == 32 {
            let cmap = c.generate_id()?;
            c.create_colormap(xproto::ColormapAlloc::NONE, cmap, self.root, visual)?;
            aux = aux.background_pixel(0).border_pixel(0).colormap(cmap);
        } else {
            aux = aux.background_pixmap(xproto::BackPixmap::PARENT_RELATIVE);
        }
        c.create_window(depth, win, self.root, 0, 0, w, h, 0, xproto::WindowClass::INPUT_OUTPUT, visual, &aux)?;
        c.change_property32(PropMode::REPLACE, win, self.atoms.xembed_info, self.atoms.xembed_info, &[0, 1])?;
        c.change_property8(PropMode::REPLACE, win, self.atoms.wm_name, self.atoms.utf8, b"peek2")?;
        c.change_property8(PropMode::REPLACE, win, xproto::AtomEnum::WM_CLASS, xproto::AtomEnum::STRING, b"peek2\0Peek2\0")?;
        let gc = c.generate_id()?;
        c.create_gc(gc, win, &xproto::CreateGCAux::new())?;
        self.win.set(win);
        self.gc.set(gc);
        self.depth.set(depth);
        let msg = xproto::ClientMessageEvent::new(32, owner, self.atoms.opcode, [x11rb::CURRENT_TIME, SYSTEM_TRAY_REQUEST_DOCK, win, 0, 0]);
        c.send_event(false, owner, EventMask::NO_EVENT, msg)?;
        c.flush()?;
        Ok(())
    }

    fn pump(&self, app: &gtk::Application) {
        while let Ok(Some(ev)) = self.conn.poll_for_event() {
            match ev {
                Event::Expose(e) if e.count == 0 && e.window == self.win.get() => self.draw(),
                Event::ConfigureNotify(e) if e.window == self.win.get() => {
                    if (e.width, e.height) != self.size.get() {
                        self.size.set((e.width, e.height));
                        self.draw();
                    }
                }
                Event::ClientMessage(e) if e.type_ == self.atoms.manager => {
                    if e.data.as_data32()[1] == self.atoms.selection {
                        self.dock();
                    }
                }
                Event::ButtonRelease(e) if e.event == self.win.get() => {
                    let replay = *self.state.borrow() == "replay";
                    let action = match e.detail {
                        1 => "toggle",
                        2 if replay => "save-replay",
                        2 => "open-folder",
                        3 => "tray-menu",
                        _ => continue,
                    };
                    app.activate_action(action, None);
                }
                _ => {}
            }
        }
    }

    fn draw(&self) {
        let win = self.win.get();
        if win == 0 {
            return;
        }
        let (w, h) = self.size.get();
        // Without an ARGB visual, draw over whatever the bar shows behind us:
        // let the server paint the parent-relative background, then read it.
        let backdrop = if self.depth.get() == 32 {
            None
        } else {
            let _ = self.conn.clear_area(false, win, 0, 0, 0, 0);
            self.conn
                .get_image(xproto::ImageFormat::Z_PIXMAP, win, 0, 0, w, h, !0)
                .ok()
                .and_then(|c| c.reply().ok())
                .filter(|r| r.data.len() == w as usize * h as usize * 4)
                .map(|r| r.data)
        };
        let Some(data) = render(&self.state.borrow(), w as i32, h as i32, backdrop) else { return };
        let _ = self.conn.put_image(xproto::ImageFormat::Z_PIXMAP, win, self.gc.get(), w, h, 0, 0, 0, self.depth.get(), &data);
        let _ = self.conn.flush();
    }
}

fn rgb(hex: u32) -> (f64, f64, f64) {
    (((hex >> 16) & 0xff) as f64 / 255.0, ((hex >> 8) & 0xff) as f64 / 255.0, (hex & 0xff) as f64 / 255.0)
}

/// Paint the seal; BGRA rows as X wants them for 24/32-bit visuals.
fn render(state: &str, w: i32, h: i32, backdrop: Option<Vec<u8>>) -> Option<Vec<u8>> {
    let format = if backdrop.is_some() { cairo::Format::Rgb24 } else { cairo::Format::ARgb32 };
    let mut surf = cairo::ImageSurface::create(format, w, h).ok()?;
    if let Some(bg) = backdrop {
        let stride = surf.stride() as usize;
        let mut data = surf.data().ok()?;
        for (y, row) in bg.chunks(w as usize * 4).enumerate() {
            data[y * stride..y * stride + row.len()].copy_from_slice(row);
        }
    }
    {
        let cr = cairo::Context::new(&surf).ok()?;
        // zk.poker colours: cream ring when idle, gold while busy, green once
        // saved; recording fills the ring with a red dot.
        // (ring, dot colour, dot size): a big dot is an active state.
        let (accent, dot, big) = match state {
            "recording" => (0xc73e3a, Some(0xc73e3a), true),
            "paused" => (0xc73e3a, None, false),
            "countdown" | "processing" => (0xd9a441, Some(0xd9a441), true),
            "saved" => (0x8fbf73, Some(0x8fbf73), true),
            // Replay buffer running: idle, with a red pilot light.
            "replay" => (0xded5c4, Some(0xc73e3a), false),
            _ => (0xded5c4, Some(0xded5c4), false),
        };
        let side = w.min(h) as f64;
        let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
        let (r, g, b) = rgb(accent);
        cr.set_source_rgb(r, g, b);
        cr.set_line_width(1.5);
        cr.arc(cx, cy, side * 0.42 - 0.75, 0.0, std::f64::consts::TAU);
        let _ = cr.stroke();
        if let Some(c) = dot {
            let (r, g, b) = rgb(c);
            cr.set_source_rgb(r, g, b);
            cr.arc(cx, cy, side * if big { 0.25 } else { 0.14 }, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
        }
    }
    surf.flush();
    let stride = surf.stride() as usize;
    let data = surf.data().ok()?;
    let mut out = Vec::with_capacity(w as usize * h as usize * 4);
    for row in data.chunks(stride).take(h as usize) {
        out.extend_from_slice(&row[..w as usize * 4]);
    }
    Some(out)
}
