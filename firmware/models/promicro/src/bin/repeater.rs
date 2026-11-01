//! MeshCore repeater firmware for an nRF52840 Pro Micro with an SX1262 module, in Rust.
//!
//! The repeater and its CLI are the same as on the Heltec boards (`meshcore::repeater`); this is
//! the board under them: the radio, the console on the USB port, the node's data in the internal
//! flash. No Wi-Fi, no BLE window and no over-the-air updates on this board yet: a new firmware
//! goes on as UF2 over USB (`tools/uf2/flash_uf2.py`).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::mem::MaybeUninit;
use core::panic::PanicInfo;

use embassy_executor::Spawner;
use embassy_futures::select::{Either3, select3};
use embassy_nrf::gpio::{Input, Level, Output, OutputDrive, Pull};
use embassy_nrf::nvmc::Nvmc;
use embassy_nrf::rng::Rng;
use embassy_nrf::saadc::{self, ChannelConfig, Saadc};
use embassy_nrf::spim::{self, Spim};
use embassy_nrf::temp::{self, Temp};
use embassy_nrf::twim::{self, Twim};
use embassy_nrf::usb::Driver;
use embassy_nrf::usb::vbus_detect::HardwareVbusDetect;
use embassy_nrf::wdt::{self, Watchdog};
use embassy_nrf::{bind_interrupts, config, peripherals, usb};
use embassy_time::{Instant, Timer};
use embassy_usb::Builder;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embedded_alloc::LlffHeap as Heap;
use log::{info, warn};
use meshcore::crypto;
use meshcore::identity::LocalIdentity;
use meshcore::mesh::{Mesh, MeshCtx};
use meshcore::prefs::{NAME_MAX, NodePrefs};
use meshcore::repeater::{BoardInfo, Repeater};
use meshcore::util::to_hex;
use meshcore::{PRV_KEY_SIZE, PUB_KEY_SIZE};
use meshcore_fw_core::noise::NoiseFloor;
use meshcore_fw_core::radio::{LoraRadio, RxEvent, WaitIrq};
use meshcore_fw_core::storage::{Blob, SharedStore, Store};
use meshcore_fw_core::sx1262::Sx1262;
use promicro::board::{self, Battery};
use promicro::ina219::{self, Ina219};
use promicro::{DATA_REGION, console, ota, println};
use static_cell::StaticCell;

const BUILD_EPOCH: &str = env!("BUILD_EPOCH");
const BUILD_DATE: &str = env!("BUILD_DATE");

/// Without a pass of the main loop for this long, the watchdog restarts the board. The longest
/// legitimate stall is one max-length SF12 transmission (~10 s); long anyway, as the watchdog
/// keeps running into the UF2 bootloader (the 1200 baud touch is a soft reset), which does not
/// feed it: copying the next firmware has to fit in.
const WATCHDOG_SECS: u32 = 60;

/// Upstream's Pro Micro build transmits at 22 dBm (`LORA_TX_POWER`).
const DEFAULT_TX_DBM: i8 = 22;

/// Wear-friendly: two sectors alternate, ~24 erases per sector per day.
const CLOCK_SAVE_INTERVAL_MS: u64 = 30 * 60 * 1000;

const HEAP_SIZE: usize = 144 * 1024;

/// Telemetry answers with sensor readings at most this old.
const SENSOR_INTERVAL_MS: u64 = 15_000;

#[global_allocator]
static HEAP: Heap = Heap::empty();

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    CLOCK_POWER => usb::vbus_detect::InterruptHandler;
    SPI2 => spim::InterruptHandler<peripherals::SPI2>;
    SAADC => saadc::InterruptHandler;
    TWISPI0 => twim::InterruptHandler<peripherals::TWISPI0>;
    TEMP => temp::InterruptHandler;
});

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    console::record_crash(&console::panic_text(info));
    cortex_m::peripheral::SCB::sys_reset()
}

