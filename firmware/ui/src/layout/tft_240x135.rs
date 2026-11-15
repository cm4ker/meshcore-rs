//! Colour TFT, 240 x 135 (Heltec T114), drawn in two colours: the wide e-paper's frame, 10 px
//! narrower and 13 px taller. A bar on top (page title, the battery's voltage, Bluetooth) whose
//! rule thickens under the current page's share of the width, then up to five rows, further apart
//! than on the e-paper; a long-press hint takes the last.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;

use super::{ADVERT_FLOOD, ADVERT_ZERO_HOP, alert_text, title};
use crate::model::{BleState, Body, View, Wifi};
use crate::text::{ago, clean, clock, duration, micro_deg, permille, plural, quarter, quarter_signed, trim_float, unread_lines, volts};
use crate::theme::Font;
use crate::theme::tft::{BIG, BIG_BOLD, DIGITS, HUGE_BOLD, HUGE_DIGITS, SMALL, TEXT, TEXT_BOLD};
use crate::widgets::{Canvas, INK, fit, qr_side, width, wrap};

const MARGIN: i32 = 3;
/// The bar's rule, two pixels; two more under the current page's share.
const RULE: i32 = 18;
/// Where the page's content starts, under the bar.
const BODY_TOP: i32 = 25;
/// Rows in [`TEXT`] or [`TEXT_BOLD`]: five from [`BODY_TOP`], or four above a hint.
const ROW: i32 = 22;
/// The long-press hint, on the last row.
const HINT_Y: i32 = 117;
/// A word in place of an empty list, in [`BIG`].
const EMPTY_Y: i32 = 68;

type C<'a, 'b, D> = &'a mut Canvas<'b, D>;

fn row_y(i: usize) -> i32 {
    BODY_TOP + i as i32 * ROW
}

