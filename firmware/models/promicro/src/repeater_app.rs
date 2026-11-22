//! The repeater on an nRF52840 board, around `meshcore::repeater` (the same as on the Heltec
//! boards): the radio, the console on the USB port, the node's data in the internal flash, a new
//! firmware over LoRa (`ota`) or in the BLE update window (`fw ble`, `ble`, on the SoftDevice the
//! bootloader keeps). The board's binary sets the chip up, puts the parts on their pins
//! (`board_parts!`) and hands over to [`run`]; what a board adds (the T114's screen and GPS) comes
//! in through [`Extras`]. No Wi-Fi.

use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

use embassy_executor::Spawner;
use embassy_futures::select::{Either4, select4};
use embassy_nrf::Peri;
use embassy_nrf::nvmc::Nvmc;
use embassy_nrf::peripherals;
use embassy_nrf::rng::Rng;
use embassy_nrf::temp::Temp;
use embassy_nrf::wdt::{self, Watchdog};
use embassy_time::{Instant, Timer};
use embassy_usb::Builder;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use log::{info, warn};
use meshcore::crypto;
use meshcore::gps::GpsReading;
use meshcore::identity::LocalIdentity;
use meshcore::mesh::{Mesh, MeshCtx};
use meshcore::prefs::{NAME_MAX, NodePrefs};
use meshcore::repeater::{BLE_OTA_WINDOW_MINS, BoardInfo, Repeater};
use meshcore::timesync::{Clock, Source};
use meshcore::util::{Shortest, to_hex};
use meshcore::{PRV_KEY_SIZE, PUB_KEY_SIZE};
use meshcore_fw_core::links::{BLE_CONNECTED, BLE_OUT};
use meshcore_fw_core::noise::NoiseFloor;
use meshcore_fw_core::radio::{LoraRadio, RxEvent};
use meshcore_fw_core::sensors::Sensors;
use meshcore_fw_core::storage::{Blob, SharedStore, Store};
use static_cell::StaticCell;

use crate::board::{self, Parts};
use crate::console::UsbDriver;
use crate::flash::Flash;
use crate::{DATA_OVERFLOW, DATA_REGION, Spec, ble, console, println, sd};

/// Without a pass of the main loop for this long, the watchdog restarts the board. The longest
/// legitimate stall is one max-length SF12 transmission (~10 s); long anyway, as the watchdog
/// keeps running into the UF2 bootloader (the 1200 baud touch is a soft reset), which does not
/// feed it: copying the next firmware has to fit in.
pub const WATCHDOG_SECS: u32 = 60;

/// A trial firmware that keeps running this long is kept.
const TRIAL_HEALTHY_MS: u64 = 120_000;

/// Upstream's Pro Micro and T114 builds transmit at 22 dBm (`LORA_TX_POWER`).
const DEFAULT_TX_DBM: i8 = 22;

/// The clock follows the GPS this often while it has a fix (upstream `TIME_SYNC_INTERVAL`).
const GPS_TIME_SYNC_MS: u64 = 30 * 60 * 1000;
/// A fix that has held this long is trusted for the time (upstream waits out three checks).
const GPS_FIX_SETTLE_MS: u64 = 3000;

/// Wear-friendly: two sectors alternate, ~24 erases per sector per day.
const CLOCK_SAVE_INTERVAL_MS: u64 = 30 * 60 * 1000;

/// Telemetry answers with sensor readings at most this old.
const SENSOR_INTERVAL_MS: u64 = 15_000;

/// The chip's peripherals the repeater takes, set up by the binary (it has the interrupts).
pub struct Chip {
    pub usb: UsbDriver,
    pub wdt: Peri<'static, peripherals::WDT>,
    pub nvmc: Peri<'static, peripherals::NVMC>,
    pub rng: Peri<'static, peripherals::RNG>,
    pub temp: Temp<'static>,
}

/// What a board adds around the repeater: the T114 its screen and its GPS.
pub trait Extras {
    /// A GPS receiver is wired (`gps` CLI).
    fn has_gps(&self) -> bool {
        false
    }

    /// The GPS on or off (`gps on|off`, and at boot).
    fn set_gps(&mut self, _on: bool) {}

    /// A speaker is wired (`set buzzer` is offered).
    fn has_buzzer(&self) -> bool {
        false
    }

