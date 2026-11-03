//! Everything X11, on one shared connection: window helpers, monitors,
//! window picking, global hotkeys, the clipboard, cursors and the event
//! dispatch to whichever object owns a window.

use crate::rt;
use anyhow::{Context, Result};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{self, AtomEnum, ConnectionExt as _, EventMask, ModMask, PropMode, Window};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub fn new(x: i32, y: i32, width: i32, height: i32) -> Rect {
        Rect { x, y, width, height }
    }

    pub fn intersect(&self, o: Rect) -> Rect {
        let (x0, y0) = (self.x.max(o.x), self.y.max(o.y));
        let (x1, y1) = ((self.x + self.width).min(o.x + o.width), (self.y + self.height).min(o.y + o.height));
        Rect::new(x0, y0, (x1 - x0).max(0), (y1 - y0).max(0))
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.width && y < self.y + self.height
    }
}

pub type Handler = Rc<dyn Fn(&Event)>;

struct Hotkey {
    codes: Vec<u8>,
    mods: u16,
    action: Rc<dyn Fn()>,
}

/// One in-flight INCR clipboard transfer.
struct Incr {
    requestor: Window,
    property: u32,
    target: u32,
    data: Rc<Vec<u8>>,
    offset: usize,
}

#[derive(Default)]
struct Clipboard {
    /// (target atom, data)
    items: Vec<(u32, Rc<Vec<u8>>)>,
    incr: Vec<Incr>,
}

pub struct Atoms {
    pub clipboard: u32,
    pub targets: u32,
    pub timestamp: u32,
    pub incr: u32,
    pub utf8: u32,
    pub net_wm_name: u32,
}

pub struct X {
    pub conn: Arc<RustConnection>,
    pub root: Window,
    pub screen: usize,
    pub root_depth: u8,
    pub root_visual: u32,
    pub atoms: Atoms,
    /// 32-bit TrueColor visual and colormap, for see-through windows.
    argb: Option<(u32, u32)>,
    /// Window that owns our selections.
    owner: Window,
    handlers: RefCell<HashMap<Window, Handler>>,
    scale: std::cell::Cell<Option<i32>>,
    root_handlers: RefCell<Vec<Handler>>,
    hotkeys: RefCell<Vec<Hotkey>>,
    keymap: RefCell<(u8, usize, Vec<u32>)>,
    clip: RefCell<Clipboard>,
    cursors: RefCell<HashMap<u16, u32>>,
    cursor_font: u32,
    max_chunk: usize,
}

thread_local! {
    static INSTANCE: RefCell<Option<Rc<X>>> = const { RefCell::new(None) };
}

/// The connection; `init` must have succeeded.
pub fn get() -> Rc<X> {
    INSTANCE.with(|x| x.borrow().clone()).expect("X11 not initialised")
}

fn intern(conn: &RustConnection, name: &str) -> Result<u32> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

