//! Square e-paper, 200 x 200 (T-Echo), in calm instrument style: a bar on top, generous breathing
//! room, 1-bit icons, a single `[◉ Action]` pill above the page dots.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;

use super::{alert_text, net_rows, sleep_label, title};
use crate::model::{BleState, Body, View};
use crate::text::{ago, clean, clock, duration, micro_deg, percent, permille, plural, quarter, quarter_signed, short_version, trim_float, volts};
use crate::theme::Font;
use crate::theme::square::{BIG, BIG_BOLD, FREQ_DIGITS, HUGE_DIGITS, SMALL, TEXT, TEXT_BOLD};
use crate::widgets::{Canvas, PAPER, fit, width, wrap};

const MARGIN: i32 = 4;
/// Where the page's content starts, under the bar.
const BODY_TOP: i32 = 30;
/// Hold-action pill geometry, above the page dots.
const PILL_Y: i32 = 152;
const PILL_H: i32 = 22;
/// List rows.
const ROW: i32 = 22;

type C<'a, 'b, D> = &'a mut Canvas<'b, D>;

fn hint<D: DrawTarget<Color = BinaryColor>>(c: C<'_, '_, D>, action: &str) {
    let max_w = c.w - 2 * MARGIN;
    c.hold_pill(&TEXT_BOLD, action, MARGIN, PILL_Y, PILL_H, 8, max_w);
}

fn hint_centered<D: DrawTarget<Color = BinaryColor>>(c: C<'_, '_, D>, action: &str) {
    let max_w = c.w - 2 * MARGIN;
    c.hold_pill_centered(&TEXT_BOLD, action, PILL_Y, PILL_H, 8, max_w);
}

fn bar<D: DrawTarget<Color = BinaryColor>>(c: C<'_, '_, D>, v: &View) {
    let w = c.w;
    let mut end = w - MARGIN;
    if let Some(ble) = v.bar.ble {
        let ble_x = end - 12;
        c.ble_large(ble_x, 4, ble);
        end = ble_x - 3;
    }
    if let Some(mv) = v.bar.batt_mv {
        let batt = volts(mv).replace(' ', "");
        c.right(&SMALL, &batt, end - 4, 8);
        end -= 4 + width(&SMALL, &batt);
    }
    let title_end = end - 3;
    let title = fit(&TEXT_BOLD, &clean(title(v)), title_end - MARGIN);
    c.text(&TEXT_BOLD, &title, MARGIN, 4);
    c.line(Point::new(0, 25), Point::new(w - 1, 25));
    c.line(Point::new(0, 26), Point::new(w - 1, 26));
}

/// Label on the left, value on the right, in list rows.
fn rows<D: DrawTarget<Color = BinaryColor>>(c: C<'_, '_, D>, rows: &[(&str, String)], y: i32) {
    let right = c.w - MARGIN;
    for (i, (label, value)) in rows.iter().enumerate() {
        c.row((&TEXT, &TEXT_BOLD), label, value, MARGIN, right, y + i as i32 * ROW);
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
        c.text(&BIG_BOLD, &fit(&BIG_BOLD, &clean(state), full - width(&TEXT_BOLD, &pct) - 8), MARGIN, BODY_TOP + 6);
        c.right(&TEXT_BOLD, &pct, w - MARGIN, BODY_TOP + 10);
        c.progress_bar(MARGIN, BODY_TOP + 42, full, 18, done, total);
        c.text(&TEXT, &format!("{} / {} КБ", done / 1024, total.div_ceil(1024)), MARGIN, BODY_TOP + 72);
        c.text(&SMALL, &fit(&SMALL, &ver, full), MARGIN, BODY_TOP + 98);
        return;
    }
    c.text(&BIG_BOLD, &fit(&BIG_BOLD, &clean(state), full), MARGIN, BODY_TOP + 4);
    let mut y = BODY_TOP + 34;
    let det = clean(detail);
    if !det.is_empty() {
        let text = wrap(&TEXT, &det, full, 3);
        y = c.lines(&TEXT, &text, MARGIN, y) + 6;
    }
    c.text(&SMALL, &fit(&SMALL, &ver, full), MARGIN, y);
    if let Some(action) = hold {
        hint(c, action);
    }
}

