//! Drawing on a recording while it runs. The marks live in an ordinary
//! window over the captured area, so they end up in the clip. The window
//! is shaped (X Shape extension) to exactly the pixels of the marks, so
//! it needs no compositor and the screen under it stays live. While
//! drawing, pointer and keyboard are grabbed; otherwise the window takes
//! no input at all and you use the screen as usual.

use crate::annotate::{self, Change, Ink, Marks, Tool, COLORS};
use crate::recorder::Area;
use crate::x11::{self, cursor, keysym, X};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::wrapper::ConnectionExt as _;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _, EventMask, ModMask};
use x11rb::protocol::Event;

/// Tools that make sense on live video (blur would need the video itself).
pub const TOOLS: [Tool; 4] = [Tool::Pen, Tool::Arrow, Tool::Box, Tool::Marker];

pub struct LiveInk {
    x: Rc<X>,
    win: u32,
    w: i32,
    h: i32,
    /// Colour content (root depth) and the 1-bit shape of the marks.
    pix: u32,
    mask: u32,
    gc: u32,
    mgc: u32,
    stipple: u32,
    pub marks: RefCell<Marks>,
    pub tool: Cell<Tool>,
    pub color: Cell<usize>,
    pub size: Cell<i32>,
    active: Cell<bool>,
    /// Called after anything visible to the control bar changes.
    on_change: RefCell<Option<Box<dyn Fn()>>>,
}

impl LiveInk {
    pub fn new(area: Area) -> Option<Rc<LiveInk>> {
        let x = x11::get();
        let c = &x.conn;
        c.extension_information(shape::X11_EXTENSION_NAME).ok()??;
        let (w, h) = (area.width, area.height);
        let win = c.generate_id().ok()?;
        let events = EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION | EventMask::KEY_PRESS;
        let aux = xproto::CreateWindowAux::new().override_redirect(1).event_mask(events).background_pixmap(x11rb::NONE);
        c.create_window(x11rb::COPY_DEPTH_FROM_PARENT, win, x.root, area.x as i16, area.y as i16, w as u16, h as u16, 0, xproto::WindowClass::INPUT_OUTPUT, x11rb::COPY_FROM_PARENT, &aux).ok()?;
        let _ = c.change_property8(xproto::PropMode::REPLACE, win, xproto::AtomEnum::WM_CLASS, xproto::AtomEnum::STRING, b"peek2-ink\0Peek2\0");
        let pix = c.generate_id().ok()?;
        c.create_pixmap(x.root_depth, pix, win, w as u16, h as u16).ok()?;
        let mask = c.generate_id().ok()?;
        c.create_pixmap(1, mask, win, w as u16, h as u16).ok()?;
        let gc = annotate::mark_gc(&x, pix)?;
        let mgc = annotate::mark_gc(&x, mask)?;
        // 50% checkerboard: the marker lets the video show through.
        let stipple = c.generate_id().ok()?;
        c.create_pixmap(1, stipple, win, 2, 2).ok()?;
        let sgc = c.generate_id().ok()?;
        c.create_gc(sgc, stipple, &xproto::CreateGCAux::new().foreground(0)).ok()?;
        let _ = c.poly_fill_rectangle(stipple, sgc, &[xproto::Rectangle { x: 0, y: 0, width: 2, height: 2 }]);
        let _ = c.change_gc(sgc, &xproto::ChangeGCAux::new().foreground(1));
        let _ = c.poly_point(xproto::CoordMode::ORIGIN, stipple, sgc, &[xproto::Point { x: 0, y: 0 }, xproto::Point { x: 1, y: 1 }]);
        let _ = c.free_gc(sgc);
        // Never takes input by itself: clicks fall through to the screen.
        let _ = c.shape_rectangles(shape::SO::SET, shape::SK::INPUT, xproto::ClipOrdering::UNSORTED, win, 0, 0, &[]);
        let this = Rc::new(LiveInk {
            x: x.clone(),
            win,
            w,
            h,
            pix,
            mask,
            gc,
            mgc,
            stipple,
            marks: RefCell::new(Marks::default()),
            tool: Cell::new(Tool::Pen),
            color: Cell::new(0),
            size: Cell::new(1),
            active: Cell::new(false),
            on_change: RefCell::new(None),
        });
        this.render();
        x.map(win);
        let weak = Rc::downgrade(&this);
        x.on(win, move |ev| {
            if let Some(t) = weak.upgrade() {
                t.event(ev);
            }
        });
        Some(this)
    }

    pub fn on_change(&self, f: impl Fn() + 'static) {
        *self.on_change.borrow_mut() = Some(Box::new(f));
    }

