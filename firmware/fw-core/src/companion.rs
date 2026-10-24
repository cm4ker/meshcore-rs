//! The companion radio firmware's main loop, for any chip and board.
//!
//! An app (MeshCore for Android/iOS, meshcore-cli, the Python `meshcore` library) talks to the
//! node over BLE (pairing PIN on the screen, if any) and USB serial, and on chips with Wi-Fi over
//! TCP port 5000. Its data (identity, contacts, channels, paired phones) lives in the store's
//! companion region.
//!
//! The board crate creates the radio driver, the [`Platform`] (the chip: links, watchdog,
//! firmware slots) and a [`Board`] (front end, LED, screen, button, battery), then hands over to
//! [`run`].

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use embassy_executor::Spawner;
use embassy_futures::select::{Either3, select3};
use embassy_time::{Instant, Timer};
use embedded_storage::nor_flash::NorFlash;
use log::{error, info, warn};
use meshcore::companion::{BlobStore, Board as BoardInfo, Companion, CompanionPrefs, RecentMessage};
use meshcore::crypto;
use meshcore::identity::LocalIdentity;
use meshcore::mesh::{Mesh, MeshCtx};
use meshcore::ota::FirmwareTarget;
use meshcore::prefs::NodePrefs;
use meshcore::radio::RadioParams;
use meshcore::util::to_hex;
use meshcore::{PRV_KEY_SIZE, PUB_KEY_SIZE};

use crate::frames::{FRAMES_IN, Link, TCP_CONNECTED, TCP_OUT};
use crate::gps;
use crate::links::{BLE_CONNECTED, BLE_ENABLED, BLE_OUT, BLE_PASSKEY, BONDS_CHANGED};
use crate::noise::NoiseFloor;
use crate::radio::{LoraRadio, RxEvent};
use crate::storage::{Blob, CompBlob, SharedStore};
use meshcore_ui::{self as ui, Action, Alert, Bar, BleState, Body, Button, GpsInfo, Node, Page, RadioInfo, Ui, View};

const SCREEN_ON_MS: u64 = 60_000;
const TRIAL_HEALTHY_MS: u64 = 120_000;
const CLOCK_SAVE_INTERVAL_MS: u64 = 30 * 60 * 1000;
/// Contact lists are written at most this often (each write appends a snapshot to the log).
const CONTACTS_SAVE_INTERVAL_MS: u64 = 30_000;
/// The companion TCP port (upstream `SerialWifiInterface`).
pub const TCP_PORT: u16 = 5000;
/// A button press this long sends a zero-hop advert.
const ADVERT_PRESS_MS: u64 = 1500;
/// Static BLE PIN of boards without a screen when none is set (upstream `BLE_PIN_CODE`).
const DEFAULT_BLE_PIN: u32 = 123_456;
/// The clock follows the GPS this often (upstream `TIME_SYNC_INTERVAL`).
const GPS_TIME_SYNC_MS: u64 = 30 * 60 * 1000;

/// Board-specific hardware around the radio.
// Only used generically (never as `dyn`), so the futures' `Send`-ness does not matter.
#[allow(async_fn_in_trait)]
pub trait Board {
    /// Shown to apps (device info, `board`).
    fn manufacturer(&self) -> &str;
    fn max_tx_power_dbm(&self) -> i8;
    /// Around each transmission: front-end module, TX LED.
    fn on_tx_start(&mut self) {}
    fn on_tx_end(&mut self) {}
    /// External LNA (if the front end has a switchable one).
    fn set_lna(&mut self, _on: bool) {}
    fn power_off(&mut self) {}
    async fn battery_mv(&mut self) -> u16 {
        0
    }
    fn button_down(&mut self) -> bool {
        false
    }
    /// A screen that can show the random BLE pairing PIN.
    fn has_screen(&self) -> bool {
        false
    }
    /// The screen went dark (a minute after the last press, unless it keeps its image).
    fn screen_off(&mut self) {}
    /// A screen that keeps its image unpowered (e-paper): it never goes dark, and the board
    /// decides when to redraw.
    fn always_on(&self) -> bool {
        false
    }
    /// The pages of the on-device interface ([`meshcore_ui`]), the home page first; none = no
    /// interface (a press of the button sends an advert).
    fn ui_pages(&self) -> &'static [Page] {
        &[]
    }
    /// The interface's page while the screen is lit, about once a second and right after a
    /// press: the board renders it and decides when to redraw.
    fn show_view(&mut self, _view: &View) {}
    /// A GPS receiver, whose readings the board's serial task hands to [`gps::publish`].
    fn has_gps(&self) -> bool {
        false
    }
    /// The GPS on or off: the board powers it, sets it up and reads it.
    fn set_gps(&mut self, _on: bool) {}
}

