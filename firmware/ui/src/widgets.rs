//! The blocks layouts are built from: text that is cut or wrapped to fit, the Bluetooth icon,
//! page dots, a box over the page.
//!
//! A [`Recorder`] (tests, `tools/ui-preview`) collects where each text and icon landed and
//! reports any two that overlap.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Circle, Line, Polyline, PrimitiveStyle, Rectangle, RoundedRectangle};
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};

use crate::model::BleState;
use crate::text::cp1251;
use crate::theme::Font;

pub const INK: BinaryColor = BinaryColor::On;
pub const PAPER: BinaryColor = BinaryColor::Off;

pub fn width(font: &Font, s: &str) -> i32 {
    font.r
        .get_rendered_dimensions(cp1251(s).as_str(), Point::zero(), VerticalPosition::Top)
        .map(|d| d.advance.x)
        .unwrap_or(0)
}

/// `s` cut to `max` pixels, with "..." if it was longer.
pub fn fit(font: &Font, s: &str, max: i32) -> String {
    if width(font, s) <= max {
        return s.into();
    }
    let mut chars: Vec<char> = s.chars().collect();
    while !chars.is_empty() {
        chars.pop();
        let t: String = chars.iter().collect::<String>().trim_end().into();
        let t = format!("{t}...");
        if width(font, &t) <= max {
            return t;
        }
    }
    String::new()
}

/// Word-wrapped to `max` pixels, at most `max_lines` (the last one cut with "..." if it goes on).
pub fn wrap(font: &Font, text: &str, max: i32, max_lines: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let candidate = if cur.is_empty() { word.into() } else { format!("{cur} {word}") };
        if width(font, &candidate) <= max {
            cur = candidate;
            continue;
        }
        if !cur.is_empty() {
            lines.push(core::mem::take(&mut cur));
        }
        // a word wider than a line is split
        let mut rest = String::from(word);
        while width(font, &rest) > max {
            let mut head = String::new();
            for c in rest.chars() {
                head.push(c);
                if width(font, &head) > max {
                    head.pop();
                    break;
                }
            }
            if head.is_empty() {
                break;
            }
            rest = rest[head.len()..].into();
            lines.push(head);
        }
        cur = rest;
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        let last = lines.pop().unwrap_or_default();
        lines.push(fit(font, &format!("{last} ..."), max));
    }
    lines
}

/// Where texts and icons landed, layer by layer (a box over the page starts a new one).
#[derive(Default)]
pub struct Recorder {
    layer: Vec<(Rectangle, String)>,
    /// Pairs of things drawn over each other.
    pub collisions: Vec<String>,
}

impl Recorder {
    fn add(&mut self, r: Rectangle, what: String) {
        for (other, name) in &self.layer {
            if !r.intersection(other).is_zero_sized() {
                self.collisions.push(format!("{what:?} over {name:?}"));
            }
        }
        self.layer.push((r, what));
    }
}

pub struct Canvas<'a, D> {
    pub d: &'a mut D,
    pub w: i32,
    pub h: i32,
    rec: Option<&'a mut Recorder>,
}

impl<'a, D: DrawTarget<Color = BinaryColor>> Canvas<'a, D> {
    pub fn new(d: &'a mut D, w: i32, h: i32, rec: Option<&'a mut Recorder>) -> Self {
        Self { d, w, h, rec }
    }

    fn mark(&mut self, r: Rectangle, what: &str) {
        if let Some(rec) = self.rec.as_mut() {
            rec.add(r, what.into());
        }
    }

    /// What follows is drawn over the page (a box): it may cover what is under it.
    pub fn layer(&mut self) {
        if let Some(rec) = self.rec.as_mut() {
            rec.layer.clear();
        }
    }

    pub fn draw(&mut self, font: &Font, s: &str, x: i32, y: i32, align: HorizontalAlignment, color: BinaryColor) {
        let t = cp1251(s);
        if self.rec.is_some()
            && let Ok(Some(bb)) = font.r.get_rendered_dimensions_aligned(t.as_str(), Point::new(x, y), VerticalPosition::Top, align)
        {
            self.mark(bb, s);
        }
        let _ = font.r.render_aligned(t.as_str(), Point::new(x, y), VerticalPosition::Top, align, FontColor::Transparent(color), self.d);
    }