pub fn init() -> Result<Rc<X>> {
    let (conn, screen) = x11rb::connect(None).context("cannot open the X display (peek2 needs X11 or XWayland)")?;
    let conn = Arc::new(conn);
    let s = &conn.setup().roots[screen];
    let (root, root_depth, root_visual) = (s.root, s.root_depth, s.root_visual);
    let argb = s
        .allowed_depths
        .iter()
        .filter(|d| d.depth == 32)
        .flat_map(|d| &d.visuals)
        .find(|v| v.class == xproto::VisualClass::TRUE_COLOR)
        .map(|v| v.visual_id)
        .and_then(|vis| {
            let cmap = conn.generate_id().ok()?;
            conn.create_colormap(xproto::ColormapAlloc::NONE, cmap, root, vis).ok()?;
            Some((vis, cmap))
        });
    let atoms = Atoms {
        clipboard: intern(&conn, "CLIPBOARD")?,
        targets: intern(&conn, "TARGETS")?,
        timestamp: intern(&conn, "TIMESTAMP")?,
        incr: intern(&conn, "INCR")?,
        utf8: intern(&conn, "UTF8_STRING")?,
        net_wm_name: intern(&conn, "_NET_WM_NAME")?,
    };
    // Root events: tray (re)starts arrive as MANAGER client messages.
    conn.change_window_attributes(root, &xproto::ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY))?;
    let owner = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        owner,
        root,
        -10,
        -10,
        1,
        1,
        0,
        xproto::WindowClass::INPUT_ONLY,
        x11rb::COPY_FROM_PARENT,
        &xproto::CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;
    conn.change_property8(PropMode::REPLACE, owner, AtomEnum::WM_NAME, AtomEnum::STRING, b"peek2")?;
    let cursor_font = conn.generate_id()?;
    conn.open_font(cursor_font, b"cursor")?;
    let max_chunk = (conn.maximum_request_bytes().saturating_sub(64)).clamp(4096, 256 * 1024);
    conn.flush()?;

    let x = Rc::new(X {
        conn: conn.clone(),
        root,
        screen,
        root_depth,
        root_visual,
        atoms,
        argb,
        owner,
        handlers: RefCell::default(),
        scale: Default::default(),
        root_handlers: RefCell::default(),
        hotkeys: RefCell::default(),
        keymap: RefCell::new((0, 1, Vec::new())),
        clip: RefCell::default(),
        cursors: RefCell::default(),
        cursor_font,
        max_chunk,
    });
    x.load_keymap();
    INSTANCE.with(|i| *i.borrow_mut() = Some(x.clone()));

    // The event reader. RustConnection is thread-safe: replies waited for
    // on the main thread and events read here don't get in each other's way.
    std::thread::spawn(move || loop {
        match conn.wait_for_event() {
            Ok(ev) => rt::send(rt::Msg::X(ev)),
            Err(e) => {
                eprintln!("peek2: X connection lost: {e}");
                rt::quit();
                break;
            }
        }
    });
    Ok(x)
}

fn event_window(ev: &Event) -> Option<Window> {
    Some(match ev {
        Event::Expose(e) => e.window,
        Event::ButtonPress(e) => e.event,
        Event::ButtonRelease(e) => e.event,
        Event::MotionNotify(e) => e.event,
        Event::KeyPress(e) => e.event,
        Event::KeyRelease(e) => e.event,
        Event::EnterNotify(e) => e.event,
        Event::LeaveNotify(e) => e.event,
        Event::FocusIn(e) => e.event,
        Event::FocusOut(e) => e.event,
        Event::ConfigureNotify(e) => e.window,
        Event::MapNotify(e) => e.window,
        Event::UnmapNotify(e) => e.window,
        Event::DestroyNotify(e) => e.window,
        Event::ClientMessage(e) => e.window,
        _ => return None,
    })
}

impl X {
    pub fn flush(&self) {
        let _ = self.conn.flush();
    }

    pub fn atom(&self, name: &str) -> u32 {
        intern(&self.conn, name).unwrap_or(0)
    }

    pub fn on(&self, win: Window, h: impl Fn(&Event) + 'static) {
        self.handlers.borrow_mut().insert(win, Rc::new(h));
    }

    pub fn on_root(&self, h: impl Fn(&Event) + 'static) {
        self.root_handlers.borrow_mut().push(Rc::new(h));
    }

    pub fn forget(&self, win: Window) {
        self.handlers.borrow_mut().remove(&win);
    }

    /// Route one event from the reader thread.
    pub fn dispatch(&self, ev: Event) {
        match &ev {
            Event::SelectionRequest(e) => return self.selection_request(e),
            Event::SelectionClear(e) if e.selection == self.atoms.clipboard => {
                let mut c = self.clip.borrow_mut();
                c.items.clear();
                return;
            }
            Event::PropertyNotify(e) if e.state == xproto::Property::DELETE => return self.incr_continue(e.window, e.atom),
            Event::MappingNotify(_) => return self.load_keymap(),
            Event::KeyPress(e) if e.event == self.root => {
                let action = self.hotkey_for(e.detail, e.state.into());
                if let Some(a) = action {
                    a();
                }
                return;
            }
            Event::Error(e) => {
                if std::env::var_os("PEEK2_DEBUG").is_some() {
                    eprintln!("peek2: X error: {e:?}");
                }
                return;
            }
            _ => {}
        }
        let win = event_window(&ev);
        if win == Some(self.root) {
            let hs: Vec<Handler> = self.root_handlers.borrow().clone();
            for h in hs {
                h(&ev);
            }
            return;
        }
        let h = win.and_then(|w| self.handlers.borrow().get(&w).cloned());
        if let Some(h) = h {
            h(&ev);
        }
    }