/// What a platform needs to bring BLE up.
pub struct BleSetup {
    /// Advertised name: `MeshCore-<node name>`.
    pub name: String,
    /// Serialized bonds from the store.
    pub bonds: Vec<u8>,
    /// Fixed pairing PIN; `None` = a random one per pairing (shown on the screen).
    pub pin: Option<u32>,
}

/// The app links a platform started (USB is always there).
#[derive(Clone, Copy, Default)]
pub struct Links {
    pub ble: bool,
    pub tcp: bool,
}

/// The chip under the companion loop.
pub trait Platform {
    type Flash: NorFlash + 'static;

    /// Called on every loop pass.
    fn feed_watchdog(&mut self) {}
    /// Reboot like a cold start.
    fn reboot(&mut self) -> !;
    /// A hardware random number.
    fn random(&mut self) -> u32;
    /// The wall clock kept in RAM across resets, if the chip has such RAM.
    fn ram_clock(&self) -> Option<u32> {
        None
    }
    fn set_ram_clock(&mut self, _epoch: u32) {}
    /// The inactive firmware slot, for updates over the app link.
    fn ota_target(&mut self) -> Option<Box<dyn FirmwareTarget>> {
        None
    }
    /// A fresh update runs on trial; [`Self::confirm_trial`] keeps it.
    fn in_trial(&self) -> bool {
        false
    }
    fn confirm_trial(&mut self) {}
    /// Start the app links besides USB (BLE, Wi-Fi/TCP) from the stored settings. From here on
    /// the USB port carries companion frames only.
    fn start_links(&mut self, spawner: &Spawner, prefs: &CompanionPrefs, ble: BleSetup) -> Links;
    /// A frame for the USB host (dropped if nobody reads the port).
    fn usb_write(&mut self, frame: &[u8]);
    /// Room for another frame towards the USB host: a port whose writes queue up (rather than
    /// block) paces the frames for the app, like the BLE and TCP queues do.
    fn usb_has_room(&self) -> bool {
        true
    }
    /// Network time (SNTP) and the LAN address, if the platform has a network.
    fn poll_network(&mut self) -> (Option<u32>, Option<String>) {
        (None, None)
    }
}

/// Defaults of this board build and the build stamp.
pub struct Config {
    /// Radio settings until the app sets others (or the repeater firmware's, if any).
    pub radio: RadioParams,
    pub tx_power_dbm: i8,
    pub build_epoch: u32,
    pub build_date: &'static str,
}

/// Raw adverts of the contacts, in the companion region's advert table.
struct FlashBlobs<F>(SharedStore<F>);

impl<F: NorFlash> BlobStore for FlashBlobs<F> {
    fn get(&self, key: &[u8; PUB_KEY_SIZE]) -> Option<Vec<u8>> {
        self.0.borrow_mut().advert_get(key)
    }

    fn put(&mut self, key: &[u8; PUB_KEY_SIZE], data: &[u8]) -> bool {
        self.0.borrow_mut().advert_put(key, data)
    }

    fn delete(&mut self, key: &[u8; PUB_KEY_SIZE]) {
        self.0.borrow_mut().advert_delete(key)
    }

    fn usage_kb(&self) -> (u32, u32) {
        self.0.borrow_mut().comp_usage_kb()
    }
}

fn now_ms() -> u64 {
    Instant::now().as_millis()
}

