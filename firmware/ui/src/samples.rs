//! Example views of every page, with the awkward cases (long names, emoji, empty and overfull
//! lists, large numbers): the layouts' tests draw them all, `tools/ui-preview` shows them.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::model::*;

pub const COMPANION_PAGES: &[Page] =
    &[Page::Home, Page::Messages, Page::Nearby, Page::Radio, Page::Bluetooth, Page::Gps, Page::Advert, Page::PowerOff];
pub const REPEATER_PAGES: &[Page] =
    &[Page::Home, Page::Neighbours, Page::Traffic, Page::Radio, Page::Power, Page::Network, Page::Update, Page::Advert];

fn frame(pages: &[Page], page: Page, bar: Bar, body: Body) -> View {
    let idx = pages.iter().position(|&p| p == page).unwrap_or(0);
    View { page, page_idx: idx as u8, pages: pages.len() as u8, bar, body, alert: None, pin: None }
}

fn companion_bar() -> Bar {
    Bar { battery_pct: 85, ble: Some(BleState::Connected), id: None }
}

fn repeater_bar() -> Bar {
    Bar { battery_pct: 70, ble: None, id: Some("3F".into()) }
}

fn msg(from: &str, text: &str, age_min: u32) -> Message {
    Message { from: from.into(), text: text.into(), age_min }
}

fn node(name: &str, hops: u8, age_min: u32) -> Node {
    Node { name: name.into(), hops, age_min }
}

fn radio() -> RadioInfo {
    RadioInfo {
        freq_mhz: 868.731,
        bw_khz: 62.5,
        sf: 7,
        cr: 7,
        tx_dbm: 22,
        noise_dbm: -100,
        last_rssi: -95,
        last_snr: -11,
        rx: 1234,
        tx: 56,
        tx_air_min: 1,
        rx_air_min: 13,
        uptime_min: 135,
        batt_mv: 4050,
        battery_pct: 85,
    }
}

pub fn passport() -> Passport {
    Passport {
        name: "Omsk-RPT".into(),
        freq_mhz: 868.731,
        bw_khz: 62.5,
        sf: 7,
        cr: 7,
        tx_dbm: 22,
        version: "v1.18.0-rs".into(),
    }
}

fn neighbour(id: &str, snr: i16, age_min: u32) -> Neighbour {
    Neighbour { id: id.into(), snr, age_min }
}

/// The usual content of a companion page.
pub fn companion_view(page: Page) -> View {
    let body = match page {
        Page::Messages => Body::Messages(vec![
            msg("Public", "Vasya: Privet vsem, kto na svyazi segodnya vecherom?", 2),
            msg("Олег 📡", "Да, слышу тебя хорошо, через PKIO", 75),
        ]),
        Page::Nearby => Body::Nearby(vec![node("PKIO", 0, 1), node("Omsk-Center-RPT", 2, 14), node("Олег 📡", 1, 125)]),
        Page::Radio => Body::Radio(radio()),
        Page::Bluetooth => Body::Bluetooth { state: BleState::Connected, name: "MeshCore-PKIO🎃".into(), pin: Some(123456) },
        Page::Gps => Body::Gps(GpsInfo { on: true, receiving: true, fix: Some((54_987_650, 73_368_800)), satellites: 7, in_view: 12 }),
        Page::Advert => Body::Advert { flood: false },
        Page::PowerOff => Body::PowerOff,
        _ => Body::Home { name: "PKIO🎃".into(), unread: 3 },
    };
    frame(COMPANION_PAGES, page, companion_bar(), body)
}

/// The usual content of a repeater page.
pub fn repeater_view(page: Page) -> View {
    let body = match page {
        Page::Neighbours => Body::Neighbours(vec![neighbour("3FA2", 33, 1), neighbour("C1", -11, 14), neighbour("07D4E1", 0, 125)]),
        Page::Traffic => Body::Traffic(Traffic {
            rx: 12_345,
            tx: 2_310,
            rx_errors: 57,
            tx_air_permille: 12,
            rx_air_permille: 87,
            queue: 0,
            noise_dbm: -112,
        }),
        Page::Radio => Body::Preset(passport()),
        Page::Power => Body::Power(PowerInfo { batt_mv: 4050, battery_pct: 85, powersaving: Some(true), asleep_pct: 94, uptime_min: 4400 }),
        Page::Network => Body::Network(NetInfo {
            wifi: Wifi::Up { ssid: "MikroTik-Home".into(), ip: "192.168.88.21".into() },
            mqtt_on: 2,
            mqtt_up: 2,
            time_synced: true,
        }),
        Page::Update => Body::Update(UpdateInfo { window: None, trial: false, version: "v1.18.0-rs (24 Oct 2026)".into() }),
        Page::Advert => Body::Advert { flood: true },
        _ => Body::Repeater(passport(), Health { neighbours: 5, battery_pct: 85, uptime_min: 4400 }),
    };
    frame(REPEATER_PAGES, page, repeater_bar(), body)
}