    pub fn text(&mut self, font: &Font, s: &str, x: i32, y: i32) {
        self.draw(font, s, x, y, HorizontalAlignment::Left, INK);
    }

    /// Ending at `x`.
    pub fn right(&mut self, font: &Font, s: &str, x: i32, y: i32) {
        self.draw(font, s, x, y, HorizontalAlignment::Right, INK);
    }

    pub fn centered(&mut self, font: &Font, s: &str, y: i32) {
        self.draw(font, s, self.w / 2, y, HorizontalAlignment::Center, INK);
    }

    /// Lines of text from `y` on; returns where the next line would go.
    pub fn lines(&mut self, font: &Font, lines: &[String], x: i32, y: i32) -> i32 {
        let mut y = y;
        for l in lines {
            self.text(font, l, x, y);
            y += font.pitch;
        }
        y
    }

    /// A label on the left and its value on the right (`fonts`: the label's and the value's), cut
    /// to leave room on either side.
    pub fn row(&mut self, fonts: (&Font, &Font), label: &str, value: &str, x: i32, right: i32, y: i32) {
        let (font, value_font) = fonts;
        let total = right - x;
        let value_max = (total - width(font, label).min(total / 2) - 6).max(0);
        let val = fit(value_font, value, value_max);
        let room = (total - width(value_font, &val) - 6).max(0);
        self.text(font, &fit(font, label, room), x, y);
        self.right(value_font, &val, right, y);
    }

    /// A short label on the left and its value on the right, the value cut to leave room.
    pub fn kv_row(&mut self, fonts: (&Font, &Font), label: &str, value: &str, x: i32, right: i32, y: i32) {
        let (font, value_font) = fonts;
        self.text(font, label, x, y);
        let room = right - x - width(font, label) - 6;
        self.right(value_font, &fit(value_font, value, room), right, y);
    }

    pub fn line(&mut self, a: Point, b: Point) {
        let _ = Line::new(a, b).into_styled(PrimitiveStyle::with_stroke(INK, 1)).draw(self.d);
    }

    pub fn rect(&mut self, x: i32, y: i32, w: u32, h: u32, fill: Option<BinaryColor>) {
        let style = match fill {
            Some(c) => PrimitiveStyle::with_fill(c),
            None => PrimitiveStyle::with_stroke(INK, 1),
        };
        let _ = Rectangle::new(Point::new(x, y), Size::new(w, h)).into_styled(style).draw(self.d);
    }

    /// The Bluetooth rune, 10 x 18 (on a dark square while connected, struck through while off).
    pub fn ble_large(&mut self, x: i32, y: i32, state: BleState) {
        let rune = [(0, 4), (8, 12), (4, 17), (4, 0), (8, 5), (0, 13)];
        self.ble_bold(x, y, state, rune, Size::new(15, 22), ((-2, 18), (10, -1)));
    }

    /// The Bluetooth rune, 8 x 13 (on a dark square while connected, struck through while off).
    pub fn ble_medium(&mut self, x: i32, y: i32, state: BleState) {
        let rune = [(0, 3), (6, 9), (3, 12), (3, 0), (6, 3), (0, 9)];
        self.ble_bold(x, y, state, rune, Size::new(14, 17), ((-2, 13), (8, -1)));
    }