/// "Удерживайте:" in [`SMALL`], the action after it in [`TEXT_BOLD`], on one baseline.
fn hint<D: DrawTarget<Color = BinaryColor>>(c: C<'_, '_, D>, action: &str) {
    let lead = "Удерживайте:";
    c.text(&SMALL, lead, MARGIN, HINT_Y + 2);
    let x = MARGIN + width(&SMALL, lead) + 5;
    c.text(&TEXT_BOLD, &fit(&TEXT_BOLD, action, c.w - MARGIN - x), x, HINT_Y);
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
        // without the space, leaving the title room
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

/// A label in [`SMALL`] over its value in [`TEXT_BOLD`] (in [`SMALL`] if wider than `room`), at
/// (`x`, `y`).
fn cell<D: DrawTarget<Color = BinaryColor>>(c: C<'_, '_, D>, label: &str, value: &str, x: i32, y: i32, room: i32) {
    c.text(&SMALL, &fit(&SMALL, label, room), x, y);
    if width(&TEXT_BOLD, value) <= room {
        c.text(&TEXT_BOLD, value, x, y + 15);
    } else {
        c.text(&SMALL, &fit(&SMALL, value, room), x, y + 17);
    }
}

pub(super) fn draw<D: DrawTarget<Color = BinaryColor>>(c: C<'_, '_, D>, v: &View) {
    let (w, h) = (c.w, c.h);
    let full = w - 2 * MARGIN;
    if let Body::Off { name } = &v.body {
        c.centered(&HUGE_BOLD, "Выключено", 18);
        c.centered(&TEXT_BOLD, &fit(&TEXT_BOLD, &clean(name), full), 55);
        c.centered(&TEXT, "Нажмите кнопку,", 89);
        c.centered(&TEXT, "чтобы включить", 107);
        return;
    }
    bar(c, v);
    match &v.body {
        Body::Home { unread, clock: Some(at), .. } => {
            // the time and the unread count beside it, centred together
            let t = clock(*at);
            let (a, b) = unread_lines(*unread);
            let tw = width(&HUGE_DIGITS, &t);
            let x = (w - tw - 12 - width(&TEXT_BOLD, &a).max(width(&TEXT, b))) / 2;
            c.text(&HUGE_DIGITS, &t, x, BODY_TOP + 12);
            c.text(&TEXT_BOLD, &a, x + tw + 12, BODY_TOP + 19);
            c.text(&TEXT, b, x + tw + 12, BODY_TOP + 37);
            c.text(&SMALL, "Нажатие - дальше,", MARGIN, HINT_Y - 12);
            c.text(&SMALL, "двойное - назад", MARGIN, HINT_Y + 2);
        }
        Body::Home { unread, .. } => {
            if *unread > 0 {
                let n = (*unread).min(999);
                let digits = format!("{n}");
                let (a, b) = match plural(n, "one", "few", "many") {
                    "one" => ("новое", "сообщение"),
                    "few" => ("новых", "сообщения"),
                    _ => ("новых", "сообщений"),
                };
                // the count and the words beside it, centred together
                let digits_w = width(&HUGE_DIGITS, &digits);
                let x = (w - digits_w - 12 - width(&BIG_BOLD, a).max(width(&BIG_BOLD, b))) / 2;
                c.text(&HUGE_DIGITS, &digits, x, BODY_TOP + 12);
                c.text(&BIG_BOLD, a, x + digits_w + 12, BODY_TOP + 14);
                c.text(&BIG_BOLD, b, x + digits_w + 12, BODY_TOP + 35);
            } else {
                c.centered(&BIG_BOLD, "Нет новых", BODY_TOP + 16);
                c.centered(&BIG_BOLD, "сообщений", BODY_TOP + 39);
            }
            c.text(&SMALL, "Нажатие - дальше,", MARGIN, HINT_Y - 12);
            c.text(&SMALL, "двойное - назад", MARGIN, HINT_Y + 2);
        }
        Body::Messages(list) => {
            if list.is_empty() {
                c.centered(&BIG, "Сообщений нет", EMPTY_Y);
            }
            let mut y = BODY_TOP;
            for m in list.iter().take(2) {
                // the second only if a line of its text fits under its sender
                let room = ((h - y - 18) / TEXT.pitch).min(3);
                if room < 1 {
                    break;
                }
                let age = ago(m.age_min);
                c.text(&TEXT_BOLD, &fit(&TEXT_BOLD, &clean(&m.from), full - width(&SMALL, &age) - 8), MARGIN, y);
                c.right(&SMALL, &age, w - MARGIN, y + 2);
                y = c.lines(&TEXT, &wrap(&TEXT, &clean(&m.text), full, room as usize), MARGIN, y + 18) + 6;
            }
        }
        Body::Nearby(list) => {
            if list.is_empty() {
                c.centered(&BIG, "Пока никого", EMPTY_Y);
            }
            for (i, n) in list.iter().take(5).enumerate() {
                let hops = if n.hops == 0 { "прямо".into() } else { format!("{} хоп", n.hops) };
                let right = format!("{hops}, {}", ago(n.age_min));
                c.text(&TEXT_BOLD, &fit(&TEXT_BOLD, &clean(&n.name), full - width(&SMALL, &right) - 8), MARGIN, row_y(i));
                c.right(&SMALL, &right, w - MARGIN, row_y(i) + 2);
            }
        }
        Body::Radio(r) => {
            // the uptime does not fit beside the battery
            let lines = [
                (format!("{} МГц", trim_float(r.freq_mhz, 3)), format!("{} дБм", r.tx_dbm)),
                (format!("SF{}  BW {}  CR{}", r.sf, trim_float(r.bw_khz, 1), r.cr), String::new()),
                (format!("RSSI {}  SNR {}", r.last_rssi, quarter(r.last_snr)), String::new()),
                (format!("Пакеты {} / {}", r.rx, r.tx), format!("шум {}", r.noise_dbm)),
                (format!("Батарея {}", volts(r.batt_mv)), String::new()),
            ];
            for (i, (left, right)) in lines.iter().enumerate() {
                let font = if i == 0 { &TEXT_BOLD } else { &TEXT };
                c.row((font, &TEXT), left, right, MARGIN, w - MARGIN, row_y(i));
            }
        }
        Body::Bluetooth { state, name, pin } => {
            let (head, link, action) = match state {
                BleState::Off => ("Выключен", "", "включить"),
                BleState::Waiting => ("Включен", "Ждет телефона", "выключить"),
                BleState::Connected => ("Включен", "Телефон подключен", "выключить"),
            };
            c.text(&BIG_BOLD, head, MARGIN, BODY_TOP + 1);
            if let Some(pin) = pin {
                c.right(&TEXT_BOLD, &format!("PIN {pin:06}"), w - MARGIN, BODY_TOP + 3);
            }
            let mut y = BODY_TOP + 30;
            if !link.is_empty() {
                c.text(&TEXT, link, MARGIN, y);
                y += 25;
            }
            c.text(&SMALL, &fit(&SMALL, &clean(name), full), MARGIN, y);
            hint(c, action);
        }
        Body::Gps(g) => {
            // a state or the position in large type, then lines in plain
            let (heads, lines): (Vec<String>, Vec<String>) = match (g.on, g.fix) {
                (false, _) => (alloc::vec!["Выключен".into()], Vec::new()),
                (true, None) if !g.receiving => (alloc::vec!["Нет связи".into()], alloc::vec!["Приемник молчит".into()]),
                (true, None) => {
                    let mut lines = alloc::vec![format!("Видно спутников: {}", g.in_view)];
                    if g.in_view == 0 {
                        lines.push("Нужно открытое небо".into());
                    }
                    (alloc::vec!["Ищет".into()], lines)
                }
                (true, Some((lat, lon))) => {
                    (alloc::vec![micro_deg(lat), micro_deg(lon)], alloc::vec![format!("Спутников: {}", g.satellites)])
                }
            };
            // the latitude and longitude further apart than the font's pitch: the height allows it
            let mut y = BODY_TOP + 1;
            for head in &heads {
                c.text(&BIG_BOLD, head, MARGIN, y);
                y += 24;
            }
            c.lines(&TEXT, &lines, MARGIN, y + 4);
            hint(c, if g.on { "выключить" } else { "включить" });
        }
        Body::Advert { flood } => {
            let text = wrap(&TEXT, if *flood { ADVERT_FLOOD } else { ADVERT_ZERO_HOP }, full, 5);
            c.lines(&TEXT, &text, MARGIN, BODY_TOP + 1);
            hint(c, "отправить");
        }
        Body::PowerOff => {
            // the screen goes dark with the node: the OLED's words, not the e-paper's
            let text = wrap(&TEXT, "Узел выключится. Включить снова - кнопкой.", full, 5);
            c.lines(&TEXT, &text, MARGIN, BODY_TOP + 1);
            hint(c, "выключить");
        }
        Body::Off { .. } => {}
        Body::Repeater(p, health) => {
            c.text(&BIG_BOLD, &format!("{} МГц", trim_float(p.freq_mhz, 3)), MARGIN, BODY_TOP + 2);
            c.right(&TEXT, &format!("{} дБм", p.tx_dbm), w - MARGIN, BODY_TOP + 4);
            c.text(&TEXT, &format!("BW {}  SF{}  CR{}", trim_float(p.bw_khz, 1), p.sf, p.cr), MARGIN, BODY_TOP + 27);
            c.line(Point::new(MARGIN, BODY_TOP + 51), Point::new(w - MARGIN, BODY_TOP + 51));
            let n = health.neighbours as u32;
            let id = format!("ID {}", clean(&p.id));
            let neighbours = format!("{n} {}", plural(n, "сосед", "соседа", "соседей"));
            c.text(&BIG_BOLD, &fit(&BIG_BOLD, &neighbours, full - width(&TEXT_BOLD, &id) - 8), MARGIN, BODY_TOP + 57);
            c.right(&TEXT_BOLD, &id, w - MARGIN, BODY_TOP + 59);
            c.row((&TEXT, &TEXT_BOLD), "В сети", &duration(health.uptime_min), MARGIN, w - MARGIN, BODY_TOP + 84);
        }
        Body::Neighbours(list) => {
            if list.is_empty() {
                c.centered(&BIG, "Пока никого", EMPTY_Y);
            }
            let shown = if list.len() > 5 { 4 } else { list.len() };
            for (i, n) in list[..shown].iter().enumerate() {
                let right = format!("{} дБ, {}", quarter_signed(n.snr), ago(n.age_min));
                c.text(&TEXT_BOLD, &fit(&TEXT_BOLD, &clean(&n.id), full - width(&SMALL, &right) - 8), MARGIN, row_y(i));
                c.right(&SMALL, &right, w - MARGIN, row_y(i) + 2);
            }
            if list.len() > shown {
                c.text(&TEXT, &format!("и еще {}", list.len() - shown), MARGIN, row_y(shown));
            }
        }
        Body::Traffic(t) => {
            // two columns of a label over its value, the counts on the left
            let col = (w - 2 * MARGIN) / 2;
            let cells = [
                ("Принято", format!("{}", t.rx)),
                ("Передано", format!("{}", t.tx)),
                ("Ошибки CRC", format!("{}", t.rx_errors)),
                ("Эфир TX / RX", format!("{} / {}", permille(t.tx_air_permille), permille(t.rx_air_permille))),
                ("Очередь", format!("{}", t.queue)),
                ("Шум", format!("{} дБм", t.noise_dbm)),
            ];
            for (i, (label, value)) in cells.iter().enumerate() {
                let x = MARGIN + (i as i32 % 2) * col;
                let room = if i % 2 == 0 { col - 6 } else { w - MARGIN - x };
                cell(c, label, value, x, BODY_TOP + (i as i32 / 2) * 36, room);
            }
        }
        Body::Preset(p) => {
            // the bandwidth on a row of its own: beside the spreading factor it would not fit
            let freq = trim_float(p.freq_mhz, 3);
            c.text(&DIGITS, &freq, MARGIN, BODY_TOP + 2);
            c.text(&TEXT_BOLD, "МГц", MARGIN + width(&DIGITS, &freq) + 6, BODY_TOP + 22);
            c.text(&BIG_BOLD, &format!("SF{}  CR{}", p.sf, p.cr), MARGIN, BODY_TOP + 45);
            c.text(&BIG_BOLD, &format!("BW {} кГц", trim_float(p.bw_khz, 1)), MARGIN, BODY_TOP + 70);
            c.right(&BIG, &format!("{} дБм", p.tx_dbm), w - MARGIN, BODY_TOP + 70);
        }
        Body::Power(pw) => {
            let sleep = match pw.powersaving {
                None => String::from("нет"),
                Some(false) => String::from("выключен"),
                Some(true) => format!("{}% времени", pw.asleep_pct),
            };
            rows(c, &[("Батарея", volts(pw.batt_mv)), ("Сон", sleep), ("В сети", duration(pw.uptime_min))]);
        }
        Body::Network(n) => {
            let (state, ssid, ip) = match &n.wifi {
                Wifi::Off => ("не настроен", "", ""),
                Wifi::Disabled { ssid } => ("выключен", ssid.as_str(), ""),
                Wifi::Connecting { ssid } => ("подключается", ssid.as_str(), ""),
                Wifi::Up { ssid, ip } => ("подключен", ssid.as_str(), ip.as_str()),
            };
            let mqtt = match n.mqtt_on {
                0 => String::from("MQTT выключен"),
                on => format!("MQTT: {} из {on}", n.mqtt_up),
            };
            // the network's name and address only when there are any
            let mut lines: Vec<(String, &Font)> = alloc::vec![(format!("Wi-Fi {state}"), &TEXT_BOLD)];
            if !ssid.is_empty() {
                lines.push((clean(ssid), &TEXT));
            }
            if !ip.is_empty() {
                lines.push((format!("IP {ip}"), &TEXT));
            }
            lines.push((mqtt, &TEXT_BOLD));
            lines.push((String::from(if n.time_synced { "Часы по сети" } else { "Часы ждут сети" }), &TEXT));
            for (i, (l, font)) in lines.iter().enumerate() {
                c.text(font, &fit(font, l, full), MARGIN, row_y(i));
            }
        }
        Body::Contact(link) => match qr_side(link, h - RULE - 5) {
            Some(side) => {
                // the code and the words beside it, each centred under the bar; the light modules
                // are the ink, so the code reads dark on light as on paper
                let (top, room) = (RULE + 5, h - RULE - 5);
                c.qr(link, MARGIN, top + (room - side) / 2, room, true);
                let text = ["Добавить", "в контакты", "камерой", "телефона"].map(String::from);
                c.lines(&TEXT_BOLD, &text, MARGIN + side + 12, top + (room - text.len() as i32 * TEXT_BOLD.pitch) / 2);
            }
            None => c.text(&TEXT, "Ссылка не помещается", MARGIN, BODY_TOP + 1),
        },
        Body::Pull(p) => {
            c.text(&BIG_BOLD, &fit(&BIG_BOLD, &clean(&p.state), full), MARGIN, BODY_TOP + 1);
            let mut y = BODY_TOP + 27;
            let mut room = 2;
            if let Some((done, total)) = p.progress {
                c.text(&TEXT, &format!("{} из {} КБ", done / 1024, total.div_ceil(1024)), MARGIN, y);
                y += TEXT.pitch;
                room -= 1;
            }
            let y = c.lines(&TEXT, &wrap(&TEXT, &clean(&p.detail), full, room), MARGIN, y);
            c.text(&SMALL, &fit(&SMALL, &format!("Сейчас {}", clean(&p.version)), full), MARGIN, y + 6);
            if let Some(hold) = p.hold {
                hint(c, hold);
            }
        }
        Body::Update(u) => match &u.window {
            None => {
                c.text(&BIG_BOLD, &fit(&BIG_BOLD, &clean(&u.state), full), MARGIN, BODY_TOP + 1);
                let detail = match u.progress {
                    Some((done, total)) => format!("{} из {} КБ", done / 1024, total.div_ceil(1024)),
                    None => clean(&u.detail),
                };
                let y = c.lines(&TEXT, &wrap(&TEXT, &detail, full, 2), MARGIN, BODY_TOP + 27);
                c.text(&SMALL, &fit(&SMALL, &format!("Версия {}", clean(&u.version)), full), MARGIN, y + 6);
                // opens the window for an update over Bluetooth
                hint(c, "окно BLE");
            }
            Some(o) => {
                c.text(&SMALL, "PIN для подключения", MARGIN, BODY_TOP + 1);
                let pin = format!("{:06}", o.pin);
                c.text(&DIGITS, &pin, MARGIN, BODY_TOP + 18);
                // closer than on the e-paper: "Подключено" ends at the margin
                let x = MARGIN + width(&DIGITS, &pin) + 9;
                c.text(&TEXT_BOLD, if o.connected { "Подключено" } else { "Ждет" }, x, BODY_TOP + 20);
                c.text(&SMALL, &format!("еще {} мин", o.mins_left), x, BODY_TOP + 40);
                c.text(&SMALL, &fit(&SMALL, &clean(&o.name), full), MARGIN, BODY_TOP + 66);
                if !o.connected {
                    hint(c, "закрыть");
                }
            }
        },
    }
    if v.pin.is_some() || v.alert.is_some() {
        // a box over the page would leave bits of the page showing around it: clear the page
        c.rect(0, RULE + 4, w as u32, (h - RULE - 4) as u32, Some(crate::widgets::PAPER));
    }
    if let Some(pin) = v.pin {
        c.overlay(&[("PIN для телефона".into(), &BIG_BOLD), (format!("{pin:06}"), &HUGE_DIGITS)], MARGIN, 2, 6);
    } else if let Some(a) = v.alert {
        let lines: Vec<(String, &Font)> = wrap(&BIG_BOLD, alert_text(a), w - 24, 2).into_iter().map(|l| (l, &BIG_BOLD)).collect();
        c.overlay(&lines, MARGIN, 2, 6);
    }
}