/// The interface's current page with what it shows; `unread`: messages since the messages page
/// was last open.
fn build_view(
    ui: &mut Ui, mesh: &Mesh<Companion>, ble: BleState, fixed_pin: Option<u32>, pin: u32, unread: u32, now: u64,
) -> View {
    let app = &mesh.app;
    let clock = mesh.ctx.current_time();
    let batt_mv = ui.battery_mv(app.board.batt_mv);
    let message = |m: &RecentMessage| ui::Message {
        from: m.from.clone(),
        text: m.text.clone(),
        age_min: ui::coarse_minutes(clock.saturating_sub(m.received)),
    };
    let body = match ui.page() {
        Page::Home => Body::Home { name: app.prefs.node_name.clone(), unread },
        Page::Messages => Body::Messages(app.recent.iter().rev().map(message).collect()),
        Page::Nearby => Body::Nearby(
            app.heard
                .iter()
                .map(|h| Node {
                    name: h.name.clone(),
                    hops: h.hops,
                    age_min: ui::coarse_minutes(clock.saturating_sub(h.heard)),
                })
                .collect(),
        ),
        Page::Radio => {
            let (s, r) = (&mesh.ctx.stats, &app.prefs.radio);
            Body::Radio(RadioInfo {
                freq_mhz: r.freq_mhz,
                bw_khz: r.bw_khz,
                sf: r.sf,
                cr: r.cr,
                tx_dbm: app.prefs.tx_power_dbm,
                noise_dbm: s.noise_floor,
                last_rssi: s.last_rssi as i16,
                last_snr: (s.last_snr * 4.0) as i16,
                rx: s.n_packets_recv,
                tx: s.n_packets_sent,
                tx_air_min: (s.total_air_time_ms / 60_000) as u32,
                rx_air_min: (s.rx_air_time_ms / 60_000) as u32,
                uptime_min: (now / 60_000) as u32,
                batt_mv: app.board.batt_mv,
            })
        }
        Page::Bluetooth => Body::Bluetooth {
            state: ble,
            name: alloc::format!("MeshCore-{}", app.prefs.node_name),
            pin: fixed_pin,
        },
        Page::Gps => {
            let fix = gps::latest();
            Body::Gps(GpsInfo {
                on: app.gps_active(),
                receiving: fix.is_some(),
                fix: fix.filter(|f| f.valid).map(|f| (f.lat, f.lon)),
                satellites: fix.map_or(0, |f| f.satellites),
                in_view: fix.map_or(0, |f| f.in_view),
            })
        }
        Page::Advert => Body::Advert { flood: false },
        Page::PowerOff => Body::PowerOff,
        // repeater pages
        _ => Body::Home { name: app.prefs.node_name.clone(), unread },
    };
    let (page_idx, pages, alert) = ui.view_frame();
    View {
        page: ui.page(),
        page_idx,
        pages,
        bar: Bar { ble: Some(ble), batt_mv: (batt_mv > 0).then_some(batt_mv) },
        body,
        alert,
        pin: (pin != 0).then_some(pin),
    }
}

fn comp_load<F: NorFlash>(store: &Option<SharedStore<F>>, blob: CompBlob) -> Option<Vec<u8>> {
    store.as_ref()?.borrow_mut().comp_load(blob)
}

fn comp_save<F: NorFlash>(store: &Option<SharedStore<F>>, blob: CompBlob, data: &[u8]) -> bool {
    store.as_ref().is_some_and(|s| s.borrow_mut().comp_save(blob, data))
}

fn load_identity<F: NorFlash>(store: &Option<SharedStore<F>>) -> Option<LocalIdentity> {
    let data = comp_load(store, CompBlob::Identity)?;
    let prv: [u8; PRV_KEY_SIZE] = data.get(..PRV_KEY_SIZE)?.try_into().ok()?;
    let id = LocalIdentity::from_private_key(&prv);
    (data.get(PRV_KEY_SIZE..) == Some(&id.pub_key()[..])).then_some(id)
}

fn save_identity<F: NorFlash>(store: &Option<SharedStore<F>>, prv: &[u8; PRV_KEY_SIZE]) -> bool {
    let id = LocalIdentity::from_private_key(prv);
    let mut blob = prv.to_vec();
    blob.extend_from_slice(id.pub_key());
    comp_save(store, CompBlob::Identity, &blob)
}

