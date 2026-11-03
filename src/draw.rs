//! Drawing with plain X11 core requests: filled rectangles, 1px lines,
//! circles and text in the server's own bitmap fonts. No toolkit, no
//! client-side rasteriser. Every surface is drawn into a pixmap and copied
//! to its window in one go, so nothing flickers.
//!
//! Colours are 0xRRGGBB; on 32-bit (ARGB) surfaces an alpha is applied and
//! the pixel premultiplied, as the compositor expects.

use crate::x11::{Rect, X};
use std::cell::OnceCell;
use std::rc::Rc;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _};

// zk.poker palette: ink-black surfaces, cream text, one gold accent, red
// only for recording.
pub const ELEV1: u32 = 0x0f1513;
pub const ELEV2: u32 = 0x151d1a;
pub const LINE_SOFT: u32 = 0x1c2724;
pub const LINE: u32 = 0x2a3833;
pub const FG_HIGH: u32 = 0xf2ecdf;
pub const FG: u32 = 0xded5c4;
pub const MUTED: u32 = 0x948a79;
pub const RED: u32 = 0xc73e3a;
pub const GOLD: u32 = 0xd9a441;
pub const GOLD_FG: u32 = 0x141008;

pub struct Font {
    pub id: u32,
    pub ascent: i32,
    pub descent: i32,
    pub width: i32,
}

impl Font {
    pub fn height(&self) -> i32 {
        self.ascent + self.descent
    }

    pub fn text_width(&self, s: &str) -> i32 {
        s.chars().count() as i32 * self.width
    }
}

thread_local! {
    static FONT: OnceCell<Rc<Font>> = const { OnceCell::new() };
}

/// The UI font: misc-fixed at a size matching the UI scale. "fixed" is
/// built into every X server, so there's always something.
pub fn font() -> Rc<Font> {
    FONT.with(|f| {
        f.get_or_init(|| {
            let x = crate::x11::get();
            let scale = x.ui_scale();
            let mut names: Vec<String> = Vec::new();
            if let Ok(n) = std::env::var("PEEK2_FONT") {
                names.push(n);
            }
            let sizes: &[&str] = match scale {
                1 => &["-misc-fixed-medium-r-normal--13-120-75-75-c-70-iso10646-1", "7x13"],
                2 => &["-misc-fixed-medium-r-normal--20-200-75-75-c-100-iso10646-1", "10x20"],
                _ => &["-misc-fixed-medium-r-normal--20-200-75-75-c-100-iso10646-1", "10x20"],
            };
            names.extend(sizes.iter().map(|s| s.to_string()));
            names.push("fixed".into());
            for name in names {
                let Ok(id) = x.conn.generate_id() else { break };
                if x.conn.open_font(id, name.as_bytes()).ok().and_then(|c| c.check().ok()).is_none() {
                    continue;
                }
                if let Some(q) = x.conn.query_font(id).ok().and_then(|c| c.reply().ok()) {
                    let width = q.max_bounds.character_width.max(1) as i32;
                    return Rc::new(Font { id, ascent: q.font_ascent as i32, descent: q.font_descent as i32, width });
                }
            }
            Rc::new(Font { id: 0, ascent: 10, descent: 3, width: 6 })
        })
        .clone()
    })
}

/// Pixel value for a colour on a surface of `depth`.
pub fn pixel(depth: u8, rgb: u32, alpha: f64) -> u32 {
    if depth != 32 {
        return rgb & 0xffffff;
    }
    let a = alpha.clamp(0.0, 1.0);
    let ch = |shift: u32| ((((rgb >> shift) & 0xff) as f64) * a).round() as u32;
    ((a * 255.0).round() as u32) << 24 | ch(16) << 16 | ch(8) << 8 | ch(0)
}

/// An offscreen pixmap the size of a window, with a GC.
pub struct Surface {
    x: Rc<X>,
    pub pixmap: u32,
    pub gc: u32,
    pub depth: u8,
    pub w: i32,
    pub h: i32,
}

impl Surface {
    pub fn new(x: &Rc<X>, win: u32, depth: u8, w: i32, h: i32) -> Option<Surface> {
        let pixmap = x.conn.generate_id().ok()?;
        x.conn.create_pixmap(depth, pixmap, win, w.max(1) as u16, h.max(1) as u16).ok()?;
        let gc = x.conn.generate_id().ok()?;
        let f = font();
        let mut aux = xproto::CreateGCAux::new().graphics_exposures(0);
        if f.id != 0 {
            aux = aux.font(f.id);
        }
        x.conn.create_gc(gc, pixmap, &aux).ok()?;
        Some(Surface { x: x.clone(), pixmap, gc, depth, w, h })
    }

    fn fg(&self, rgb: u32, alpha: f64) {
        let _ = self.x.conn.change_gc(self.gc, &xproto::ChangeGCAux::new().foreground(pixel(self.depth, rgb, alpha)));
    }

