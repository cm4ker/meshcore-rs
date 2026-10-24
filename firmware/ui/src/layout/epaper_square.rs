//! Square e-paper, 200 x 200 (T-Echo), in large type: a bar on top (page title, the battery's
//! voltage, Bluetooth), the page, the page dots at the bottom; a long-press hint in two lines.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;

use super::{ADVERT_FLOOD, ADVERT_ZERO_HOP, POWER_OFF, alert_text, title};
use crate::model::{BleState, Body, View, Wifi};
use crate::text::{ago, clean, duration, micro_deg, permille, plural, quarter, quarter_signed, trim_float, volts};
use crate::theme::Font;
use crate::theme::square::{BIG, BIG_BOLD, FREQ_DIGITS, HUGE_DIGITS, SMALL, TEXT, TEXT_BOLD};
use crate::widgets::{Canvas, fit, qr_side, width, wrap};

const MARGIN: i32 = 4;
/// Where the page's content starts, under the bar.
const BODY_TOP: i32 = 30;
/// The top of a long-press hint, above the page dots.
const HINT_Y: i32 = 146;
/// List rows.
const ROW: i32 = 22;
/// The contact's QR code, at most (with the caption under it and the dots below).
const QR_MAX: i32 = 138;

type C<'a, 'b, D> = &'a mut Canvas<'b, D>;

fn hint<D: DrawTarget<Color = BinaryColor>>(c: C<'_, '_, D>, action: &str) {
    c.text(&SMALL, "Удерживайте кнопку:", MARGIN, HINT_Y);
    c.text(&TEXT_BOLD, action, MARGIN, HINT_Y + 15);
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
        // small and without the space, leaving the title room
        let batt = volts(mv).replace(' ', "");
        c.right(&SMALL, &batt, end - 6, 8);
        end -= 6 + width(&SMALL, &batt);
    }
    let title_end = end - 5;
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

