//! The bits GTK4 no longer exposes on X11: absolute positions, stacking,
//! override-redirect windows, window picking and global key grabs.

use anyhow::{Context, Result};
use gtk::prelude::*;
use std::rc::Rc;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, AtomEnum, ConnectionExt as _, ModMask, PropMode};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.width && y < self.y + self.height
    }
}

pub struct X {
    conn: RustConnection,
    root: u32,
    screen: usize,
}

thread_local! {
    static INSTANCE: Option<Rc<X>> = X::connect().map(Rc::new).map_err(|e| eprintln!("peek2: X11: {e:#}")).ok();
}

pub fn get() -> Option<Rc<X>> {
    INSTANCE.with(|x| x.clone())
}

pub fn xid(w: &impl IsA<gtk::Native>) -> Option<u32> {
    let surface = w.as_ref().surface()?;
    let s = surface.downcast_ref::<gdk4_x11::X11Surface>()?;
    Some(s.xid() as u32)
}

impl X {
    fn connect() -> Result<X> {
        let (conn, screen) = x11rb::connect(None).context("connecting to X server")?;
        let root = conn.setup().roots[screen].root;
        Ok(X { conn, root, screen })
    }

    fn atom(&self, name: &str) -> u32 {
        self.conn
            .intern_atom(false, name.as_bytes())
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.atom)
            .unwrap_or(0)
    }

    /// A composite manager is required for see-through windows.
    pub fn compositor_running(&self) -> bool {
        let sel = self.atom(&format!("_NET_WM_CM_S{}", self.screen));
        self.conn
            .get_selection_owner(sel)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|r| r.owner != 0)
    }

    /// Must be called before the window is mapped.
    pub fn set_override_redirect(&self, win: u32) {
        let _ = self
            .conn
            .change_window_attributes(win, &xproto::ChangeWindowAttributesAux::new().override_redirect(1));
        let _ = self.conn.flush();
    }

    /// Hint the WM to float the window (tiling WMs like bspwm/i3 float dialogs).
    pub fn set_dialog_type(&self, win: u32) {
        let ty = self.atom("_NET_WM_WINDOW_TYPE");
        let dialog = self.atom("_NET_WM_WINDOW_TYPE_DIALOG");
        let _ = self.conn.change_property32(PropMode::REPLACE, win, ty, AtomEnum::ATOM, &[dialog]);
        let _ = self.conn.flush();
    }

    /// Ask the WM to keep a mapped window above others and on all desktops.
    pub fn set_above(&self, win: u32) {
        let state = self.atom("_NET_WM_STATE");
        for name in ["_NET_WM_STATE_ABOVE", "_NET_WM_STATE_STICKY"] {
            let a = self.atom(name);
            let ev = xproto::ClientMessageEvent::new(32, win, state, [1, a, 0, 1, 0]);
            let _ = self.conn.send_event(
                false,
                self.root,
                xproto::EventMask::SUBSTRUCTURE_REDIRECT | xproto::EventMask::SUBSTRUCTURE_NOTIFY,
                ev,
            );
        }
        let _ = self.conn.flush();
    }

    pub fn move_resize(&self, win: u32, r: Rect) {
        let aux = xproto::ConfigureWindowAux::new()
            .x(r.x)
            .y(r.y)
            .width(r.width.max(1) as u32)
            .height(r.height.max(1) as u32)
            .stack_mode(xproto::StackMode::ABOVE);
        let _ = self.conn.configure_window(win, &aux);
        let _ = self.conn.flush();
    }

    pub fn move_to(&self, win: u32, x: i32, y: i32) {
        let aux = xproto::ConfigureWindowAux::new().x(x).y(y).stack_mode(xproto::StackMode::ABOVE);
        let _ = self.conn.configure_window(win, &aux);
        let _ = self.conn.flush();
    }

    /// A plain, solid-colour override-redirect window painted by the X
    /// server itself (no toolkit, no alpha), used for recording borders.
    pub fn create_solid(&self, r: Rect, rgb: u32) -> Option<u32> {
        let id = self.conn.generate_id().ok()?;
        let aux = xproto::CreateWindowAux::new().background_pixel(rgb).override_redirect(1);
        self.conn
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                id,
                self.root,
                r.x as i16,
                r.y as i16,
                r.width.max(1) as u16,
                r.height.max(1) as u16,
                0,
                xproto::WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &aux,
            )
            .ok()?;
        let _ = self.conn.map_window(id);
        let _ = self.conn.flush();
        Some(id)
    }

    pub fn set_mapped(&self, win: u32, mapped: bool) {
        let _ = if mapped { self.conn.map_window(win) } else { self.conn.unmap_window(win) };
        let _ = self.conn.flush();
    }

    pub fn destroy(&self, win: u32) {
        let _ = self.conn.destroy_window(win);
        let _ = self.conn.flush();
    }

    pub fn focus(&self, win: u32) {
        let _ = self.conn.set_input_focus(xproto::InputFocus::PARENT, win, x11rb::CURRENT_TIME);
        let _ = self.conn.flush();
    }

    pub fn root_size(&self) -> (u16, u16) {
        let s = &self.conn.setup().roots[self.screen];
        (s.width_in_pixels, s.height_in_pixels)
    }

    pub fn pointer(&self) -> Option<(i32, i32)> {
        let r = self.conn.query_pointer(self.root).ok()?.reply().ok()?;
        Some((r.root_x as i32, r.root_y as i32))
    }

    /// Position of a window's (0,0) in root coordinates.
    pub fn root_origin(&self, win: u32) -> Option<(i32, i32)> {
        let r = self.conn.translate_coordinates(win, self.root, 0, 0).ok()?.reply().ok()?;
        Some((r.dst_x as i32, r.dst_y as i32))
    }

    /// Visible top-level client windows in root coordinates, topmost first,
    /// excluding `skip`.
    pub fn client_rects(&self, skip: &[u32]) -> Vec<Rect> {
        let list = self.atom("_NET_CLIENT_LIST_STACKING");
        let extents = self.atom("_NET_FRAME_EXTENTS");
        let mut wins: Vec<u32> = self
            .conn
            .get_property(false, self.root, list, AtomEnum::WINDOW, 0, 4096)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().map(|v| v.collect()))
            .unwrap_or_default();
        if wins.is_empty() {
            // No EWMH window manager: fall back to the root's children.
            wins = self
                .conn
                .query_tree(self.root)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|t| t.children)
                .unwrap_or_default();
        }
        let mut out = Vec::new();
        for &w in wins.iter().rev() {
            if skip.contains(&w) {
                continue;
            }
            let Some(attrs) = self.conn.get_window_attributes(w).ok().and_then(|c| c.reply().ok()) else {
                continue;
            };
            if attrs.map_state != xproto::MapState::VIEWABLE || attrs.override_redirect {
                continue;
            }
            let Some(geo) = self.conn.get_geometry(w).ok().and_then(|c| c.reply().ok()) else {
                continue;
            };
            let Some((x, y)) = self.root_origin(w) else { continue };
            let ext: Vec<u32> = self
                .conn
                .get_property(false, w, extents, AtomEnum::CARDINAL, 0, 4)
                .ok()
                .and_then(|c| c.reply().ok())
                .and_then(|r| r.value32().map(|v| v.collect()))
                .unwrap_or_default();
            let (l, r, t, b) = match ext.as_slice() {
                [l, r, t, b] => (*l as i32, *r as i32, *t as i32, *b as i32),
                _ => (0, 0, 0, 0),
            };
            out.push(Rect {
                x: x - l,
                y: y - t,
                width: geo.width as i32 + l + r,
                height: geo.height as i32 + t + b,
            });
        }
        out
    }
}

