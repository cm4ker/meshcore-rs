//! Monochrome 128 x 64 (Heltec OLED): a bar on top (page title, the node's ID or Bluetooth)
//! whose rule thickens under the current page's share of the width (page dots would
//! take a row), then four short rows. A long-press hint takes the last row.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;

use super::{alert_text, title};
use crate::model::{BleState, Body, View, Wifi};
use crate::text::{ago, clean, duration, duration_short, micro_deg, permille, plural, quarter, quarter_signed, trim_float, volts};
use crate::theme::Font;
use crate::theme::mono::{BIG_BOLD, DIGITS, MID_DIGITS, SMALL, SMALL_BOLD, TEXT, TEXT_BOLD};
use crate::widgets::{Canvas, fit, width, wrap};

/// The rows under the bar: a heading row in [`TEXT`] or [`TEXT_BOLD`] (13 px), then rows in
/// [`SMALL`] (11 px).
const ROWS: [i32; 4] = [16, 29, 40, 51];
/// The longest contact link the class shows as a QR code (version 7 with low error correction:
/// 49 px with its margin, the height under the bar).
pub const CONTACT_LINK_MAX: usize = 154;

/// Four rows of [`SMALL`].
const SMALL_ROWS: [i32; 4] = [17, 28, 39, 50];

type C<'a, 'b, D> = &'a mut Canvas<'b, D>;

fn hint<D: DrawTarget<Color = BinaryColor>>(c: C<'_, '_, D>, action: &str) {
    c.text(&SMALL, &format!("Держите: {action}"), 0, ROWS[3]);
}

