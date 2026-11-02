//! System tray icon (freedesktop XEmbed tray, as polybar's `internal/tray`,
//! stalonetray, trayer… implement it): a plain X window docked by hand and
//! painted pixel by pixel: a record ring, with a dot inside while busy.
//!
//! Left click starts a selection or stops the recording, middle saves the
//! replay buffer (or opens the recordings folder), right opens the menu.

use crate::x11::{self, X};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _, EventMask, PropMode};
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as _;

const SYSTEM_TRAY_REQUEST_DOCK: u32 = 0;

struct Tray {
    x: Rc<X>,
    selection: u32,
    opcode: u32,
    visual_atom: u32,
    manager: u32,
    xembed_info: u32,
    win: Cell<u32>,
    gc: Cell<u32>,
    depth: Cell<u8>,
    size: Cell<(u16, u16)>,
    state: RefCell<String>,
    on_click: Rc<dyn Fn(u8, &str)>,
}

thread_local! {
    static TRAY: RefCell<Option<Rc<Tray>>> = const { RefCell::new(None) };
}

/// Dock the icon and keep it docked across tray (bar) restarts.
/// `on_click(button, state)` handles clicks.
pub fn start(on_click: impl Fn(u8, &str) + 'static) {
    let x = x11::get();
    let atom = |n: &str| x.atom(n);
    let tray = Rc::new(Tray {
        selection: atom(&format!("_NET_SYSTEM_TRAY_S{}", x.screen)),
        opcode: atom("_NET_SYSTEM_TRAY_OPCODE"),
        visual_atom: atom("_NET_SYSTEM_TRAY_VISUAL"),
        manager: atom("MANAGER"),
        xembed_info: atom("_XEMBED_INFO"),
        x: x.clone(),
        win: Cell::new(0),
        gc: Cell::new(0),
        depth: Cell::new(0),
        size: Cell::new((20, 20)),
        state: RefCell::new("idle".into()),
        on_click: Rc::new(on_click),
    });
    let weak = Rc::downgrade(&tray);
    x.on_root(move |ev| {
        if let (Some(t), Event::ClientMessage(e)) = (weak.upgrade(), ev) {
            // A (re)started tray announces itself with MANAGER on the root.
            if e.type_ == t.manager && e.data.as_data32()[1] == t.selection {
                t.dock();
            }
        }
    });
    tray.dock();
    TRAY.with(|t| *t.borrow_mut() = Some(tray));
}

/// Called with every status change.
pub fn set_state(state: &str) {
    if let Some(t) = TRAY.with(|t| t.borrow().clone())
        && *t.state.borrow() != state {
            *t.state.borrow_mut() = state.to_string();
            t.draw();
        }
}