fn keysym_to_keycodes(conn: &RustConnection, keysym: u32) -> Vec<u8> {
    let setup = conn.setup();
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let Some(map) = conn
        .get_keyboard_mapping(min, max - min + 1)
        .ok()
        .and_then(|c| c.reply().ok())
    else {
        return Vec::new();
    };
    let per = map.keysyms_per_keycode as usize;
    let mut codes = Vec::new();
    for (i, syms) in map.keysyms.chunks(per.max(1)).enumerate() {
        if syms.iter().any(|&s| s == keysym) {
            codes.push(min + i as u8);
        }
    }
    codes
}

/// Grab a global hotkey given in GTK accelerator syntax (e.g. `<Ctrl><Alt>R`).
/// Each press is delivered on the returned channel.
pub fn grab_hotkey(accel: &str) -> Result<async_channel::Receiver<()>> {
    let (key, mods) = gtk::accelerator_parse(accel).context("invalid hotkey")?;
    let keysym = key.to_lower().into_glib_keysym();
    let mut mask = ModMask::from(0u16);
    if mods.contains(gtk::gdk::ModifierType::CONTROL_MASK) {
        mask |= ModMask::CONTROL;
    }
    if mods.contains(gtk::gdk::ModifierType::ALT_MASK) {
        mask |= ModMask::M1;
    }
    if mods.contains(gtk::gdk::ModifierType::SHIFT_MASK) {
        mask |= ModMask::SHIFT;
    }
    if mods.contains(gtk::gdk::ModifierType::SUPER_MASK) {
        mask |= ModMask::M4;
    }

    let (conn, screen) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen].root;
    let codes = keysym_to_keycodes(&conn, keysym);
    anyhow::ensure!(!codes.is_empty(), "no keycode for {accel}");
    // Also grab with NumLock (M2) and CapsLock (LOCK) combinations.
    let extras = [0u16, u16::from(ModMask::M2), u16::from(ModMask::LOCK), u16::from(ModMask::M2 | ModMask::LOCK)];
    for &code in &codes {
        for &extra in &extras {
            conn.grab_key(
                true,
                root,
                mask | ModMask::from(extra),
                code,
                xproto::GrabMode::ASYNC,
                xproto::GrabMode::ASYNC,
            )?
            .check()
            .with_context(|| format!("{accel} is already grabbed by another program"))?;
        }
    }
    conn.flush()?;

    let (tx, rx) = async_channel::unbounded();
    std::thread::spawn(move || {
        while let Ok(ev) = conn.wait_for_event() {
            if let x11rb::protocol::Event::KeyPress(_) = ev {
                if tx.send_blocking(()).is_err() {
                    break;
                }
            }
        }
    });
    Ok(rx)
}

trait IntoKeysym {
    fn into_glib_keysym(self) -> u32;
}

impl IntoKeysym for gtk::gdk::Key {
    fn into_glib_keysym(self) -> u32 {
        use gtk::glib::translate::IntoGlib;
        self.into_glib()
    }
}