    pub fn fill(&self, r: Rect, rgb: u32, alpha: f64) {
        if r.width <= 0 || r.height <= 0 {
            return;
        }
        self.fg(rgb, alpha);
        let rect = xproto::Rectangle { x: r.x as i16, y: r.y as i16, width: r.width as u16, height: r.height as u16 };
        let _ = self.x.conn.poly_fill_rectangle(self.pixmap, self.gc, &[rect]);
    }

    /// Clear to fully transparent (ARGB) or black.
    pub fn clear(&self, r: Rect) {
        self.fill(r, 0, 0.0);
    }

    /// 1px outline just inside `r`.
    pub fn stroke(&self, r: Rect, rgb: u32) {
        if r.width <= 0 || r.height <= 0 {
            return;
        }
        self.fg(rgb, 1.0);
        let rect = xproto::Rectangle { x: r.x as i16, y: r.y as i16, width: (r.width - 1) as u16, height: (r.height - 1) as u16 };
        let _ = self.x.conn.poly_rectangle(self.pixmap, self.gc, &[rect]);
    }

    pub fn line(&self, x0: i32, y0: i32, x1: i32, y1: i32, rgb: u32, alpha: f64) {
        self.fg(rgb, alpha);
        let pts = [xproto::Point { x: x0 as i16, y: y0 as i16 }, xproto::Point { x: x1 as i16, y: y1 as i16 }];
        let _ = self.x.conn.poly_line(xproto::CoordMode::ORIGIN, self.pixmap, self.gc, &pts);
    }

    pub fn circle(&self, cx: i32, cy: i32, r: i32, rgb: u32, filled: bool) {
        self.fg(rgb, 1.0);
        let arc = xproto::Arc { x: (cx - r) as i16, y: (cy - r) as i16, width: (2 * r) as u16, height: (2 * r) as u16, angle1: 0, angle2: 360 * 64 };
        let _ = if filled {
            self.x.conn.poly_fill_arc(self.pixmap, self.gc, &[arc])
        } else {
            self.x.conn.poly_arc(self.pixmap, self.gc, &[arc])
        };
    }

    /// Text with its top-left at (x, y).
    pub fn text(&self, x: i32, y: i32, s: &str, rgb: u32) {
        let f = font();
        if f.id == 0 || s.is_empty() {
            return;
        }
        self.fg(rgb, 1.0);
        // PolyText16 items: [count, delta, (byte1, byte2)…], ≤254 chars each.
        let chars: Vec<u16> = s.chars().map(|c| if (c as u32) < 0x10000 { c as u32 as u16 } else { b'?' as u16 }).collect();
        let mut items = Vec::new();
        for chunk in chars.chunks(254) {
            items.push(chunk.len() as u8);
            items.push(0);
            for c in chunk {
                items.extend_from_slice(&c.to_be_bytes());
            }
        }
        let _ = self.x.conn.poly_text16(self.pixmap, self.gc, x as i16, (y + f.ascent) as i16, &items);
    }

    pub fn copy_from(&self, src: u32, sx: i32, sy: i32, r: Rect) {
        let _ = self.x.conn.copy_area(src, self.pixmap, self.gc, sx as i16, sy as i16, r.x as i16, r.y as i16, r.width as u16, r.height as u16);
    }

    /// Show the pixmap in `win`.
    pub fn present(&self, win: u32) {
        let _ = self.x.conn.copy_area(self.pixmap, win, self.gc, 0, 0, 0, 0, self.w as u16, self.h as u16);
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        let _ = self.x.conn.free_gc(self.gc);
        let _ = self.x.conn.free_pixmap(self.pixmap);
    }
}

// ---- a minimal toolbar: a row of labels, buttons and meters -------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Style {
    Normal,
    Primary,
    Danger,
    Flat,
}

