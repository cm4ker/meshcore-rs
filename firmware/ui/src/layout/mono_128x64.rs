//! Monochrome 128 x 64 (Heltec OLED, Pro Micro, T096, T3-S3): a compact top bar whose rule
//! thickens under the current page's share of the width, then quiet instrument-style content and
//! a compact `[◉ Action]` hold pill on the bottom row.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;

use super::{alert_text, net_rows, sleep_label, title};
use crate::model::{BleState, Body, View};
use crate::text::{ago, clean, clock, duration, micro_deg, percent, permille, plural, quarter, quarter_signed, short_version, trim_float, volts};
use crate::theme::Font;
use crate::theme::mono::{DIGITS, MID_DIGITS, SMALL, SMALL_BOLD, TEXT, TEXT_BOLD};
use crate::widgets::{Canvas, PAPER, fit, width, wrap};

/// The rows under the bar: a heading row in [`TEXT`] or [`TEXT_BOLD`] (13 px), then [`SMALL`] (11 px).
const ROWS: [i32; 4] = [16, 29, 40, 51];
/// Four rows of [`SMALL`].
const SMALL_ROWS: [i32; 4] = [17, 28, 39, 50];
/// Bottom hold-action pill geometry.
const PILL_Y: i32 = 50;
const PILL_H: i32 = 13;

type C<'a, 'b, D> = &'a mut Canvas<'b, D>;

fn hint<D: DrawTarget<Color = BinaryColor>>(c: C<'_, '_, D>, action: &str) {
    let w = c.w;
    c.hold_pill(&SMALL_BOLD, action, 0, PILL_Y, PILL_H, 4, w);
}

fn bar<D: DrawTarget<Color = BinaryColor>>(c: C<'_, '_, D>, v: &View) {
    let w = c.w;
    let mut end = w;
    if let Some(ble) = v.bar.ble {
        let ble_x = end - 7;
        c.ble_small(ble_x, 2, ble);
        end = ble_x - 2;
    }
    if let Some(mv) = v.bar.batt_mv {
        // small and without the space, leaving the title room
        let batt = volts(mv).replace(' ', "");
        c.right(&SMALL, &batt, end - 3, 2);
        end -= 3 + width(&SMALL, &batt);
    }
    let title = fit(&TEXT_BOLD, &clean(title(v)), end - 4);
    c.text(&TEXT_BOLD, &title, 0, 0);
    c.line(Point::new(0, 13), Point::new(w - 1, 13));
    if v.pages > 1 {
        let (i, n) = (v.page_idx as i32, v.pages as i32);
        c.line(Point::new(i * w / n, 14), Point::new((i + 1) * w / n - 1, 14));
    }
}

/// Label on the left, value on the right, in [`SMALL`] rows.
fn rows<D: DrawTarget<Color = BinaryColor>>(c: C<'_, '_, D>, rows: &[(&str, String)]) {
    let right = c.w - 1;
    for ((label, value), y) in rows.iter().zip(SMALL_ROWS) {
        c.row((&SMALL, &SMALL_BOLD), label, value, 0, right, y);
    }
}

/// Shared OTA status body for companion [`Body::Pull`] and closed repeater [`Body::Update`].
fn ota_body<D: DrawTarget<Color = BinaryColor>>(
    c: C<'_, '_, D>, state: &str, detail: &str, progress: Option<(u32, u32)>, version: &str, hold: Option<&str>,
) {
    let w = c.w;
    let ver = short_version(version);
    if let Some((done, total)) = progress {
        let pct = format!("{}%", percent(done, total));
        c.text(&TEXT_BOLD, &fit(&TEXT_BOLD, &clean(state), w - width(&SMALL_BOLD, &pct) - 6), 0, 16);
        c.right(&SMALL_BOLD, &pct, w - 1, 18);
        c.progress_bar(0, 33, w, 11, done, total);
        let kb = format!("{}/{} КБ", done / 1024, total.div_ceil(1024));
        let ver_room = w - width(&SMALL, &kb) - 6;
        c.text(&SMALL, &kb, 0, 50);
        if ver_room > 16 {
            c.right(&SMALL, &fit(&SMALL, &ver, ver_room), w - 1, 50);
        }
        return;
    }
    let st = clean(state);
    let show_ver_top = width(&TEXT_BOLD, &st) + width(&SMALL, &ver) + 6 <= w;
    if show_ver_top {
        c.text(&TEXT_BOLD, &st, 0, 16);
        c.right(&SMALL, &ver, w - 1, 18);
    } else {
        c.text(&TEXT_BOLD, &fit(&TEXT_BOLD, &st, w), 0, 16);
    }
    let det = clean(detail);
    if !det.is_empty() {
        c.text(&SMALL, &fit(&SMALL, &det, w), 0, 31);
        if !show_ver_top && hold.is_none() {
            c.text(&SMALL, &fit(&SMALL, &ver, w), 0, 44);
        }
    } else if !show_ver_top {
        c.text(&SMALL, &fit(&SMALL, &ver, w), 0, 31);
    }
    if let Some(action) = hold {
        hint(c, action);
    }
}