pub(super) fn draw<D: DrawTarget<Color = BinaryColor>>(c: C<'_, '_, D>, v: &View) {
    let (w, h) = (c.w, c.h);
    let full = w - 2 * MARGIN;
    if let Body::Off { name } = &v.body {
        c.centered(&BIG_BOLD, "Выключено", 50);
        c.centered(&TEXT_BOLD, &fit(&TEXT_BOLD, &clean(name), full), 88);
        c.centered(&TEXT, "Нажмите кнопку,", 135);
        c.centered(&TEXT, "чтобы включить", 155);
        return;
    }
    bar(c, v);
    match &v.body {
        Body::Home { unread, .. } => {
            if *unread > 0 {
                let n = (*unread).min(999);
                c.centered(&HUGE_DIGITS, &format!("{n}"), BODY_TOP + 14);
                c.centered(&TEXT, plural(n, "новое сообщение", "новых сообщения", "новых сообщений"), BODY_TOP + 62);
            } else {
                c.centered(&BIG, "Нет новых", BODY_TOP + 24);
                c.centered(&BIG, "сообщений", BODY_TOP + 50);
            }
            c.text(&SMALL, "Нажатие - дальше,", MARGIN, HINT_Y);
            c.text(&SMALL, "двойное - назад", MARGIN, HINT_Y + 15);
        }
        Body::Messages(list) => {
            if list.is_empty() {
                c.centered(&BIG, "Сообщений нет", 80);
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
                c.centered(&BIG, "Пока никого", 80);
            }
            let mut y = BODY_TOP + 2;
            for n in list.iter().take(6) {
                let hops = if n.hops == 0 { "прямо".into() } else { format!("{} хоп", n.hops) };
                let right = format!("{hops}, {}", ago(n.age_min));
                let name_w = full - width(&SMALL, &right) - 8;
                c.text(&TEXT, &fit(&TEXT, &clean(&n.name), name_w), MARGIN, y);
                c.right(&SMALL, &right, w - MARGIN, y + 3);
                y += ROW;
            }
        }
        Body::Radio(r) => {
            let lines = [
                format!("{} МГц", trim_float(r.freq_mhz, 3)),
                format!("SF{}  BW {}  CR{}", r.sf, trim_float(r.bw_khz, 1), r.cr),
                format!("{} дБм, шум {}", r.tx_dbm, r.noise_dbm),
                format!("RSSI {}  SNR {}", r.last_rssi, quarter(r.last_snr)),
                format!("Пакеты {} / {}", r.rx, r.tx),
                format!("Батарея {}", volts(r.batt_mv)),
                format!("Работает {}", duration(r.uptime_min)),
            ];
            let lines: Vec<String> = lines.iter().map(|l| fit(&TEXT, l, full)).collect();
            c.lines(&TEXT, &lines, MARGIN, BODY_TOP + 1);
        }
        Body::Bluetooth { state, name, pin } => {
            let (head, link, action) = match state {
                BleState::Off => ("Выключен", "", "включить"),
                BleState::Waiting => ("Включен", "Ждет телефона", "выключить"),
                BleState::Connected => ("Включен", "Телефон подключен", "выключить"),
            };
            c.text(&BIG_BOLD, head, MARGIN, BODY_TOP + 2);
            c.text(&TEXT, &fit(&TEXT, link, full), MARGIN, BODY_TOP + 32);
            c.text(&SMALL, &fit(&SMALL, &clean(name), full), MARGIN, BODY_TOP + 60);
            if let Some(pin) = pin {
                c.text(&TEXT_BOLD, &format!("PIN {pin:06}"), MARGIN, BODY_TOP + 82);
            }
            hint(c, action);
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
            hint(c, if g.on { "выключить" } else { "включить" });
        }
        Body::Advert { flood } => {
            let text = wrap(&TEXT, if *flood { ADVERT_FLOOD } else { ADVERT_ZERO_HOP }, full, 5);
            c.lines(&TEXT, &text, MARGIN, BODY_TOP + 2);
            hint(c, "отправить");
        }
        Body::PowerOff => {
            let text = wrap(&TEXT, POWER_OFF, full, 5);
            c.lines(&TEXT, &text, MARGIN, BODY_TOP + 2);
            hint(c, "выключить");
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
                    ("В сети", duration(health.uptime_min)),
                ],
                BODY_TOP + 84,
            );
        }
        Body::Neighbours(list) => {
            if list.is_empty() {
                c.centered(&BIG, "Пока никого", 80);
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
                c.text(&TEXT, &format!("и еще {}", list.len() - shown), MARGIN, y);
            }
        }
        Body::Traffic(t) => rows(
            c,
            &[
                ("Принято", format!("{}", t.rx)),
                ("Передано", format!("{}", t.tx)),
                ("Ошибки CRC", format!("{}", t.rx_errors)),
                ("Эфир TX", permille(t.tx_air_permille)),
                ("Эфир RX", permille(t.rx_air_permille)),
                ("Шум", format!("{} дБм", t.noise_dbm)),
                ("Очередь", format!("{}", t.queue)),
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
        Body::Power(pw) => {
            let sleep = match pw.powersaving {
                None => String::from("нет"),
                Some(false) => String::from("выключен"),
                Some(true) => format!("{}% времени", pw.asleep_pct),
            };
            rows(
                c,
                &[
                    ("Батарея", volts(pw.batt_mv)),
                    ("Сон", sleep),
                    ("В сети", duration(pw.uptime_min)),
                ],
                BODY_TOP + 2,
            );
        }
        Body::Network(n) => {
            let (state, ssid, ip) = match &n.wifi {
                Wifi::Off => ("не настроен", "", ""),
                Wifi::Connecting { ssid } => ("подключается", ssid.as_str(), ""),
                Wifi::Up { ssid, ip } => ("подключен", ssid.as_str(), ip.as_str()),
            };
            c.text(&TEXT_BOLD, &fit(&TEXT_BOLD, &format!("Wi-Fi {state}"), full), MARGIN, BODY_TOP + 2);
            c.text(&TEXT, &fit(&TEXT, &clean(ssid), full), MARGIN, BODY_TOP + 24);
            if !ip.is_empty() {
                c.text(&TEXT, &format!("IP {ip}"), MARGIN, BODY_TOP + 46);
            }
            let mqtt = match n.mqtt_on {
                0 => String::from("MQTT выключен"),
                on => format!("MQTT: {} из {on}", n.mqtt_up),
            };
            c.text(&TEXT_BOLD, &mqtt, MARGIN, BODY_TOP + 76);
            c.text(&TEXT, if n.time_synced { "Часы по сети" } else { "Часы ждут сети" }, MARGIN, BODY_TOP + 98);
        }
        Body::Contact(link) => match qr_side(link, QR_MAX) {
            Some(side) => {
                c.qr(link, (w - side) / 2, BODY_TOP, QR_MAX, false);
                c.centered(&SMALL, "Сканируйте телефоном", BODY_TOP + side + 1);
            }
            None => c.text(&TEXT, "Ссылка не помещается", MARGIN, BODY_TOP + 2),
        },
        Body::Update(u) => match &u.window {
            None => {
                c.text(&TEXT_BOLD, "Окно закрыто", MARGIN, BODY_TOP + 2);
                let text = wrap(&TEXT, "Откройте его и загрузите прошивку по Bluetooth.", full, 3);
                let y = c.lines(&TEXT, &text, MARGIN, BODY_TOP + 24);
                let mut version = format!("Версия {}", clean(&u.version));
                if u.trial {
                    version.push_str(", пробная");
                }
                c.text(&SMALL, &fit(&SMALL, &version, full), MARGIN, y + 4);
                hint(c, "открыть");
            }
            Some(o) => {
                c.text(&SMALL, "PIN для подключения", MARGIN, BODY_TOP + 2);
                c.centered(&HUGE_DIGITS, &format!("{:06}", o.pin), BODY_TOP + 16);
                c.text(&TEXT, if o.connected { "Подключено" } else { "Ждет подключения" }, MARGIN, BODY_TOP + 62);
                c.text(&SMALL, &format!("Закроется через {} мин", o.mins_left), MARGIN, BODY_TOP + 84);
                c.text(&SMALL, &fit(&SMALL, &clean(&o.name), full), MARGIN, BODY_TOP + 100);
                if !o.connected {
                    hint(c, "закрыть");
                }
            }
        },
    }
    c.dots(v.page_idx, v.pages, h - 8, 14, 9, 6);
    if v.pin.is_some() || v.alert.is_some() {
        // a box over the page leaves the page's big numbers showing around it: clear the page
        c.rect(0, 27, w as u32, (h - 27) as u32, Some(crate::widgets::PAPER));
    }
    if let Some(pin) = v.pin {
        c.overlay(&[("PIN для телефона".into(), &TEXT), (format!("{pin:06}"), &HUGE_DIGITS)], 4, 3, 9);
    } else if let Some(a) = v.alert {
        let lines: Vec<(String, &Font)> = wrap(&BIG_BOLD, alert_text(a), w - 24, 3).into_iter().map(|l| (l, &BIG_BOLD)).collect();
        c.overlay(&lines, 4, 3, 9);
    }
}
