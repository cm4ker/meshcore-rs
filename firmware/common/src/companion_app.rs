//! The companion radio firmware's main loop, for any board.
//!
//! An app (MeshCore for Android/iOS, meshcore-cli, the Python `meshcore` library) talks to the
//! node over BLE (pairing PIN on the screen, if any) and USB serial, or over TCP port 5000 instead
//! of BLE when Wi-Fi is configured. Its data (identity, contacts, channels, paired phones) lives in
//! the `companion` partition, apart from the repeater firmware's. Firmware updates also go over
//! the app link (`tools/ota-app`).
//!
//! The board crate creates the radio driver and a [`Board`] (front end, LED, screen, button,
//! battery), then hands over to [`run`].

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use embassy_executor::Spawner;
use embassy_futures::select::{Either3, select3};
use embassy_time::{Instant, Timer};
use esp_hal::Blocking;
use esp_hal::peripherals::{BT, WIFI};
use esp_hal::rng::Rng;
use esp_hal::usb::usb_serial_jtag::UsbSerialJtagRx;
use esp_println::println;
use log::{info, warn};
use meshcore::companion::{BlobStore, Board as BoardInfo, Companion, CompanionPrefs};
use meshcore::crypto;
use meshcore::identity::LocalIdentity;
use meshcore::mesh::{Mesh, MeshCtx};
use meshcore::prefs::NodePrefs;
use meshcore::radio::RadioParams;
use meshcore::util::to_hex;
use meshcore::{PRV_KEY_SIZE, PUB_KEY_SIZE};

use crate::ble::{self, BLE_CONNECTED, BLE_OUT, BLE_PASSKEY, BONDS_CHANGED};
use crate::frames::{self, FRAMES_IN, Link, TCP_CONNECTED, TCP_OUT};
use crate::noise::NoiseFloor;
use crate::ota::{self, SharedStore};
use crate::radio::{LoraRadio, RxEvent};
use crate::storage::{Blob, CompBlob};
use crate::{clock_cell, net, reset};

const SCREEN_ON_MS: u64 = 60_000;
const TRIAL_HEALTHY_MS: u64 = 120_000;
const CLOCK_SAVE_INTERVAL_MS: u64 = 30 * 60 * 1000;
/// Contact lists are written at most this often (each write appends a snapshot to the log).
const CONTACTS_SAVE_INTERVAL_MS: u64 = 30_000;
const TCP_PORT: u16 = 5000;
/// A button press this long sends a zero-hop advert.
const ADVERT_PRESS_MS: u64 = 1500;
/// Static BLE PIN of boards without a screen when none is set (upstream `BLE_PIN_CODE`).
const DEFAULT_BLE_PIN: u32 = 123_456;

/// What the status screen shows (once a second while it is on).
pub struct Status<'a> {
    pub name: &'a str,
    pub radio: &'a str,
    pub rx: u32,
    pub tx: u32,
    pub noise_floor: i16,
    pub last_rssi: f32,
    pub last_snr: f32,
    pub batt_mv: u16,
    pub uptime_s: u64,
    pub contacts: usize,
    pub id_prefix: &'a str,
    pub note: Option<&'a str>,
}

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
    fn show_status(&mut self, _status: &Status<'_>) {}
    fn screen_off(&mut self) {}
}

/// Defaults of this board build and the build stamp.
pub struct Config {
    /// Radio settings until the app sets others (or the repeater firmware's, if any).
    pub radio: RadioParams,
    pub tx_power_dbm: i8,
    pub build_epoch: u32,
    pub build_date: &'static str,
}

/// The peripherals the companion loop owns.
pub struct Hardware<R> {
    /// Not initialised yet: [`run`] configures it from the stored settings.
    pub radio: R,
    pub store: Option<SharedStore<'static>>,
    pub usb_rx: UsbSerialJtagRx<'static, Blocking>,
    pub wifi: WIFI<'static>,
    pub bt: BT<'static>,
    /// From the hardware TRNG, mixed with the radio's own random numbers.
    pub entropy: [u8; 32],
}