    // ---- windows -------------------------------------------------------

    /// A composite manager is required for see-through windows.
    pub fn compositor_running(&self) -> bool {
        let sel = self.atom(&format!("_NET_WM_CM_S{}", self.screen));
        self.conn.get_selection_owner(sel).ok().and_then(|c| c.reply().ok()).is_some_and(|r| r.owner != 0)
    }

    /// True when windows can be see-through (ARGB visual + compositor).
    pub fn can_blend(&self) -> bool {
        self.argb.is_some() && self.compositor_running()
    }

    /// An override-redirect window (the WM leaves it alone), ARGB when
    /// `argb` and available. Returns (window, depth).
    pub fn popup(&self, r: Rect, argb: bool, events: EventMask) -> Result<(Window, u8)> {
        let id = self.conn.generate_id()?;
        let mut aux = xproto::CreateWindowAux::new().override_redirect(1).event_mask(events).border_pixel(0);
        let (depth, visual) = match (argb, self.argb) {
            (true, Some((vis, cmap))) => {
                aux = aux.background_pixel(0).colormap(cmap);
                (32, vis)
            }
            _ => {
                aux = aux.background_pixel(0x0f1513);
                (self.root_depth, self.root_visual)
            }
        };
        self.conn.create_window(
            depth,
            id,
            self.root,
            r.x as i16,
            r.y as i16,
            r.width.max(1) as u16,
            r.height.max(1) as u16,
            0,
            xproto::WindowClass::INPUT_OUTPUT,
            visual,
            &aux,
        )?;
        self.conn.change_property8(PropMode::REPLACE, id, AtomEnum::WM_CLASS, AtomEnum::STRING, b"peek2\0Peek2\0")?;
        self.conn.change_property8(PropMode::REPLACE, id, self.atoms.net_wm_name, self.atoms.utf8, b"Peek2")?;
        Ok((id, depth))
    }

    /// A plain, solid-colour override-redirect window painted by the X
    /// server itself, used for recording borders.
    pub fn create_solid(&self, r: Rect, rgb: u32) -> Option<Window> {
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
        Some(id)
    }

    pub fn move_resize(&self, win: Window, r: Rect) {
        let aux = xproto::ConfigureWindowAux::new()
            .x(r.x)
            .y(r.y)
            .width(r.width.max(1) as u32)
            .height(r.height.max(1) as u32)
            .stack_mode(xproto::StackMode::ABOVE);
        let _ = self.conn.configure_window(win, &aux);
    }

    pub fn raise(&self, win: Window) {
        let _ = self.conn.configure_window(win, &xproto::ConfigureWindowAux::new().stack_mode(xproto::StackMode::ABOVE));
    }

    pub fn map(&self, win: Window) {
        let _ = self.conn.map_window(win);
    }

    pub fn set_mapped(&self, win: Window, mapped: bool) {
        let _ = if mapped { self.conn.map_window(win) } else { self.conn.unmap_window(win) };
    }

    pub fn destroy(&self, win: Window) {
        self.forget(win);
        let _ = self.conn.destroy_window(win);
    }

