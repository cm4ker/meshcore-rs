//! The blocks layouts are built from: text that is cut or wrapped to fit, the battery and
//! Bluetooth icons, page dots, a box over the page.
//!
//! A [`Recorder`] (tests, `tools/ui-preview`) collects where each text and icon landed and
//! reports any two that overlap.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Circle, Line, Polyline, PrimitiveStyle, Rectangle};
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

    /// A label on the left and its value on the right (`fonts`: the label's and the value's), the
    /// label cut to leave room.
    pub fn row(&mut self, fonts: (&Font, &Font), label: &str, value: &str, x: i32, right: i32, y: i32) {
        let (font, value_font) = fonts;
        let room = right - x - width(value_font, value) - 6;
        self.text(font, &fit(font, label, room), x, y);
        self.right(value_font, value, right, y);
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

    /// 29 x 14: a double outline, the tip, a bar of 20 px.
    pub fn battery_large(&mut self, x: i32, y: i32, pct: u8) {
        self.mark(Rectangle::new(Point::new(x, y), Size::new(29, 14)), "battery");
        self.rect(x, y, 26, 14, None);
        self.rect(x + 1, y + 1, 24, 12, None);
        self.rect(x + 26, y + 4, 3, 6, Some(INK));
        let fill = 20 * pct.min(100) as u32 / 100;
        if fill > 0 {
            self.rect(x + 3, y + 3, fill, 8, Some(INK));
        }
    }

    /// 15 x 8: an outline, the tip, a bar of 10 px.
    pub fn battery_small(&mut self, x: i32, y: i32, pct: u8) {
        self.mark(Rectangle::new(Point::new(x, y), Size::new(15, 8)), "battery");
        self.rect(x, y, 13, 8, None);
        self.rect(x + 13, y + 2, 2, 4, Some(INK));
        let fill = 9 * pct.min(100) as u32 / 100;
        if fill > 0 {
            self.rect(x + 2, y + 2, fill, 4, Some(INK));
        }
    }

    /// The Bluetooth rune, 10 x 18 (on a dark square while connected, struck through while off).
    pub fn ble_large(&mut self, x: i32, y: i32, state: BleState) {
        self.mark(Rectangle::new(Point::new(x - 3, y - 2), Size::new(15, 22)), "ble");
        let color = if state == BleState::Connected {
            self.rect(x - 3, y - 2, 15, 22, Some(INK));
            PAPER
        } else {
            INK
        };
        let pts = [
            Point::new(x, y + 4),
            Point::new(x + 8, y + 12),
            Point::new(x + 4, y + 17),
            Point::new(x + 4, y),
            Point::new(x + 8, y + 5),
            Point::new(x, y + 13),
        ];
        for dx in 0..2 {
            let shifted: Vec<Point> = pts.iter().map(|p| *p + Point::new(dx, 0)).collect();
            let _ = Polyline::new(&shifted).into_styled(PrimitiveStyle::with_stroke(color, 1)).draw(self.d);
        }
        if state == BleState::Off {
            self.line(Point::new(x - 2, y + 18), Point::new(x + 10, y - 1));
            self.line(Point::new(x - 1, y + 18), Point::new(x + 11, y - 1));
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