impl Tray {
    /// Visual the tray asks icons to use; 32-bit ARGB gives a transparent icon.
    fn tray_visual(&self, owner: u32) -> (u32, u8) {
        let c = &self.x.conn;
        let id = c
            .get_property(false, owner, self.visual_atom, xproto::AtomEnum::VISUALID, 0, 1)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().and_then(|mut v| v.next()));
        let depth = id.and_then(|id| {
            c.setup().roots.iter().flat_map(|s| &s.allowed_depths).find_map(|d| d.visuals.iter().any(|v| v.visual_id == id).then_some(d.depth))
        });
        match (id, depth) {
            (Some(id), Some(d)) => (id, d),
            _ => (self.x.root_visual, self.x.root_depth),
        }
    }

    fn dock(self: &Rc<Self>) {
        if let Err(e) = self.try_dock() {
            eprintln!("peek2: tray: {e:#}");
        }
    }

    fn try_dock(self: &Rc<Self>) -> anyhow::Result<()> {
        let c = &self.x.conn;
        let owner = c.get_selection_owner(self.selection)?.reply()?.owner;
        if owner == x11rb::NONE {
            return Ok(()); // no tray yet; MANAGER will tell us
        }
        if self.win.get() != 0 {
            let _ = c.free_gc(self.gc.get());
            self.x.destroy(self.win.get());
        }
        let (visual, depth) = self.tray_visual(owner);
        let (w, h) = self.size.get();
        let win = c.generate_id()?;
        let mut aux = xproto::CreateWindowAux::new()
            .event_mask(EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY | EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE);
        if depth == 32 {
            let cmap = c.generate_id()?;
            c.create_colormap(xproto::ColormapAlloc::NONE, cmap, self.x.root, visual)?;
            aux = aux.background_pixel(0).border_pixel(0).colormap(cmap);
        } else {
            aux = aux.background_pixmap(xproto::BackPixmap::PARENT_RELATIVE);
        }
        c.create_window(depth, win, self.x.root, 0, 0, w, h, 0, xproto::WindowClass::INPUT_OUTPUT, visual, &aux)?;
        c.change_property32(PropMode::REPLACE, win, self.xembed_info, self.xembed_info, &[0, 1])?;
        c.change_property8(PropMode::REPLACE, win, self.x.atoms.net_wm_name, self.x.atoms.utf8, b"peek2")?;
        c.change_property8(PropMode::REPLACE, win, xproto::AtomEnum::WM_CLASS, xproto::AtomEnum::STRING, b"peek2\0Peek2\0")?;
        let gc = c.generate_id()?;
        c.create_gc(gc, win, &xproto::CreateGCAux::new())?;
        self.win.set(win);
        self.gc.set(gc);
        self.depth.set(depth);
        let weak = Rc::downgrade(self);
        self.x.on(win, move |ev| {
            if let Some(t) = weak.upgrade() {
                t.event(ev);
            }
        });
        let msg = xproto::ClientMessageEvent::new(32, owner, self.opcode, [x11rb::CURRENT_TIME, SYSTEM_TRAY_REQUEST_DOCK, win, 0, 0]);
        c.send_event(false, owner, EventMask::NO_EVENT, msg)?;
        Ok(())
    }

    fn event(&self, ev: &Event) {
        match ev {
            Event::Expose(e) if e.count == 0 => self.draw(),
            Event::ConfigureNotify(e) => {
                if (e.width, e.height) != self.size.get() {
                    self.size.set((e.width, e.height));
                    self.draw();
                }
            }
            Event::ButtonRelease(e) => {
                let state = self.state.borrow().clone();
                (self.on_click)(e.detail, &state);
            }
            _ => {}
        }
    }

    fn draw(&self) {
        let win = self.win.get();
        if win == 0 {
            return;
        }
        let (w, h) = self.size.get();
        let c = &self.x.conn;
        // Without an ARGB visual, draw over whatever the bar shows behind us:
        // let the server paint the parent-relative background, then read it.
        let backdrop = if self.depth.get() == 32 {
            None
        } else {
            let _ = c.clear_area(false, win, 0, 0, 0, 0);
            c.get_image(xproto::ImageFormat::Z_PIXMAP, win, 0, 0, w, h, !0)
                .ok()
                .and_then(|c| c.reply().ok())
                .filter(|r| r.data.len() == w as usize * h as usize * 4)
                .map(|r| r.data)
        };
        let data = render(&self.state.borrow(), w as usize, h as usize, backdrop);
        self.x.put_image(win, self.gc.get(), w, h, self.depth.get(), &data);
    }
}

/// Paint the icon: BGRA (premultiplied) as X wants it for 24/32-bit
/// visuals, antialiased by pixel coverage.
fn render(state: &str, w: usize, h: usize, backdrop: Option<Vec<u8>>) -> Vec<u8> {
    // Cream ring when idle, gold while busy, green once saved; recording
    // fills the ring with a red dot. (ring, dot colour, big dot)
    let (ring, dot, big) = match state {
        "recording" => (0xc73e3a, Some(0xc73e3a), true),
        "paused" => (0xc73e3a, None, false),
        "countdown" | "processing" => (0xd9a441, Some(0xd9a441), true),
        "saved" => (0x8fbf73, Some(0x8fbf73), true),
        // Replay buffer running: idle, with a red pilot light.
        "replay" => (0xded5c4, Some(0xc73e3a), false),
        _ => (0xded5c4, Some(0xded5c4), false),
    };
    let opaque = backdrop.is_some();
    let mut out = backdrop.unwrap_or_else(|| vec![0u8; w * h * 4]);
    let side = w.min(h) as f64;
    let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
    let ring_r = side * 0.42 - 0.75;
    let dot_r = side * if big { 0.25 } else { 0.14 };
    for y in 0..h {
        for x in 0..w {
            // 4×4 supersampling.
            let (mut ring_cov, mut dot_cov) = (0.0, 0.0);
            for sy in 0..4 {
                for sx in 0..4 {
                    let px = x as f64 + (sx as f64 + 0.5) / 4.0 - cx;
                    let py = y as f64 + (sy as f64 + 0.5) / 4.0 - cy;
                    let d = (px * px + py * py).sqrt();
                    if (d - ring_r).abs() <= 0.75 {
                        ring_cov += 1.0 / 16.0;
                    }
                    if dot.is_some() && d <= dot_r {
                        dot_cov += 1.0 / 16.0;
                    }
                }
            }
            let i = (y * w + x) * 4;
            for (cov, rgb) in [(ring_cov, ring), (dot_cov, dot.unwrap_or(0))] {
                if cov <= 0.0 {
                    continue;
                }
                let src = [(rgb & 0xff) as f64, ((rgb >> 8) & 0xff) as f64, ((rgb >> 16) & 0xff) as f64];
                for k in 0..3 {
                    out[i + k] = (src[k] * cov + out[i + k] as f64 * (1.0 - cov)).round() as u8;
                }
                out[i + 3] = if opaque { 0xff } else { (255.0 * cov + out[i + 3] as f64 * (1.0 - cov)).round() as u8 };
            }
        }
    }
    out
}