    /// Grab keyboard and pointer for a popup (menus, the selector). Fails
    /// while another client holds a grab, so callers retry briefly.
    pub fn grab_input(&self, win: Window, pointer: bool) -> bool {
        let kb = self
            .conn
            .grab_keyboard(true, win, x11rb::CURRENT_TIME, xproto::GrabMode::ASYNC, xproto::GrabMode::ASYNC)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|r| r.status == xproto::GrabStatus::SUCCESS);
        if !pointer {
            return kb;
        }
        let mask = EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION;
        let ptr = self
            .conn
            .grab_pointer(true, win, mask, xproto::GrabMode::ASYNC, xproto::GrabMode::ASYNC, x11rb::NONE, x11rb::NONE, x11rb::CURRENT_TIME)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|r| r.status == xproto::GrabStatus::SUCCESS);
        kb && ptr
    }

    pub fn ungrab(&self) {
        let _ = self.conn.ungrab_keyboard(x11rb::CURRENT_TIME);
        let _ = self.conn.ungrab_pointer(x11rb::CURRENT_TIME);
    }

    /// Cursor from the server's cursor font (X11/cursorfont.h shapes).
    pub fn set_cursor(&self, win: Window, shape: u16) {
        let id = self.cursor_id(shape);
        let _ = self.conn.change_window_attributes(win, &xproto::ChangeWindowAttributesAux::new().cursor(id));
    }

    pub fn cursor_id(&self, shape: u16) -> u32 {
        *self.cursors.borrow_mut().entry(shape).or_insert_with(|| {
            let Ok(id) = self.conn.generate_id() else { return 0 };
            let _ = self.conn.create_glyph_cursor(id, self.cursor_font, self.cursor_font, shape, shape + 1, 0, 0, 0, 0xffff, 0xffff, 0xffff);
            id
        })
    }

    pub fn pointer(&self) -> Option<(i32, i32)> {
        let r = self.conn.query_pointer(self.root).ok()?.reply().ok()?;
        Some((r.root_x as i32, r.root_y as i32))
    }

    pub fn root_rect(&self) -> Rect {
        let s = &self.conn.setup().roots[self.screen];
        Rect::new(0, 0, s.width_in_pixels as i32, s.height_in_pixels as i32)
    }

    /// Monitors in root pixels (RandR 1.5), else the whole screen.
    pub fn monitors(&self) -> Vec<Rect> {
        let mons: Vec<Rect> = self
            .conn
            .randr_get_monitors(self.root, true)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.monitors.iter().map(|m| Rect::new(m.x as i32, m.y as i32, m.width as i32, m.height as i32)).collect())
            .unwrap_or_default();
        if mons.is_empty() { vec![self.root_rect()] } else { mons }
    }

    /// The monitor under the pointer.
    pub fn current_monitor(&self) -> Rect {
        let mons = self.monitors();
        let p = self.pointer();
        mons.iter().find(|m| p.is_some_and(|(x, y)| m.contains(x, y))).or(mons.first()).copied().unwrap_or_else(|| self.root_rect())
    }

    /// UI scale from Xft.dpi (1 at 96 dpi, 2 at 192 …).
    /// UI scale from Xft.dpi (or PEEK2_SCALE), read once: it is asked for
    /// on every redraw and each read is a server round trip.
    pub fn ui_scale(&self) -> i32 {
        if let Some(s) = self.scale.get() {
            return s;
        }
        let s = self.read_ui_scale();
        self.scale.set(Some(s));
        s
    }

    fn read_ui_scale(&self) -> i32 {
        if let Some(s) = std::env::var("PEEK2_SCALE").ok().and_then(|s| s.parse::<i32>().ok()) {
            return s.clamp(1, 4);
        }
        let db = self
            .conn
            .get_property(false, self.root, AtomEnum::RESOURCE_MANAGER, AtomEnum::STRING, 0, 1 << 16)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| String::from_utf8_lossy(&r.value).into_owned())
            .unwrap_or_default();
        let dpi = db
            .lines()
            .find_map(|l| l.strip_prefix("Xft.dpi:"))
            .and_then(|v| v.trim().parse::<f64>().ok())
            .unwrap_or(96.0);
        ((dpi / 96.0).round() as i32).clamp(1, 4)
    }

    fn root_origin(&self, win: Window) -> Option<(i32, i32)> {
        let r = self.conn.translate_coordinates(win, self.root, 0, 0).ok()?.reply().ok()?;
        Some((r.dst_x as i32, r.dst_y as i32))
    }

    /// Visible top-level client windows in root coordinates, topmost first,
    /// frames included.
    pub fn client_rects(&self) -> Vec<Rect> {
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
            wins = self.conn.query_tree(self.root).ok().and_then(|c| c.reply().ok()).map(|t| t.children).unwrap_or_default();
        }
        let mut out = Vec::new();
        for &w in wins.iter().rev() {
            let Some(attrs) = self.conn.get_window_attributes(w).ok().and_then(|c| c.reply().ok()) else { continue };
            if attrs.map_state != xproto::MapState::VIEWABLE || attrs.override_redirect {
                continue;
            }
            let Some(geo) = self.conn.get_geometry(w).ok().and_then(|c| c.reply().ok()) else { continue };
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
            out.push(Rect::new(x - l, y - t, geo.width as i32 + l + r, geo.height as i32 + t + b));
        }
        out
    }

    /// Root window pixels as BGRX rows, if the server uses the usual
    /// 32-bit little-endian layout for its depth.
    pub fn screenshot(&self, r: Rect) -> Option<Vec<u8>> {
        self.get_image(self.root, r)
    }

    /// Pixels of `r` in a root-depth drawable, as BGRX rows.
    pub fn get_image(&self, drawable: u32, r: Rect) -> Option<Vec<u8>> {
        let setup = self.conn.setup();
        let bpp = setup.pixmap_formats.iter().find(|f| f.depth == self.root_depth).map(|f| f.bits_per_pixel);
        if bpp != Some(32) || setup.image_byte_order != xproto::ImageOrder::LSB_FIRST {
            return None;
        }
        let img = self
            .conn
            .get_image(xproto::ImageFormat::Z_PIXMAP, drawable, r.x as i16, r.y as i16, r.width as u16, r.height as u16, !0)
            .ok()?
            .reply()
            .ok()?;
        (img.data.len() == (r.width * r.height * 4) as usize).then_some(img.data)
    }

    /// PutImage in row bands that fit the request size limit.
    pub fn put_image(&self, drawable: u32, gc: u32, w: u16, h: u16, depth: u8, data: &[u8]) {
        self.put_image_at(drawable, gc, 0, 0, w, h, depth, data)
    }

    /// PutImage at (dx, dy), in row bands that fit the request size limit.
    #[allow(clippy::too_many_arguments)]
    pub fn put_image_at(&self, drawable: u32, gc: u32, dx: i32, dy: i32, w: u16, h: u16, depth: u8, data: &[u8]) {
        let stride = w as usize * 4;
        let rows = (self.max_chunk.max(stride) / stride).max(1);
        let mut y = 0usize;
        while y < h as usize {
            let n = rows.min(h as usize - y);
            let _ = self.conn.put_image(
                xproto::ImageFormat::Z_PIXMAP,
                drawable,
                gc,
                w,
                n as u16,
                dx as i16,
                (dy + y as i32) as i16,
                0,
                depth,
                &data[y * stride..(y + n) * stride],
            );
            y += n;
        }
    }

    // ---- keyboard ------------------------------------------------------

    fn load_keymap(&self) {
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        if let Some(map) = self.conn.get_keyboard_mapping(min, max - min + 1).ok().and_then(|c| c.reply().ok()) {
            *self.keymap.borrow_mut() = (min, (map.keysyms_per_keycode as usize).max(1), map.keysyms);
        }
    }

    /// Keysym for a key event (shift picks the second column).
    pub fn keysym(&self, code: u8, state: u16) -> u32 {
        let km = self.keymap.borrow();
        let (min, per, syms) = (&km.0, km.1, &km.2);
        let base = (code.saturating_sub(*min)) as usize * per;
        let shift = state & u16::from(ModMask::SHIFT) != 0;
        let col = if shift && per > 1 && syms.get(base + 1).is_some_and(|s| *s != 0) { 1 } else { 0 };
        syms.get(base + col).copied().unwrap_or(0)
    }

    fn keycodes(&self, keysym: u32) -> Vec<u8> {
        let km = self.keymap.borrow();
        let (min, per, syms) = (km.0, km.1, &km.2);
        let mut codes: Vec<u8> = syms.chunks(per).enumerate().filter(|(_, s)| s.contains(&keysym)).map(|(i, _)| min + i as u8).collect();
        codes.dedup();
        codes
    }

    /// Grab a global hotkey given in GTK accelerator syntax (`<Ctrl><Alt>R`).
    pub fn grab_hotkey(&self, accel: &str, action: impl Fn() + 'static) -> Result<()> {
        let (mods, keysym) = parse_accel(accel).with_context(|| format!("invalid hotkey {accel:?}"))?;
        let codes = self.keycodes(keysym);
        anyhow::ensure!(!codes.is_empty(), "no key produces {accel}");
        // Also grab with NumLock (M2) and CapsLock (LOCK) on.
        let extras = [0u16, u16::from(ModMask::M2), u16::from(ModMask::LOCK), u16::from(ModMask::M2 | ModMask::LOCK)];
        for &code in &codes {
            for &extra in &extras {
                self.conn
                    .grab_key(true, self.root, ModMask::from(mods | extra), code, xproto::GrabMode::ASYNC, xproto::GrabMode::ASYNC)?
                    .check()
                    .with_context(|| format!("{accel} is already grabbed by another program"))?;
            }
        }
        self.hotkeys.borrow_mut().push(Hotkey { codes, mods, action: Rc::new(action) });
        Ok(())
    }

    fn hotkey_for(&self, code: u8, state: u16) -> Option<Rc<dyn Fn()>> {
        let relevant = u16::from(ModMask::CONTROL | ModMask::SHIFT | ModMask::M1 | ModMask::M4);
        self.hotkeys.borrow().iter().find(|h| h.codes.contains(&code) && state & relevant == h.mods).map(|h| h.action.clone())
    }

    /// Is this key event one of our global hotkeys? (While a popup holds the
    /// keyboard grab, hotkeys arrive there instead of on the root.)
    pub fn is_hotkey(&self, code: u8, state: u16) -> Option<Rc<dyn Fn()>> {
        self.hotkey_for(code, state)
    }

    // ---- clipboard -----------------------------------------------------

    /// Own CLIPBOARD with these (mime/target name, data) flavours.
    pub fn set_clipboard(&self, items: Vec<(&str, Vec<u8>)>) {
        let items: Vec<(u32, Rc<Vec<u8>>)> = items.into_iter().map(|(t, d)| (self.atom(t), Rc::new(d))).collect();
        {
            let mut c = self.clip.borrow_mut();
            c.items = items;
            c.incr.clear();
        }
        let _ = self.conn.set_selection_owner(self.owner, self.atoms.clipboard, x11rb::CURRENT_TIME);
        let owner = self.conn.get_selection_owner(self.atoms.clipboard).ok().and_then(|c| c.reply().ok()).map(|r| r.owner);
        if owner != Some(self.owner) {
            eprintln!("peek2: could not take the clipboard");
        }
    }

    fn selection_request(&self, e: &xproto::SelectionRequestEvent) {
        let a = &self.atoms;
        // Obsolete clients pass no property: use the target.
        let property = if e.property == x11rb::NONE { e.target } else { e.property };
        let mut ok = e.selection == a.clipboard;
        if ok {
            let c = self.clip.borrow();
            if c.items.is_empty() {
                ok = false;
            } else if e.target == a.targets {
                let mut list = vec![a.targets, a.timestamp];
                list.extend(c.items.iter().map(|(t, _)| *t));
                let _ = self.conn.change_property32(PropMode::REPLACE, e.requestor, property, AtomEnum::ATOM, &list);
            } else if e.target == a.timestamp {
                let _ = self.conn.change_property32(PropMode::REPLACE, e.requestor, property, AtomEnum::INTEGER, &[e.time]);
            } else if let Some((_, data)) = c.items.iter().find(|(t, _)| *t == e.target) {
                if data.len() <= self.max_chunk {
                    let _ = self.conn.change_property8(PropMode::REPLACE, e.requestor, property, e.target, data);
                } else {
                    // INCR: announce the size, then send chunks each time the
                    // requestor deletes the property.
                    let data = data.clone();
                    drop(c);
                    let _ = self.conn.change_window_attributes(
                        e.requestor,
                        &xproto::ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
                    );
                    let _ = self.conn.change_property32(PropMode::REPLACE, e.requestor, property, a.incr, &[data.len() as u32]);
                    let mut c = self.clip.borrow_mut();
                    c.incr.retain(|t| !(t.requestor == e.requestor && t.property == property));
                    c.incr.push(Incr { requestor: e.requestor, property, target: e.target, data, offset: 0 });
                }
            } else {
                ok = false;
            }
        }
        let notify = xproto::SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: e.time,
            requestor: e.requestor,
            selection: e.selection,
            target: e.target,
            property: if ok { property } else { x11rb::NONE },
        };
        let _ = self.conn.send_event(false, e.requestor, EventMask::NO_EVENT, notify);
    }

    fn incr_continue(&self, window: Window, property: u32) {
        let mut c = self.clip.borrow_mut();
        let Some(i) = c.incr.iter().position(|t| t.requestor == window && t.property == property) else { return };
        let t = &mut c.incr[i];
        let end = (t.offset + self.max_chunk).min(t.data.len());
        // A zero-length chunk ends the transfer.
        let _ = self.conn.change_property8(PropMode::REPLACE, t.requestor, t.property, t.target, &t.data[t.offset..end]);
        if t.offset == end {
            c.incr.remove(i);
        } else {
            t.offset = end;
        }
    }
}

