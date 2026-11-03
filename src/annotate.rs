//! Drawing on a screenshot before it is saved: pen, arrow, box, marker and
//! blur (pixelate), Flameshot-style. Marks are rasterised by the X server
//! into a canvas pixmap (the frozen frame plus every mark), which the
//! selector shows inside the selection and the screenshot is read from, so
//! the PNG is exactly what was on screen.

use crate::x11::{Rect, X};
use std::rc::Rc;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, CapStyle, ConnectionExt as _, CoordMode, FillStyle, JoinStyle, Point, PolyShape, GX};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    Pen,
    Arrow,
    Box,
    Marker,
    Blur,
}

impl Tool {
    pub const ALL: [Tool; 5] = [Tool::Pen, Tool::Arrow, Tool::Box, Tool::Marker, Tool::Blur];

    pub fn id(self) -> &'static str {
        match self {
            Tool::Pen => "pen",
            Tool::Arrow => "arrow",
            Tool::Box => "box",
            Tool::Marker => "marker",
            Tool::Blur => "blur",
        }
    }

    pub fn from_id(id: &str) -> Option<Tool> {
        Tool::ALL.into_iter().find(|t| t.id() == id)
    }

    /// Keyboard shortcut.
    pub fn key(self) -> char {
        match self {
            Tool::Pen => 'p',
            Tool::Arrow => 'a',
            Tool::Box => 'r',
            Tool::Marker => 'm',
            Tool::Blur => 'b',
        }
    }

    /// Default stroke width (pixels) at size step 1.
    fn base_width(self) -> i32 {
        match self {
            Tool::Pen | Tool::Arrow | Tool::Box => 3,
            Tool::Marker => 16,
            Tool::Blur => 12, // block size
        }
    }

    /// Freehand tools collect every pointer position; others two corners.
    pub fn freehand(self) -> bool {
        matches!(self, Tool::Pen | Tool::Marker)
    }
}

pub const COLORS: [(&str, u32); 6] =
    [("red", 0xe5322d), ("yellow", 0xffd400), ("green", 0x2fbf5b), ("blue", 0x2f7ff0), ("white", 0xffffff), ("black", 0x101010)];

#[derive(Clone, Debug)]
pub struct Mark {
    tool: Tool,
    rgb: u32,
    width: i32,
    pts: Vec<(i32, i32)>,
}

impl Mark {
    /// Worth keeping: a dot for freehand, some extent otherwise.
    fn is_visible(&self) -> bool {
        match (self.pts.first(), self.pts.last()) {
            (Some(a), Some(b)) => self.tool.freehand() || (a.0 - b.0).abs() + (a.1 - b.1).abs() >= 3,
            _ => false,
        }
    }
}

/// What changed after a pointer move.
pub enum Change {
    Nothing,
    /// A pen segment was added: it can be drawn on top.
    Segment((i32, i32), (i32, i32)),
    /// Everything has to be redrawn.
    All,
}

/// The marks of one drawing, with undo and redo.
#[derive(Default)]
pub struct Marks {
    done: Vec<Mark>,
    undone: Vec<Mark>,
    current: Option<Mark>,
}

impl Marks {
    pub fn is_empty(&self) -> bool {
        self.done.is_empty() && self.current.is_none()
    }

    pub fn can_undo(&self) -> bool {
        !self.done.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.undone.is_empty()
    }

    pub fn drawing(&self) -> bool {
        self.current.is_some()
    }

    pub fn current(&self) -> Option<&Mark> {
        self.current.as_ref()
    }

    /// Every mark, oldest first, including the one being drawn.
    pub fn all(&self) -> impl Iterator<Item = &Mark> {
        self.done.iter().chain(self.current.as_ref())
    }

    pub fn begin(&mut self, tool: Tool, rgb: u32, size: i32, x: i32, y: i32) {
        let width = tool.base_width() * size.max(1);
        self.current = Some(Mark { tool, rgb, width, pts: vec![(x, y)] });
    }

    pub fn extend(&mut self, x: i32, y: i32) -> Change {
        let Some(m) = self.current.as_mut() else { return Change::Nothing };
        if m.tool.freehand() {
            let &last = m.pts.last().unwrap();
            if last == (x, y) {
                return Change::Nothing;
            }
            m.pts.push((x, y));
            // Marker strokes are drawn as one polyline (AND/stipple must
            // not be applied twice where segments meet): redraw it whole.
            if m.tool == Tool::Pen { Change::Segment(last, (x, y)) } else { Change::All }
        } else {
            m.pts.truncate(1);
            m.pts.push((x, y));
            Change::All
        }
    }

    /// Finish the current mark; a new mark clears the redo history.
    pub fn end(&mut self) {
        if let Some(m) = self.current.take()
            && m.is_visible() {
                self.done.push(m);
                self.undone.clear();
            }
    }