    /// A screen is wired (`set screen` is offered).
    fn has_screen(&self) -> bool {
        false
    }

    /// The firmware started: a boot chirp if the node is not muted (`on`), and a board with a
    /// speaker sounds it.
    fn boot_sound(&mut self, _on: bool) {}

    /// The receiver's latest reading.
    fn gps(&self) -> Option<GpsReading> {
        None
    }

    /// Every pass of the main loop: the screen and the button (`down`), with the BLE update
    /// window's PIN, name and end while it is open. `true`: the button asks to open the window,
    /// or to close it.
    fn poll(&mut self, _mesh: &mut Mesh<Repeater>, _down: bool, _window: Option<(u32, String, u64)>, _now: u64) -> bool {
        false
    }

    /// The BLE update window opened: light the screen.
    fn wake(&mut self, _now: u64) {}
}

/// A board with nothing around the repeater (the Pro Micro).
pub struct NoExtras;

impl Extras for NoExtras {}

type NrfStore = Option<SharedStore<Flash>>;

/// The BLE update window's name, as on the Heltec boards.
fn ota_name(node_name: &str) -> String {
    let mut s = alloc::format!("MeshCore-OTA-{node_name}");
    while s.len() > 32 {
        s.pop();
    }
    s
}

fn now_ms() -> u64 {
    Instant::now().as_millis()
}

fn load(store: &NrfStore, blob: Blob) -> Option<Vec<u8>> {
    store.as_ref()?.borrow_mut().load(blob)
}

fn save(store: &NrfStore, blob: Blob, data: &[u8]) -> bool {
    store.as_ref().is_some_and(|s| s.borrow_mut().save(blob, data))
}

fn load_identity(store: &NrfStore) -> Option<LocalIdentity> {
    let data = load(store, Blob::Identity)?;
    if data.len() != PRV_KEY_SIZE + PUB_KEY_SIZE {
        return None;
    }
    let prv: [u8; PRV_KEY_SIZE] = data[..PRV_KEY_SIZE].try_into().ok()?;
    let id = LocalIdentity::from_private_key(&prv);
    // stored public key must match the private key
    (id.pub_key()[..] == data[PRV_KEY_SIZE..]).then_some(id)
}

fn save_identity(store: &NrfStore, prv: &[u8; PRV_KEY_SIZE]) -> bool {
    let id = LocalIdentity::from_private_key(prv);
    let mut blob = Vec::with_capacity(PRV_KEY_SIZE + PUB_KEY_SIZE);
    blob.extend_from_slice(prv);
    blob.extend_from_slice(id.pub_key());
    save(store, Blob::Identity, &blob)
}

/// Hosted room server: `[private key][prefs text]` in [`Blob::Room`].
fn save_room(store: &NrfStore, prv: &[u8; PRV_KEY_SIZE], prefs: &NodePrefs) -> bool {
    let mut blob = prv.to_vec();
    blob.extend_from_slice(prefs.serialize().as_bytes());
    save(store, Blob::Room, &blob)
}

