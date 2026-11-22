//! Wide e-paper, 250 x 122 (Heltec Wireless Paper, MeshPocket): crisp bold instrument layout with
//! top page-share bar, 1-bit icons and a compact `[◉ Action]` hold pill at the bottom.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;

use super::{alert_text, net_rows, sleep_label, title};
use crate::model::{BleState, Body, View};
use crate::text::{ago, clean, clock, duration, micro_deg, percent, permille, plural, quarter, quarter_signed, short_version, trim_float, volts};
use crate::theme::Font;
use crate::theme::wide::{BIG, BIG_BOLD, DIGITS, HUGE_DIGITS, SMALL, TEXT, TEXT_BOLD};
use crate::widgets::{Canvas, INK, PAPER, fit, width, wrap};

const MARGIN: i32 = 3;
/// The bar's rule, two pixels; two more under the current page's share.
const RULE: i32 = 18;
/// Where the page's content starts, under the bar.
const BODY_TOP: i32 = 24;
/// Rows in [`TEXT`] or [`TEXT_BOLD`]: five from [`BODY_TOP`], or four above a hint.
const ROW: i32 = 19;
/// Bottom hold-action pill geometry.
const PILL_Y: i32 = 99;
const PILL_H: i32 = 19;
/// A word in place of an empty list, in [`BIG`].
const EMPTY_Y: i32 = 60;

type C<'a, 'b, D> = &'a mut Canvas<'b, D>;

fn row_y(i: usize) -> i32 {
    BODY_TOP + i as i32 * ROW
}

fn hint<D: DrawTarget<Color = BinaryColor>>(c: C<'_, '_, D>, action: &str) {
    let max_w = c.w - 2 * MARGIN;
    c.hold_pill(&SMALL, action, MARGIN, PILL_Y, PILL_H, 7, max_w);
}

fn bar<D: DrawTarget<Color = BinaryColor>>(c: C<'_, '_, D>, v: &View) {
    let w = c.w;
    let mut end = w - MARGIN;
    if let Some(ble) = v.bar.ble {
        let ble_x = end - 10;
        c.ble_medium(ble_x, 2, ble);
        end = ble_x - 7;
    }
    if let Some(mv) = v.bar.batt_mv {
        let batt = volts(mv).replace(' ', "");
        c.right(&SMALL, &batt, end, 3);
        end -= width(&SMALL, &batt);
    }
    let title = fit(&TEXT_BOLD, &clean(title(v)), end - 8 - MARGIN);
    c.text(&TEXT_BOLD, &title, MARGIN, 1);
    c.rect(0, RULE, w as u32, 2, Some(INK));
    if v.pages > 1 {
        let (i, n) = (v.page_idx as i32, v.pages as i32);
        c.rect(i * w / n, RULE + 2, ((i + 1) * w / n - i * w / n) as u32, 2, Some(INK));
    }
}

/// Label on the left in [`TEXT`], value on the right in [`TEXT_BOLD`], a row each.
fn rows<D: DrawTarget<Color = BinaryColor>>(c: C<'_, '_, D>, rows: &[(&str, String)]) {
    let right = c.w - MARGIN;
    for (i, (label, value)) in rows.iter().enumerate() {
        c.row((&TEXT, &TEXT_BOLD), label, value, MARGIN, right, row_y(i));
    }
}

/// A label in [`SMALL`] over its value in [`TEXT_BOLD`] (in [`SMALL`] if wider than `room`), at (`x`, `y`).
fn cell<D: DrawTarget<Color = BinaryColor>>(c: C<'_, '_, D>, label: &str, value: &str, x: i32, y: i32, room: i32) {
    c.text(&SMALL, &fit(&SMALL, label, room), x, y);
    if width(&TEXT_BOLD, value) <= room {
        c.text(&TEXT_BOLD, value, x, y + 14);
    } else {
        c.text(&SMALL, &fit(&SMALL, value, room), x, y + 16);
    }
}