/// `<Ctrl><Alt>R` → (modifier mask, keysym).
pub fn parse_accel(accel: &str) -> Option<(u16, u32)> {
    let mut mods = 0u16;
    let mut rest = accel.trim();
    while let Some(r) = rest.strip_prefix('<') {
        let (m, after) = r.split_once('>')?;
        mods |= match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "primary" => u16::from(ModMask::CONTROL),
            "alt" | "mod1" | "meta" => u16::from(ModMask::M1),
            "shift" => u16::from(ModMask::SHIFT),
            "super" | "mod4" | "hyper" => u16::from(ModMask::M4),
            _ => return None,
        };
        rest = after;
    }
    Some((mods, keysym_from_name(rest)?))
}

fn keysym_from_name(name: &str) -> Option<u32> {
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        // Latin-1 keysyms equal their code point; letters are grabbed lower case.
        let c = c.to_ascii_lowercase();
        return (c as u32 >= 0x20 && (c as u32) < 0x100).then_some(c as u32);
    }
    if let Some(n) = name.strip_prefix(['F', 'f']).and_then(|n| n.parse::<u32>().ok()) {
        return (1..=35).contains(&n).then(|| 0xffbe + n - 1);
    }
    let table: &[(&str, u32)] = &[
        ("space", 0x20),
        ("Return", 0xff0d),
        ("Escape", 0xff1b),
        ("Tab", 0xff09),
        ("BackSpace", 0xff08),
        ("Delete", 0xffff),
        ("Insert", 0xff63),
        ("Home", 0xff50),
        ("End", 0xff57),
        ("Page_Up", 0xff55),
        ("Page_Down", 0xff56),
        ("Left", 0xff51),
        ("Up", 0xff52),
        ("Right", 0xff53),
        ("Down", 0xff54),
        ("Print", 0xff61),
        ("Pause", 0xff13),
        ("Scroll_Lock", 0xff14),
        ("Menu", 0xff67),
    ];
    table.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, k)| *k)
}