/// The room server hosted next to the repeater (`set room on`): a second node with its own
/// identity, prefs and ACL on the same radio. Radio settings, clock and regions are the repeater's.
fn start_room(store: &NrfStore, rep: &Mesh<Repeater>, entropy: &[u8], rng_seed: [u8; 16]) -> Mesh<Repeater> {
    let saved = load(store, Blob::Room).and_then(|d| {
        let prv: [u8; PRV_KEY_SIZE] = d.get(..PRV_KEY_SIZE)?.try_into().ok()?;
        LocalIdentity::validate_private_key(&prv).then(|| {
            (LocalIdentity::from_private_key(&prv), NodePrefs::deserialize(&String::from_utf8_lossy(&d[PRV_KEY_SIZE..])))
        })
    });
    let rp = &rep.app.prefs;
    let (id, mut prefs) = saved.unwrap_or_else(|| {
        // a fresh room: its hash must differ from the repeater's (and 0x00/0xFF are reserved)
        let taken = rep.ctx.self_id.pub_key()[0];
        let mut counter = 0u8;
        let id = loop {
            let mut seed = [0u8; 32];
            crypto::sha256(&mut seed, &[entropy, b"room", &[counter]]);
            let id = LocalIdentity::from_seed(&seed);
            counter = counter.wrapping_add(1);
            if ![0x00, 0xFF, taken].contains(&id.pub_key()[0]) {
                break id;
            }
        };
        let prefs = NodePrefs {
            node_name: meshcore::util::truncate_str(&alloc::format!("{} Room", rp.node_name), NAME_MAX),
            password: rp.password.clone(),
            node_lat: rp.node_lat,
            node_lon: rp.node_lon,
            advert_loc_policy: rp.advert_loc_policy,
            advert_interval: rp.advert_interval,
            flood_advert_interval: rp.flood_advert_interval,
            ..NodePrefs::room_server_defaults()
        };
        if !save_room(store, id.private_key(), &prefs) {
            warn!("room: identity save failed");
        }
        (id, prefs)
    });
    prefs.radio = rp.radio;
    prefs.tx_power_dbm = rp.tx_power_dbm;
    let ctx = MeshCtx::new(id.clone(), rng_seed, rep.ctx.radio, rep.ctx.current_time());
    // the battery reading is the repeater's: `adc.multiplier` is set there; the BLE window too
    let board = BoardInfo { adc_multiplier: None, ble_ota_name: None, ..rep.app.board.clone() };
    let mut room = Mesh::new(ctx, Repeater::new_room_server(prefs, board));
    if let Some(acl) = load(store, Blob::RoomAcl) {
        room.app.acl_load(&acl, &id);
    }
    room.app.regions = rep.app.regions.clone();
    println!("Room server ID: {} ({})", to_hex(id.pub_key()), room.app.prefs.node_name);
    room
}