    pub fn undo(&mut self) -> bool {
        self.current = None;
        match self.done.pop() {
            Some(m) => {
                self.undone.push(m);
                true
            }
            None => false,
        }
    }

    pub fn redo(&mut self) -> bool {
        match self.undone.pop() {
            Some(m) => {
                self.done.push(m);
                true
            }
            None => false,
        }
    }

    /// Remove everything (undoable mark by mark with redo).
    pub fn clear(&mut self) -> bool {
        self.current = None;
        if self.done.is_empty() {
            return false;
        }
        while let Some(m) = self.done.pop() {
            self.undone.push(m);
        }
        true
    }
}

/// How a mark is rasterised.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Ink {
    /// On a picture: the marker tints (AND) what is under it.
    Picture,
    /// On a shaped window's content: plain colour, the shape does the rest.
    Solid,
    /// Into a 1-bit shape mask: the marker is stippled so what is under it
    /// shows through.
    Mask { stipple: u32 },
}

/// Draw mark `m` (not blur) into drawable `d` with `gc`.
pub fn paint(x: &X, d: u32, gc: u32, m: &Mark, ink: Ink) {
    let c = &x.conn;
    let fg = if matches!(ink, Ink::Mask { .. }) { 1 } else { m.rgb & 0xffffff };
    let function = if m.tool == Tool::Marker && ink == Ink::Picture { GX::AND } else { GX::COPY };
    let mut aux = xproto::ChangeGCAux::new().foreground(fg).line_width(m.width.max(1) as u32).function(function).fill_style(FillStyle::SOLID);
    if let (Tool::Marker, Ink::Mask { stipple }) = (m.tool, ink) {
        aux = aux.fill_style(FillStyle::STIPPLED).stipple(stipple);
    }
    let _ = c.change_gc(gc, &aux);
    let (a, b) = (m.pts[0], *m.pts.last().unwrap());
    match m.tool {
        Tool::Pen | Tool::Marker => {
            let pts: Vec<Point> = m.pts.iter().map(|&(x, y)| pt(x, y)).collect();
            let pts = if pts.len() == 1 { vec![pts[0], pts[0]] } else { pts };
            let _ = c.poly_line(CoordMode::ORIGIN, d, gc, &pts);
        }
        Tool::Box => {
            let r = norm(a, b);
            let rect = xproto::Rectangle { x: r.x as i16, y: r.y as i16, width: r.width as u16, height: r.height as u16 };
            let _ = c.poly_rectangle(d, gc, &[rect]);
        }
        Tool::Arrow => {
            let (dx, dy) = ((b.0 - a.0) as f64, (b.1 - a.1) as f64);
            let len = (dx * dx + dy * dy).sqrt().max(1.0);
            let head = (m.width as f64 * 4.0 + 8.0).min(len);
            let (ux, uy) = (dx / len, dy / len);
            // Stop the shaft inside the head so its round cap can't poke out.
            let shaft_end = (b.0 as f64 - ux * head * 0.6, b.1 as f64 - uy * head * 0.6);
            let _ = c.poly_line(CoordMode::ORIGIN, d, gc, &[pt(a.0, a.1), pt(shaft_end.0.round() as i32, shaft_end.1.round() as i32)]);
            let spread = 0.45f64; // ~26°
            let side = |s: f64| {
                let (cs, sn) = (spread.cos(), s * spread.sin());
                let (rx, ry) = (ux * cs - uy * sn, uy * cs + ux * sn);
                pt((b.0 as f64 - rx * head).round() as i32, (b.1 as f64 - ry * head).round() as i32)
            };
            let tri = [pt(b.0, b.1), side(1.0), side(-1.0)];
            let _ = c.fill_poly(d, gc, PolyShape::CONVEX, CoordMode::ORIGIN, &tri);
        }
        Tool::Blur => {}
    }
    let _ = c.change_gc(gc, &xproto::ChangeGCAux::new().function(GX::COPY).fill_style(FillStyle::SOLID).line_width(1));
}

/// Draw a pen segment of `m` (Picture/Solid ink only).
pub fn paint_segment(x: &X, d: u32, gc: u32, m: &Mark, a: (i32, i32), b: (i32, i32), mask: bool) {
    let fg = if mask { 1 } else { m.rgb & 0xffffff };
    let _ = x.conn.change_gc(gc, &xproto::ChangeGCAux::new().foreground(fg).line_width(m.width.max(1) as u32).function(GX::COPY));
    let _ = x.conn.poly_line(CoordMode::ORIGIN, d, gc, &[pt(a.0, a.1), pt(b.0, b.1)]);
}

/// A GC with round caps and joins, for drawing marks.
pub fn mark_gc(x: &X, drawable: u32) -> Option<u32> {
    let gc = x.conn.generate_id().ok()?;
    let aux = xproto::CreateGCAux::new().graphics_exposures(0).cap_style(CapStyle::ROUND).join_style(JoinStyle::ROUND);
    x.conn.create_gc(gc, drawable, &aux).ok()?;
    Some(gc)
}