/// Raw adverts of the contacts, in the companion partition's advert table.
struct FlashBlobs(SharedStore<'static>);

impl BlobStore for FlashBlobs {
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

fn comp_load(store: &Option<SharedStore<'_>>, blob: CompBlob) -> Option<Vec<u8>> {
    store.as_ref()?.borrow_mut().comp_load(blob)
}

fn comp_save(store: &Option<SharedStore<'_>>, blob: CompBlob, data: &[u8]) -> bool {
    store.as_ref().is_some_and(|s| s.borrow_mut().comp_save(blob, data))
}

fn load_identity(store: &Option<SharedStore<'_>>) -> Option<LocalIdentity> {
    let data = comp_load(store, CompBlob::Identity)?;
    let prv: [u8; PRV_KEY_SIZE] = data.get(..PRV_KEY_SIZE)?.try_into().ok()?;
    let id = LocalIdentity::from_private_key(&prv);
    (data.get(PRV_KEY_SIZE..) == Some(&id.pub_key()[..])).then_some(id)
}

fn save_identity(store: &Option<SharedStore<'_>>, prv: &[u8; PRV_KEY_SIZE]) -> bool {
    let id = LocalIdentity::from_private_key(prv);
    let mut blob = prv.to_vec();
    blob.extend_from_slice(id.pub_key());
    comp_save(store, CompBlob::Identity, &blob)
}

pub async fn run<R: LoraRadio, B: Board>(spawner: Spawner, hw: Hardware<R>, mut board: B, cfg: Config) -> ! {
    let Hardware { mut radio, store, usb_rx, wifi, bt, entropy: trng } = hw;
    if !store.as_ref().is_some_and(|s| s.borrow().has_companion_partition()) {
        warn!("no 'companion' partition: contacts and settings will not persist");
    }
    let slots = store.as_ref().and_then(|s| ota::slots(&mut s.borrow_mut()));
    let mut in_trial = match (&store, slots) {
        (Some(s), Some(sl)) => ota::check_trial(&mut s.borrow_mut(), sl.booted),
        _ => false,
    };

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
        println!("ERROR: radio init failed: {e:?}");
        loop {
            board.on_tx_start();
            Timer::after_millis(200).await;
            board.on_tx_end();
            Timer::after_millis(200).await;
            reset::feed();
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
    println!("Companion ID: {}", to_hex(self_id.pub_key()));
    let batt_mv = board.battery_mv().await;
    reset::feed();

    // ---- mesh
    let mut rng_seed = [0u8; 16];
    Rng::new().read(&mut rng_seed);
    for (s, e) in rng_seed.iter_mut().zip(entropy[32..].iter()) {
        *s ^= *e;
    }
    let ram_epoch = clock_cell::load().map(|t| t.saturating_add(2)).unwrap_or(0);
    let saved_epoch = comp_load(&store, CompBlob::Clock)
        .and_then(|d| d.get(..4).map(|b| u32::from_le_bytes(b.try_into().unwrap())))
        .unwrap_or(0);
    let epoch = cfg.build_epoch.max(saved_epoch.saturating_add(1)).max(ram_epoch);
    clock_cell::store(epoch);

    let ctx = MeshCtx::new(self_id.clone(), rng_seed, prefs.radio, epoch);
    let info = BoardInfo {
        manufacturer: board.manufacturer().into(),
        build_date: cfg.build_date.into(),
        batt_mv,
        mcu_temp_c: None,
        max_tx_power_dbm: max_tx,
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
    if let (Some(s), Some(sl)) = (&store, slots) {
        // firmware updates over the app link (BLE, USB, TCP)
        mesh.app.ota.target = Some(Box::new(ota::FlashTarget::new(s.clone(), sl)));
    }
    mesh.begin(now_ms());
    mesh.app.start(&mut mesh.ctx);
    info!("{} contacts", mesh.app.num_contacts());

    // ---- Wi-Fi: the TCP companion interface
    let mut station = None;
    let mut wifi = Some(wifi);
    if mesh.app.prefs.wifi_enabled && !mesh.app.prefs.wifi_ssid.is_empty() {
        let seed = (Rng::new().random() as u64) << 32 | Rng::new().random() as u64;
        let params = net::StationParams {
            ssid: mesh.app.prefs.wifi_ssid.clone(),
            password: mesh.app.prefs.wifi_pwd.clone(),
            power_save: false, // the app connects in
        };
        match net::start(&spawner, wifi.take().unwrap(), seed, params) {
            Ok(stack) => {
                station = Some(stack);
                spawner.spawn(frames::tcp_server_task(stack, TCP_PORT).unwrap());
            }
            Err(e) => warn!("Wi-Fi station: {e}"),
        }
    }

    // ---- BLE, next to Wi-Fi only where esp-radio coordinates the two radios (`coex`). On the C6
    // its BLE side is still stubbed out (esp-radio 1.0.0-beta.1): Wi-Fi starves while an app is
    // connected over BLE, so there Wi-Fi (TCP) replaces BLE as upstream's builds do.
    let ble_on = station.is_none() || cfg!(feature = "esp32s3");
    if ble_on {
        let params = ble::Params {
            name: alloc::format!("MeshCore-{}", mesh.app.prefs.node_name),
            address: ble::static_address(),
            bonds: comp_load(&store, CompBlob::Bonds).unwrap_or_default(),
            // the PIN set by the app; else a random one on the screen, or upstream's default
            pin: match mesh.app.prefs.ble_pin {
                0 if board.has_screen() => None,
                0 => Some(DEFAULT_BLE_PIN),
                pin => Some(pin),
            },
        };
        spawner.spawn(ble::ble_task(bt, params).unwrap());
    }

    // ---- USB: from here on the port carries companion frames only
    spawner.spawn(frames::usb_rx_task(usb_rx).unwrap());
    println!(
        "Ready: companion frames on USB{}{}",
        if ble_on { ", BLE" } else { "" },
        if station.is_some() { ", TCP 5000" } else { "" }
    );
    log::set_max_level(log::LevelFilter::Off);

    let id_prefix = alloc::format!("ID {}", to_hex(&self_id.pub_key()[..4]));
    let mut noise = NoiseFloor::new();
    let mut next_batt = now_ms() + 60_000;
    let mut next_clock_save = now_ms() + CLOCK_SAVE_INTERVAL_MS;
    let mut last_clock = mesh.ctx.current_time();
    let mut contacts_pending = false;
    let mut last_contacts_save = 0u64;
    let mut reboot_at: Option<u64> = None;
    let confirm_at = now_ms() + TRIAL_HEALTHY_MS;
    let mut button_down_at: Option<u64> = None;
    let mut screen_until = now_ms() + SCREEN_ON_MS;
    // where replies go: the link of the latest request
    let mut reply_to = Link::Usb;
    let mut next_screen = 0u64;
    loop {
        reset::feed();
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
                screen_until = screen_until.max(now_ms() + 10_000);
            }
            Either3::Third(()) => {}
        }

        let now = now_ms();
        mesh.poll(now);

        // frames for the app: to every interface, no faster than the slowest one takes them (the
        // rest wait in the companion; the contact list is only produced as it drains)
        let tcp = TCP_CONNECTED.load(Ordering::Relaxed);
        let ble_app = BLE_CONNECTED.load(Ordering::Relaxed);
        while (!ble_app || BLE_OUT.free_capacity() > 0)
            && (!tcp || TCP_OUT.free_capacity() > 0)
            && let Some(f) = mesh.app.take_frame()
        {
            // pushes (codes from 0x80) go to every app; replies (and a contact list in progress)
            // to the link of the latest request, so two apps do not see each other's answers
            let push = f.first().is_some_and(|c| *c >= 0x80);
            if push || reply_to == Link::Usb {
                frames::usb_write(&f);
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
        if pin != 0 && screen_until < now + 5000 {
            screen_until = now + 30_000; // the phone asks for the PIN on the screen
            next_screen = 0;
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

        // button: a press wakes the screen, a long press sends a zero-hop advert
        match (board.button_down(), button_down_at) {
            (true, None) => {
                button_down_at = Some(now);
                screen_until = now + SCREEN_ON_MS;
                next_screen = 0;
            }
            (false, Some(t)) => {
                button_down_at = None;
                if now - t >= ADVERT_PRESS_MS {
                    mesh.app.advert(&mut mesh.ctx);
                }
            }
            _ => {}
        }
        if now >= screen_until {
            board.screen_off();
        } else if now >= next_screen {
            next_screen = now + 1000;
            let pr = &mesh.app.prefs;
            let radio_line = alloc::format!(
                "{} {}k SF{} CR{}",
                meshcore::util::ftoa(pr.radio.freq_mhz as f64, 3),
                meshcore::util::ftoa(pr.radio.bw_khz as f64, 1),
                pr.radio.sf,
                pr.radio.cr
            );
            let waiting = mesh.app.queued_messages();
            let note = if pin != 0 {
                Some(alloc::format!("BLE PIN {pin:06}"))
            } else if in_trial {
                Some(String::from("OTA trial boot"))
            } else if waiting > 0 {
                Some(alloc::format!("{waiting} new message(s)"))
            } else if ble_app {
                Some(String::from("BLE: app connected"))
            } else {
                station
                    .and_then(|s| net::ipv4(&s))
                    .map(|ip| alloc::format!("{ip}:{TCP_PORT}"))
                    .or_else(|| ble_on.then(|| String::from("BLE: waiting for app")))
            };
            let s = &mesh.ctx.stats;
            board.show_status(&Status {
                name: &pr.node_name,
                radio: &radio_line,
                rx: s.n_packets_recv,
                tx: s.n_packets_sent,
                noise_floor: s.noise_floor,
                last_rssi: s.last_rssi,
                last_snr: s.last_snr,
                batt_mv: mesh.app.board.batt_mv,
                uptime_s: now / 1000,
                contacts: mesh.app.num_contacts(),
                id_prefix: &id_prefix,
                note: note.as_deref(),
            });
        }

        // network: SNTP time, status for `get wifi.*`
        if let Some(t) = net::TIME_SYNC.try_take()
            && t.abs_diff(mesh.ctx.current_time()) > 2
        {
            mesh.ctx.set_current_time(t);
        }
        if let Some(stack) = station {
            mesh.app.board.wifi_ip = net::ipv4(&stack);
        }

        // clock persistence
        let clock = mesh.ctx.current_time();
        let jumped = clock.abs_diff(last_clock) > 120;
        if clock != last_clock {
            clock_cell::store(clock);
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
            board.power_off();
            loop {
                Timer::after_secs(3600).await;
            }
        }
        if in_trial && now >= confirm_at {
            if let Some(s) = &store {
                ota::confirm(&mut s.borrow_mut());
            }
            in_trial = false;
        }
        if reboot_at.is_some_and(|t| now >= t) {
            comp_save(&store, CompBlob::Clock, &mesh.ctx.current_time().to_le_bytes());
            reset::hard_reset();
        }
    }
}