#[cortex_m_rt::exception]
unsafe fn HardFault(frame: &cortex_m_rt::ExceptionFrame) -> ! {
    let mut s: heapless::String<120> = heapless::String::new();
    let _ = core::fmt::write(&mut s, format_args!("hard fault at pc {:#010x} lr {:#010x}", frame.pc(), frame.lr()));
    console::record_crash(&s);
    cortex_m::peripheral::SCB::sys_reset()
}

type NrfStore = Option<SharedStore<Nvmc<'static>>>;

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
    // the battery reading is the repeater's: `adc.multiplier` is set there
    let board = BoardInfo { adc_multiplier: None, ..rep.app.board.clone() };
    let mut room = Mesh::new(ctx, Repeater::new_room_server(prefs, board));
    if let Some(acl) = load(store, Blob::RoomAcl) {
        room.app.acl_load(&acl, &id);
    }
    room.app.regions = rep.app.regions.clone();
    println!("Room server ID: {} ({})", to_hex(id.pub_key()), room.app.prefs.node_name);
    room
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    {
        static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];
        // SAFETY: called once, before anything allocates
        unsafe { HEAP.init(&raw mut HEAP_MEM as usize, HEAP_SIZE) }
    }
    // the board has no 32 kHz crystal (upstream `USE_LFRC`); USB needs the HF crystal
    let mut ncfg = config::Config::default();
    ncfg.hfclk_source = config::HfclkSource::ExternalXtal;
    ncfg.lfclk_source = config::LfclkSource::InternalRC;
    ncfg.dcdc.reg1 = true; // the DC/DC converter instead of the LDO (upstream `NRF52BoardDCDC`)
    let p = embassy_nrf::init(ncfg);
    // a watchdog still running from before a soft reset is taken over with the same settings
    let mut wcfg = wdt::Config::default();
    wcfg.timeout_ticks = 32_768 * WATCHDOG_SECS;
    let mut watchdog = match Watchdog::try_new::<_, 1>(p.WDT, wcfg) {
        Ok((_, [handle])) => Some(handle),
        Err(_) => None, // running with other settings: it restarts the board, and starts afresh
    };

    // ---- the console on USB first: the boot log waits there for a host
    let driver = Driver::new(p.USBD, Irqs, HardwareVbusDetect::new(Irqs));
    // the IDs and serial number of upstream's Pro Micro firmware: the host keeps its COM port
    let mut usb_cfg = embassy_usb::Config::new(0x239A, 0x00B3);
    usb_cfg.manufacturer = Some("MeshCore-rs");
    usb_cfg.product = Some("ProMicro NRF52840");
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
    println!("MeshCore repeater (Rust) {} {} for ProMicro", meshcore::FIRMWARE_VERSION, meshcore::release::build_label(BUILD_DATE));
    if let Some(crash) = console::take_crash() {
        warn!("last reset: {crash}");
    }

    // ---- persistent state
    let flash = Nvmc::new(p.NVMC);
    let store: NrfStore = Some(Rc::new(RefCell::new(Store::with_regions(flash, DATA_REGION, None))));
    if let Some(s) = &store {
        ota::clear_request(s);
    }
    let prefs = load(&store, Blob::Prefs)
        .map(|d| NodePrefs::deserialize(&String::from_utf8_lossy(&d)))
        .unwrap_or_else(|| NodePrefs { tx_power_dbm: DEFAULT_TX_DBM, ..Default::default() });

    // ---- radio: its supply, the receive side of the antenna switch, the chip
    let mut led = Output::new(p.P0_15, Level::Low, OutputDrive::Standard);
    let _radio_power = Output::new(p.P0_13, Level::High, OutputDrive::Standard);
    let mut rxen = Output::new(p.P0_17, Level::High, OutputDrive::Standard);
    Timer::after_millis(10).await; // the radio powers up
    let mut scfg = spim::Config::default();
    scfg.frequency = spim::Frequency::M8;
    let spi = Spim::new(p.SPI2, Irqs, p.P1_11, p.P0_02, p.P1_15, scfg);
    let mut radio = Sx1262::new(
        spi,
        Output::new(p.P1_13, Level::High, OutputDrive::Standard),
        Output::new(p.P0_09, Level::High, OutputDrive::Standard),
        Input::new(p.P0_29, Pull::None),
        WaitIrq(Input::new(p.P0_10, Pull::Down)),
    )
    .without_rx_patch();
    if let Err(e) = radio.init(prefs.radio, prefs.tx_power_dbm, prefs.rx_boosted_gain).await {
        println!("ERROR: radio init failed: {e:?}");
        loop {
            led.toggle();
            Timer::after_millis(200).await;
            if let Some(w) = &mut watchdog {
                w.pet();
            }
        }
    }
    info!(
        "radio: {} MHz BW{} SF{} CR{} tx {} dBm",
        prefs.radio.freq_mhz, prefs.radio.bw_khz, prefs.radio.sf, prefs.radio.cr, prefs.tx_power_dbm
    );

    // ---- entropy: the hardware RNG mixed with the radio's RSSI noise
    let mut entropy = [0u8; 64];
    Rng::new_blocking(p.RNG).blocking_fill_bytes(&mut entropy[..48]);
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

    // ---- battery
    let mut battery = Battery { adc: Saadc::new(p.SAADC, Irqs, saadc::Config::default(), [ChannelConfig::single_ended(p.P0_31)]) };
    let batt_mv = battery.read_mv(if prefs.adc_multiplier > 0.0 { prefs.adc_multiplier } else { board::BATT_MV_PER_LSB }).await;
    // the button wakes the board from `poweroff`
    let _button = Input::new(p.P1_00, Pull::Up);
    // the chip's own thermometer, as upstream's nRF52 boards report
    let mut mcu_temp = Temp::new(p.TEMP, Irqs);
    let mcu_temp_c = mcu_temp.read().await.to_bits() as f32 / 4.0;

    // ---- sensors on the I2C (upstream scans for them the same way: present or not)
    let mut icfg = twim::Config::default();
    icfg.sda_pullup = true;
    icfg.scl_pullup = true;
    static TWIM_BUF: StaticCell<[u8; 8]> = StaticCell::new();
    let i2c = Twim::new(p.TWISPI0, Irqs, p.P1_04, p.P0_11, icfg, TWIM_BUF.init([0; 8]));
    let mut ina = Ina219::probe(i2c).await;
    match ina.as_mut() {
        Some(s) => match s.read().await {
            Some(r) => info!("INA219 at 0x{:02X}: {:.3} V, {:.1} mA, {:.0} mW", ina219::ADDRESS, r.bus_v, r.current_ma, r.power_mw),
            None => warn!("INA219 at 0x{:02X}: no reading", ina219::ADDRESS),
        },
        None => info!("no INA219 on the I2C"),
    }

    // ---- mesh
    let rng_seed: [u8; 16] = entropy[32..48].try_into().unwrap();
    // wall clock: never behind the build time, the last time saved to flash (survives power
    // loss), or the copy in RAM (survives resets; +2 s covers the reboot itself)
    let ram_epoch = board::clock_load().map(|t| t.saturating_add(2)).unwrap_or(0);
    let saved_epoch = load(&store, Blob::Clock)
        .and_then(|d| d.get(..4).map(|b| u32::from_le_bytes(b.try_into().unwrap())))
        .unwrap_or(0);
    let epoch: u32 = BUILD_EPOCH.parse::<u32>().unwrap_or(0).max(saved_epoch.saturating_add(1)).max(ram_epoch);
    board::clock_store(epoch);
    let ctx = MeshCtx::new(self_id.clone(), rng_seed, prefs.radio, epoch);
    let board_info = BoardInfo {
        manufacturer: "ProMicro DIY".into(),
        batt_mv,
        mcu_temp_c: Some(mcu_temp_c),
        build_date: BUILD_DATE.into(),
        adc_multiplier: Some(board::BATT_MV_PER_LSB),
        ..Default::default()
    };
    let mut mesh = Mesh::new(ctx, Repeater::new(prefs, board_info));
    if let Some(acl) = load(&store, Blob::Acl) {
        mesh.app.acl_load(&acl, &self_id);
    }
    if let Some(regions) = load(&store, Blob::Regions) {
        mesh.app.regions = meshcore::region::RegionMap::deserialize(&String::from_utf8_lossy(&regions));
    }
    mesh.app.ota.build.board = meshcore::release::board::PROMICRO;
    match store.clone().and_then(ota::StagingTarget::new) {
        Some(t) => mesh.app.ota.target = Some(Box::new(t)),
        None => warn!("OTA: the bootloader checks this app's CRC (BLE DFU): updates over USB only"),
    }
    mesh.begin(now_ms());
    mesh.app.start(&mut mesh.ctx);
    let mut room = mesh.app.prefs.room.then(|| {
        let mut seed = rng_seed;
        seed[0] ^= 0x5A; // its own random stream
        let mut room = start_room(&store, &mesh, &entropy, seed);
        room.begin(now_ms());
        room.app.start(&mut room.ctx);
        room
    });
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
    loop {
        if let Some(w) = &mut watchdog {
            w.pet();
        }
        let now = now_ms();

        // transmit if something is due and the channel is clear: the repeater first, then the
        // hosted room server (the radio settings are the repeater's)
        let thr = mesh.app.prefs.interference_threshold as i16;
        let cad = mesh.app.prefs.cad_enabled;
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
                rxen.set_low();
                led.set_high();
                let res = radio.transmit(frame.bytes(), frame.timeout_ms).await;
                led.set_low();
                rxen.set_high();
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

        // sleep until a frame arrives, a CLI line comes in, or a timer is due
        let wake = room.as_ref().map_or(u64::MAX, |r| r.next_wakeup()).min(mesh.next_wakeup()).min(now + 50).max(now + 1);
        match select3(radio.wait_irq(), console::LINES.receive(), Timer::at(Instant::from_millis(wake))).await {
            Either3::First(()) => match radio.read_rx() {
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
            Either3::Second(line) => {
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
            Either3::Third(()) => {}
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
            mesh.app.board.batt_mv = battery.read_mv(mesh.app.adc_multiplier().unwrap_or(board::BATT_MV_PER_LSB)).await;
            mesh.app.board.mcu_temp_c = Some(mcu_temp.read().await.to_bits() as f32 / 4.0);
            if let Some(r) = room.as_mut() {
                r.app.board.batt_mv = mesh.app.board.batt_mv;
                r.app.board.mcu_temp_c = mesh.app.board.mcu_temp_c;
            }
        }

        // sensors for telemetry: the first one found on channel 2, as upstream numbers them
        if now >= next_sensors {
            next_sensors = now + SENSOR_INTERVAL_MS;
            if let Some(s) = ina.as_mut() {
                let mut lpp = Vec::new();
                if let Some(r) = s.read().await {
                    r.lpp(meshcore::lpp::CHANNEL_SELF + 1, &mut lpp);
                }
                if let Some(r) = room.as_mut() {
                    r.app.board.sensor_lpp = lpp.clone();
                }
                mesh.app.board.sensor_lpp = lpp;
            }
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
        if fx.start_ota.is_some() || fx.start_ble_ota.is_some() {
            println!("no Wi-Fi or BLE on this board: updates go over LoRa or USB (UF2)");
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
        if fx.power_off {
            println!("Powering off (the button turns it on again)...");
            save(&store, Blob::Clock, &mesh.ctx.current_time().to_le_bytes());
            let _ = radio.sleep();
            Timer::after_millis(200).await; // the line goes out
            board::system_off();
        }

        if reboot_at.is_some_and(|t| now >= t) {
            save(&store, Blob::Clock, &mesh.ctx.current_time().to_le_bytes());
            if ota::INSTALL_PENDING.load(core::sync::atomic::Ordering::Relaxed) {
                ota::hand_over();
            }
            cortex_m::peripheral::SCB::sys_reset();
        }
    }
}