    /// The Bluetooth rune through `rune` (from `x`, `y`) in strokes two pixels wide, in a box of
    /// `size` from 3 px left of it and 2 px above; struck through from `strike.0` to `strike.1`.
    fn ble_bold(&mut self, x: i32, y: i32, state: BleState, rune: [(i32, i32); 6], size: Size, strike: ((i32, i32), (i32, i32))) {
        self.mark(Rectangle::new(Point::new(x - 3, y - 2), size), "ble");
        let color = if state == BleState::Connected {
            self.rect(x - 3, y - 2, size.width, size.height, Some(INK));
            PAPER
        } else {
            INK
        };
        for dx in 0..2 {
            let pts: Vec<Point> = rune.iter().map(|&(px, py)| Point::new(x + px + dx, y + py)).collect();
            let _ = Polyline::new(&pts).into_styled(PrimitiveStyle::with_stroke(color, 1)).draw(self.d);
        }
        if state == BleState::Off {
            let ((ax, ay), (bx, by)) = strike;
            for dx in 0..2 {
                self.line(Point::new(x + ax + dx, y + ay), Point::new(x + bx + dx, y + by));
            }
        }
    }

    /// The Bluetooth rune, 5 x 9, in a 9 x 11 box (dark while connected, struck through while
    /// off).
    pub fn ble_small(&mut self, x: i32, y: i32, state: BleState) {
        self.mark(Rectangle::new(Point::new(x - 2, y - 1), Size::new(9, 11)), "ble");
        let color = if state == BleState::Connected {
            self.rect(x - 2, y - 1, 9, 11, Some(INK));
            PAPER
        } else {
            INK
        };
        let pts = [
            Point::new(x, y + 2),
            Point::new(x + 4, y + 6),
            Point::new(x + 2, y + 8),
            Point::new(x + 2, y),
            Point::new(x + 4, y + 2),
            Point::new(x, y + 6),
        ];
        let _ = Polyline::new(&pts).into_styled(PrimitiveStyle::with_stroke(color, 1)).draw(self.d);
        if state == BleState::Off {
            self.line(Point::new(x - 1, y + 9), Point::new(x + 5, y - 1));
        }
    }

    /// Inverted pill for a long-press action: `[ (.) label ]`, clamped to `max_w`.
    pub fn hold_pill(&mut self, font: &Font, label: &str, x: i32, y: i32, h: i32, pad_x: i32, max_w: i32) {
        let d = (h - 4).clamp(7, 13) | 1;
        let gap = if h >= 18 { 5 } else { 4 };
        let icon_w = d + gap;
        let lbl = fit(font, label, (max_w - 2 * pad_x - icon_w).max(0));
        let w = (pad_x + icon_w + width(font, &lbl) + pad_x).min(max_w).max(2 * pad_x + d);
        let pill = Rectangle::new(Point::new(x, y), Size::new(w as u32, h as u32));
        self.mark(pill, "hold");
        let _ = RoundedRectangle::with_equal_corners(pill, Size::new(3, 3))
            .into_styled(PrimitiveStyle::with_fill(INK))
            .draw(self.d);
        let cx = x + pad_x + d / 2;
        let cy = y + h / 2;
        let _ = Circle::with_center(Point::new(cx, cy), d as u32)
            .into_styled(PrimitiveStyle::with_stroke(PAPER, 1))
            .draw(self.d);
        let d_in: u32 = if d >= 13 { 5 } else if d >= 9 { 3 } else { 1 };
        let _ = Circle::with_center(Point::new(cx, cy), d_in)
            .into_styled(PrimitiveStyle::with_fill(PAPER))
            .draw(self.d);
        let t = cp1251(&lbl);
        let _ = font.r.render_aligned(
            t.as_str(),
            Point::new(x + pad_x + icon_w, cy),
            VerticalPosition::Center,
            HorizontalAlignment::Left,
            FontColor::Transparent(PAPER),
            self.d,
        );
    }

    /// Horizontally centred [`Self::hold_pill`].
    pub fn hold_pill_centered(&mut self, font: &Font, label: &str, y: i32, h: i32, pad_x: i32, max_w: i32) {
        let d = (h - 4).clamp(7, 13) | 1;
        let gap = if h >= 18 { 5 } else { 4 };
        let icon_w = d + gap;
        let lbl = fit(font, label, (max_w - 2 * pad_x - icon_w).max(0));
        let w = (pad_x + icon_w + width(font, &lbl) + pad_x).min(max_w).max(2 * pad_x + d);
        self.hold_pill(font, &lbl, (self.w - w) / 2, y, h, pad_x, w);
    }

