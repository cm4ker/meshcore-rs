//! The interface's pages on the T-Echo's 200 x 200 e-paper, in Russian and in large type: a bar
//! on top (page title, messages the app has not fetched, Bluetooth, battery), the page, the page
//! dots at the bottom.
//!
//! The text is in U8g2's crox fonts (proportional, Helvetica-like), which keep Cyrillic where
//! code page 1251 has it, in place of Latin-1: [`cp1251`] moves it there.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Circle, Line, Polyline, PrimitiveStyle, Rectangle};
use meshcore_fw_core::ui::{Alert, BleState, Body, Page, View};
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};
use u8g2_fonts::{FontRenderer, fonts};

use crate::epaper::{Frame, HEIGHT, WIDTH};

const W: i32 = WIDTH as i32;
const H: i32 = HEIGHT as i32;
const MARGIN: i32 = 4;
const INK: BinaryColor = BinaryColor::On;
const PAPER: BinaryColor = BinaryColor::Off;
/// Where the page's content starts, under the bar.
const BODY_TOP: i32 = 30;
/// The top of a long-press hint, above the page dots.
const HINT_Y: i32 = 146;

static SMALL: FontRenderer = FontRenderer::new::<fonts::u8g2_font_crox3h_tf>();
static TEXT: FontRenderer = FontRenderer::new::<fonts::u8g2_font_crox4h_tf>();
static TEXT_BOLD: FontRenderer = FontRenderer::new::<fonts::u8g2_font_crox4hb_tf>();
static BIG: FontRenderer = FontRenderer::new::<fonts::u8g2_font_crox5h_tf>();
static BIG_BOLD: FontRenderer = FontRenderer::new::<fonts::u8g2_font_crox5hb_tf>();
/// Digits only (the pairing PIN).
static HUGE_DIGITS: FontRenderer = FontRenderer::new::<fonts::u8g2_font_inr33_t_cyrillic>();

/// Line pitch of each font.
fn pitch(font: &FontRenderer) -> i32 {
    if core::ptr::eq(font, &SMALL) {
        15
    } else if core::ptr::eq(font, &TEXT) || core::ptr::eq(font, &TEXT_BOLD) {
        20
    } else {
        25
    }
}

/// What the fonts can draw (ASCII and Cyrillic; Ё as Е, the fonts lack it); the rest (emoji,
/// accented Latin) is dropped.
fn clean(s: &str) -> String {
    let kept: String = s
        .chars()
        .map(|c| match c {
            'Ё' => 'Е',
            'ё' => 'е',
            c if c.is_whitespace() => ' ',
            c => c,
        })
        .filter(|&c| matches!(c, ' '..='~' | 'А'..='я'))
        .collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cyrillic to where the crox fonts keep it (code page 1251 in the Latin-1 range).
fn cp1251(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'А'..='я' => char::from_u32(0xC0 + c as u32 - 'А' as u32).unwrap_or('?'),
            'Ё' => 'Е',
            'ё' => 'е',
            c => c,
        })
        .collect()
}

fn width(font: &FontRenderer, s: &str) -> i32 {
    font.get_rendered_dimensions(cp1251(s).as_str(), Point::zero(), VerticalPosition::Top)
        .map(|d| d.advance.x)
        .unwrap_or(0)
}