/// Every page, then the awkward cases.
pub fn all() -> Vec<(String, View)> {
    let mut out: Vec<(String, View)> = Vec::new();
    for &p in COMPANION_PAGES {
        out.push((format!("companion-{p:?}"), companion_view(p)));
    }
    for &p in REPEATER_PAGES {
        out.push((format!("repeater-{p:?}"), repeater_view(p)));
    }
    let long = "Очень-длинное-имя-узла Ёлка 12345 🎃";
    let mut add = |name: &str, v: View| out.push((name.into(), v));
    let c = companion_view;
    let r = repeater_view;
    let with = |mut v: View, body: Body| {
        v.body = body;
        v
    };

    for n in [0, 1, 21, 999, 5000] {
        add(&format!("home-unread-{n}"), with(c(Page::Home), Body::Home { name: long.into(), unread: n }));
    }
    add("messages-empty", with(c(Page::Messages), Body::Messages(vec![])));
    add(
        "messages-long",
        with(
            c(Page::Messages),
            Body::Messages(vec![
                msg(long, "Сверхдлинноесловобезпробеловкотороенепомещаетсявстроку и ещё текст после него, много текста", 0),
                msg("x", "коротко", 59),
            ]),
        ),
    );
    add("nearby-empty", with(c(Page::Nearby), Body::Nearby(vec![])));
    let crowd: Vec<Node> = (0..9).map(|i| node(if i == 3 { long } else { "Узел" }, i * 9, i as u32 * 400)).collect();
    add("nearby-many", with(c(Page::Nearby), Body::Nearby(crowd)));
    add(
        "radio-big",
        with(
            c(Page::Radio),
            Body::Radio(RadioInfo {
                freq_mhz: 2400.0,
                bw_khz: 812.5,
                sf: 12,
                last_snr: 37,
                rx: 4_000_000_000,
                tx: 3_999_999_999,
                uptime_min: 100_000,
                noise_dbm: -120,
                last_rssi: -130,
                ..radio()
            }),
        ),
    );
    for (i, state) in [BleState::Off, BleState::Waiting].into_iter().enumerate() {
        add(&format!("ble-{i}"), with(c(Page::Bluetooth), Body::Bluetooth { state, name: format!("MeshCore-{long}"), pin: None }));
    }
    let gps = |on, receiving, fix, satellites, in_view| Body::Gps(GpsInfo { on, receiving, fix, satellites, in_view });
    add("gps-off", with(c(Page::Gps), gps(false, false, None, 0, 0)));
    add("gps-silent", with(c(Page::Gps), gps(true, false, None, 0, 0)));
    add("gps-searching", with(c(Page::Gps), gps(true, true, None, 0, 0)));
    add("gps-south-west", with(c(Page::Gps), gps(true, true, Some((-33_868_820, -151_209_290)), 11, 30)));
    for a in [Alert::AdvertSent, Alert::AdvertFailed, Alert::BleOn, Alert::BleOff, Alert::GpsOn, Alert::GpsOff] {
        let mut v = c(Page::Advert);
        v.alert = Some(a);
        add(&format!("alert-{a:?}"), v);
    }
    let mut v = c(Page::Home);
    v.pin = Some(12345);
    v.bar.ble = Some(BleState::Waiting);
    add("pairing-pin", v);
    for (i, ble) in [Some(BleState::Off), Some(BleState::Waiting), None].into_iter().enumerate() {
        let mut v = c(Page::Radio);
        v.bar.battery_pct = [0, 5, 100][i];
        v.bar.ble = ble;
        add(&format!("bar-{i}"), v);
    }
    add("off", with(c(Page::Home), Body::Off { name: long.into() }));

    add("repeater-home-long", {
        let mut v = r(Page::Home);
        v.bar.id = Some("3FA291".into());
        let p = Passport { name: long.into(), freq_mhz: 2400.0, bw_khz: 812.5, sf: 12, cr: 8, tx_dbm: -9, ..passport() };
        with(v, Body::Repeater(p, Health { neighbours: 101, battery_pct: 100, uptime_min: 400_000 }))
    });
    for id in ["3FA2", "3FA291"] {
        let mut v = r(Page::Home);
        v.bar.id = Some(id.into());
        add(&format!("repeater-home-id-{}-bytes", id.len() / 2), v);
    }
    add("neighbours-empty", with(r(Page::Neighbours), Body::Neighbours(vec![])));
    let many: Vec<Neighbour> = (0..12).map(|i| neighbour("3FA291", -80 + i * 13, i as u32 * 300)).collect();
    add("neighbours-many", with(r(Page::Neighbours), Body::Neighbours(many)));
    add(
        "traffic-big",
        with(
            r(Page::Traffic),
            Body::Traffic(Traffic {
                rx: 4_000_000_000,
                tx: 4_000_000_000,
                rx_errors: 4_000_000_000,
                tx_air_permille: 1000,
                rx_air_permille: 1000,
                queue: 64,
                noise_dbm: -120,
            }),
        ),
    );
    add(
        "preset-wide",
        with(r(Page::Radio), Body::Preset(Passport { freq_mhz: 2400.125, bw_khz: 812.5, sf: 12, cr: 8, ..passport() })),
    );
    for (i, ps) in [None, Some(false)].into_iter().enumerate() {
        add(
            &format!("power-{i}"),
            with(r(Page::Power), Body::Power(PowerInfo { batt_mv: 3300, battery_pct: 0, powersaving: ps, asleep_pct: 0, uptime_min: 59 })),
        );
    }
    for (i, wifi) in [Wifi::Off, Wifi::Connecting { ssid: format!("Сеть {long}") }].into_iter().enumerate() {
        add(&format!("network-{i}"), with(r(Page::Network), Body::Network(NetInfo { wifi, mqtt_on: [0, 4][i], mqtt_up: 0, time_synced: false })));
    }
    let window = |connected| OtaWindow { name: "MeshCore-OTA-Wan3-SVOBODA-".into(), pin: 482_913, connected, mins_left: 14 };
    add(
        "update-open",
        with(r(Page::Update), Body::Update(UpdateInfo { window: Some(window(false)), trial: false, version: "v1.18.0-rs".into() })),
    );
    add(
        "update-connected",
        with(r(Page::Update), Body::Update(UpdateInfo { window: Some(window(true)), trial: false, version: "v1.18.0-rs".into() })),
    );
    add(
        "update-trial",
        with(r(Page::Update), Body::Update(UpdateInfo { window: None, trial: true, version: "v1.18.0-rs (24 Oct 2026)".into() })),
    );
    out
}