#[derive(Clone, Debug)]
pub enum Item {
    /// Record indicator: red dot when active, muted ring otherwise.
    Dot(bool),
    Text(String, u32),
    Button { id: &'static str, label: String, style: Style },
    /// A colour chip with its name; clickable.
    Swatch { id: &'static str, rgb: u32, label: String },
    /// Per source: (name, left dBFS, right dBFS, silent).
    Meters(Vec<(&'static str, f32, f32, bool)>),
}

/// Layout metrics, scaled.
pub struct Metrics {
    pub s: i32,
    pub pad: i32,
    pub gap: i32,
    pub btn_pad: i32,
    pub h: i32,
}

pub fn metrics() -> Metrics {
    let s = crate::x11::get().ui_scale();
    let f = font();
    let btn_pad = 3 * s;
    Metrics { s, pad: 5 * s, gap: 6 * s, btn_pad, h: f.height() + 2 * btn_pad + 2 * s }
}

fn item_width(item: &Item, m: &Metrics) -> i32 {
    let f = font();
    match item {
        Item::Dot(_) => 10 * m.s,
        Item::Text(t, _) => f.text_width(t),
        Item::Button { label, .. } => f.text_width(label) + 2 * (m.btn_pad + 3 * m.s),
        Item::Swatch { label, .. } => f.text_width(label) + 2 * (m.btn_pad + 3 * m.s) + 12 * m.s,
        Item::Meters(v) => v.iter().map(|(n, ..)| f.text_width(n) + 4 * m.s + 56 * m.s).sum::<i32>() + (v.len() as i32 - 1).max(0) * m.gap,
    }
}

/// Size of a bar holding `items`.
pub fn bar_size(items: &[Item]) -> (i32, i32) {
    let m = metrics();
    let inner: i32 = items.iter().map(|i| item_width(i, &m)).sum::<i32>() + (items.len() as i32 - 1).max(0) * m.gap;
    (inner + 2 * m.pad, m.h + 2 * m.pad)
}

/// Draw a bar at (x, y); returns the button hit areas.
pub fn draw_bar(surf: &Surface, x: i32, y: i32, items: &[Item], hover: Option<&str>) -> Vec<(&'static str, Rect)> {
    let m = metrics();
    let f = font();
    let (w, h) = bar_size(items);
    let frame = Rect::new(x, y, w, h);
    surf.fill(frame, ELEV1, 0.97);
    surf.stroke(frame, LINE);
    let mut hits = Vec::new();
    let mut cx = x + m.pad;
    let top = y + m.pad;
    let text_y = top + (m.h - f.height()) / 2;
    for item in items {
        let iw = item_width(item, &m);
        match item {
            Item::Dot(active) => {
                let (dx, dy, r) = (cx + iw / 2, top + m.h / 2, 4 * m.s);
                surf.circle(dx, dy, r, if *active { RED } else { MUTED }, *active);
            }
            Item::Text(t, ink) => surf.text(cx, text_y, t, *ink),
            Item::Button { id, label, style } => {
                let r = Rect::new(cx, top, iw, m.h);
                let hot = hover == Some(*id);
                let (bg, border, ink) = match style {
                    Style::Primary => (if hot { 0xe6b85c } else { GOLD }, GOLD, GOLD_FG),
                    Style::Danger => (if hot { 0xd95b4e } else { RED }, RED, FG_HIGH),
                    Style::Normal => (if hot { LINE_SOFT } else { ELEV2 }, LINE_SOFT, FG_HIGH),
                    Style::Flat => (if hot { ELEV2 } else { ELEV1 }, if hot { LINE_SOFT } else { ELEV1 }, if hot { FG_HIGH } else { FG }),
                };
                surf.fill(r, bg, 1.0);
                surf.stroke(r, border);
                surf.text(cx + (iw - f.text_width(label)) / 2, text_y, label, ink);
                hits.push((*id, r));
            }
            Item::Swatch { id, rgb, label } => {
                let r = Rect::new(cx, top, iw, m.h);
                let hot = hover == Some(*id);
                surf.fill(r, if hot { LINE_SOFT } else { ELEV2 }, 1.0);
                surf.stroke(r, LINE_SOFT);
                let chip = 8 * m.s;
                let cxp = cx + m.btn_pad + 2 * m.s;
                surf.fill(Rect::new(cxp, top + (m.h - chip) / 2, chip, chip), *rgb, 1.0);
                surf.stroke(Rect::new(cxp, top + (m.h - chip) / 2, chip, chip), LINE);
                surf.text(cxp + chip + 4 * m.s, text_y, label, FG_HIGH);
                hits.push((*id, r));
            }
            Item::Meters(v) => {
                let mut mx = cx;
                for (name, l, rr, silent) in v {
                    let ink = if *silent { LINE } else { MUTED };
                    surf.text(mx, text_y, name, ink);
                    mx += f.text_width(name) + 4 * m.s;
                    let bw = 56 * m.s;
                    let bh = 3 * m.s;
                    let mid = top + m.h / 2;
                    for (i, db) in [*l, *rr].into_iter().enumerate() {
                        let by = if i == 0 { mid - bh - m.s } else { mid + m.s };
                        surf.fill(Rect::new(mx, by, bw, bh), LINE_SOFT, 1.0);
                        // -60 dBFS .. 0 dBFS → 0 .. 1
                        let v = if db.is_finite() { ((db + 60.0) / 60.0).clamp(0.0, 1.0) } else { 0.0 };
                        let color = if *silent {
                            LINE
                        } else if v >= 0.97 {
                            RED
                        } else if v >= 0.8 {
                            GOLD
                        } else {
                            FG
                        };
                        surf.fill(Rect::new(mx, by, (bw as f32 * v) as i32, bh), color, 1.0);
                    }
                    mx += bw + m.gap;
                }
            }
        }
        cx += iw + m.gap;
    }
    hits
}

/// Which hit area contains (x, y).
pub fn hit(hits: &[(&'static str, Rect)], x: i32, y: i32) -> Option<&'static str> {
    hits.iter().find(|(_, r)| r.contains(x, y)).map(|(id, _)| *id)
}