fn ota_body<D: DrawTarget<Color = BinaryColor>>(
    c: C<'_, '_, D>, state: &str, detail: &str, progress: Option<(u32, u32)>, version: &str, hold: Option<&str>,
) {
    let w = c.w;
    let full = w - 2 * MARGIN;
    let ver = short_version(version);
    if let Some((done, total)) = progress {
        let pct = format!("{}%", percent(done, total));
        c.text(&BIG_BOLD, &fit(&BIG_BOLD, &clean(state), full - width(&TEXT_BOLD, &pct) - 8), MARGIN, BODY_TOP + 2);
        c.right(&TEXT_BOLD, &pct, w - MARGIN, BODY_TOP + 5);
        c.progress_bar(MARGIN, BODY_TOP + 30, full, 14, done, total);
        let kb = format!("{}/{} КБ", done / 1024, total.div_ceil(1024));
        c.text(&SMALL, &kb, MARGIN, BODY_TOP + 52);
        c.right(&SMALL, &fit(&SMALL, &ver, full - width(&SMALL, &kb) - 8), w - MARGIN, BODY_TOP + 52);
        return;
    }
    c.text(&BIG_BOLD, &fit(&BIG_BOLD, &clean(state), full), MARGIN, BODY_TOP + 2);
    let mut y = BODY_TOP + 28;
    let det = clean(detail);
    if !det.is_empty() {
        y = c.lines(&TEXT, &wrap(&TEXT, &det, full, 2), MARGIN, y) + 4;
    }
    if y + SMALL.pitch <= PILL_Y - 2 || hold.is_none() {
        c.text(&SMALL, &fit(&SMALL, &ver, full), MARGIN, y);
    }
    if let Some(action) = hold {
        hint(c, action);
    }
}