pub(super) fn draw<D: DrawTarget<Color = BinaryColor>>(c: C<'_, '_, D>, v: &View) {
    let w = c.w;
    if let Body::Off { name } = &v.body {
        c.power_icon(w / 2, 22, 11);
        c.centered(&TEXT_BOLD, &fit(&TEXT_BOLD, &clean(name), w - 4), 43);
        return;
    }
    bar(c, v);
    match &v.body {
        Body::Home { unread, clock: Some(at), .. } => {
            c.centered(&DIGITS, &clock(*at), 17);
            let count = format!("{}", (*unread).min(999));
            let cw = width(&TEXT_BOLD, &count);
            let total_w = 13 + 5 + cw;
            let x = (w - total_w) / 2;
            c.envelope(x, 49, 13, 9, *unread > 0);
            c.text(&TEXT_BOLD, &count, x + 18, 47);
        }
        Body::Home { unread, .. } => {
            if *unread > 0 {
                let digits = format!("{}", (*unread).min(999));
                let dw = width(&DIGITS, &digits);
                let total_w = 24 + 8 + dw;
                let x = (w - total_w) / 2;
                c.envelope(x, 31, 24, 17, true);
                c.text(&DIGITS, &digits, x + 32, 26);
            } else {
                c.envelope((w - 26) / 2, 22, 26, 18, false);
                c.centered(&TEXT_BOLD, "0", 45);
            }
        }
        Body::Messages(list) => match list.first() {
            None => c.centered(&TEXT, "Пусто", 30),
            Some(m) => {
                let age = ago(m.age_min);
                c.text(&TEXT_BOLD, &fit(&TEXT_BOLD, &clean(&m.from), w - width(&SMALL, &age) - 6), 0, ROWS[0]);
                c.right(&SMALL, &age, w - 1, ROWS[0] + 1);
                let text = wrap(&TEXT, &clean(&m.text), w, 2);
                c.lines(&TEXT, &text, 0, ROWS[0] + 14);
            }
        },
        Body::Nearby(list) => {
            if list.is_empty() {
                c.centered(&TEXT, "Пусто", 30);
            }
            for (n, y) in list.iter().zip([ROWS[0], ROWS[0] + 14, ROWS[0] + 28]) {
                let hops = format!("{}х", n.hops);
                c.text(&TEXT, &fit(&TEXT, &clean(&n.name), w - width(&SMALL, &hops) - 6), 0, y);
                c.right(&SMALL, &hops, w - 1, y + 1);
            }
        }
        Body::Radio(r) => {
            let lines = [
                format!("{} МГц  {} дБм", trim_float(r.freq_mhz, 3), r.tx_dbm),
                format!("SF{}  BW {}  CR{}", r.sf, trim_float(r.bw_khz, 1), r.cr),
                format!("RSSI {}  SNR {}", r.last_rssi, quarter(r.last_snr)),
                format!("RX/TX {}/{}", r.rx, r.tx),
            ];
            for (l, y) in lines.iter().zip(SMALL_ROWS) {
                c.text(&SMALL, &fit(&SMALL, l, w), 0, y);
            }
        }
        Body::Bluetooth { state, name, pin } => {
            let (head, action) = match state {
                BleState::Off => ("Выкл", "Вкл BLE"),
                BleState::Waiting => ("Ожидание", "Выкл BLE"),
                BleState::Connected => ("Подключен", "Выкл BLE"),
            };
            if let Some(pin) = pin {
                let p = format!("{pin:06}");
                c.text(&TEXT_BOLD, &fit(&TEXT_BOLD, head, w - width(&SMALL, &p) - 4), 0, 17);
                c.right(&SMALL, &p, w - 1, 19);
            } else {
                c.text(&TEXT_BOLD, head, 0, 17);
            }
            c.text(&SMALL, &fit(&SMALL, &clean(name), w), 0, 33);
            hint(c, action);
        }
        Body::Gps(g) => {
            match (g.on, g.fix) {
                (false, _) => {
                    c.text(&TEXT_BOLD, "Выкл", 0, 17);
                }
                (true, None) if !g.receiving => {
                    c.text(&TEXT_BOLD, "Нет связи", 0, 17);
                    c.text(&SMALL, "Приемник молчит", 0, 33);
                }
                (true, None) => {
                    c.text(&TEXT_BOLD, "Поиск", 0, 17);
                    c.sat_icon(0, 32, 10);
                    c.text(&SMALL_BOLD, &format!("{}", g.in_view), 14, 33);
                }
                (true, Some((lat, lon))) => {
                    let coords = format!("{}, {}", micro_deg(lat), micro_deg(lon));
                    c.text(&SMALL_BOLD, &fit(&SMALL_BOLD, &coords, w), 0, 18);
                    c.sat_icon(0, 32, 10);
                    c.text(&SMALL_BOLD, &format!("{}", g.satellites), 14, 33);
                }
            }
            hint(c, if g.on { "Выкл GPS" } else { "Вкл GPS" });
        }
        Body::Advert { flood } => {
            c.beacon(17, 28, 10, *flood);
            let (title, sub) = if *flood { ("Вся сеть", "flood") } else { ("Рядом", "0 хопов") };
            c.text(&TEXT_BOLD, &fit(&TEXT_BOLD, title, w - 35), 35, 19);
            c.text(&SMALL, sub, 35, 34);
            hint(c, "Отправить");
        }
        Body::PowerOff => {
            c.power_icon(18, 31, 10);
            c.text(&TEXT_BOLD, "Выключить", 38, 25);
            hint(c, "Выкл");
        }
        Body::Off { .. } => {}
        Body::Repeater(p, health) => {
            let tx = format!("{} дБм", p.tx_dbm);
            c.text(&TEXT, &format!("{} МГц", trim_float(p.freq_mhz, 3)), 0, ROWS[0]);
            c.right(&SMALL, &tx, w - 1, ROWS[0] + 1);
            c.text(&SMALL, &format!("BW {}  SF{}  CR{}", trim_float(p.bw_khz, 1), p.sf, p.cr), 0, ROWS[1]);
            let n = health.neighbours as u32;
            let rest = format!("ID {}", clean(&p.id));
            let neighbours = format!("{n} {}", plural(n, "сосед", "соседа", "соседей"));
            let room = w - width(&SMALL, &rest) - 6;
            let font = if width(&TEXT_BOLD, &neighbours) <= room { &TEXT_BOLD } else { &SMALL_BOLD };
            c.text(font, &fit(font, &neighbours, room), 0, if core::ptr::eq(font, &TEXT_BOLD) { 43 } else { 44 });
            c.right(&SMALL, &rest, w - 1, 44);
        }
        Body::Neighbours(list) => {
            if list.is_empty() {
                c.centered(&TEXT, "Пусто", 30);
            }
            let shown = if list.len() > 4 { 3 } else { list.len() };
            for (n, y) in list[..shown].iter().zip(SMALL_ROWS) {
                let right = format!("{} дБ, {}", quarter_signed(n.snr), ago(n.age_min));
                c.text(&SMALL_BOLD, &fit(&SMALL_BOLD, &clean(&n.id), w - width(&SMALL, &right) - 6), 0, y);
                c.right(&SMALL, &right, w - 1, y);
            }
            if list.len() > shown {
                c.text(&SMALL, &format!("+{}", list.len() - shown), 0, SMALL_ROWS[3]);
            }
        }
        Body::Traffic(t) => rows(
            c,
            &[
                ("RX", format!("{}", t.rx)),
                ("TX", format!("{}", t.tx)),
                ("CRC", format!("{}", t.rx_errors)),
                ("Эфир", format!("{} / {}", permille(t.tx_air_permille), permille(t.rx_air_permille))),
            ],
        ),
        Body::Preset(p) => {
            let freq = trim_float(p.freq_mhz, 3);
            c.text(&MID_DIGITS, &freq, 0, ROWS[0] + 1);
            c.text(&SMALL, "МГц", width(&MID_DIGITS, &freq) + 4, ROWS[0] + 7);
            c.text(&TEXT_BOLD, &fit(&TEXT_BOLD, &format!("SF{} CR{} BW {}", p.sf, p.cr, trim_float(p.bw_khz, 1)), w), 0, 35);
            c.text(&SMALL, &format!("{} дБм", p.tx_dbm), 0, ROWS[3]);
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
                c.text(&SMALL, "PIN", 0, ROWS[0] + 6);
                c.text(&MID_DIGITS, &format!("{:06}", o.pin), 24, ROWS[0] + 1);
                let state = format!("{} · {}м", if o.connected { "Подкл" } else { "Ждет" }, o.mins_left);
                c.text(&SMALL, &state, 0, 36);
                if !o.connected {
                    hint(c, "Закрыть");
                }
            }
        },
    }
    if v.pin.is_some() || v.alert.is_some() {
        // a box over the page would leave bits of its text showing around it on so small a screen
        c.rect(0, 15, w as u32, (c.h - 15) as u32, Some(PAPER));
    }
    if let Some(pin) = v.pin {
        c.overlay(&[("BLE PIN".into(), &SMALL), (format!("{pin:06}"), &DIGITS)], 2, 1, 3);
    } else if let Some(a) = v.alert {
        let lines: Vec<(String, &Font)> = wrap(&TEXT_BOLD, alert_text(a), w - 12, 2).into_iter().map(|l| (l, &TEXT_BOLD)).collect();
        c.overlay(&lines, 2, 1, 4);
    }
}