/// The frozen frame plus marks, as a server-side pixmap.
pub struct Canvas {
    x: Rc<X>,
    pub pixmap: u32,
    gc: u32,
    /// The untouched frame (pixmap) and its pixels (BGRX), for blur.
    base: u32,
    frame: Rc<Vec<u8>>,
    w: i32,
    h: i32,
    pub marks: Marks,
}

impl Canvas {
    /// `base` is a root-depth pixmap of the frame, `frame` its pixels.
    pub fn new(x: &Rc<X>, win: u32, base: u32, frame: Rc<Vec<u8>>, w: i32, h: i32) -> Option<Canvas> {
        let pixmap = x.conn.generate_id().ok()?;
        x.conn.create_pixmap(x.root_depth, pixmap, win, w as u16, h as u16).ok()?;
        let gc = mark_gc(x, pixmap)?;
        let c = Canvas { x: x.clone(), pixmap, gc, base, frame, w, h, marks: Marks::default() };
        c.render();
        Some(c)
    }

    pub fn is_empty(&self) -> bool {
        self.marks.is_empty()
    }

    pub fn begin(&mut self, tool: Tool, rgb: u32, size: i32, x: i32, y: i32) {
        self.marks.begin(tool, rgb, size, x, y);
        if tool.freehand() {
            self.render();
        }
    }

    pub fn extend(&mut self, x: i32, y: i32) {
        match self.marks.extend(x, y) {
            Change::Nothing => {}
            Change::Segment(a, b) => {
                if let Some(m) = self.marks.current() {
                    paint_segment(&self.x, self.pixmap, self.gc, m, a, b, false);
                }
            }
            Change::All => self.render(),
        }
    }

    pub fn end(&mut self) {
        self.marks.end();
        self.render();
    }

    /// Undo, redo or clear: `f` edits the marks, then the canvas is redrawn.
    pub fn edit(&mut self, f: impl FnOnce(&mut Marks) -> bool) -> bool {
        let changed = f(&mut self.marks);
        self.render();
        changed
    }

    /// Pixels of `r` (canvas coordinates), BGRX.
    pub fn read(&self, r: Rect) -> Option<Vec<u8>> {
        self.x.get_image(self.pixmap, r)
    }

    fn render(&self) {
        let _ = self.x.conn.copy_area(self.base, self.pixmap, self.gc, 0, 0, 0, 0, self.w as u16, self.h as u16);
        for m in self.marks.all() {
            if m.tool == Tool::Blur {
                self.pixelate(norm(m.pts[0], *m.pts.last().unwrap()), m.width.max(4));
            } else {
                paint(&self.x, self.pixmap, self.gc, m, Ink::Picture);
            }
        }
    }

    /// Replace `r` with flat blocks of its average colour, from the
    /// original frame (earlier marks underneath are not blurred).
    fn pixelate(&self, r: Rect, block: i32) {
        let r = r.intersect(Rect::new(0, 0, self.w, self.h));
        if r.width <= 0 || r.height <= 0 {
            return;
        }
        let stride = self.w as usize * 4;
        let mut out = vec![0u8; (r.width * r.height * 4) as usize];
        let ostride = r.width as usize * 4;
        let mut by = 0;
        while by < r.height {
            let bh = block.min(r.height - by);
            let mut bx = 0;
            while bx < r.width {
                let bw = block.min(r.width - bx);
                let mut sum = [0u64; 3];
                for yy in 0..bh {
                    let row = (r.y + by + yy) as usize * stride;
                    for xx in 0..bw {
                        let i = row + (r.x + bx + xx) as usize * 4;
                        for k in 0..3 {
                            sum[k] += self.frame[i + k] as u64;
                        }
                    }
                }
                let n = (bw * bh) as u64;
                let px = [(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8, 0];
                for yy in 0..bh {
                    let row = (by + yy) as usize * ostride;
                    for xx in 0..bw {
                        let i = row + (bx + xx) as usize * 4;
                        out[i..i + 4].copy_from_slice(&px);
                    }
                }
                bx += block;
            }
            by += block;
        }
        self.x.put_image_at(self.pixmap, self.gc, r.x, r.y, r.width as u16, r.height as u16, self.x.root_depth, &out);
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        let _ = self.x.conn.free_gc(self.gc);
        let _ = self.x.conn.free_pixmap(self.pixmap);
    }
}

fn pt(x: i32, y: i32) -> Point {
    Point { x: x as i16, y: y as i16 }
}

fn norm(a: (i32, i32), b: (i32, i32)) -> Rect {
    Rect::new(a.0.min(b.0), a.1.min(b.1), (a.0 - b.0).abs(), (a.1 - b.1).abs())
}