pub(super) fn draw<D: DrawTarget<Color = BinaryColor>>(c: C<'_, '_, D>, v: &View) {
    let (w, h) = (c.w, c.h);
    let full = w - 2 * MARGIN;
    if let Body::Off { name } = &v.body {
        c.power_icon(w / 2, 44, 18);
        c.centered(&TEXT_BOLD, &fit(&TEXT_BOLD, &clean(name), full), 78);
        return;
    }
    bar(c, v);
    match &v.body {
        Body::Home { unread, clock: Some(at), .. } => {
            let t = clock(*at);
            let count = format!("{}", (*unread).min(999));
            let tw = width(&HUGE_DIGITS, &t);
            let cw = width(&BIG_BOLD, &count);
            let total_w = tw + 16 + 24 + 6 + cw;
            let x = (w - total_w) / 2;
            c.text(&HUGE_DIGITS, &t, x, 48);
            c.envelope(x + tw + 16, 61, 24, 17, *unread > 0);
            c.text(&BIG_BOLD, &count, x + tw + 46, 59);
        }
        Body::Home { unread, .. } => {
            if *unread > 0 {
                let digits = format!("{}", (*unread).min(999));
                let dw = width(&HUGE_DIGITS, &digits);
                let total_w = 36 + 12 + dw;
                let x = (w - total_w) / 2;
                c.envelope(x, 57, 36, 25, true);
                c.text(&HUGE_DIGITS, &digits, x + 48, 48);
            } else {
                c.envelope((w - 36) / 2, 42, 36, 25, false);
                c.centered(&BIG_BOLD, "0", 76);
            }
        }
        Body::Messages(list) => {
            if list.is_empty() {
                c.centered(&BIG, "Пусто", EMPTY_Y);
            }
            let mut y = BODY_TOP;
            for m in list.iter().take(2) {
                let room = ((h - y - 17) / TEXT.pitch).min(3);
                if room < 1 {
                    break;
                }
                let age = ago(m.age_min);
                c.text(&TEXT_BOLD, &fit(&TEXT_BOLD, &clean(&m.from), full - width(&SMALL, &age) - 8), MARGIN, y);
                c.right(&SMALL, &age, w - MARGIN, y + 2);
                y = c.lines(&TEXT, &wrap(&TEXT, &clean(&m.text), full, room as usize), MARGIN, y + 17) + 4;
            }
        }
        Body::Nearby(list) => {
            if list.is_empty() {
                c.centered(&BIG, "Пусто", EMPTY_Y);
            }
            for (i, n) in list.iter().take(5).enumerate() {
                let right = format!("{}х · {}", n.hops, ago(n.age_min));
                c.text(&TEXT_BOLD, &fit(&TEXT_BOLD, &clean(&n.name), full - width(&SMALL, &right) - 8), MARGIN, row_y(i));
                c.right(&SMALL, &right, w - MARGIN, row_y(i) + 2);
            }
        }
        Body::Radio(r) => {
            let lines = [
                (format!("{} МГц", trim_float(r.freq_mhz, 3)), format!("{} дБм", r.tx_dbm)),
                (format!("SF{}  BW {}  CR{}", r.sf, trim_float(r.bw_khz, 1), r.cr), String::new()),
                (format!("RSSI {}  SNR {}", r.last_rssi, quarter(r.last_snr)), format!("Шум {}", r.noise_dbm)),
                (format!("RX/TX {} / {}", r.rx, r.tx), String::new()),
                (format!("Аптайм {}", duration(r.uptime_min)), String::new()),
            ];
            for (i, (left, right)) in lines.iter().enumerate() {
                let font = if i == 0 { &TEXT_BOLD } else { &TEXT };
                c.row((font, &TEXT), left, right, MARGIN, w - MARGIN, row_y(i));
            }
        }
        Body::Bluetooth { state, name, pin } => {
            let (head, action) = match state {
                BleState::Off => ("Выкл", "Вкл BLE"),
                BleState::Waiting => ("Ожидание", "Выкл BLE"),
                BleState::Connected => ("Подключен", "Выкл BLE"),
            };
            c.text(&BIG_BOLD, head, MARGIN, BODY_TOP + 2);
            if let Some(pin) = pin {
                c.right(&TEXT_BOLD, &format!("PIN {pin:06}"), w - MARGIN, BODY_TOP + 4);
            }
            c.text(&TEXT, &fit(&TEXT, &clean(name), full), MARGIN, BODY_TOP + 34);
            hint(c, action);
        }
        Body::Gps(g) => {
            match (g.on, g.fix) {
                (false, _) => {
                    c.text(&BIG_BOLD, "Выкл", MARGIN, BODY_TOP + 2);
                }
                (true, None) if !g.receiving => {
                    c.text(&BIG_BOLD, "Нет связи", MARGIN, BODY_TOP + 2);
                    c.text(&TEXT, "Приемник молчит", MARGIN, BODY_TOP + 30);
                }
                (true, None) => {
                    c.text(&BIG_BOLD, "Поиск", MARGIN, BODY_TOP + 2);
                    c.sat_icon(MARGIN, BODY_TOP + 32, 14);
                    c.text(&TEXT_BOLD, &format!("{}", g.in_view), MARGIN + 20, BODY_TOP + 31);
                }
                (true, Some((lat, lon))) => {
                    c.text(&BIG_BOLD, &micro_deg(lat), MARGIN, BODY_TOP + 1);
                    c.text(&BIG_BOLD, &micro_deg(lon), MARGIN, BODY_TOP + 23);
                    c.sat_icon(MARGIN, BODY_TOP + 48, 14);
                    c.text(&TEXT_BOLD, &format!("{}", g.satellites), MARGIN + 20, BODY_TOP + 47);
                }
            }
            hint(c, if g.on { "Выкл GPS" } else { "Вкл GPS" });
        }
        Body::Advert { flood } => {
            c.beacon(30, 50, 16, *flood);
            let (head, sub) = if *flood { ("На всю сеть", "flood") } else { ("Локальный анонс", "0 хопов") };
            c.text(&BIG_BOLD, &fit(&BIG_BOLD, head, w - MARGIN - 62), 62, 34);
            c.text(&SMALL, sub, 62, 58);
            hint(c, "Отправить");
        }
        Body::PowerOff => {
            c.power_icon(30, 54, 16);
            c.text(&BIG_BOLD, "Выключить", 62, 44);
            hint(c, "Выключить");
        }
        Body::Off { .. } => {}
        Body::Repeater(p, health) => {
            c.text(&BIG_BOLD, &format!("{} МГц", trim_float(p.freq_mhz, 3)), MARGIN, BODY_TOP + 1);
            c.right(&TEXT, &format!("{} дБм", p.tx_dbm), w - MARGIN, BODY_TOP + 3);
            c.text(&TEXT, &format!("BW {}  SF{}  CR{}", trim_float(p.bw_khz, 1), p.sf, p.cr), MARGIN, BODY_TOP + 24);
            c.line(Point::new(MARGIN, BODY_TOP + 45), Point::new(w - MARGIN, BODY_TOP + 45));
            let n = health.neighbours as u32;
            let id = format!("ID {}", clean(&p.id));
            let neighbours = format!("{n} {}", plural(n, "сосед", "соседа", "соседей"));
            c.text(&BIG_BOLD, &fit(&BIG_BOLD, &neighbours, full - width(&TEXT_BOLD, &id) - 8), MARGIN, BODY_TOP + 50);
            c.right(&TEXT_BOLD, &id, w - MARGIN, BODY_TOP + 52);
            c.row((&TEXT, &TEXT_BOLD), "Аптайм", &duration(health.uptime_min), MARGIN, w - MARGIN, BODY_TOP + 74);
        }
        Body::Neighbours(list) => {
            if list.is_empty() {
                c.centered(&BIG, "Пусто", EMPTY_Y);
            }
            let shown = if list.len() > 5 { 4 } else { list.len() };
            for (i, n) in list[..shown].iter().enumerate() {
                let right = format!("{} дБ, {}", quarter_signed(n.snr), ago(n.age_min));
                c.text(&TEXT_BOLD, &fit(&TEXT_BOLD, &clean(&n.id), full - width(&SMALL, &right) - 8), MARGIN, row_y(i));
                c.right(&SMALL, &right, w - MARGIN, row_y(i) + 2);
            }
            if list.len() > shown {
                c.text(&TEXT, &format!("+{}", list.len() - shown), MARGIN, row_y(shown));
            }
        }
        Body::Traffic(t) => {
            let col = (w - 2 * MARGIN) / 2;
            let cells = [
                ("RX", format!("{}", t.rx)),
                ("TX", format!("{}", t.tx)),
                ("CRC", format!("{}", t.rx_errors)),
                ("Эфир", format!("{} / {}", permille(t.tx_air_permille), permille(t.rx_air_permille))),
                ("Очер.", format!("{}", t.queue)),
                ("Шум", format!("{} дБм", t.noise_dbm)),
            ];
            for (i, (label, value)) in cells.iter().enumerate() {
                let x = MARGIN + (i as i32 % 2) * col;
                let room = if i % 2 == 0 { col - 6 } else { w - MARGIN - x };
                cell(c, label, value, x, BODY_TOP + (i as i32 / 2) * 32, room);
            }
        }
        Body::Preset(p) => {
            let freq = trim_float(p.freq_mhz, 3);
            c.text(&DIGITS, &freq, MARGIN, BODY_TOP + 1);
            c.text(&TEXT_BOLD, "МГц", MARGIN + width(&DIGITS, &freq) + 6, BODY_TOP + 21);
            c.text(&BIG_BOLD, &format!("SF{}  CR{}", p.sf, p.cr), MARGIN, BODY_TOP + 42);
            c.right(&BIG_BOLD, &format!("BW {} кГц", trim_float(p.bw_khz, 1)), w - MARGIN, BODY_TOP + 42);
            c.text(&BIG, &format!("{} дБм", p.tx_dbm), MARGIN, BODY_TOP + 66);
        }
        Body::Power(pw) => rows(
            c,
            &[
                ("Батарея", volts(pw.batt_mv)),
                ("Сон", sleep_label(pw)),
                ("Аптайм", duration(pw.uptime_min)),
            ],
        ),
        Body::Network(n) => rows(c, &net_rows(n)),
        Body::Pull(p) => ota_body(c, &p.state, &p.detail, p.progress, &p.version, p.hold),
        Body::Update(u) => match &u.window {
            None => ota_body(c, &u.state, &u.detail, u.progress, &u.version, Some("Окно BLE")),
            Some(o) => {
                c.text(&SMALL, "BLE PIN", MARGIN, BODY_TOP);
                let pin = format!("{:06}", o.pin);
                c.text(&DIGITS, &pin, MARGIN, BODY_TOP + 15);
                let x = MARGIN + width(&DIGITS, &pin) + 14;
                c.text(&TEXT_BOLD, if o.connected { "Подкл" } else { "Ждет" }, x, BODY_TOP + 17);
                c.text(&SMALL, &format!("{}м", o.mins_left), x, BODY_TOP + 36);
                c.text(&SMALL, &fit(&SMALL, &clean(&o.name), full), MARGIN, BODY_TOP + 55);
                if !o.connected {
                    hint(c, "Закрыть");
                }
            }
        },
    }
    if v.pin.is_some() || v.alert.is_some() {
        c.rect(0, RULE + 4, w as u32, (h - RULE - 4) as u32, Some(PAPER));
    }
    if let Some(pin) = v.pin {
        c.overlay(&[("BLE PIN".into(), &BIG_BOLD), (format!("{pin:06}"), &DIGITS)], MARGIN, 2, 6);
    } else if let Some(a) = v.alert {
        let lines: Vec<(String, &Font)> = wrap(&BIG_BOLD, alert_text(a), w - 24, 2).into_iter().map(|l| (l, &BIG_BOLD)).collect();
        c.overlay(&lines, MARGIN, 2, 6);
    }
}