/// Run the companion: `radio` not initialised yet (configured from the stored settings), `trng`
/// from the chip's random number generator (mixed with the radio's own random numbers).
pub async fn run<P: Platform, R: LoraRadio, B: Board>(
    spawner: Spawner, mut platform: P, mut radio: R, store: Option<SharedStore<P::Flash>>, trng: [u8; 32],
    mut board: B, cfg: Config,
) -> ! {
    if !store.as_ref().is_some_and(|s| s.borrow().has_companion_partition()) {
        warn!("no companion storage: contacts and settings will not persist");
    }
    let mut in_trial = platform.in_trial();

    let max_tx = board.max_tx_power_dbm();
    let mut prefs = match comp_load(&store, CompBlob::Prefs) {
        Some(d) => CompanionPrefs::deserialize(&String::from_utf8_lossy(&d), max_tx),
        None => {
            let mut p = CompanionPrefs { radio: cfg.radio, tx_power_dbm: cfg.tx_power_dbm, ..Default::default() };
            // first start after the repeater firmware: join the mesh it was set up for
            if let Some(d) = store.as_ref().and_then(|s| s.borrow_mut().load(Blob::Prefs)) {
                let rp = NodePrefs::deserialize(&String::from_utf8_lossy(&d));
                p.radio = rp.radio;
                p.tx_power_dbm = rp.tx_power_dbm;
                p.rx_boosted_gain = rp.rx_boosted_gain;
                p.radio_fem_rxgain = rp.radio_fem_rxgain;
            }
            p
        }
    };
    prefs.sanitize(max_tx);
    board.set_lna(prefs.radio_fem_rxgain);

    if let Err(e) = radio.init(prefs.radio, prefs.tx_power_dbm, prefs.rx_boosted_gain).await {
        error!("radio init failed: {e:?}");
        loop {
            board.on_tx_start();
            Timer::after_millis(200).await;
            board.on_tx_end();
            Timer::after_millis(200).await;
            platform.feed_watchdog();
        }
    }
    let mut entropy = [0u8; 64];
    entropy[..32].copy_from_slice(&trng);
    let _ = radio.random_bytes(&mut entropy[32..]).await;

    // ---- identity (its own, not the repeater's)
    let self_id = load_identity(&store).unwrap_or_else(|| {
        let mut counter = 0u8;
        let id = loop {
            let mut seed = [0u8; 32];
            crypto::sha256(&mut seed, &[&entropy, b"companion", &[counter]]);
            let id = LocalIdentity::from_seed(&seed);
            counter = counter.wrapping_add(1);
            if ![0x00, 0xFF].contains(&id.pub_key()[0]) || counter > 10 {
                break id;
            }
        };
        if !save_identity(&store, id.private_key()) {
            warn!("identity save failed");
        }
        id
    });
    info!("Companion ID: {}", to_hex(self_id.pub_key()));
    let batt_mv = board.battery_mv().await;
    platform.feed_watchdog();

    // ---- mesh
    let mut rng_seed = [0u8; 16];
    for (k, s) in rng_seed.iter_mut().enumerate() {
        *s = (platform.random() >> (8 * (k % 4))) as u8 ^ entropy[32 + k];
    }
    let ram_epoch = platform.ram_clock().map(|t| t.saturating_add(2)).unwrap_or(0);
    let saved_epoch = comp_load(&store, CompBlob::Clock)
        .and_then(|d| d.get(..4).map(|b| u32::from_le_bytes(b.try_into().unwrap())))
        .unwrap_or(0);
    let epoch = cfg.build_epoch.max(saved_epoch.saturating_add(1)).max(ram_epoch);
    platform.set_ram_clock(epoch);

    let ctx = MeshCtx::new(self_id.clone(), rng_seed, prefs.radio, epoch);
    let info = BoardInfo {
        manufacturer: board.manufacturer().into(),
        build_date: cfg.build_date.into(),
        batt_mv,
        mcu_temp_c: None,
        max_tx_power_dbm: max_tx,
        has_gps: board.has_gps(),
        ..Default::default()
    };
    let mut mesh = Mesh::new(ctx, Companion::new(prefs, info));
    if let Some(s) = &store {
        mesh.app.blobs = Box::new(FlashBlobs(s.clone()));
        if let Some(contacts) = s.borrow_mut().comp_log_latest() {
            mesh.app.contacts_load(&contacts);
        }
    }
    if let Some(channels) = comp_load(&store, CompBlob::Channels) {
        mesh.app.channels_load(&channels);
    }
    // firmware updates over the app link
    mesh.app.ota.target = platform.ota_target();
    mesh.begin(now_ms());
    mesh.app.start(&mut mesh.ctx);
    info!("{} contacts", mesh.app.num_contacts());
    if board.has_gps() {
        board.set_gps(mesh.app.prefs.gps_enabled);
    }

    // ---- the app links
    let ble = BleSetup {
        name: alloc::format!("MeshCore-{}", mesh.app.prefs.node_name),
        bonds: comp_load(&store, CompBlob::Bonds).unwrap_or_default(),
        // the PIN set by the app; else a random one on the screen, or upstream's default
        pin: match mesh.app.prefs.ble_pin {
            0 if board.has_screen() => None,
            0 => Some(DEFAULT_BLE_PIN),
            pin => Some(pin),
        },
    };
    let fixed_pin = ble.pin;
    let links = platform.start_links(&spawner, &mesh.app.prefs, ble);
    info!(
        "Ready: companion frames on USB{}{}",
        if links.ble { ", BLE" } else { "" },
        if links.tcp { ", TCP 5000" } else { "" }
    );
    // from here on the USB port carries companion frames only
    log::set_max_level(log::LevelFilter::Off);

    let mut noise = NoiseFloor::new();
    let mut next_batt = now_ms() + 60_000;
    let mut next_clock_save = now_ms() + CLOCK_SAVE_INTERVAL_MS;
    let mut last_clock = mesh.ctx.current_time();
    let mut contacts_pending = false;
    let mut last_contacts_save = 0u64;
    let mut reboot_at: Option<u64> = None;
    let confirm_at = now_ms() + TRIAL_HEALTHY_MS;
    let mut button_down_at: Option<u64> = None;
    // where replies go: the link of the latest request
    let mut reply_to = Link::Usb;
    let mut next_screen = 0u64;
    let mut ui = Ui::new(board.ui_pages());
    if !board.always_on() {
        ui = ui.with_screen_timeout(SCREEN_ON_MS, now_ms());
    }
    let mut button = Button::default();
    let mut next_gps = 0u64;
    let mut next_gps_position = 0u64;
    let mut next_gps_time = 0u64;
    let mut gps_fix_secs = 0u32;
    let mut messages_seen = 0u32;
    let mut messages_lit = 0u32;
    loop {
        platform.feed_watchdog();
        let now = now_ms();

        // transmit if something is due and the channel is clear
        if mesh.tx_pending(now) {
            let thr = mesh.app.prefs.interference_threshold as i16;
            let mut busy = radio.is_receiving().unwrap_or(false);
            if !busy && thr != 0 {
                busy = radio.rssi_inst().is_ok_and(|r| r > noise.floor + thr);
            }
            if !busy && mesh.app.prefs.cad_enabled {
                busy = radio.channel_activity().await.unwrap_or(false);
            }
            if !(busy && mesh.tx_defer_busy(now))
                && let Some(frame) = mesh.tx_take(now)
            {
                board.on_tx_start();
                let res = radio.transmit(frame.bytes(), frame.timeout_ms).await;
                board.on_tx_end();
                let _ = radio.start_rx();
                match res {
                    Ok(()) => mesh.tx_done(now_ms()),
                    Err(_) => mesh.tx_failed(now_ms()),
                }
                continue;
            }
        }

        let wake = mesh.next_wakeup().min(now + 50).max(now + 1);
        match select3(radio.wait_irq(), FRAMES_IN.receive(), Timer::at(Instant::from_millis(wake))).await {
            Either3::First(()) => match radio.read_rx() {
                Ok(RxEvent::Frame(f)) => mesh.on_rx_frame(now_ms(), &f.data[..f.len], f.snr, f.rssi),
                Ok(RxEvent::CrcError) => mesh.ctx.stats.n_recv_errors += 1,
                _ => {}
            },
            Either3::Second((link, frame)) => {
                reply_to = link;
                mesh.ctx.now = now_ms();
                mesh.app.handle_frame(&mut mesh.ctx, &frame);
            }
            Either3::Third(()) => {}
        }

        let now = now_ms();
        mesh.poll(now);

        // frames for the app: to every interface, no faster than the slowest one takes them (the
        // rest wait in the companion; the contact list is only produced as it drains)
        let tcp = TCP_CONNECTED.load(Ordering::Relaxed);
        let ble_app = BLE_CONNECTED.load(Ordering::Relaxed);
        while platform.usb_has_room()
            && (!ble_app || BLE_OUT.free_capacity() > 0)
            && (!tcp || TCP_OUT.free_capacity() > 0)
            && let Some(f) = mesh.app.take_frame()
        {
            // pushes (codes from 0x80) go to every app; replies (and a contact list in progress)
            // to the link of the latest request, so two apps do not see each other's answers
            let push = f.first().is_some_and(|c| *c >= 0x80);
            if push || reply_to == Link::Usb {
                platform.usb_write(&f);
            }
            if ble_app && (push || reply_to == Link::Ble) {
                let _ = BLE_OUT.try_send(f.clone());
            }
            if tcp && (push || reply_to == Link::Tcp) {
                let _ = TCP_OUT.try_send(f);
            }
        }
        if let Some(bonds) = BONDS_CHANGED.try_take() {
            comp_save(&store, CompBlob::Bonds, &bonds);
        }
        let pin = BLE_PASSKEY.load(Ordering::Relaxed);
        if pin != 0 {
            // the phone asks for the PIN on the screen: keep it lit while pairing
            if !ui.screen_on(now) {
                next_screen = 0;
            }
            ui.wake(now);
        }

        if !radio.is_receiving().unwrap_or(true)
            && let Ok(rssi) = radio.rssi_inst()
        {
            noise.sample(rssi);
            mesh.ctx.stats.noise_floor = noise.floor;
        }
        if now >= next_batt {
            next_batt = now + 60_000;
            mesh.app.board.batt_mv = board.battery_mv().await;
        }

        let ble_state = match (links.ble && BLE_ENABLED.load(Ordering::Relaxed), ble_app) {
            (false, _) => BleState::Off,
            (true, false) => BleState::Waiting,
            (true, true) => BleState::Connected,
        };
        if ui.enabled() {
            ui.tick(now);
            let down = board.button_down();
            if down && !button.is_down() {
                ui.button_down(now);
                next_screen = 0;
            }
            if let Some(intent) = button.poll(down, now) {
                next_screen = 0;
                match ui.press(intent, now) {
                    Some(Action::Advert) => {
                        let sent = mesh.app.advert(&mut mesh.ctx);
                        ui.alert(if sent { Alert::AdvertSent } else { Alert::AdvertFailed }, now);
                    }
                    Some(Action::ToggleBle) => {
                        let on = !BLE_ENABLED.load(Ordering::Relaxed);
                        BLE_ENABLED.store(on, Ordering::Relaxed);
                        ui.alert(if on { Alert::BleOn } else { Alert::BleOff }, now);
                    }
                    Some(Action::ToggleGps) => {
                        let on = !mesh.app.prefs.gps_enabled;
                        mesh.app.prefs.gps_enabled = on;
                        (mesh.app.effects.gps, mesh.app.effects.save_prefs) = (true, true);
                        ui.alert(if on { Alert::GpsOn } else { Alert::GpsOff }, now);
                    }
                    Some(Action::PowerOff) => mesh.app.effects.power_off = true,
                    Some(Action::ToggleUpdate) | None => {}
                }
            }
            if ui.page() == Page::Messages {
                messages_seen = mesh.app.messages_received;
            }
            // a new message lights a dark screen
            if mesh.app.messages_received != messages_lit {
                messages_lit = mesh.app.messages_received;
                ui.wake(now);
                next_screen = 0;
            }
            if !ui.screen_on(now) {
                board.screen_off();
            } else if now >= next_screen {
                next_screen = now + 1000;
                let unread = mesh.app.messages_received.wrapping_sub(messages_seen);
                let view = build_view(&mut ui, &mesh, ble_state, fixed_pin, pin, unread, now);
                board.show_view(&view);
            }
        } else {
            // no interface: a long press of the button sends a zero-hop advert
            match (board.button_down(), button_down_at) {
                (true, None) => button_down_at = Some(now),
                (false, Some(t)) => {
                    button_down_at = None;
                    if now - t >= ADVERT_PRESS_MS {
                        mesh.app.advert(&mut mesh.ctx);
                    }
                }
                _ => {}
            }
        }

        // network: SNTP time, address for `get wifi.*`
        let (time, ip) = platform.poll_network();
        if let Some(t) = time
            && t.abs_diff(mesh.ctx.current_time()) > 2
        {
            mesh.ctx.set_current_time(t);
        }
        if links.tcp {
            mesh.app.board.wifi_ip = ip;
        }

        // GPS: the node's position, and now and then the clock, once the fix has held for a few
        // seconds (upstream `EnvironmentSensorManager`, `MicroNMEALocationProvider`)
        if mesh.app.gps_active() && now >= next_gps {
            next_gps = now + 1000;
            match gps::latest().filter(|f| f.valid) {
                Some(fix) => {
                    gps_fix_secs += 1;
                    if now >= next_gps_position {
                        next_gps_position = now + mesh.app.prefs.gps_interval.max(1) as u64 * 1000;
                        mesh.app.set_gps_position(fix.lat, fix.lon, fix.alt_mm);
                    }
                    if gps_fix_secs > 2
                        && now >= next_gps_time
                        && let Some(t) = fix.time
                    {
                        next_gps_time = now + GPS_TIME_SYNC_MS;
                        mesh.ctx.set_current_time(t);
                    }
                }
                None => gps_fix_secs = 0,
            }
        }

        // clock persistence
        let clock = mesh.ctx.current_time();
        let jumped = clock.abs_diff(last_clock) > 120;
        if clock != last_clock {
            platform.set_ram_clock(clock);
        }
        last_clock = clock;
        if now >= next_clock_save || jumped {
            next_clock_save = now + CLOCK_SAVE_INTERVAL_MS;
            comp_save(&store, CompBlob::Clock, &clock.to_le_bytes());
        }

        // side effects
        let fx = mesh.app.take_effects();
        if fx.save_prefs {
            comp_save(&store, CompBlob::Prefs, mesh.app.prefs.serialize().as_bytes());
        }
        if fx.save_channels {
            comp_save(&store, CompBlob::Channels, &mesh.app.channels_serialize());
        }
        if let Some(prv) = fx.save_identity {
            save_identity(&store, &prv);
        }
        contacts_pending |= fx.save_contacts;
        if contacts_pending && (now >= last_contacts_save + CONTACTS_SAVE_INTERVAL_MS || fx.reboot) {
            contacts_pending = false;
            last_contacts_save = now;
            if let Some(s) = &store {
                s.borrow_mut().comp_log_append(&mesh.app.contacts_serialize());
            }
        }
        if let Some(params) = fx.apply_radio
            && radio.apply_params(params).and_then(|_| radio.start_rx()).is_ok()
        {
            mesh.ctx.radio = params;
        }
        if let Some(dbm) = fx.tx_power {
            let _ = radio.set_tx_power(dbm);
        }
        if let Some(on) = fx.rx_boosted_gain {
            let _ = radio.set_rx_boosted_gain(on);
        }
        if let Some(on) = fx.fem_lna {
            board.set_lna(on);
        }
        if fx.gps {
            board.set_gps(mesh.app.gps_active());
            gps::clear();
            gps_fix_secs = 0;
        }
        if fx.factory_reset
            && let Some(s) = &store
        {
            s.borrow_mut().comp_erase();
        }
        if fx.reboot {
            reboot_at = Some(now + 1000);
        }
        if fx.power_off {
            let _ = radio.sleep();
            if ui.enabled() {
                let mut view = build_view(&mut ui, &mesh, BleState::Off, None, 0, 0, now);
                view.body = Body::Off { name: mesh.app.prefs.node_name.clone() };
                (view.alert, view.pin) = (None, None);
                board.show_view(&view);
            }
            board.power_off();
            loop {
                Timer::after_secs(3600).await;
            }
        }
        if in_trial && now >= confirm_at {
            platform.confirm_trial();
            in_trial = false;
        }
        if reboot_at.is_some_and(|t| now >= t) {
            comp_save(&store, CompBlob::Clock, &mesh.ctx.current_time().to_le_bytes());
            platform.reboot();
        }
    }
}