pub mod keysym {
    pub const ESCAPE: u32 = 0xff1b;
    pub const DELETE: u32 = 0xffff;
    pub const BACKSPACE: u32 = 0xff08;
    pub const RETURN: u32 = 0xff0d;
    pub const KP_ENTER: u32 = 0xff8d;
    pub const SPACE: u32 = 0x20;
    pub const LEFT: u32 = 0xff51;
    pub const UP: u32 = 0xff52;
    pub const RIGHT: u32 = 0xff53;
    pub const DOWN: u32 = 0xff54;
}

/// Shapes from X11/cursorfont.h.
pub mod cursor {
    pub const CROSSHAIR: u16 = 34;
    pub const FLEUR: u16 = 52;
    pub const LEFT_PTR: u16 = 68;
    pub const PENCIL: u16 = 86;
    pub const H_DOUBLE: u16 = 108;
    pub const V_DOUBLE: u16 = 116;
    pub const TOP_LEFT: u16 = 134;
    pub const TOP_RIGHT: u16 = 136;
    pub const BOTTOM_LEFT: u16 = 12;
    pub const BOTTOM_RIGHT: u16 = 14;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accels() {
        let ctrl_alt = u16::from(ModMask::CONTROL | ModMask::M1);
        assert_eq!(parse_accel("<Ctrl><Alt>R"), Some((ctrl_alt, 'r' as u32)));
        assert_eq!(parse_accel("<Super>F9"), Some((u16::from(ModMask::M4), 0xffc6)));
        assert_eq!(parse_accel("<Shift>Print"), Some((u16::from(ModMask::SHIFT), 0xff61)));
        assert_eq!(parse_accel("<Bogus>R"), None);
    }
}