    /// Mail envelope icon at (`x`, `y`) of size (`w`, `h`), filled when messages are unread.
    pub fn envelope(&mut self, x: i32, y: i32, w: i32, h: i32, filled: bool) {
        let rect = Rectangle::new(Point::new(x, y), Size::new(w as u32, h as u32));
        self.mark(rect, "env");
        let thick = if h >= 20 { 2 } else { 1 };
        let corner = if h >= 20 { 3 } else { 2 };
        let flap = if filled {
            let _ = RoundedRectangle::with_equal_corners(rect, Size::new(corner, corner))
                .into_styled(PrimitiveStyle::with_fill(INK))
                .draw(self.d);
            PAPER
        } else {
            let _ = RoundedRectangle::with_equal_corners(rect, Size::new(corner, corner))
                .into_styled(PrimitiveStyle::with_stroke(INK, thick as u32))
                .draw(self.d);
            INK
        };
        let style = PrimitiveStyle::with_stroke(flap, 1);
        for dy in 0..thick {
            let left = Point::new(x + 1, y + 1 + dy);
            let mid = Point::new(x + w / 2, y + h / 2 + 1 + dy);
            let right = Point::new(x + w - 2, y + 1 + dy);
            let _ = Line::new(left, mid).into_styled(style).draw(self.d);
            let _ = Line::new(mid, right).into_styled(style).draw(self.d);
        }
    }

    /// Radio beacon icon centred on the emitter at (`cx`, `cy`), 2 wave pairs for zero-hop or 3 for flood.
    pub fn beacon(&mut self, cx: i32, cy: i32, r: i32, flood: bool) {
        let thick = if r >= 14 { 2 } else { 1 };
        let rings: &[i32] = if flood { &[r * 50 / 100, r * 82 / 100, r * 114 / 100] } else { &[r * 55 / 100, r * 95 / 100] };
        let max_rr = *rings.last().unwrap() + thick - 1;
        let dot_d = if r >= 14 { 5 } else { 3 };
        let top_ext = (max_rr * 57 / 100).max(dot_d / 2);
        self.mark(
            Rectangle::new(Point::new(cx - max_rr, cy - top_ext), Size::new((2 * max_rr + 1) as u32, (top_ext + r + thick) as u32)),
            "beacon",
        );
        let _ = Circle::with_center(Point::new(cx, cy), dot_d as u32).into_styled(PrimitiveStyle::with_fill(INK)).draw(self.d);
        self.rect(cx - thick / 2, cy, thick as u32, (r + 1) as u32, Some(INK));
        let foot = (r / 3).max(3);
        self.rect(cx - foot, cy + r, (2 * foot + 1) as u32, thick as u32, Some(INK));
        for &rr in rings {
            let half = (rr * 57 / 100).max(2);
            let r2 = (rr * rr) as u32;
            for dy in -half..=half {
                let dx = r2.saturating_sub((dy * dy) as u32).isqrt() as i32;
                for t in 0..thick {
                    let _ = Pixel(Point::new(cx - dx - t, cy + dy), INK).draw(self.d);
                    let _ = Pixel(Point::new(cx + dx + t, cy + dy), INK).draw(self.d);
                }
            }
        }
    }

    /// IEC standby / power icon centred at (`cx`, `cy`) with circle radius `r`.
    pub fn power_icon(&mut self, cx: i32, cy: i32, r: i32) {
        let thick = if r >= 14 { 2 } else { 1 };
        let top_stem = r + 2;
        self.mark(
            Rectangle::new(Point::new(cx - r, cy - top_stem), Size::new((2 * r + 1) as u32, (top_stem + r + 1) as u32)),
            "power",
        );
        let _ = Circle::with_center(Point::new(cx, cy), (2 * r + 1) as u32)
            .into_styled(PrimitiveStyle::with_stroke(INK, thick as u32))
            .draw(self.d);
        let gap = (r * 45 / 100).max(3);
        self.rect(cx - gap, cy - top_stem, (2 * gap + 1) as u32, (top_stem - r / 3).max(4) as u32, Some(PAPER));
        self.rect(cx - thick / 2, cy - top_stem, thick as u32, (top_stem + 1) as u32, Some(INK));
    }