/// `s` cut to `max` pixels, with "..." if it was longer.
fn fit(font: &FontRenderer, s: &str, max: i32) -> String {
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
fn wrap(font: &FontRenderer, text: &str, max: i32, max_lines: usize) -> Vec<String> {
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

fn ago(min: u32) -> String {
    match min {
        0 => "сейчас".into(),
        1..60 => format!("{min} мин"),
        60..1440 => format!("{} ч", min / 60),
        _ => format!("{} д", min / 1440),
    }
}

fn duration(min: u32) -> String {
    match min {
        0..60 => format!("{min} мин"),
        60..1440 => format!("{} ч {} мин", min / 60, min % 60),
        _ => format!("{} д {} ч", min / 1440, min % 1440 / 60),
    }
}

/// x.y from a value multiplied by 4 (SNR).
fn quarter(v: i16) -> String {
    let tenths = v as i32 * 10 / 4;
    let sign = if tenths < 0 { "-" } else { "" };
    format!("{sign}{}.{}", tenths.abs() / 10, tenths.abs() % 10)
}

fn trim_float(v: f32, decimals: usize) -> String {
    let s = format!("{v:.decimals$}");
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').into() } else { s }
}

fn micro_deg(v: i32) -> String {
    let sign = if v < 0 { "-" } else { "" };
    format!("{sign}{}.{:05}", v.unsigned_abs() / 1_000_000, v.unsigned_abs() % 1_000_000 / 10)
}

struct Canvas {
    f: Frame,
}

impl Canvas {
    fn draw(&mut self, font: &FontRenderer, s: &str, x: i32, y: i32, align: HorizontalAlignment, color: BinaryColor) {
        let _ = font.render_aligned(
            cp1251(s).as_str(),
            Point::new(x, y),
            VerticalPosition::Top,
            align,
            FontColor::Transparent(color),
            &mut self.f,
        );
    }

    fn text(&mut self, font: &FontRenderer, s: &str, x: i32, y: i32) {
        self.draw(font, s, x, y, HorizontalAlignment::Left, INK);
    }

    fn right(&mut self, font: &FontRenderer, s: &str, y: i32) {
        self.draw(font, s, W - MARGIN, y, HorizontalAlignment::Right, INK);
    }

    fn centered(&mut self, font: &FontRenderer, s: &str, y: i32) {
        self.draw(font, s, W / 2, y, HorizontalAlignment::Center, INK);
    }

    /// Lines of text from `y` on; returns where the next line would go.
    fn lines(&mut self, font: &FontRenderer, lines: &[String], y: i32) -> i32 {
        let mut y = y;
        for l in lines {
            self.text(font, l, MARGIN, y);
            y += pitch(font);
        }
        y
    }

    fn line(&mut self, a: Point, b: Point) {
        let _ = Line::new(a, b).into_styled(PrimitiveStyle::with_stroke(INK, 1)).draw(&mut self.f);
    }

    fn rect(&mut self, x: i32, y: i32, w: u32, h: u32, fill: Option<BinaryColor>) {
        let style = match fill {
            Some(c) => PrimitiveStyle::with_fill(c),
            None => PrimitiveStyle::with_stroke(INK, 1),
        };
        let _ = Rectangle::new(Point::new(x, y), Size::new(w, h)).into_styled(style).draw(&mut self.f);
    }

    fn hint(&mut self, action: &str) {
        self.text(&SMALL, "Удерживайте кнопку:", MARGIN, HINT_Y);
        self.text(&TEXT_BOLD, action, MARGIN, HINT_Y + 15);
    }

    fn battery(&mut self, x: i32, y: i32, pct: u8) {
        self.rect(x, y, 26, 14, None);
        self.rect(x + 1, y + 1, 24, 12, None);
        self.rect(x + 26, y + 4, 3, 6, Some(INK));
        let fill = 20 * pct.min(100) as u32 / 100;
        if fill > 0 {
            self.rect(x + 3, y + 3, fill, 8, Some(INK));
        }
    }

    fn ble(&mut self, x: i32, y: i32, state: BleState) {
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
            let _ = Polyline::new(&shifted).into_styled(PrimitiveStyle::with_stroke(color, 1)).draw(&mut self.f);
        }
        if state == BleState::Off {
            self.line(Point::new(x - 2, y + 18), Point::new(x + 10, y - 1));
            self.line(Point::new(x - 1, y + 18), Point::new(x + 11, y - 1));
        }
    }

    fn envelope(&mut self, x: i32, y: i32) {
        self.rect(x, y, 20, 14, None);
        self.line(Point::new(x, y), Point::new(x + 10, y + 8));
        self.line(Point::new(x + 10, y + 8), Point::new(x + 19, y));
    }

    fn bar(&mut self, v: &View) {
        let battery_x = W - MARGIN - 29;
        self.battery(battery_x, 6, v.bar.battery_pct);
        let ble_x = battery_x - 18;
        self.ble(ble_x, 4, v.bar.ble);
        let mut title_end = ble_x - 8;
        // on the home page only: elsewhere the title needs the room
        if v.bar.waiting > 0 && v.page == Page::Home {
            let count = format!("{}", v.bar.waiting.min(99));
            let x = ble_x - 8 - width(&TEXT_BOLD, &count);
            self.text(&TEXT_BOLD, &count, x, 4);
            self.envelope(x - 23, 6);
            title_end = x - 28;
        }
        let title = match (&v.page, &v.body) {
            (_, Body::Home { name, .. }) => clean(name),
            (Page::Messages, _) => "Сообщения".into(),
            (Page::Nearby, _) => "Рядом".into(),
            (Page::Radio, _) => "Радио".into(),
            (Page::Bluetooth, _) => "Bluetooth".into(),
            (Page::Gps, _) => "GPS".into(),
            (Page::Advert, _) => "Объявить".into(),
            (Page::PowerOff, _) => "Выключить".into(),
            (Page::Home, _) => String::new(),
        };
        let title = fit(&TEXT_BOLD, &title, title_end - MARGIN);
        self.text(&TEXT_BOLD, &title, MARGIN, 4);
        self.line(Point::new(0, 25), Point::new(W - 1, 25));
        self.line(Point::new(0, 26), Point::new(W - 1, 26));
    }

    fn dots(&mut self, idx: u8, n: u8) {
        let pitch = 14;
        let x0 = W / 2 - (n as i32 - 1) * pitch / 2;
        for i in 0..n as i32 {
            let c = Point::new(x0 + i * pitch, H - 8);
            let current = i == idx as i32;
            let style = if current { PrimitiveStyle::with_fill(INK) } else { PrimitiveStyle::with_stroke(INK, 1) };
            let _ = Circle::with_center(c, if current { 9 } else { 6 }).into_styled(style).draw(&mut self.f);
        }
    }

    /// A box over the middle of the page.
    fn overlay(&mut self, lines: &[(String, &'static FontRenderer)]) {
        let height: i32 = lines.iter().map(|(_, f)| pitch(f) + if core::ptr::eq(*f, &HUGE_DIGITS) { 14 } else { 0 }).sum::<i32>() + 18;
        let top = (H - height) / 2;
        self.rect(4, top, (W - 8) as u32, height as u32, Some(PAPER));
        for i in 0..3 {
            self.rect(4 + i, top + i, (W - 8 - 2 * i) as u32, (height - 2 * i) as u32, None);
        }
        let mut y = top + 9;
        for (s, f) in lines {
            self.centered(f, s, y);
            y += pitch(f) + if core::ptr::eq(*f, &HUGE_DIGITS) { 14 } else { 0 };
        }
    }
}

fn alert_text(a: Alert) -> &'static str {
    match a {
        Alert::AdvertSent => "Объявление отправлено",
        Alert::AdvertFailed => "Не удалось отправить",
        Alert::BleOn => "Bluetooth включен",
        Alert::BleOff => "Bluetooth выключен",
        Alert::GpsOn => "GPS включен",
        Alert::GpsOff => "GPS выключен",
    }
}

pub fn render(v: &View) -> Frame {
    let mut c = Canvas { f: Frame::blank() };
    let full = W - 2 * MARGIN;
    if let Body::Off { name } = &v.body {
        c.centered(&BIG_BOLD, "Выключено", 50);
        c.centered(&TEXT_BOLD, &fit(&TEXT_BOLD, &clean(name), full), 88);
        c.centered(&TEXT, "Нажмите кнопку,", 135);
        c.centered(&TEXT, "чтобы включить", 155);
        return c.f;
    }
    c.bar(v);
    match &v.body {
        Body::Home { last, .. } => match last {
            Some(m) => {
                c.text(&TEXT_BOLD, &fit(&TEXT_BOLD, &clean(&m.from), full), MARGIN, BODY_TOP + 2);
                c.lines(&BIG, &wrap(&BIG, &clean(&m.text), full, 5), BODY_TOP + 24);
            }
            None => c.centered(&BIG, "Сообщений нет", 80),
        },
        Body::Messages(list) => {
            if list.is_empty() {
                c.centered(&BIG, "Сообщений нет", 80);
            }
            let mut y = BODY_TOP + 2;
            for m in list.iter().take(2) {
                let age = ago(m.age_min);
                let name_w = full - width(&SMALL, &age) - 8;
                c.text(&TEXT_BOLD, &fit(&TEXT_BOLD, &clean(&m.from), name_w), MARGIN, y);
                c.right(&SMALL, &age, y + 3);
                y = c.lines(&TEXT, &wrap(&TEXT, &clean(&m.text), full, 2), y + 21) + 8;
            }
        }
        Body::Nearby(list) => {
            if list.is_empty() {
                c.centered(&BIG, "Пока никого", 80);
            }
            let mut y = BODY_TOP + 2;
            for n in list.iter().take(6) {
                let hops = if n.hops == 0 { "прямо".into() } else { format!("{} хоп", n.hops) };
                let right = format!("{hops}, {}", ago(n.age_min));
                let name_w = full - width(&SMALL, &right) - 8;
                c.text(&TEXT, &fit(&TEXT, &clean(&n.name), name_w), MARGIN, y);
                c.right(&SMALL, &right, y + 3);
                y += 22;
            }
        }
        Body::Radio(r) => {
            let lines = [
                format!("{} МГц", trim_float(r.freq_mhz, 3)),
                format!("SF{}  BW {}  CR{}", r.sf, trim_float(r.bw_khz, 1), r.cr),
                format!("{} дБм, шум {}", r.tx_dbm, r.noise_dbm),
                format!("RSSI {}  SNR {}", r.last_rssi, quarter(r.last_snr)),
                format!("Пакеты {} / {}", r.rx, r.tx),
                format!("Батарея {}% {}.{:02} В", r.battery_pct, r.batt_mv / 1000, r.batt_mv % 1000 / 10),
                format!("Работает {}", duration(r.uptime_min)),
            ];
            c.lines(&TEXT, &lines, BODY_TOP + 1);
        }
        Body::Bluetooth { state, name, pin } => {
            let (head, link, action) = match state {
                BleState::Off => ("Выключен", "", "включить"),
                BleState::Waiting => ("Включен", "Телефон не подключен", "выключить"),
                BleState::Connected => ("Включен", "Телефон подключен", "выключить"),
            };
            c.text(&BIG_BOLD, head, MARGIN, BODY_TOP + 2);
            c.text(&TEXT, link, MARGIN, BODY_TOP + 32);
            c.text(&SMALL, &fit(&SMALL, &clean(name), full), MARGIN, BODY_TOP + 60);
            if let Some(pin) = pin {
                c.text(&TEXT_BOLD, &format!("PIN {pin:06}"), MARGIN, BODY_TOP + 82);
            }
            c.hint(action);
        }
        Body::Gps(g) => {
            match (g.on, g.fix) {
                (false, _) => {
                    c.text(&BIG_BOLD, "Выключен", MARGIN, BODY_TOP + 2);
                }
                (true, None) if !g.receiving => {
                    c.text(&BIG_BOLD, "Нет связи", MARGIN, BODY_TOP + 2);
                    c.text(&TEXT, "Приемник молчит", MARGIN, BODY_TOP + 34);
                }
                (true, None) => {
                    c.text(&BIG_BOLD, "Ищет", MARGIN, BODY_TOP + 2);
                    c.text(&TEXT, &format!("Видно спутников: {}", g.in_view), MARGIN, BODY_TOP + 34);
                    if g.in_view == 0 {
                        c.text(&SMALL, "Нужно открытое небо", MARGIN, BODY_TOP + 60);
                    }
                }
                (true, Some((lat, lon))) => {
                    c.text(&BIG, &micro_deg(lat), MARGIN, BODY_TOP + 2);
                    c.text(&BIG, &micro_deg(lon), MARGIN, BODY_TOP + 30);
                    c.text(&TEXT, &format!("Спутников: {}", g.satellites), MARGIN, BODY_TOP + 64);
                }
            }
            c.hint(if g.on { "выключить" } else { "включить" });
        }
        Body::Advert => {
            let text = wrap(&TEXT, "Узлы рядом узнают, что вы в сети, и смогут добавить вас в контакты.", full, 6);
            c.lines(&TEXT, &text, BODY_TOP + 2);
            c.hint("отправить");
        }
        Body::PowerOff => {
            let text = wrap(&TEXT, "Экран сохранит надпись \"Выключено\". Включить снова - кнопкой.", full, 6);
            c.lines(&TEXT, &text, BODY_TOP + 2);
            c.hint("выключить");
        }
        Body::Off { .. } => {}
    }
    c.dots(v.page_idx, v.pages);
    if let Some(pin) = v.pin {
        c.overlay(&[("PIN для телефона".into(), &TEXT), (format!("{pin:06}"), &HUGE_DIGITS)]);
    } else if let Some(a) = v.alert {
        let lines: Vec<(String, &'static FontRenderer)> =
            wrap(&BIG_BOLD, alert_text(a), W - 24, 3).into_iter().map(|l| (l, &BIG_BOLD)).collect();
        c.overlay(&lines);
    }
    c.f
}