    fn changed(&self) {
        if let Some(f) = self.on_change.borrow().as_ref() {
            f();
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.get()
    }

    /// Start or stop drawing. While drawing the pointer and keyboard are
    /// grabbed (our own control bar still gets its clicks).
    pub fn set_active(self: &Rc<Self>, on: bool) {
        if on == self.active.get() {
            return;
        }
        if on {
            self.x.raise(self.win);
            self.x.set_cursor(self.win, cursor::PENCIL);
            if !self.x.grab_input(self.win, true) {
                // Another client holds a grab (a menu, a game): retry briefly.
                let weak = Rc::downgrade(self);
                let tries = Cell::new(0);
                crate::rt::timeout(std::time::Duration::from_millis(30), move || {
                    let Some(t) = weak.upgrade() else { return false };
                    tries.set(tries.get() + 1);
                    if !t.active.get() || t.x.grab_input(t.win, true) {
                        return false;
                    }
                    if tries.get() > 30 {
                        t.active.set(false);
                        t.changed();
                        crate::util::notify_error("Drawing: could not grab the pointer");
                        return false;
                    }
                    true
                });
            }
            // The cursor shows over the whole screen while grabbed.
            let _ = self.x.conn.change_active_pointer_grab(self.x.cursor_id(cursor::PENCIL), x11rb::CURRENT_TIME, EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION);
        } else {
            self.marks.borrow_mut().end();
            self.x.ungrab();
            self.render();
        }
        self.active.set(on);
        self.changed();
    }

    pub fn edit(&self, f: fn(&mut Marks) -> bool) {
        if f(&mut self.marks.borrow_mut()) {
            self.render();
            self.changed();
        }
    }

    pub fn cycle_color(&self) {
        self.color.set((self.color.get() + 1) % COLORS.len());
        self.changed();
    }

    pub fn set_tool(self: &Rc<Self>, t: Tool) {
        self.tool.set(t);
        self.set_active(true);
        self.changed();
    }

    fn event(self: &Rc<Self>, ev: &Event) {
        match ev {
            Event::Expose(e) if e.count == 0 => self.present(),
            Event::ButtonPress(e) if self.active.get() => {
                let (x, y) = (e.event_x as i32, e.event_y as i32);
                match e.detail {
                    1 if x >= 0 && y >= 0 && x < self.w && y < self.h => {
                        let rgb = COLORS[self.color.get()].1;
                        self.marks.borrow_mut().begin(self.tool.get(), rgb, self.size.get(), x, y);
                        self.render();
                    }
                    // A click outside the recording puts the pen down.
                    1 | 3 => self.set_active(false),
                    4 | 5 => {
                        self.size.set((self.size.get() + if e.detail == 4 { 1 } else { -1 }).clamp(1, 6));
                        self.changed();
                    }
                    _ => {}
                }
            }
            Event::MotionNotify(e) if self.active.get() => {
                let (x, y) = (e.event_x as i32, e.event_y as i32);
                let change = self.marks.borrow_mut().extend(x.clamp(0, self.w - 1), y.clamp(0, self.h - 1));
                match change {
                    Change::Nothing => {}
                    Change::Segment(a, b) => {
                        if let Some(m) = self.marks.borrow().current() {
                            annotate::paint_segment(&self.x, self.pix, self.gc, m, a, b, false);
                            annotate::paint_segment(&self.x, self.mask, self.mgc, m, a, b, true);
                        }
                        self.apply_shape();
                    }
                    Change::All => self.render(),
                }
            }
            Event::ButtonRelease(e) if e.detail == 1 && self.marks.borrow().drawing() => {
                self.marks.borrow_mut().end();
                self.render();
                self.changed();
            }
            Event::KeyPress(e) => self.key(e.detail, e.state.into()),
            _ => {}
        }
    }

    fn key(self: &Rc<Self>, code: u8, state: u16) {
        // The keyboard grab hides our global hotkeys; run them from here.
        if let Some(action) = self.x.is_hotkey(code, state) {
            return action();
        }
        let sym = self.x.keysym(code, 0);
        let ctrl = state & u16::from(ModMask::CONTROL) != 0;
        let shift = state & u16::from(ModMask::SHIFT) != 0;
        if ctrl && (sym == 'z' as u32 || sym == 'y' as u32) {
            let redo = sym == 'y' as u32 || shift;
            return self.edit(if redo { Marks::redo } else { Marks::undo });
        }
        match sym {
            keysym::ESCAPE | keysym::RETURN => self.set_active(false),
            keysym::DELETE | keysym::BACKSPACE => self.edit(Marks::clear),
            s if s == 'c' as u32 => self.cycle_color(),
            s => {
                if let Some(t) = TOOLS.into_iter().find(|t| s == t.key() as u32) {
                    self.set_tool(t);
                }
            }
        }
    }

    // ---- rasterising -------------------------------------------------------

    fn render(&self) {
        let c = &self.x.conn;
        let full = xproto::Rectangle { x: 0, y: 0, width: self.w as u16, height: self.h as u16 };
        let _ = c.change_gc(self.mgc, &xproto::ChangeGCAux::new().foreground(0));
        let _ = c.poly_fill_rectangle(self.mask, self.mgc, &[full]);
        for m in self.marks.borrow().all() {
            annotate::paint(&self.x, self.mask, self.mgc, m, Ink::Mask { stipple: self.stipple });
            annotate::paint(&self.x, self.pix, self.gc, m, Ink::Solid);
        }
        self.apply_shape();
    }

    fn apply_shape(&self) {
        let _ = self.x.conn.shape_mask(shape::SO::SET, shape::SK::BOUNDING, self.win, 0, 0, self.mask);
        self.present();
    }

    fn present(&self) {
        let _ = self.x.conn.copy_area(self.pix, self.win, self.gc, 0, 0, 0, 0, self.w as u16, self.h as u16);
        let _ = self.x.conn.flush();
    }

    pub fn close(&self) {
        if self.active.replace(false) {
            self.x.ungrab();
        }
        self.x.destroy(self.win);
        for p in [self.pix, self.mask, self.stipple] {
            let _ = self.x.conn.free_pixmap(p);
        }
        let _ = self.x.conn.free_gc(self.gc);
        let _ = self.x.conn.free_gc(self.mgc);
        self.x.flush();
    }
}