pub(super) fn draw<D: DrawTarget<Color = BinaryColor>>(c: C<'_, '_, D>, v: &View) {
    let (w, h) = (c.w, c.h);
    let full = w - 2 * MARGIN;
    if let Body::Off { name } = &v.body {
        c.power_icon(w / 2, 74, 24);
        c.centered(&TEXT_BOLD, &fit(&TEXT_BOLD, &clean(name), full), 120);
        return;
    }
    bar(c, v);
    match &v.body {
        Body::Home { unread, clock: Some(at), .. } => {
            c.centered(&HUGE_DIGITS, &clock(*at), 60);
            let count = format!("{}", (*unread).min(999));
            let cw = width(&BIG_BOLD, &count);
            let total_w = 22 + 8 + cw;
            let x = (w - total_w) / 2;
            c.envelope(x, 124, 22, 16, *unread > 0);
            c.text(&BIG_BOLD, &count, x + 30, 120);
        }
        Body::Home { unread, .. } => {
            c.envelope((w - 40) / 2, 54, 40, 28, *unread > 0);
            let digits = format!("{}", (*unread).min(999));
            c.centered(&HUGE_DIGITS, &digits, 98);
        }
        Body::Messages(list) => {
            if list.is_empty() {
                c.centered(&BIG, "Пусто", 86);
            }
            let mut y = BODY_TOP + 2;
            for m in list.iter().take(2) {
                let age = ago(m.age_min);
                let name_w = full - width(&SMALL, &age) - 8;
                c.text(&TEXT_BOLD, &fit(&TEXT_BOLD, &clean(&m.from), name_w), MARGIN, y);
                c.right(&SMALL, &age, w - MARGIN, y + 3);
                y = c.lines(&TEXT, &wrap(&TEXT, &clean(&m.text), full, 2), MARGIN, y + 21) + 8;
            }
        }
        Body::Nearby(list) => {
            if list.is_empty() {
                c.centered(&BIG, "Пусто", 86);
            }
            let mut y = BODY_TOP + 2;
            for n in list.iter().take(6) {
                let right = format!("{}х · {}", n.hops, ago(n.age_min));
                let name_w = full - width(&SMALL, &right) - 8;
                c.text(&TEXT, &fit(&TEXT, &clean(&n.name), name_w), MARGIN, y);
                c.right(&SMALL, &right, w - MARGIN, y + 3);
                y += ROW;
            }
        }
        Body::Radio(r) => {
            let lines = [
                format!("{} МГц  {} дБм", trim_float(r.freq_mhz, 3), r.tx_dbm),
                format!("SF{}  BW {}  CR{}", r.sf, trim_float(r.bw_khz, 1), r.cr),
                format!("RSSI {}  SNR {}", r.last_rssi, quarter(r.last_snr)),
                format!("Шум {} дБм", r.noise_dbm),
                format!("RX/TX {} / {}", r.rx, r.tx),
                format!("Аптайм {}", duration(r.uptime_min)),
            ];
            let lines: Vec<String> = lines.iter().map(|l| fit(&TEXT, l, full)).collect();
            c.lines(&TEXT, &lines, MARGIN, BODY_TOP + 4);
        }
        Body::Bluetooth { state, name, pin } => {
            let (head, action) = match state {
                BleState::Off => ("Выкл", "Вкл BLE"),
                BleState::Waiting => ("Ожидание", "Выкл BLE"),
                BleState::Connected => ("Подключен", "Выкл BLE"),
            };
            c.text(&BIG_BOLD, head, MARGIN, BODY_TOP + 4);
            c.text(&SMALL, &fit(&SMALL, &clean(name), full), MARGIN, BODY_TOP + 38);
            if let Some(pin) = pin {
                c.text(&TEXT_BOLD, &format!("PIN {pin:06}"), MARGIN, BODY_TOP + 64);
            }
            hint(c, action);
        }
        Body::Gps(g) => {
            match (g.on, g.fix) {
                (false, _) => {
                    c.text(&BIG_BOLD, "Выкл", MARGIN, BODY_TOP + 4);
                }
                (true, None) if !g.receiving => {
                    c.text(&BIG_BOLD, "Нет связи", MARGIN, BODY_TOP + 4);
                    c.text(&TEXT, "Приемник молчит", MARGIN, BODY_TOP + 36);
                }
                (true, None) => {
                    c.text(&BIG_BOLD, "Поиск", MARGIN, BODY_TOP + 4);
                    c.sat_icon(MARGIN, BODY_TOP + 38, 16);
                    c.text(&TEXT_BOLD, &format!("{}", g.in_view), MARGIN + 22, BODY_TOP + 37);
                }
                (true, Some((lat, lon))) => {
                    c.text(&BIG, &micro_deg(lat), MARGIN, BODY_TOP + 4);
                    c.text(&BIG, &micro_deg(lon), MARGIN, BODY_TOP + 32);
                    c.sat_icon(MARGIN, BODY_TOP + 66, 16);
                    c.text(&TEXT_BOLD, &format!("{}", g.satellites), MARGIN + 22, BODY_TOP + 65);
                }
            }
            hint(c, if g.on { "Выкл GPS" } else { "Вкл GPS" });
        }
        Body::Advert { flood } => {
            c.beacon(w / 2, 66, 22, *flood);
            c.centered(&BIG_BOLD, if *flood { "На всю сеть" } else { "Локальный" }, 108);
            hint_centered(c, "Отправить");
        }
        Body::PowerOff => {
            c.power_icon(w / 2, 72, 24);
            c.centered(&BIG_BOLD, "Выключить", 112);
            hint_centered(c, "Выключить");
        }
        Body::Off { .. } => {}
        Body::Repeater(p, health) => {
            c.text(&BIG_BOLD, &format!("{} МГц", trim_float(p.freq_mhz, 3)), MARGIN, BODY_TOP + 2);
            c.text(&TEXT, &format!("BW {}  SF{}  CR{}", trim_float(p.bw_khz, 1), p.sf, p.cr), MARGIN, BODY_TOP + 30);
            c.text(&TEXT, &format!("{} дБм", p.tx_dbm), MARGIN, BODY_TOP + 52);
            c.line(Point::new(MARGIN, BODY_TOP + 78), Point::new(w - MARGIN, BODY_TOP + 78));
            let n = health.neighbours as u32;
            rows(
                c,
                &[
                    (plural(n, "Сосед", "Соседа", "Соседей"), format!("{n}")),
                    ("ID", clean(&p.id)),
                    ("Аптайм", duration(health.uptime_min)),
                ],
                BODY_TOP + 84,
            );
        }
        Body::Neighbours(list) => {
            if list.is_empty() {
                c.centered(&BIG, "Пусто", 86);
            }
            let shown = if list.len() > 6 { 5 } else { list.len() };
            let mut y = BODY_TOP + 2;
            for n in &list[..shown] {
                let right = format!("{} дБ, {}", quarter_signed(n.snr), ago(n.age_min));
                c.text(&TEXT_BOLD, &fit(&TEXT_BOLD, &clean(&n.id), full - width(&SMALL, &right) - 8), MARGIN, y);
                c.right(&SMALL, &right, w - MARGIN, y + 3);
                y += ROW;
            }
            if list.len() > shown {
                c.text(&TEXT, &format!("+{}", list.len() - shown), MARGIN, y);
            }
        }
        Body::Traffic(t) => rows(
            c,
            &[
                ("RX", format!("{}", t.rx)),
                ("TX", format!("{}", t.tx)),
                ("CRC", format!("{}", t.rx_errors)),
                ("Эфир TX", permille(t.tx_air_permille)),
                ("Эфир RX", permille(t.rx_air_permille)),
                ("Шум", format!("{} дБм", t.noise_dbm)),
                ("Очер.", format!("{}", t.queue)),
            ],
            BODY_TOP + 2,
        ),
        Body::Preset(p) => {
            c.text(&FREQ_DIGITS, &trim_float(p.freq_mhz, 3), MARGIN, BODY_TOP + 4);
            c.text(&TEXT, "МГц", MARGIN, BODY_TOP + 42);
            c.text(&BIG_BOLD, &format!("SF{}  CR{}", p.sf, p.cr), MARGIN, BODY_TOP + 64);
            c.text(&TEXT_BOLD, &format!("BW {} кГц", trim_float(p.bw_khz, 1)), MARGIN, BODY_TOP + 92);
            c.text(&TEXT, &format!("{} дБм", p.tx_dbm), MARGIN, BODY_TOP + 114);
        }
        Body::Power(pw) => rows(
            c,
            &[
                ("Батарея", volts(pw.batt_mv)),
                ("Сон", sleep_label(pw)),
                ("Аптайм", duration(pw.uptime_min)),
            ],
            BODY_TOP + 4,
        ),
        Body::Network(n) => rows(c, &net_rows(n), BODY_TOP + 4),
        Body::Pull(p) => ota_body(c, &p.state, &p.detail, p.progress, &p.version, p.hold),
        Body::Update(u) => match &u.window {
            None => ota_body(c, &u.state, &u.detail, u.progress, &u.version, Some("Окно BLE")),
            Some(o) => {
                c.centered(&SMALL, "BLE PIN", BODY_TOP + 2);
                c.centered(&HUGE_DIGITS, &format!("{:06}", o.pin), BODY_TOP + 20);
                let state = format!("{} · {}м", if o.connected { "Подкл" } else { "Ждет" }, o.mins_left);
                c.centered(&TEXT_BOLD, &state, BODY_TOP + 68);
                c.centered(&SMALL, &fit(&SMALL, &clean(&o.name), full), BODY_TOP + 94);
                if !o.connected {
                    hint_centered(c, "Закрыть");
                }
            }
        },
    }
    c.dots(v.page_idx, v.pages, h - 8, 14, 9, 6);
    if v.pin.is_some() || v.alert.is_some() {
        c.rect(0, 27, w as u32, (h - 27) as u32, Some(PAPER));
    }
    if let Some(pin) = v.pin {
        c.overlay(&[("BLE PIN".into(), &TEXT), (format!("{pin:06}"), &HUGE_DIGITS)], 4, 3, 9);
    } else if let Some(a) = v.alert {
        let lines: Vec<(String, &Font)> = wrap(&BIG_BOLD, alert_text(a), w - 24, 3).into_iter().map(|l| (l, &BIG_BOLD)).collect();
        c.overlay(&lines, 4, 3, 9);
    }
}
