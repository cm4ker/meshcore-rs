//! The repeater's pages on the T114's screen, as the Heltec repeaters show them
//! (`common::repeater_app`), without the network page: no Wi-Fi here.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use meshcore::mesh::Mesh;
use meshcore::repeater::Repeater;
use meshcore::util::to_hex;
use meshcore_fw_core::links::BLE_CONNECTED;
use meshcore_ui::{
    Action, Alert, Bar, BleState, Body, Button, Health, OtaWindow, Page, Passport, PowerInfo, Traffic, Ui, UpdateInfo, View,
};

use crate::tft::SCREEN;

/// The pages, turned with the button.
pub const PAGES: &[Page] =
    &[Page::Home, Page::Contact, Page::Neighbours, Page::Traffic, Page::Radio, Page::Power, Page::Update, Page::Advert];

/// The screen stays lit this long after boot or a press, as on the Heltec repeaters.
const SCREEN_ON_MS: u64 = 60_000;

/// The repeater's interface: a click turns to the next page, a double click back, a long press
/// acts on the page (an advert, the update window, power off on the home page).
pub struct RepeaterUi {
    ui: Ui,
    button: Button,
    next_view: u64,
    lit: bool,
}

impl RepeaterUi {
    /// `down`: the button is down at boot (its press woke the board): that press is spent.
    pub fn new(down: bool, now: u64) -> Self {
        let ui = Ui::new(PAGES).with_home_action(Action::PowerOff).with_screen_timeout(SCREEN_ON_MS, now);
        Self { ui, button: if down { Button::held() } else { Button::default() }, next_view: 0, lit: false }
    }

    /// One pass of the main loop: the button, then the page when it is due (four times a second
    /// at most; the screen's task draws only what changed). An advert and power off are done
    /// here; the update window is the caller's ([`Action::ToggleUpdate`] comes back).
    pub fn poll(
        &mut self, mesh: &mut Mesh<Repeater>, down: bool, ble_window: Option<(u32, String, u64)>, now: u64, build_date: &str,
    ) -> Option<Action> {
        if down && !self.button.is_down() {
            self.ui.button_down(now);
            self.next_view = 0;
        }
        self.ui.tick(now);
        let mut asked = None;
        if let Some(intent) = self.button.poll(down, now) {
            self.next_view = 0;
            match self.ui.press(intent, now) {
                Some(Action::PowerOff) => mesh.app.effects.power_off = true,
                Some(Action::Advert) => {
                    let _ = mesh.app.handle_command(&mut mesh.ctx, 0, "advert");
                    self.ui.alert(Alert::AdvertSent, now);
                }
                action => asked = action,
            }
        }
        if !self.ui.screen_on(now) {
            if self.lit {
                SCREEN.signal(None);
                self.lit = false;
            }
        } else if now >= self.next_view {
            self.next_view = now + 250;
            SCREEN.signal(Some(repeater_view(&mut self.ui, mesh, ble_window, now, build_date)));
            self.lit = true;
        }
        asked
    }

    /// Light the screen (an update window opened).
    pub fn wake(&mut self, now: u64) {
        self.ui.wake(now);
        self.next_view = 0;
    }
}

/// Time on air as tenths of a percent of the uptime.
fn permille(air_ms: u64, uptime_ms: u64) -> u16 {
    (air_ms * 1000 / uptime_ms.max(1)).min(1000) as u16
}

/// The interface's current page with what it shows; `ble_window`: the update window's PIN, name
/// and when it closes, while it is open.
pub fn repeater_view(ui: &mut Ui, mesh: &Mesh<Repeater>, ble_window: Option<(u32, String, u64)>, now: u64, build_date: &str) -> View {
    let (app, ctx) = (&mesh.app, &mesh.ctx);
    let p = &app.prefs;
    let key = ctx.self_id.pub_key();
    let id_len = p.path_hash_mode as usize + 1;
    let id = to_hex(&key[..id_len]);
    let version = alloc::format!("{} {}", meshcore::FIRMWARE_VERSION, meshcore::release::build_label(build_date));
    let passport = || Passport {
        name: p.node_name.clone(),
        id: id.clone(),
        freq_mhz: p.radio.freq_mhz,
        bw_khz: p.radio.bw_khz,
        sf: p.radio.sf,
        cr: p.radio.cr,
        tx_dbm: p.tx_power_dbm,
        version: version.clone(),
    };
    let uptime_min = (now / 60_000) as u32;
    let s = &ctx.stats;
    let ble_connected = BLE_CONNECTED.load(Ordering::Relaxed);
    let body = match ui.page() {
        Page::Neighbours => {
            let clock = ctx.current_time();
            let mut list: Vec<_> = app.neighbours().iter().collect();
            list.sort_unstable_by_key(|n| core::cmp::Reverse(n.heard_timestamp));
            Body::Neighbours(
                list.iter()
                    .map(|n| meshcore_ui::Neighbour {
                        id: to_hex(&n.id.pub_key[..id_len]),
                        snr: n.snr as i16,
                        age_min: meshcore_ui::coarse_minutes(clock.saturating_sub(n.heard_timestamp)),
                    })
                    .collect(),
            )
        }
        Page::Traffic => Body::Traffic(Traffic {
            rx: s.n_packets_recv,
            tx: s.n_packets_sent,
            rx_errors: s.n_recv_errors,
            tx_air_permille: permille(s.total_air_time_ms, now),
            rx_air_permille: permille(s.rx_air_time_ms, now),
            queue: ctx.outbound_len() as u16,
            noise_dbm: s.noise_floor,
        }),
        Page::Radio => Body::Preset(passport()),
        Page::Power => Body::Power(PowerInfo { batt_mv: app.board.batt_mv, powersaving: None, asleep_pct: 0, uptime_min }),
        Page::Update => {
            let trial = app.board.fw_trial_until.filter(|&t| t > now).map(|t| ((t - now) / 1000, app.board.fw_prev.as_str()));
            let text = meshcore_fw_core::fwtext::fw_text(&app.fw_state(), trial, false);
            Body::Update(UpdateInfo {
                window: ble_window.as_ref().map(|(pin, name, until)| OtaWindow {
                    name: name.clone(),
                    pin: *pin,
                    connected: ble_connected,
                    mins_left: until.saturating_sub(now).div_ceil(60_000) as u32,
                }),
                state: text.state,
                detail: text.detail,
                progress: text.progress,
                version: meshcore::release::running_label(build_date),
            })
        }
        Page::Advert => Body::Advert { flood: true },
        Page::Contact => Body::Contact(meshcore::util::contact_link_within(
            &p.node_name,
            key,
            meshcore::advert::ADV_TYPE_REPEATER,
            meshcore_ui::layout::mono_128x64::CONTACT_LINK_MAX,
        )),
        _ => Body::Repeater(passport(), Health { neighbours: app.neighbours().len() as u16, uptime_min }),
    };
    let (page_idx, pages, alert) = ui.view_frame();
    let ble = ble_window.map(|_| if ble_connected { BleState::Connected } else { BleState::Waiting });
    let batt_mv = ui.battery_mv(app.board.batt_mv);
    View { page: ui.page(), page_idx, pages, bar: Bar { ble, batt_mv: (batt_mv > 0).then_some(batt_mv) }, body, alert, pin: None }
}
