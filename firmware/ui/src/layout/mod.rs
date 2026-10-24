//! One layout per screen class, not per board: a board with a 200 x 200 e-paper or a 128 x 64
//! monochrome screen picks the class and hands it its frame buffer.

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;

use crate::model::{Body, Page, View};
use crate::widgets::{Canvas, Recorder};

pub mod epaper_square;
pub mod mono_128x64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    /// Square e-paper, 200 x 200 (T-Echo).
    EpaperSquare,
    /// Monochrome OLED, 128 x 64 (Heltec V3/V4).
    Mono128x64,
}

impl Class {
    pub fn size(self) -> Size {
        match self {
            Class::EpaperSquare => Size::new(200, 200),
            Class::Mono128x64 => Size::new(128, 64),
        }
    }

    /// Draws `v` on `d` (cleared to paper first is the caller's business).
    pub fn render<D: DrawTarget<Color = BinaryColor>>(self, v: &View, d: &mut D) {
        self.render_recorded(v, d, None);
    }

    /// As [`Self::render`], noting where each text and icon went.
    pub fn render_recorded<D: DrawTarget<Color = BinaryColor>>(self, v: &View, d: &mut D, rec: Option<&mut Recorder>) {
        let s = self.size();
        let mut c = Canvas::new(d, s.width as i32, s.height as i32, rec);
        match self {
            Class::EpaperSquare => epaper_square::draw(&mut c, v),
            Class::Mono128x64 => mono_128x64::draw(&mut c, v),
        }
    }
}

/// The bar's title: the node's name on the home page, else the page's.
pub(crate) fn title(v: &View) -> &str {
    match (&v.page, &v.body) {
        (_, Body::Home { name, .. }) => name,
        (_, Body::Repeater(p, _)) => &p.name,
        (Page::Messages, _) => "Сообщения",
        (Page::Nearby, _) => "Рядом",
        (Page::Radio, _) => "Радио",
        (Page::Bluetooth, _) => "Bluetooth",
        (Page::Gps, _) => "GPS",
        (Page::Advert, _) => "Объявить",
        (Page::PowerOff, _) => "Выключить",
        (Page::Neighbours, _) => "Соседи",
        (Page::Traffic, _) => "Трафик",
        (Page::Power, _) => "Питание",
        (Page::Network, _) => "Сеть",
        (Page::Update, _) => "Прошивка",
        (Page::Home, _) => "",
    }
}

pub(crate) fn alert_text(a: crate::model::Alert) -> &'static str {
    use crate::model::Alert;
    match a {
        Alert::AdvertSent => "Объявление отправлено",
        Alert::AdvertFailed => "Не удалось отправить",
        Alert::BleOn => "Bluetooth включен",
        Alert::BleOff => "Bluetooth выключен",
        Alert::GpsOn => "GPS включен",
        Alert::GpsOff => "GPS выключен",
    }
}

pub(crate) const ADVERT_ZERO_HOP: &str = "Узлы рядом узнают, что вы в сети, и смогут добавить вас в контакты.";
pub(crate) const ADVERT_FLOOD: &str = "Объявление о репитере разойдется по всей сети: узлы добавят его в свои списки.";
pub(crate) const POWER_OFF: &str = "Экран сохранит надпись \"Выключено\". Включить снова - кнопкой.";

/// A screen of `size` that takes pixels anywhere and keeps the ink that fell off it.
pub struct Probe {
    size: Size,
    on: alloc::vec::Vec<bool>,
    pub outside: alloc::vec::Vec<Point>,
}

impl Probe {
    pub fn new(size: Size) -> Self {
        Self { size, on: alloc::vec![false; (size.width * size.height) as usize], outside: alloc::vec::Vec::new() }
    }

    /// The inked pixels on the screen.
    pub fn ink(&self) -> impl Iterator<Item = Point> + '_ {
        let w = self.size.width as usize;
        self.on.iter().enumerate().filter(|(_, on)| **on).map(move |(i, _)| Point::new((i % w) as i32, (i / w) as i32))
    }
}

impl OriginDimensions for Probe {
    fn size(&self) -> Size {
        // larger than the screen, so nothing is clipped before it is seen
        Size::new(4096, 4096)
    }
}

impl DrawTarget for Probe {
    type Color = BinaryColor;
    type Error = core::convert::Infallible;

    fn draw_iter<I: IntoIterator<Item = Pixel<BinaryColor>>>(&mut self, pixels: I) -> Result<(), Self::Error> {
        for Pixel(p, c) in pixels {
            if (0..self.size.width as i32).contains(&p.x) && (0..self.size.height as i32).contains(&p.y) {
                self.on[(p.y as u32 * self.size.width + p.x as u32) as usize] = c.is_on();
            } else if c.is_on() {
                self.outside.push(p);
            }
        }
        Ok(())
    }
}

/// Draws `v` and lists what went wrong: ink off the screen, texts or icons over each other.
pub fn check(class: Class, v: &View) -> alloc::vec::Vec<alloc::string::String> {
    let mut probe = Probe::new(class.size());
    let mut rec = Recorder::default();
    class.render_recorded(v, &mut probe, Some(&mut rec));
    let mut problems = rec.collisions;
    if let Some(p) = probe.outside.first() {
        problems.push(alloc::format!("{} pixels off the screen, the first at {p:?}", probe.outside.len()));
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;

    #[test]
    fn every_page_fits_every_class() {
        let mut failures = alloc::vec::Vec::new();
        for class in [Class::EpaperSquare, Class::Mono128x64] {
            for (name, v) in samples::all() {
                for p in check(class, &v) {
                    failures.push(alloc::format!("{class:?} {name}: {p}"));
                }
            }
        }
        assert!(failures.is_empty(), "\n{}", failures.join("\n"));
    }
}