/// The repeater: from the watchdog and the console to the main loop, for good.
pub async fn run(spawner: Spawner, chip: Chip, parts: Parts, spec: &'static Spec, mut extras: impl Extras) -> ! {
    // a watchdog still running from before a soft reset is taken over with the same settings
    let mut wcfg = wdt::Config::default();
    wcfg.timeout_ticks = 32_768 * WATCHDOG_SECS;
    let mut watchdog = match Watchdog::try_new::<_, 1>(chip.wdt, wcfg) {
        Ok((_, [handle])) => Some(handle),
        Err(_) => None, // running with other settings: it restarts the board, and starts afresh
    };

    // ---- the console on USB first: the boot log waits there for a host
    let driver = chip.usb;
    let mut usb_cfg = embassy_usb::Config::new(spec.usb.0, spec.usb.1);
    usb_cfg.manufacturer = Some("MeshCore-rs");
    usb_cfg.product = Some(spec.usb.2);
    static SERIAL: StaticCell<heapless::String<16>> = StaticCell::new();
    usb_cfg.serial_number = Some(SERIAL.init(board::usb_serial()).as_str());
    usb_cfg.max_power = 100;
    static CONFIG_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static CONTROL_BUF: StaticCell<[u8; 64]> = StaticCell::new();
    static STATE: StaticCell<State> = StaticCell::new();
    let mut builder = Builder::new(
        driver,
        usb_cfg,
        CONFIG_DESC.init([0; 256]),
        BOS_DESC.init([0; 256]),
        &mut [],
        CONTROL_BUF.init([0; 64]),
    );
    let class = CdcAcmClass::new(&mut builder, STATE.init(State::new()), 64);
    let (tx, rx, control) = class.split_with_control();
    spawner.spawn(console::device_task(builder.build()).unwrap());
    spawner.spawn(console::rx_task(rx, control).unwrap());
    spawner.spawn(console::tx_task(tx).unwrap());
    console::init_logger();
    println!();
    println!("MeshCore repeater (Rust) {} {} for {}", meshcore::FIRMWARE_VERSION, meshcore::release::build_label(spec.build_date), spec.name);
    if let Some(crash) = console::take_crash() {
        warn!("last reset: {crash}");
    }

    // ---- persistent state
    let flash = Flash::new(Nvmc::new(chip.nvmc));
    let store = Store::with_regions(flash, DATA_REGION, None).with_overflow(DATA_OVERFLOW);
    let store: NrfStore = Some(Rc::new(RefCell::new(store)));
    let updates = spec.updates.expect("repeater requires an update layout");
    if let Some(s) = &store {
        updates.clear_request(s);
    }
    // count trial boots of a fresh update before anything can hang
    let in_trial = store.as_ref().is_some_and(|s| updates.check_trial(s));
    let prefs = load(&store, Blob::Prefs)
        .map(|d| NodePrefs::deserialize(&String::from_utf8_lossy(&d)))
        .unwrap_or_else(|| NodePrefs { tx_power_dbm: DEFAULT_TX_DBM, ..Default::default() });

    // ---- the radio: its supply, the receive side of the antenna switch, the chip
    let Parts { mut radio, mut led, mut rxen, button, mut battery, i2c, hold: _hold } = parts;
    Timer::after_millis(10).await; // the radio powers up
    if let Err(e) = radio.init(prefs.radio, prefs.tx_power_dbm, prefs.rx_boosted_gain).await {
        println!("ERROR: radio init failed: {e:?}");
        // a firmware on trial that cannot use the radio counts as one that did not start
        let give_up = in_trial.then(|| now_ms() + 10_000);
        loop {
            if prefs.led_enabled {
                led.toggle();
            }
            Timer::after_millis(200).await;
            if let Some(w) = &mut watchdog {
                w.pet();
            }
            if give_up.is_some_and(|t| now_ms() >= t) {
                cortex_m::peripheral::SCB::sys_reset();
            }
        }
    }
    info!(
        "radio: {} MHz BW{} SF{} CR{} tx {} dBm",
        Shortest(prefs.radio.freq_mhz), Shortest(prefs.radio.bw_khz), prefs.radio.sf, prefs.radio.cr, prefs.tx_power_dbm
    );

    // ---- entropy: the hardware RNG mixed with the radio's RSSI noise
    let mut entropy = [0u8; 64];
    Rng::new_blocking(chip.rng).blocking_fill_bytes(&mut entropy[..48]);
    let _ = radio.random_bytes(&mut entropy[48..]).await;

    // ---- identity
    let self_id = match load_identity(&store) {
        Some(id) => id,
        None => {
            info!("generating new keypair");
            let mut id;
            let mut counter = 0u8;
            loop {
                let mut seed = [0u8; 32];
                crypto::sha256(&mut seed, &[&entropy, &[counter]]);
                id = LocalIdentity::from_seed(&seed);
                counter += 1;
                // 0x00 and 0xFF hashes are reserved
                if (id.pub_key()[0] != 0x00 && id.pub_key()[0] != 0xFF) || counter > 10 {
                    break;
                }
            }
            save_identity(&store, id.private_key());
            id
        }
    };
    println!("Repeater ID: {}", to_hex(self_id.pub_key()));

    // ---- battery; the button (in `Parts`) wakes the board from `poweroff`
    let batt_mv = battery.read_mv(if prefs.adc_multiplier > 0.0 { prefs.adc_multiplier } else { spec.batt_mv_per_lsb }).await;
    // the chip's own thermometer, as upstream's nRF52 boards report
    let mut mcu_temp = chip.temp;
    let mcu_temp_c = mcu_temp.read().await.to_bits() as f32 / 4.0;

    // ---- sensors on the I2C (upstream scans for them the same way: present or not)
    let mut sensors = Sensors::probe(i2c).await;

    // ---- mesh
    let rng_seed: [u8; 16] = entropy[32..48].try_into().unwrap();
    // wall clock: never behind the build time, the last time saved to flash (survives power
    // loss), or the copy in RAM (survives resets; +2 s covers the reboot itself)
    let ram_epoch = board::clock_load().map(|t| t.saturating_add(2)).unwrap_or(0);
    let saved_epoch = load(&store, Blob::Clock)
        .and_then(|d| d.get(..4).map(|b| u32::from_le_bytes(b.try_into().unwrap())))
        .unwrap_or(0);
    let epoch: u32 = spec.build_epoch.parse::<u32>().unwrap_or(0).max(saved_epoch.saturating_add(1)).max(ram_epoch);
    board::clock_store(epoch);
    let ctx = MeshCtx::new(self_id.clone(), rng_seed, prefs.radio, epoch);
    let board_info = BoardInfo {
        manufacturer: spec.manufacturer.into(),
        batt_mv,
        mcu_temp_c: Some(mcu_temp_c),
        build_date: spec.build_date.into(),
        adc_multiplier: Some(spec.batt_mv_per_lsb),
        bootloader_version: Some(board::bootloader_version()),
        ble_ota_name: sd::present().then(|| ota_name(&prefs.node_name)),
        sensor_scan: sensors.scan.clone(),
        has_gps: extras.has_gps(),
        has_buzzer: extras.has_buzzer(),
        has_screen: extras.has_screen(),
        has_led: true,
        ..Default::default()
    };
    if sd::present() {
        spawner.spawn(ble::task().unwrap());
    } else {
        warn!("no SoftDevice S140 6.1.1 below the app: no BLE window");
    }
    let mut mesh = Mesh::new(ctx, Repeater::new(prefs, board_info));
    if let Some(acl) = load(&store, Blob::Acl) {
        mesh.app.acl_load(&acl, &self_id);
    }
    if let Some(regions) = load(&store, Blob::Regions) {
        mesh.app.regions = meshcore::region::RegionMap::deserialize(&String::from_utf8_lossy(&regions));
    }
    if let Some(sensors) = load(&store, Blob::Sensors) {
        mesh.app.sensors_load(&String::from_utf8_lossy(&sensors));
    }
    mesh.app.ota.build.board = spec.release_board;
    match store.clone().and_then(|s| updates.target::<false>(s)) {
        Some(t) => mesh.app.ota.target = Some(t),
        None => warn!("OTA: the bootloader checks this app's CRC (BLE DFU): updates over USB only"),
    }
    if let Some(s) = &store {
        let b = &mut mesh.app.board;
        (b.fw_rollback, b.fw_prev, b.fw_skip, b.fw_prev_failed) = updates.previous(s);
    }
    mesh.begin(now_ms());
    mesh.app.start(&mut mesh.ctx);
    // a boot chirp only if the node asked for sound: a board may have no speaker soldered
    extras.boot_sound(!mesh.app.prefs.buzzer_quiet);
    let mut room = mesh.app.prefs.room.then(|| {
        let mut seed = rng_seed;
        seed[0] ^= 0x5A; // its own random stream
        let mut room = start_room(&store, &mesh, &entropy, seed);
        room.begin(now_ms());
        room.app.start(&mut room.ctx);
        room
    });
    // the GPS, if the board has one; the button wakes the board from `poweroff`, and on a board
    // with a screen turns its pages
    extras.set_gps(mesh.app.prefs.gps_enabled);
    let (mut gps_fix_since, mut next_gps_time) = (None::<u64>, 0u64);
    let mut ble_pin: Option<u32> = None;
    println!("Ready. Type CLI commands (e.g. 'get name', 'set name X', 'get radio', 'neighbors').");

    // ---- main loop
    let mut noise = NoiseFloor::new();
    let mut next_noise = 0u64;
    let mut next_batt = now_ms() + 60_000;
    let mut next_sensors = 0u64;
    let mut next_agc_reset = 0u64;
    let mut next_clock_save = now_ms() + CLOCK_SAVE_INTERVAL_MS;
    let mut last_clock = mesh.ctx.current_time();
    let mut reboot_at: Option<u64> = None;
    let mut in_trial = in_trial;
    let mut ble_until: Option<u64> = None;
    let confirm_at = now_ms() + TRIAL_HEALTHY_MS;
    mesh.app.board.fw_trial_until = in_trial.then_some(confirm_at);
    loop {
        if let Some(w) = &mut watchdog {
            w.pet();
        }
        let now = now_ms();

        // transmit if something is due and the channel is clear: the repeater first, then the
        // hosted room server (the radio settings are the repeater's)
        let thr = mesh.app.prefs.interference_threshold as i16;
        let cad = mesh.app.prefs.cad_enabled;
        let led_on = mesh.app.prefs.led_enabled;
        let mut transmitted = false;
        for m in core::iter::once(&mut mesh).chain(room.as_mut()) {
            if !m.tx_pending(now) {
                continue;
            }
            let mut busy = radio.is_receiving().unwrap_or(false);
            if !busy && thr != 0 {
                busy = radio.rssi_inst().is_ok_and(|r| r > noise.floor + thr);
            }
            if !busy && cad {
                busy = radio.channel_activity().await.unwrap_or(false);
            }
            if !(busy && m.tx_defer_busy(now))
                && let Some(frame) = m.tx_take(now)
            {
                if frame.cr.is_some() {
                    let _ = radio.set_frame_cr(frame.cr);
                }
                // DIO2 turns the antenna switch to TX; its receive side is ours to turn off
                if let Some(r) = &mut rxen {
                    r.set_low();
                }
                if led_on {
                    led.on();
                }
                let res = radio.transmit(frame.bytes(), frame.timeout_ms).await;
                led.off();
                if let Some(r) = &mut rxen {
                    r.set_high();
                }
                if frame.cr.is_some() {
                    let _ = radio.set_frame_cr(None);
                }
                if let Err(e) = radio.start_rx() {
                    warn!("start_rx: {e:?}");
                }
                match res {
                    Ok(()) => m.tx_done(now_ms()),
                    Err(e) => {
                        warn!("tx failed: {e:?}");
                        m.tx_failed(now_ms());
                    }
                }
                transmitted = true;
            }
            break; // one transmission (or deferral) per pass
        }
        if transmitted {
            continue;
        }

        // sleep until a frame arrives, a CLI line or a BLE frame comes in, or a timer is due
        let wake = room.as_ref().map_or(u64::MAX, |r| r.next_wakeup()).min(mesh.next_wakeup()).min(now + 50).max(now + 1);
        let woke = select4(radio.wait_irq(), console::LINES.receive(), Timer::at(Instant::from_millis(wake)), ble::FRAMES_IN.receive());
        match woke.await {
            Either4::First(()) => match radio.read_rx() {
                Ok(RxEvent::Frame(f)) => {
                    let t = now_ms();
                    mesh.on_rx_frame(t, &f.data[..f.len], f.snr, f.rssi);
                    if let Some(r) = room.as_mut() {
                        r.on_rx_frame(t, &f.data[..f.len], f.snr, f.rssi);
                    }
                }
                Ok(RxEvent::CrcError) => mesh.ctx.stats.n_recv_errors += 1,
                Ok(RxEvent::Nothing) => {}
                Err(e) => warn!("rx: {e:?}"),
            },
            Either4::Second(line) => {
                let cmd = String::from_utf8_lossy(&line);
                let line = cmd.trim_end();
                if line.trim() == "wdt test" {
                    println!("  -> hanging the main loop; the watchdog should reboot in {WATCHDOG_SECS}s");
                    #[allow(clippy::empty_loop)]
                    loop {}
                }
                // `room <command>` (and `room.post`) go to the hosted room server
                let room_cmd = line.strip_prefix("room ").map(str::trim_start).or(line.starts_with("room.post").then_some(line));
                let reply = match (room_cmd, room.as_mut()) {
                    (Some(c), Some(r)) => r.app.handle_command(&mut r.ctx, 0, c),
                    (Some(_), None) => String::from("room server is off (set room on, then reboot)"),
                    (None, _) => mesh.app.handle_command(&mut mesh.ctx, 0, line),
                };
                if !reply.is_empty() {
                    println!("  -> {reply}");
                }
            }
            Either4::Third(()) => {}
            Either4::Fourth(frame) => {
                if let Some(reply) = mesh.app.handle_link_frame(&frame) {
                    let _ = BLE_OUT.try_send(reply);
                }
            }
        }

        let now = now_ms();
        mesh.poll(now);
        if let Some(r) = room.as_mut() {
            r.poll(now);
        }

        // a burst every 2 s; one cut short by a packet goes on in the next one
        if now >= next_noise {
            next_noise = now + NoiseFloor::INTERVAL_MS;
            if noise.measure(|| if radio.is_receiving().unwrap_or(true) { None } else { radio.rssi_inst().ok() }) {
                mesh.ctx.stats.noise_floor = noise.floor;
                if let Some(r) = room.as_mut() {
                    r.ctx.stats.noise_floor = noise.floor;
                }
            }
        }

        if now >= next_batt {
            next_batt = now + 60_000;
            mesh.app.board.batt_mv = battery.read_mv(mesh.app.adc_multiplier().unwrap_or(spec.batt_mv_per_lsb)).await;
            // the TEMP peripheral is the SoftDevice's while it runs
            mesh.app.board.mcu_temp_c = match sd::enabled() {
                true => sd::temp_c(),
                false => Some(mcu_temp.read().await.to_bits() as f32 / 4.0),
            };
            if let Some(r) = room.as_mut() {
                r.app.board.batt_mv = mesh.app.board.batt_mv;
                r.app.board.mcu_temp_c = mesh.app.board.mcu_temp_c;
            }
        }

        // the GPS for the `gps` CLI, and the clock from it once a fix has held for a moment, and
        // again every half hour (upstream `MicroNMEALocationProvider::loop`)
        if extras.has_gps() {
            let reading = extras.gps();
            mesh.app.board.gps = reading;
            if let Some(r) = room.as_mut() {
                r.app.board.gps = reading;
            }
            match reading.filter(|g| g.valid) {
                Some(g) => {
                    let since = *gps_fix_since.get_or_insert(now);
                    if now >= since + GPS_FIX_SETTLE_MS
                        && now >= next_gps_time
                        && let Some(t) = g.time
                    {
                        next_gps_time = now + GPS_TIME_SYNC_MS;
                        if t.abs_diff(mesh.ctx.current_time()) > 2 {
                            info!("GPS: clock set to {t}");
                            mesh.ctx.set_current_time(t);
                        }
                        mesh.app.timesync.note_set(&mesh.ctx, Clock::Trusted, Some(Source::Gps));
                    }
                }
                None => gps_fix_since = None,
            }
        }

        // the screen and the button
        let window = ble_pin.zip(ble_until).map(|(pin, until)| (pin, ota_name(&mesh.app.prefs.node_name), until));
        if extras.poll(&mut mesh, button.is_low(), window, now) {
            if ble_until.is_none() {
                let _ = mesh.app.handle_command(&mut mesh.ctx, 0, "start ota ble");
            } else if !BLE_CONNECTED.load(core::sync::atomic::Ordering::Relaxed) {
                (ble_until, ble_pin) = (None, None);
                ble::stop();
                println!("BLE update window closed with the button");
            }
        }

        // sensors for telemetry: each detected one takes the next channel from 2 up, in probe
        // order, as upstream numbers them
        if now >= next_sensors {
            next_sensors = now + SENSOR_INTERVAL_MS;
            let mut lpp = Vec::new();
            let mut channel = sensors.read_lpp(&mut lpp).await;
            // text-described sensors (`sensor add`): each takes the next channel, in list order
            for d in &mesh.app.sensor_descriptors {
                meshcore_fw_core::sensors::read_descriptor(d, &mut sensors.i2c, channel, &mut lpp).await;
                channel += 1;
            }
            let _ = channel;
            if let Some(r) = room.as_mut() {
                r.app.board.sensor_lpp = lpp.clone();
            }
            mesh.app.board.sensor_lpp = lpp;
        }

        // periodic receiver reset against a stuck AGC (`agc.reset.interval`, off by default)
        let agc_secs = mesh.app.prefs.agc_reset_interval as u64 * 4;
        if agc_secs > 0 && now >= next_agc_reset {
            if next_agc_reset != 0 && !mesh.tx_in_flight() && !radio.is_receiving().unwrap_or(true) && radio.reset_agc().await.is_err() {
                warn!("AGC reset failed");
            }
            next_agc_reset = now + agc_secs * 1000;
        }

        // the hosted room server follows the repeater's clock (`clock sync`, `time`)
        if let Some(r) = room.as_mut() {
            let t = mesh.ctx.current_time();
            if r.ctx.current_time().abs_diff(t) > 2 {
                r.ctx.set_current_time(t);
            }
        }

        // persist the clock periodically, and right after it was set (`clock sync`, `time`);
        // the RAM copy survives resets, the flash copy survives power loss
        let clock = mesh.ctx.current_time();
        let jumped = clock.abs_diff(last_clock) > 120;
        if clock != last_clock {
            board::clock_store(clock);
        }
        last_clock = clock;
        if now >= next_clock_save || jumped {
            next_clock_save = now + CLOCK_SAVE_INTERVAL_MS;
            save(&store, Blob::Clock, &clock.to_le_bytes());
        }

        // hardware side effects requested by the repeater
        let fx = mesh.app.take_effects();
        for line in &fx.console {
            println!("{line}");
        }
        if fx.save_prefs && !save(&store, Blob::Prefs, mesh.app.prefs.serialize().as_bytes()) {
            warn!("prefs save failed");
        }
        if fx.save_acl && !save(&store, Blob::Acl, &mesh.app.acl_serialize()) {
            warn!("ACL save failed");
        }
        if fx.save_regions && !save(&store, Blob::Regions, mesh.app.regions.serialize().as_bytes()) {
            warn!("regions save failed");
        }
        if fx.save_sensors && !save(&store, Blob::Sensors, mesh.app.sensors_serialize().as_bytes()) {
            warn!("sensors save failed");
        }
        if let Some(prv) = fx.save_identity
            && !save_identity(&store, &prv)
        {
            warn!("identity save failed");
        }
        if let Some(params) = fx.apply_radio {
            if radio.apply_params(params).and_then(|_| radio.start_rx()).is_err() {
                warn!("radio reconfigure failed");
            }
            mesh.ctx.radio = params;
        }
        if let Some(dbm) = fx.tx_power {
            let _ = radio.set_tx_power(dbm);
        }
        if let Some(on) = fx.rx_boosted_gain {
            let _ = radio.set_rx_boosted_gain(on);
        }
        if let Some(pin) = fx.start_ble_ota {
            ble_until = Some(now + BLE_OTA_WINDOW_MINS as u64 * 60_000);
            ble_pin = Some(pin);
            extras.wake(now);
            ble::start(ble::Params { name: ota_name(&mesh.app.prefs.node_name), pin, role: ble::Role::Window });
        }
        // the window stays open while an app is connected
        if ble_until.is_some_and(|t| now >= t) {
            if BLE_CONNECTED.load(core::sync::atomic::Ordering::Relaxed) {
                ble_until = Some(now + 60_000);
            } else {
                (ble_until, ble_pin) = (None, None);
                ble::stop();
                println!("BLE update window closed");
            }
        }
        if fx.erase {
            if let Some(s) = &store {
                s.borrow_mut().erase_all();
            }
            reboot_at = Some(now + 500);
        }
        if fx.reboot {
            // leave time for a remote CLI reply to go out
            reboot_at = Some(now + 3000);
        }
        if fx.fw_rollback {
            if store.as_ref().is_some_and(|s| updates.rollback(s)) {
                reboot_at = Some(now + 3000);
            } else {
                warn!("fw rollback: no firmware to go back to");
            }
        }
        // an update coming in takes the staging area over from the firmware to go back to
        mesh.app.board.fw_rollback &= updates.previous_kept();
        if let Some(r) = room.as_mut() {
            let rfx = r.app.take_effects();
            for line in &rfx.console {
                println!("[room] {line}");
            }
            if (rfx.save_prefs || rfx.save_identity.is_some())
                && !save_room(&store, &rfx.save_identity.unwrap_or(*r.ctx.self_id.private_key()), &r.app.prefs)
            {
                warn!("room: prefs save failed");
            }
            if rfx.save_acl && !save(&store, Blob::RoomAcl, &r.app.acl_serialize()) {
                warn!("room: ACL save failed");
            }
            if rfx.reboot {
                reboot_at = Some(now + 3000);
            }
            if rfx.apply_radio.is_some() || rfx.tx_power.is_some() || rfx.save_regions || rfx.erase || rfx.power_off {
                println!("[room] radio, regions, erase and power are managed on the repeater: ignored");
            }
        }
        if fx.gps {
            extras.set_gps(mesh.app.prefs.gps_enabled);
        }
        if fx.power_off {
            println!("Powering off (the button turns it on again)...");
            save(&store, Blob::Clock, &mesh.ctx.current_time().to_le_bytes());
            let _ = radio.sleep();
            Timer::after_millis(200).await; // the line goes out
            sd::disable(); // System OFF goes through the POWER registers
            (spec.system_off)();
        }

        if in_trial && now >= confirm_at {
            if let Some(s) = &store {
                updates.confirm(s);
            }
            in_trial = false;
            mesh.app.board.fw_trial_until = None;
            info!("OTA: new firmware confirmed");
        }

        if reboot_at.is_some_and(|t| now >= t) {
            save(&store, Blob::Clock, &mesh.ctx.current_time().to_le_bytes());
            if updates.install_pending() {
                updates.hand_over();
            }
            cortex_m::peripheral::SCB::sys_reset();
        }
    }
}