    /// Compact satellite signal icon in a `size x size` box at (`x`, `y`).
    pub fn sat_icon(&mut self, x: i32, y: i32, size: i32) {
        self.mark(Rectangle::new(Point::new(x, y), Size::new(size as u32, size as u32)), "sat");
        let thick = if size >= 14 { 2 } else { 1 };
        let (ox, oy) = (x + 2, y + size - 3);
        let dot_d: u32 = if size >= 14 { 4 } else { 3 };
        let _ = Circle::with_center(Point::new(ox, oy), dot_d).into_styled(PrimitiveStyle::with_fill(INK)).draw(self.d);
        let r1 = (size * 52 / 100).max(4);
        let r2 = (size - 3 - thick).max(r1 + 2);
        for rr in [r1, r2] {
            let r2sq = (rr * rr) as u32;
            for d in 0..=rr {
                let o = r2sq.saturating_sub((d * d) as u32).isqrt() as i32;
                for (dx, dy) in [(d, o), (o, d)] {
                    for t in 0..thick {
                        let px = (ox + dx + t).min(x + size - 1);
                        let py = (oy - dy).max(y);
                        let _ = Pixel(Point::new(px, py), INK).draw(self.d);
                    }
                }
            }
        }
    }

    /// Rounded progress bar at (`x`, `y`) of size (`w`, `h`).
    pub fn progress_bar(&mut self, x: i32, y: i32, w: i32, h: i32, done: u32, total: u32) {
        let rect = Rectangle::new(Point::new(x, y), Size::new(w as u32, h as u32));
        self.mark(rect, "bar");
        let stroke: u32 = if h >= 16 { 2 } else { 1 };
        let _ = RoundedRectangle::with_equal_corners(rect, Size::new(2, 2))
            .into_styled(PrimitiveStyle::with_stroke(INK, stroke))
            .draw(self.d);
        let pad = stroke as i32 + 1;
        let inner_w = (w - 2 * pad).max(0) as u64;
        let inner_h = (h - 2 * pad).max(1) as u32;
        let fill_w = if total == 0 { 0 } else { (done.min(total) as u64 * inner_w / total as u64) as u32 };
        if fill_w > 0 {
            self.rect(x + pad, y + pad, fill_w, inner_h, Some(INK));
        }
    }

    /// Page dots centred at `y`: `pitch` apart, the current one filled and larger.
    pub fn dots(&mut self, idx: u8, n: u8, y: i32, pitch: i32, d_current: u32, d_other: u32) {
        let x0 = self.w / 2 - (n as i32 - 1) * pitch / 2;
        for i in 0..n as i32 {
            let c = Point::new(x0 + i * pitch, y);
            let current = i == idx as i32;
            let style = if current { PrimitiveStyle::with_fill(INK) } else { PrimitiveStyle::with_stroke(INK, 1) };
            let _ = Circle::with_center(c, if current { d_current } else { d_other }).into_styled(style).draw(self.d);
        }
    }

    /// A box over the middle of the page, `inset` from the sides with a `border` px frame, its
    /// lines centred; `pad` above the first line and below the last.
    pub fn overlay(&mut self, lines: &[(String, &Font)], inset: i32, border: i32, pad: i32) {
        self.layer();
        let height: i32 = lines.iter().map(|(_, f)| f.pitch).sum::<i32>() + 2 * pad;
        let top = (self.h - height) / 2;
        self.rect(inset, top, (self.w - 2 * inset) as u32, height as u32, Some(PAPER));
        for i in 0..border {
            self.rect(inset + i, top + i, (self.w - 2 * inset - 2 * i) as u32, (height - 2 * i) as u32, None);
        }
        let mut y = top + pad;
        for (s, f) in lines {
            self.centered(f, s, y);
            y += f.pitch;
        }
    }
}