fn bar<D: DrawTarget<Color = BinaryColor>>(c: C<'_, '_, D>, v: &View) {
    let w = c.w;
    let mut end = w;
    if let Some(ble) = v.bar.ble {
        let ble_x = end - 7;
        c.ble_small(ble_x, 2, ble);
        end = ble_x - 2;
    }
    if let Some(id) = &v.bar.id {
        let id = clean(id);
        c.right(&TEXT_BOLD, &id, end - 3, 0);
        end -= 3 + width(&TEXT_BOLD, &id);
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

/// Lines in [`SMALL`], each cut to the width, from row `first` on.
fn small_lines<D: DrawTarget<Color = BinaryColor>>(c: C<'_, '_, D>, lines: &[String], first: usize) {
    let w = c.w;
    for (l, y) in lines.iter().zip(&ROWS[first..]) {
        c.text(&SMALL, &fit(&SMALL, l, w), 0, *y);
    }
}

pub(super) fn draw<D: DrawTarget<Color = BinaryColor>>(c: C<'_, '_, D>, v: &View) {
    let w = c.w;
    if let Body::Off { name } = &v.body {
        c.centered(&BIG_BOLD, "Выключено", 6);
        c.centered(&TEXT, &fit(&TEXT, &clean(name), w), 27);
        c.centered(&SMALL, "Включить - кнопкой", 47);
        return;
    }
    bar(c, v);
    match &v.body {
        Body::Home { unread, .. } => {
            if *unread > 0 {
                let n = (*unread).min(999);
                let digits = format!("{n}");
                c.text(&DIGITS, &digits, 0, 18);
                let x = width(&DIGITS, &digits) + 6;
                let (a, b) = match plural(n, "one", "few", "many") {
                    "one" => ("новое", "сообщение"),
                    "few" => ("новых", "сообщения"),
                    _ => ("новых", "сообщений"),
                };
                c.text(&TEXT, a, x, 19);
                c.text(&TEXT, b, x, 32);
            } else {
                c.centered(&BIG_BOLD, "Нет новых", 20);
                c.centered(&BIG_BOLD, "сообщений", 38);
            }
        }
        Body::Messages(list) => match list.first() {
            None => c.centered(&TEXT, "Сообщений нет", 27),
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
                c.centered(&TEXT, "Пока никого", 27);
            }
            for (n, y) in list.iter().zip([ROWS[0], ROWS[0] + 14, ROWS[0] + 28]) {
                let hops = if n.hops == 0 { "прямо".into() } else { format!("{} хоп", n.hops) };
                c.text(&TEXT, &fit(&TEXT, &clean(&n.name), w - width(&SMALL, &hops) - 6), 0, y);
                c.right(&SMALL, &hops, w - 1, y + 1);
            }
        }
        Body::Radio(r) => {
            let lines = [
                format!("{} МГц  {} дБм", trim_float(r.freq_mhz, 3), r.tx_dbm),
                format!("SF{}  BW {}  CR{}", r.sf, trim_float(r.bw_khz, 1), r.cr),
                format!("RSSI {}  SNR {}", r.last_rssi, quarter(r.last_snr)),
                format!("Батарея {}", volts(r.batt_mv)),
            ];
            for (l, y) in lines.iter().zip(SMALL_ROWS) {
                c.text(&SMALL, &fit(&SMALL, l, w), 0, y);
            }
        }
        Body::Bluetooth { state, name, pin } => {
            let (head, link, action) = match state {
                BleState::Off => ("Выключен", "", "включить"),
                BleState::Waiting => ("Включен", "Ждет телефона", "выключить"),
                BleState::Connected => ("Включен", "Телефон подключен", "выключить"),
            };
            c.text(&TEXT_BOLD, head, 0, ROWS[0]);
            let second = match pin {
                Some(pin) => format!("PIN {pin:06}"),
                None => clean(name),
            };
            small_lines(c, &[link.into(), second], 1);
            hint(c, action);
        }
        Body::Gps(g) => {
            let (head, lines): (&str, Vec<String>) = match (g.on, g.fix) {
                (false, _) => ("Выключен", Vec::new()),
                (true, None) if !g.receiving => ("Нет связи", alloc::vec!["Приемник молчит".into()]),
                (true, None) => ("Ищет", alloc::vec![format!("Видно спутников: {}", g.in_view)]),
                (true, Some((lat, lon))) => (
                    "Координаты",
                    alloc::vec![format!("{}, {}", micro_deg(lat), micro_deg(lon)), format!("Спутников: {}", g.satellites)],
                ),
            };
            c.text(&TEXT_BOLD, head, 0, ROWS[0]);
            small_lines(c, &lines, 1);
            hint(c, if g.on { "выключить" } else { "включить" });
        }
        Body::Advert { flood } => {
            let text = if *flood { "Репитер объявит себя по всей сети." } else { "Узлы рядом узнают, что вы в сети." };
            let lines = wrap(&SMALL, text, w, 3);
            for (l, y) in lines.iter().zip(SMALL_ROWS) {
                c.text(&SMALL, l, 0, y);
            }
            hint(c, "отправить");
        }
        Body::PowerOff => {
            let lines = wrap(&SMALL, "Узел выключится. Включить снова - кнопкой.", w, 3);
            for (l, y) in lines.iter().zip(SMALL_ROWS) {
                c.text(&SMALL, l, 0, y);
            }
            hint(c, "выключить");
        }
        Body::Off { .. } => {}
        Body::Repeater(p, health) => {
            let tx = format!("{} дБм", p.tx_dbm);
            c.text(&TEXT, &format!("{} МГц", trim_float(p.freq_mhz, 3)), 0, ROWS[0]);
            c.right(&SMALL, &tx, w - 1, ROWS[0] + 1);
            c.text(&SMALL, &format!("BW {}  SF{}  CR{}", trim_float(p.bw_khz, 1), p.sf, p.cr), 0, ROWS[1]);
            let n = health.neighbours as u32;
            let rest = format!("{}, {}", volts(health.batt_mv), duration_short(health.uptime_min));
            let neighbours = format!("{n} {}", plural(n, "сосед", "соседа", "соседей"));
            let room = w - width(&SMALL, &rest) - 6;
            let font = if width(&TEXT_BOLD, &neighbours) <= room { &TEXT_BOLD } else { &SMALL_BOLD };
            c.text(font, &fit(font, &neighbours, room), 0, if core::ptr::eq(font, &TEXT_BOLD) { 43 } else { 44 });
            c.right(&SMALL, &rest, w - 1, 44);
        }
        Body::Neighbours(list) => {
            if list.is_empty() {
                c.centered(&TEXT, "Пока никого", 27);
            }
            let shown = if list.len() > 4 { 3 } else { list.len() };
            for (n, y) in list[..shown].iter().zip(SMALL_ROWS) {
                let right = format!("{} дБ, {}", quarter_signed(n.snr), ago(n.age_min));
                c.text(&SMALL_BOLD, &fit(&SMALL_BOLD, &clean(&n.id), w - width(&SMALL, &right) - 6), 0, y);
                c.right(&SMALL, &right, w - 1, y);
            }
            if list.len() > shown {
                c.text(&SMALL, &format!("и еще {}", list.len() - shown), 0, SMALL_ROWS[3]);
            }
        }
        Body::Traffic(t) => rows(
            c,
            &[
                ("Принято", format!("{}", t.rx)),
                ("Передано", format!("{}", t.tx)),
                ("Ошибки CRC", format!("{}", t.rx_errors)),
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
        Body::Power(pw) => {
            c.text(&TEXT_BOLD, &format!("Батарея {}", volts(pw.batt_mv)), 0, ROWS[0]);
            let sleep = match pw.powersaving {
                None => String::from("недоступен"),
                Some(false) => String::from("выключен"),
                Some(true) => format!("включен, {}%", pw.asleep_pct),
            };
            small_lines(c, &[String::from("Сон между пакетами:"), sleep, format!("В сети {}", duration(pw.uptime_min))], 1);
        }
        Body::Network(n) => {
            let (state, ssid, ip) = match &n.wifi {
                Wifi::Off => ("не настроен", "", ""),
                Wifi::Connecting { ssid } => ("подключается", ssid.as_str(), ""),
                Wifi::Up { ssid, ip } => ("подключен", ssid.as_str(), ip.as_str()),
            };
            c.text(&TEXT_BOLD, &fit(&TEXT_BOLD, &format!("Wi-Fi {state}"), w), 0, ROWS[0]);
            let mqtt = match n.mqtt_on {
                0 => String::from("MQTT выключен"),
                on => format!("MQTT {} из {on}", n.mqtt_up),
            };
            // without an address, the clock's state takes its row
            let second = match ip {
                "" if n.time_synced => String::from("Часы по сети"),
                "" => String::from("Часы ждут сети"),
                ip => format!("IP {ip}"),
            };
            small_lines(c, &[clean(ssid), second, mqtt], 1);
        }
        Body::Contact(link) => match c.qr(link, 0, 15, c.h - 15, true) {
            Some(side) => {
                for (l, y) in ["Добавить", "в контакты", "камерой", "телефона"].iter().zip(SMALL_ROWS) {
                    c.text(&SMALL, l, side + 3, y);
                }
            }
            None => c.text(&SMALL, "Ссылка не помещается", 0, ROWS[1]),
        },
        Body::Update(u) => match &u.window {
            None => {
                c.text(&TEXT_BOLD, "Окно закрыто", 0, ROWS[0]);
                let mut version = clean(&u.version);
                if u.trial {
                    version.push_str(", пробная");
                }
                small_lines(c, &[String::from("Прошивка по Bluetooth"), version], 1);
                hint(c, "открыть");
            }
            Some(o) => {
                c.text(&SMALL, "PIN", 0, ROWS[0] + 6);
                c.text(&MID_DIGITS, &format!("{:06}", o.pin), 24, ROWS[0] + 1);
                let state = if o.connected { String::from("Подключено") } else { format!("Ждет: еще {} мин", o.mins_left) };
                c.text(&SMALL, &state, 0, 36);
                if o.connected {
                    c.text(&SMALL, &format!("Закроется через {} мин", o.mins_left), 0, ROWS[3]);
                } else {
                    hint(c, "закрыть");
                }
            }
        },
    }
    if v.pin.is_some() || v.alert.is_some() {
        // a box over the page would leave bits of its text showing around it on so small a screen
        c.rect(0, 15, w as u32, (c.h - 15) as u32, Some(crate::widgets::PAPER));
    }
    if let Some(pin) = v.pin {
        c.overlay(&[("PIN для телефона".into(), &SMALL), (format!("{pin:06}"), &DIGITS)], 2, 1, 3);
    } else if let Some(a) = v.alert {
        let lines: Vec<(String, &Font)> = wrap(&TEXT_BOLD, alert_text(a), w - 12, 2).into_iter().map(|l| (l, &TEXT_BOLD)).collect();
        c.overlay(&lines, 2, 1, 4);
    }
}
