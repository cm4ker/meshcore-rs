//! MeshCore repeater firmware for the Heltec WiFi LoRa 32 V4, in Rust.

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

extern crate alloc;

use alloc::string::String;
use alloc::boxed::Box;
use alloc::vec::Vec;

use embassy_executor::Spawner;
use embassy_futures::select::{Either4, select4};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Instant, Timer};
use esp_backtrace as _;
#[cfg(feature = "heltec-v3")]
use esp_hal::Blocking;
use esp_hal::analog::adc::{Adc, AdcCalCurve, AdcConfig, Attenuation};
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Event, Input, InputConfig, Level, Output, OutputConfig, Pin, Pull};
use esp_hal::peripherals::ADC1;
use esp_hal::rng::{Rng, Trng, TrngSource};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_println::{print, println};
use esp_storage::FlashStorage;
use log::{info, warn};

use alloc::rc::Rc;
use core::cell::RefCell;

use heltec_v4::ble::{self, BLE_CONNECTED, BLE_OUT, BONDS_CHANGED};
use heltec_v4::board::Fem;
use heltec_v4::common::{Battery, NoiseFloor, clock_cell};
use heltec_v4::broker;
use heltec_v4::display::Oled;
use heltec_v4::ota::{self, FlashTarget, SharedStore};
use heltec_v4::power::Sleeper;
use heltec_v4::reset;
use heltec_v4::frames::FRAMES_IN;
use meshcore_fw::fwstore;
use heltec_v4::storage::{self, Blob, CompBlob};
use heltec_v4::espnow;
use heltec_v4::net::{self, BrokerState};
use heltec_v4::wifi_ota;
use meshcore::uplink;
use heltec_v4::radio::{EspIrq, LoraRadio, RxEvent};
use heltec_v4::sx1262::Sx1262;
use meshcore::crypto;
use meshcore::identity::LocalIdentity;
use meshcore::mesh::{Mesh, MeshCtx};
use meshcore::prefs::{NAME_MAX, NodePrefs};
use meshcore::repeater::{BLE_OTA_WINDOW_MINS, BoardInfo, Repeater};
use meshcore::util::to_hex;
use meshcore::{PRV_KEY_SIZE, PUB_KEY_SIZE};
use meshcore_ui::refresh::oled::Oled as ScreenPolicy;
use meshcore_ui::{
    Action, Alert, Bar, BleState, Body, Button, Health, NetInfo, OtaWindow, Page, Passport, PowerInfo, Traffic, Ui, UpdateInfo,
    View, Wifi,
};

esp_bootloader_esp_idf::esp_app_desc!();

const BUILD_EPOCH: &str = env!("BUILD_EPOCH");
const BUILD_DATE: &str = env!("BUILD_DATE");

/// Longest legitimate stall is one max-length SF12 transmission (~10 s).
const WATCHDOG_SECS: u64 = 30;

/// Debug flight recorder: the last main-loop sections, kept across resets in RTC RAM.
mod trace {
    #[esp_hal::ram(unstable(rtc_fast, persistent))]
    static mut RING: [u8; 64] = [0; 64];
    #[esp_hal::ram(unstable(rtc_fast, persistent))]
    static mut POS: u32 = 0;

    pub fn push(id: u8) {
        // SAFETY: main task only
        unsafe {
            let p = (core::ptr::read_volatile(&raw const POS) as usize) % 64;
            core::ptr::write_volatile((&raw mut RING as *mut u8).add(p), id);
            core::ptr::write_volatile(&raw mut POS, (p as u32 + 1) % 64);
        }
    }

    pub fn dump() -> alloc::string::String {
        let mut s = alloc::string::String::new();
        unsafe {
            let p = core::ptr::read_volatile(&raw const POS) as usize % 64;
            for i in 0..64 {
                let id = core::ptr::read_volatile((&raw const RING as *const u8).add((p + i) % 64));
                if id != 0 {
                    s.push(id as char);
                }
            }
        }
        s
    }
}

/// Log main-loop sections that block for long (and feed the flight recorder).
struct Prof {
    last: Instant,
    name: &'static str,
}

impl Prof {
    fn new() -> Self {
        Self { last: Instant::now(), name: "start" }
    }

    fn mark(&mut self, name: &'static str) {
        let ids = ["top", "tx", "wait", "poll", "noise", "batt", "agc", "screen", "net", "clock", "effects", "end"];
        trace::push(ids.iter().position(|n| *n == name).map_or(b'?', |i| b"0123456789EZ"[i]));
        let el = self.last.elapsed().as_millis();
        // a transmission blocks for its airtime (up to seconds at high SF), waiting is the point
        let limit = match self.name {
            "tx" => 10_000,
            "wait" => 2_000,
            _ => 300,
        };
        if el > limit {
            warn!("slow: {} -> {} took {} ms", self.name, name, el);
        }
        self.last = Instant::now();
        self.name = name;
    }
}

/// `wdt.feed()` at the call sites, backed by the RTC watchdog.
struct WatchdogFeeder;

impl WatchdogFeeder {
    fn feed(&self) {
        reset::feed();
    }
}

/// The screen stays lit this long after boot or a button press.
const SCREEN_ON_MS: u64 = 60_000;
/// The screen's pages; the update page only with two firmware slots.
const PAGES: &[Page] = &[
    Page::Home,
    Page::Contact,
    Page::Neighbours,
    Page::Traffic,
    Page::Radio,
    Page::Power,
    Page::Network,
    Page::Update,
    Page::Advert,
];
const PAGES_SINGLE_SLOT: &[Page] =
    &[Page::Home, Page::Contact, Page::Neighbours, Page::Traffic, Page::Radio, Page::Power, Page::Network, Page::Advert];

/// `powersaving on`: the first sleep comes this long after boot, a button press or a console line
/// (upstream: two minutes after boot), and one sleep lasts at most this long (below the watchdog).
const AWAKE_AFTER_ACTIVITY_MS: u64 = 120_000;
const MAX_SLEEP_MS: u64 = 20_000;
/// An update counts as abandoned (and no longer keeps the board awake) after this long without
/// a request.
const OTA_IDLE_MS: u64 = 300_000;

/// A trial firmware that keeps running this long is kept.
const TRIAL_HEALTHY_MS: u64 = 120_000;

/// Wear-friendly: two sectors alternate, ~24 erases per sector per day.
const CLOCK_SAVE_INTERVAL_MS: u64 = 30 * 60 * 1000;

/// CLI lines from the USB console to the main loop.
static CLI_LINES: Channel<CriticalSectionRawMutex, Vec<u8>, 4> = Channel::new();

/// `client_version` in observer status messages.
const CLIENT_VERSION: &str = concat!("rs-", env!("BUILD_DATE"));

fn prefs_radio(mesh: &Mesh<Repeater>) -> meshcore::radio::RadioParams {
    mesh.app.prefs.radio
}

/// `get mqtt.status` line (MeshCoreTel format).
fn mqtt_status_line(p: &NodePrefs, station: bool) -> alloc::string::String {
    let mut s = alloc::format!(
        "wifi:{} ntp:{} iata:{}",
        if net::WIFI_UP.load(core::sync::atomic::Ordering::Relaxed) { "up" } else { "down" },
        if net::TIME_SYNCED.load(core::sync::atomic::Ordering::Relaxed) { "up" } else { "wait" },
        p.mqtt_iata
    );
    for (i, (name, bit)) in meshcore::repeater::MQTT_BROKERS.iter().enumerate() {
        let state = if p.mqtt_brokers & bit == 0 {
            "off"
        } else if !station || p.mqtt_iata == "UNSET" {
            "invalid iata"
        } else {
            BrokerState::load(i).name()
        };
        s.push_str(&alloc::format!(" {name}:{state}"));
    }
    s.push_str(&alloc::format!(" status:{} tx:{}", if p.mqtt_status { "on" } else { "off" }, if p.mqtt_tx { "on" } else { "off" }));
    s
}

/// SSID of the update access point (at most 32 bytes).
fn ota_ssid(node_name: &str) -> String {
    let mut s = alloc::format!("MeshCore-OTA-{node_name}");
    while s.len() > 32 {
        s.pop();
    }
    s
}

/// What the screen shows that lives outside the repeater.
struct ScreenInfo {
    in_trial: bool,
    /// The update window over BLE: its PIN, and when it closes.
    ble_window: Option<(u32, u64)>,
    /// The Wi-Fi station runs.
    wifi: bool,
    asleep_pct: u8,
}

/// Time on air as tenths of a percent of the uptime.
fn permille(air_ms: u64, uptime_ms: u64) -> u16 {
    (air_ms * 1000 / uptime_ms.max(1)).min(1000) as u16
}

/// The interface's current page with what it shows.
fn repeater_view(ui: &mut Ui, mesh: &Mesh<Repeater>, info: &ScreenInfo, now: u64) -> View {
    let (app, ctx) = (&mesh.app, &mesh.ctx);
    let p = &app.prefs;
    let key = ctx.self_id.pub_key();
    // as paths and traces show nodes
    let id_len = p.path_hash_mode as usize + 1;
    let id = to_hex(&key[..id_len]);
    let version = alloc::format!("{} {}", meshcore::FIRMWARE_VERSION, meshcore::release::build_label(BUILD_DATE));
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
    let ble_connected = BLE_CONNECTED.load(core::sync::atomic::Ordering::Relaxed);
    let body = match ui.page() {
        Page::Neighbours => {
            let clock = ctx.current_time();
            let mut list: Vec<_> = app.neighbours().iter().collect();
            list.sort_by_key(|n| core::cmp::Reverse(n.heard_timestamp));
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
        Page::Power => Body::Power(PowerInfo {
            batt_mv: app.board.batt_mv,
            powersaving: Some(p.powersaving_enabled),
            asleep_pct: info.asleep_pct,
            uptime_min,
        }),
        Page::Network => {
            let up = |a: &core::sync::atomic::AtomicBool| a.load(core::sync::atomic::Ordering::Relaxed);
            let wifi = match (info.wifi, up(&net::WIFI_UP)) {
                (false, _) => Wifi::Off,
                (true, false) => Wifi::Connecting { ssid: p.wifi_ssid.clone() },
                (true, true) => Wifi::Up { ssid: p.wifi_ssid.clone(), ip: app.board.wifi_ip.clone().unwrap_or_default() },
            };
            let on: Vec<usize> = (0..meshcore::repeater::MQTT_BROKERS.len())
                .filter(|&i| info.wifi && p.mqtt_brokers & meshcore::repeater::MQTT_BROKERS[i].1 != 0)
                .collect();
            Body::Network(NetInfo {
                wifi,
                mqtt_on: on.len() as u8,
                mqtt_up: on.iter().filter(|&&i| BrokerState::load(i) == BrokerState::Up).count() as u8,
                time_synced: up(&net::TIME_SYNCED),
            })
        }
        Page::Update => Body::Update(UpdateInfo {
            window: info.ble_window.map(|(pin, until)| OtaWindow {
                name: ota_ssid(&p.node_name),
                pin,
                connected: ble_connected,
                mins_left: until.saturating_sub(now).div_ceil(60_000) as u32,
            }),
            trial: info.in_trial,
            version: version.clone(),
        }),
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
    let ble = info.ble_window.map(|_| if ble_connected { BleState::Connected } else { BleState::Waiting });
    let batt_mv = ui.battery_mv(app.board.batt_mv);
    View { page: ui.page(), page_idx, pages, bar: Bar { ble, batt_mv: (batt_mv > 0).then_some(batt_mv) }, body, alert, pin: None }
}

fn now_ms() -> u64 {
    Instant::now().as_millis()
}

/// The V4's console is the chip's USB Serial/JTAG port, read in its interrupt (`meshcore_fw::usb`).
#[cfg(not(feature = "heltec-v3"))]
type ConsoleRx = meshcore_fw::usb::Input;
/// The V3's USB port is a CP2102 bridge to UART0 (TX 43, RX 44).
#[cfg(feature = "heltec-v3")]
type ConsoleRx = esp_hal::uart::UartRx<'static, Blocking>;

/// Wait for console input.
async fn console_read(rx: &mut ConsoleRx, buf: &mut [u8]) -> usize {
    #[cfg(not(feature = "heltec-v3"))]
    return rx.read(buf).await;
    // the UART's 128-byte FIFO holds a pasted line between polls
    #[cfg(feature = "heltec-v3")]
    loop {
        let n = rx.read_buffered(buf).unwrap_or(0);
        if n > 0 {
            return n;
        }
        Timer::after_millis(20).await;
    }
}

/// Read the USB console, echo, and hand complete lines to the main loop.
#[embassy_executor::task]
async fn console_task(mut rx: ConsoleRx) {
    let mut line: Vec<u8> = Vec::with_capacity(160);
    let mut buf = [0u8; 64];
    // How much of `line` is on the screen. Typed keys show at once; a pasted piece shows once its
    // line has come: printed while the rest of it was arriving, it kept the S3's port input off
    // (esp-println waits for the host with interrupts masked) long enough to lose a packet.
    let mut shown = 0;
    let show = |part: &[u8]| part.iter().for_each(|&c| print!("{}", c as char));
    loop {
        let n = console_read(&mut rx, &mut buf).await;
        let typed = n <= 2;
        for &c in &buf[..n] {
            match c {
                b'\r' | b'\n' => {
                    if !line.is_empty() {
                        show(&line[shown..]);
                        println!();
                        shown = 0;
                        CLI_LINES.send(core::mem::take(&mut line)).await;
                    }
                }
                0x08 | 0x7F => {
                    if line.pop().is_some() && shown > line.len() {
                        shown = line.len();
                        print!("\x08 \x08");
                    }
                }
                _ if line.len() < 159 => {
                    line.push(c);
                    if typed {
                        show(&line[shown..]);
                        shown = line.len();
                    }
                }
                _ => {}
            }
        }
    }
}

fn load(store: &Option<SharedStore<'_>>, blob: Blob) -> Option<Vec<u8>> {
    store.as_ref()?.borrow_mut().load(blob)
}

fn save(store: &Option<SharedStore<'_>>, blob: Blob, data: &[u8]) -> bool {
    store.as_ref().is_some_and(|s| s.borrow_mut().save(blob, data))
}

fn load_identity(store: &Option<SharedStore<'_>>) -> Option<LocalIdentity> {
    let data = load(store, Blob::Identity)?;
    if data.len() != PRV_KEY_SIZE + PUB_KEY_SIZE {
        return None;
    }
    let prv: [u8; PRV_KEY_SIZE] = data[..PRV_KEY_SIZE].try_into().ok()?;
    let id = LocalIdentity::from_private_key(&prv);
    // stored public key must match the private key
    (id.pub_key()[..] == data[PRV_KEY_SIZE..]).then_some(id)
}

fn save_identity(store: &Option<SharedStore<'_>>, prv: &[u8; PRV_KEY_SIZE]) -> bool {
    let id = LocalIdentity::from_private_key(prv);
    let mut blob = Vec::with_capacity(PRV_KEY_SIZE + PUB_KEY_SIZE);
    blob.extend_from_slice(prv);
    blob.extend_from_slice(id.pub_key());
    save(store, Blob::Identity, &blob)
}

/// Hosted room server: `[private key][prefs text]` in [`Blob::Room`].
fn save_room(store: &Option<SharedStore<'_>>, prv: &[u8; PRV_KEY_SIZE], prefs: &NodePrefs) -> bool {
    let mut blob = prv.to_vec();
    blob.extend_from_slice(prefs.serialize().as_bytes());
    save(store, Blob::Room, &blob)
}

/// The room server hosted next to the repeater (`set room on`): a second node with its own
/// identity, prefs and ACL on the same radio. Radio settings, clock and regions are the repeater's.
fn start_room(store: &Option<SharedStore<'_>>, rep: &Mesh<Repeater>, entropy: &[u8], rng_seed: [u8; 16]) -> Mesh<Repeater> {
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
    let board = BoardInfo { wifi_ota_ssid: None, ble_ota_name: None, ..rep.app.board.clone() };
    let ctx = MeshCtx::new(id.clone(), rng_seed, rep.ctx.radio, rep.ctx.current_time());
    let mut room = Mesh::new(ctx, Repeater::new_room_server(prefs, board));
    if let Some(acl) = load(store, Blob::RoomAcl) {
        room.app.acl_load(&acl, &id);
    }
    room.app.regions = rep.app.regions.clone();
    println!("Room server ID: {} ({})", to_hex(id.pub_key()), room.app.prefs.node_name);
    room
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();

    // upstream runs Heltec V4 repeaters at 80 MHz to save power
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let mut p = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);
    // Wi-Fi, MQTT over TLS and BLE need a large heap (about 140 KB of it stays free with TLS up),
    // but the main task's stack is what DRAM leaves after the heap, and a TLS handshake needs
    // about 30 KB of stack: 150 KB of heap left 28 KB and overflowed it on connecting
    esp_alloc::heap_allocator!(size: 126 * 1024);

    let timg0 = TimerGroup::new(p.TIMG0);
    esp_rtos::start(timg0.timer0, p.FROM_CPU_INTR0);

    // an unattended repeater must recover from hangs: full reset if the main loop stops feeding
    reset::install(esp_hal::rtc_cntl::Rtc::new(p.RTC_TIMER), WATCHDOG_SECS);
    let wdt = WatchdogFeeder;

    println!();
    println!("MeshCore repeater (Rust) {} {}", meshcore::FIRMWARE_VERSION, meshcore::release::build_label(BUILD_DATE));

    // ---- persistent state
    let store: Option<SharedStore> = storage::open(FlashStorage::new(p.FLASH)).map(|s| Rc::new(RefCell::new(s)));
    if store.is_none() {
        warn!("no '{}' partition: settings will not persist", heltec_v4::storage::PARTITION_LABEL);
    }
    // firmware slots: count trial boots of a fresh update before anything can hang
    let slots = store.as_ref().and_then(|s| ota::slots(&mut s.borrow_mut()));
    let in_trial = match (&store, slots) {
        (Some(s), Some(sl)) => ota::check_trial(&mut s.borrow_mut(), sl.booted),
        _ => false,
    };
    match slots {
        Some(sl) => info!("firmware slot {}{}", sl.booted, if in_trial { " (trial)" } else { "" }),
        None => warn!("single-slot partition table: OTA updates unavailable"),
    }

    let prefs = load(&store, Blob::Prefs)
        .map(|d| NodePrefs::deserialize(&String::from_utf8_lossy(&d)))
        .unwrap_or_else(|| NodePrefs { tx_power_dbm: heltec_v4::board::DEFAULT_TX_DBM, ..Default::default() });

    // ---- board
    let mut led = Output::new(p.GPIO35, Level::Low, OutputConfig::default());
    let mut fem = Fem::init(
        p.GPIO7.degrade(),
        p.GPIO2.degrade(),
        p.GPIO46.degrade(),
        p.GPIO5.degrade(),
        prefs.radio_fem_rxgain,
    )
    .await;
    info!("FEM: {:?}", fem.kind);

    // ---- radio
    let spi = Spi::new(p.SPI2, SpiConfig::default().with_frequency(Rate::from_mhz(8)))
        .unwrap()
        .with_sck(p.GPIO9)
        .with_mosi(p.GPIO10)
        .with_miso(p.GPIO11);
    let nss = Output::new(p.GPIO8, Level::High, OutputConfig::default());
    let reset = Output::new(p.GPIO12, Level::High, OutputConfig::default());
    let busy = Input::new(p.GPIO13, InputConfig::default().with_pull(Pull::None));
    let dio1 = Input::new(p.GPIO14, InputConfig::default().with_pull(Pull::Down));
    let mut radio = Sx1262::new(spi, nss, reset, busy, EspIrq(dio1));
    if let Err(e) = radio.init(prefs.radio, prefs.tx_power_dbm, prefs.rx_boosted_gain).await {
        println!("ERROR: radio init failed: {e:?}");
        loop {
            led.toggle();
            Timer::after_millis(200).await;
        }
    }
    info!(
        "radio: {} MHz BW{} SF{} CR{} tx {} dBm",
        prefs.radio.freq_mhz, prefs.radio.bw_khz, prefs.radio.sf, prefs.radio.cr, prefs.tx_power_dbm
    );

    // ---- entropy: hardware TRNG (RF/ADC noise) mixed with the radio's RSSI noise
    let mut entropy = [0u8; 64];
    {
        let _source = TrngSource::new(p.RNG.reborrow(), p.ADC1.reborrow());
        if let Ok(trng) = Trng::try_new() {
            trng.read(&mut entropy[..32]);
        }
    }
    let _ = radio.random_bytes(&mut entropy[32..]).await;

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
    let mut adc_cfg = AdcConfig::new();
    let bat_pin = adc_cfg.enable_pin_with_cal::<_, AdcCalCurve<ADC1>>(p.GPIO1, Attenuation::_11dB);
    let mut battery = Battery::new(Adc::new(p.ADC1, adc_cfg), bat_pin, p.GPIO37);
    info!("boot: battery");
    let batt_mv = battery.read_mv().await;
    wdt.feed();

    // ---- mesh
    let mut rng_seed = [0u8; 16];
    Rng::new().read(&mut rng_seed);
    for (s, e) in rng_seed.iter_mut().zip(entropy[32..].iter()) {
        *s ^= *e;
    }
    // wall clock: never behind the build time, the last time saved to flash (survives power
    // loss), or the copy in RTC RAM (survives resets; +2 s covers the reboot itself)
    let ram_epoch = clock_cell::load().map(|t| t.saturating_add(2)).unwrap_or(0);
    let saved_epoch = load(&store, Blob::Clock)
        .and_then(|d| d.get(..4).map(|b| u32::from_le_bytes(b.try_into().unwrap())))
        .unwrap_or(0);
    let epoch: u32 = BUILD_EPOCH.parse::<u32>().unwrap_or(0).max(saved_epoch.saturating_add(1)).max(ram_epoch);
    clock_cell::store(epoch);
    let ctx = MeshCtx::new(self_id.clone(), rng_seed, prefs.radio, epoch);
    let board = BoardInfo {
        manufacturer: fem.name().into(),
        batt_mv,
        mcu_temp_c: None,
        fem_lna_controllable: fem.lna_controllable(),
        powersaving_supported: true,
        bridge: Some("espnow"),
        build_date: BUILD_DATE.into(),
        wifi_ota_ssid: slots.map(|_| ota_ssid(&prefs.node_name)),
        ble_ota_name: slots.map(|_| ota_ssid(&prefs.node_name)),
        ..Default::default()
    };
    let mut mesh = Mesh::new(ctx, Repeater::new(prefs, board));
    if let Some(acl) = load(&store, Blob::Acl) {
        mesh.app.acl_load(&acl, &self_id);
    }
    if let Some(regions) = load(&store, Blob::Regions) {
        mesh.app.regions = meshcore::region::RegionMap::deserialize(&String::from_utf8_lossy(&regions));
    }
    if let (Some(s), Some(sl)) = (&store, slots) {
        mesh.app.ota.target = Some(Box::new(FlashTarget::new(s.clone(), sl)));
    }
    mesh.app.ota.build.board = heltec_v4::RELEASE_BOARD;
    // updates handed on to neighbours (`fw.serve`): kept past the running image in its slot
    let fw_region = match (&store, slots, mesh.app.ota.running()) {
        (Some(s), Some(sl), Some((image_len, _))) => fwstore::region(&sl, image_len).inspect(|&region| {
            mesh.app.fw_store = Some(Box::new(fwstore::FlashFwStore::new(s.clone(), region)));
        }),
        _ => None,
    };
    info!("boot: mesh");
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

    let mut wifi = Some(p.WIFI);
    // BLE only runs during `start ota ble`
    let bt = p.BT;

    // ---- Wi-Fi station + MQTT observer (when configured)
    let device_id = to_hex(self_id.pub_key());
    let model = alloc::string::String::from(fem.name());
    let mut station: Option<embassy_net::Stack<'static>> = None;
    // the bridge (ESP-NOW) shares the Wi-Fi radio: on the router's channel next to the station,
    // else on `bridge.channel`
    let bridge_on = mesh.app.prefs.bridge_enabled;
    let mut bridge_radio = false;
    if !mesh.app.prefs.wifi_ssid.is_empty() {
        let seed = (Rng::new().random() as u64) << 32 | Rng::new().random() as u64;
        let params = net::StationParams {
            ssid: mesh.app.prefs.wifi_ssid.clone(),
            password: mesh.app.prefs.wifi_pwd.clone(),
            // modem sleep would miss bridge frames between beacons
            power_save: !bridge_on,
            esp_now: bridge_on,
        };
        info!("boot: Wi-Fi station");
        wdt.feed();
        match net::start(&spawner, wifi.take().unwrap(), seed, params) {
            Ok((stack, esp_now)) => {
                station = Some(stack);
                if let Some(e) = esp_now {
                    match espnow::spawn(&spawner, e) {
                        Ok(()) => info!("bridge: ESP-NOW on the router's channel"),
                        Err(e) => warn!("bridge: {e}"),
                    }
                }
            }
            Err(e) => warn!("Wi-Fi station: {e}"),
        }
    }
    if bridge_on
        && station.is_none()
        && let Some(w) = wifi.take()
    {
        let channel = mesh.app.prefs.bridge_channel;
        match net::start_esp_now(&spawner, w, channel).and_then(|e| espnow::spawn(&spawner, e)) {
            Ok(()) => info!("bridge: ESP-NOW on channel {channel}"),
            Err(e) => warn!("bridge: {e}"),
        }
        bridge_radio = true;
    }
    let iata = mesh.app.prefs.mqtt_iata.clone();
    if let Some(stack) = station
        && iata != "UNSET"
    {
        let radio_str = uplink::radio_string(prefs_radio(&mesh).freq_mhz, prefs_radio(&mesh).bw_khz, prefs_radio(&mesh).sf, prefs_radio(&mesh).cr);
        let obs = uplink::Observer {
            name: &mesh.app.prefs.node_name,
            device_id: &device_id,
            model: &model,
            firmware_version: meshcore::FIRMWARE_VERSION,
            client_version: CLIENT_VERSION,
            radio: &radio_str,
        };
        let will = uplink::status_json(&obs, mesh.ctx.current_time(), false, None);
        let prefix = alloc::format!("meshcore/{iata}/{device_id}/");
        let mut brokers = alloc::vec::Vec::new();
        if mesh.app.prefs.mqtt_brokers & 0x01 != 0 {
            brokers.push(broker::BrokerConfig {
                index: 0,
                host: "meshcoretel.ru",
                port: 1883,
                transport: broker::Transport::Tcp,
                user: "meshcore".into(),
                password: broker::Credential::Fixed("meshcore"),
                client_id: alloc::format!("mqtt_meshcoretel-{}", &device_id[..6]),
                topic_prefix: prefix.clone(),
                will: will.clone(),
            });
        }
        // the WSS brokers share the JWT scheme: user `v1_<KEY>`, token minted per connection
        let wss: [(usize, &'static str, &'static [&'static [u8]]); 3] = [
            (1, "mqtt-eu-v1.letsmesh.net", broker::GTS_ROOTS),
            (2, "mqtt-us-v1.letsmesh.net", broker::GTS_ROOTS),
            (3, "agent.ommesh.ru", broker::ISRG_ROOTS),
        ];
        let non_empty = |s: &str| (!s.is_empty()).then(|| alloc::string::String::from(s));
        for (index, host, roots) in wss {
            let (name, bit) = meshcore::repeater::MQTT_BROKERS[index];
            if mesh.app.prefs.mqtt_brokers & bit == 0 {
                continue;
            }
            brokers.push(broker::BrokerConfig {
                index,
                host,
                port: 443,
                transport: broker::Transport::Wss("/mqtt", roots),
                user: alloc::format!("v1_{device_id}"),
                password: broker::Credential::Jwt {
                    id: self_id.clone(),
                    owner: non_empty(&mesh.app.prefs.mqtt_owner),
                    email: non_empty(&mesh.app.prefs.mqtt_email),
                },
                client_id: alloc::format!("mqtt_{name}-{}", &device_id[..6]),
                topic_prefix: prefix.clone(),
                will: will.clone(),
            });
        }
        for cfg in brokers {
            if let Err(e) = broker::spawn(&spawner, stack, cfg) {
                warn!("MQTT: {e}");
            }
        }
        mesh.app.uplink_enabled = true;
    }
    if let (Some(stack), Some(s), Some(region)) = (station, &store, fw_region) {
        spawner.spawn(fwstore::fetch_task(stack, s.clone(), region).unwrap());
    }
    let mut fw_status_seen = 0u32;
    let uplink_pub = net::UPLINK.publisher().unwrap();
    let mut next_status = 0u64;

    info!("boot: OLED");
    wdt.feed();
    // ---- OLED status screen (Vext powers it; absent on boards without a display)
    // Vext is on with GPIO36 LOW. Upstream declares PIN_VEXT_EN_ACTIVE=HIGH for the V4 but never
    // claims the pin, so its begin() leaves it LOW; driving it HIGH cut the OLED's supply and the
    // controller limped on through the I2C pull-ups (dim panel, failing transfers).
    let _vext = Output::new(p.GPIO36, Level::Low, OutputConfig::default());
    let oled_rst = Output::new(p.GPIO21, Level::High, OutputConfig::default());
    Timer::after_millis(10).await;
    // Bounded I2C: by default esp-hal waits forever on a stuck bus, which once hung the whole
    // main loop until the watchdog fired.
    let i2c_config = I2cConfig::default()
        .with_frequency(Rate::from_khz(400))
        .with_timeout(esp_hal::i2c::master::BusTimeout::Maximum)
        // a 17-byte transfer takes ~0.5 ms at 400 kHz; failed ones are retried, so give up quickly
        .with_software_timeout(esp_hal::i2c::master::SoftwareTimeout::Transaction(esp_hal::time::Duration::from_millis(5)));
    let mut oled = I2c::new(p.I2C0, i2c_config)
        .ok()
        .map(|i2c| i2c.with_sda(p.GPIO17).with_scl(p.GPIO18))
        .and_then(|i2c| Oled::new(i2c, oled_rst));
    let mut button = Input::new(p.GPIO0, InputConfig::default().with_pull(Pull::Up));
    let mut sleeper = Sleeper::new(p.LPWR);
    let mut awake_until = now_ms() + AWAKE_AFTER_ACTIVITY_MS;
    let (mut ota_requests, mut ota_seen) = (0u32, 0u64);
    let mut next_power_status = 0u64;
    // the on-device interface: pages on the OLED, turned with the PRG button
    let pages: &'static [Page] = match (&oled, slots) {
        (None, _) => &[],
        (Some(_), Some(_)) => PAGES,
        (Some(_), None) => PAGES_SINGLE_SLOT,
    };
    let mut ui = Ui::new(pages).with_screen_timeout(SCREEN_ON_MS, now_ms());
    let mut ui_button = Button::default();
    let mut screen = ScreenPolicy::default();
    let (mut next_view, mut next_oled_try, mut oled_failures) = (0u64, 0u64, 0u8);
    // the PIN of the open update window, for the screen
    let mut ble_pin: Option<u32> = None;

    info!("boot: console");
    wdt.feed();
    // ---- console
    #[cfg(not(feature = "heltec-v3"))]
    let console_rx = meshcore_fw::usb::start(p.USB_DEVICE);
    // esp-println keeps writing to UART0's TX on its own
    #[cfg(feature = "heltec-v3")]
    let console_rx = esp_hal::uart::UartRx::new(p.UART0, esp_hal::uart::Config::default()).unwrap().with_rx(p.GPIO44);
    spawner.spawn(console_task(console_rx).unwrap());
    println!("Ready. Type CLI commands (e.g. 'get name', 'set name X', 'get radio', 'neighbors').");

    // ---- main loop
    let mut noise = NoiseFloor::new();
    let mut next_batt = now_ms() + 60_000;
    let mut next_agc_reset = 0u64;
    let mut next_clock_save = now_ms() + CLOCK_SAVE_INTERVAL_MS;
    let mut last_clock = mesh.ctx.current_time();
    let mut reboot_at: Option<u64> = None;
    let mut ble_until: Option<u64> = None;
    let mut in_trial = in_trial;
    let confirm_at = now_ms() + TRIAL_HEALTHY_MS;
    info!("boot: main loop (last trace: {})", trace::dump());
    let mut prof = Prof::new();
    loop {
        wdt.feed();
        prof.mark("top");
        let now = now_ms();

        prof.mark("tx");
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
                fem.tx_mode();
                led.set_high();
                let res = radio.transmit(frame.bytes(), frame.timeout_ms).await;
                led.set_low();
                fem.rx_mode();
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

        prof.mark("wait");
        // `powersaving on`: light sleep until the next timer or a packet, as upstream does, when
        // nothing needs the chip awake (radios other than LoRa, a console, queued packets)
        if mesh.app.ota.requests != ota_requests {
            (ota_requests, ota_seen) = (mesh.app.ota.requests, now);
        }
        let usb_host = sleeper.usb_host_active();
        if mesh.app.prefs.powersaving_enabled
            && now >= awake_until
            && !ui.screen_on(now)
            && !usb_host
            && station.is_none()
            && wifi.is_some() // no update access point either
            && ble_until.is_none()
            && reboot_at.is_none()
            && !(mesh.app.ota.busy() && now < ota_seen + OTA_IDLE_MS)
            && mesh.ctx.outbound_len() == 0
            && room.as_ref().is_none_or(|r| r.ctx.outbound_len() == 0)
        {
            let next = room.as_ref().and_then(|r| r.next_deadline()).into_iter().chain(mesh.next_deadline()).min();
            let next = next.unwrap_or(u64::MAX);
            let span = next.saturating_sub(now).min(MAX_SLEEP_MS);
            if span >= 100 && radio.arm_wake() {
                button.listen(Event::LowLevel);
                sleeper.light_sleep(esp_hal::time::Duration::from_millis(span));
                button.unlisten();
                continue;
            }
        }
        if now >= next_power_status {
            next_power_status = now + 60_000;
            mesh.app.board.power_status = sleeper.summary();
        }
        // sleep until a frame arrives, a CLI line or a BLE frame comes in, or a timer is due
        let wake = room.as_ref().map_or(u64::MAX, |r| r.next_wakeup()).min(mesh.next_wakeup()).min(now + 50).max(now + 1);
        match select4(radio.wait_irq(), CLI_LINES.receive(), Timer::at(Instant::from_millis(wake)), FRAMES_IN.receive()).await {
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
                awake_until = now_ms() + AWAKE_AFTER_ACTIVITY_MS;
                let cmd = String::from_utf8_lossy(&line);
                if cmd.trim() == "ota info" || cmd.trim() == "ota rollback" {
                    let reply = match (slots, &store) {
                        (None, _) | (_, None) => alloc::string::String::from("single-slot partition table, OTA unavailable"),
                        (Some(sl), Some(s)) if cmd.trim() == "ota rollback" => {
                            if ota::rollback(&mut s.borrow_mut(), sl) {
                                reboot_at = Some(now_ms() + 500);
                                alloc::format!("OK - switching to slot {}, rebooting", sl.target.0)
                            } else {
                                alloc::string::String::from("Error: could not select the other slot")
                            }
                        }
                        (Some(sl), Some(_)) => alloc::format!(
                            "running slot {}{}, update slot {} ({} KiB)",
                            sl.booted,
                            if in_trial { " (trial, not yet confirmed)" } else { "" },
                            sl.target.0,
                            sl.target.2 / 1024
                        ),
                    };
                    println!("  -> {reply}");
                    continue;
                }
                if cmd.trim() == "wdt test" {
                    println!("  -> hanging the main loop; the watchdog should reboot in {WATCHDOG_SECS}s");
                    #[allow(clippy::empty_loop)]
                    loop {}
                }
                // `room <command>` (and `room.post`) go to the hosted room server
                let line = cmd.trim_end();
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
            // firmware update over BLE (`start ota ble`)
            Either4::Fourth((_, frame)) => {
                if let Some(reply) = mesh.app.handle_link_frame(&frame) {
                    let _ = BLE_OUT.try_send(reply);
                }
            }
        }

        let now = now_ms();
        prof.mark("poll");
        mesh.poll(now);
        if let Some(r) = room.as_mut() {
            r.poll(now);
        }
        // the bridge: frames out for what the repeater sent (or heard), packets in from others
        for frame in mesh.app.take_bridge_out() {
            let _ = espnow::OUT.try_send(frame);
        }
        while let Ok(frame) = espnow::IN.try_receive() {
            if let Some((pkt, delay)) = mesh.app.bridge_receive(&frame) {
                mesh.queue_bridged(now, pkt, delay);
            }
        }

        prof.mark("noise");
        if !radio.is_receiving().unwrap_or(true)
            && let Ok(rssi) = radio.rssi_inst()
        {
            noise.sample(rssi);
            mesh.ctx.stats.noise_floor = noise.floor;
            if let Some(r) = room.as_mut() {
                r.ctx.stats.noise_floor = noise.floor;
            }
        }

        prof.mark("batt");
        if now >= next_batt {
            next_batt = now + 60_000;
            mesh.app.board.batt_mv = battery.read_mv().await;
            if let Some(r) = room.as_mut() {
                r.app.board.batt_mv = mesh.app.board.batt_mv;
            }
        }

        prof.mark("agc");
        // periodic receiver reset against a stuck AGC (`agc.reset.interval`, off by default)
        let agc_secs = mesh.app.prefs.agc_reset_interval as u64 * 4;
        if agc_secs > 0 && now >= next_agc_reset {
            if next_agc_reset != 0 && !mesh.tx_in_flight() && !radio.is_receiving().unwrap_or(true) {
                if radio.reset_agc().await.is_err() {
                    warn!("AGC reset failed");
                }
                noise = NoiseFloor::new();
            }
            next_agc_reset = now + agc_secs * 1000;
        }

        prof.mark("screen");
        // the interface: the PRG button lights the screen and turns the pages
        let down = button.is_low();
        if down && !ui_button.is_down() {
            ui.button_down(now);
            awake_until = now + AWAKE_AFTER_ACTIVITY_MS;
            next_view = 0;
        }
        ui.tick(now);
        if let Some(intent) = ui_button.poll(down, now) {
            next_view = 0;
            match ui.press(intent, now) {
                Some(Action::Advert) => {
                    let _ = mesh.app.handle_command(&mut mesh.ctx, 0, "advert");
                    ui.alert(Alert::AdvertSent, now);
                }
                // opened with the other effects below, as by `start ota ble`; the PIN goes on the
                // screen (whoever holds the board may update it anyway, over USB)
                Some(Action::ToggleUpdate) if ble_until.is_none() => {
                    let _ = mesh.app.handle_command(&mut mesh.ctx, 0, "start ota ble");
                }
                Some(Action::ToggleUpdate) if !BLE_CONNECTED.load(core::sync::atomic::Ordering::Relaxed) => {
                    (ble_until, ble_pin) = (None, None);
                    ble::stop();
                    println!("BLE update window closed with the button");
                }
                _ => {}
            }
        }
        if oled_failures >= 10 {
            warn!("OLED: disabled after repeated bus errors");
            oled = None;
            oled_failures = 0;
        }
        if let Some(o) = oled.as_mut() {
            if !ui.screen_on(now) {
                if o.is_on() {
                    info!("OLED: screen off ({} failed I2C transfers since boot)", o.bus_errors());
                }
                o.set_on(false);
                screen.forget();
            } else if now >= next_view && now >= next_oled_try {
                next_view = now + 250;
                let info = ScreenInfo {
                    in_trial,
                    ble_window: ble_pin.zip(ble_until),
                    wifi: station.is_some(),
                    asleep_pct: sleeper.asleep_pct(),
                };
                let view = repeater_view(&mut ui, &mesh, &info, now);
                if screen.due(&view, now) {
                    o.set_on(true);
                    if o.show(&view) {
                        if oled_failures > 0 {
                            info!("OLED: back after {oled_failures} failed frames");
                        }
                        oled_failures = 0;
                        screen.drawn(view, now);
                    } else {
                        // the next frame resets the controller first; back off if that keeps failing
                        oled_failures += 1;
                        next_oled_try = now + 1000 * oled_failures as u64;
                        screen.forget();
                        warn!("OLED: no answer ({oled_failures}), resetting it");
                    }
                }
            }
        }

        prof.mark("net");
        // network: SNTP time, observer uplink, status lines for the CLI
        if let Some(t) = net::TIME_SYNC.try_take()
            && t.abs_diff(mesh.ctx.current_time()) > 2
        {
            info!("SNTP: clock set to {t}");
            mesh.ctx.set_current_time(t);
        }
        if let Some(stack) = station {
            let ip = net::ipv4(&stack);
            mesh.app.board.wifi_status = alloc::format!(
                "ssid:{} status:{} ip:{}",
                mesh.app.prefs.wifi_ssid,
                if net::WIFI_UP.load(core::sync::atomic::Ordering::Relaxed) { "connected" } else { "disconnected" },
                ip.as_deref().unwrap_or("-")
            );
            let online = net::WIFI_UP.load(core::sync::atomic::Ordering::Relaxed) && ip.is_some();
            fwstore::sync(online, &mesh.app.prefs.fw_server);
            mesh.app.board.wifi_ip = ip;
        } else {
            mesh.app.board.wifi_status = alloc::string::String::from("off (set wifi.ssid / wifi.pwd)");
        }
        if let Some(line) = fwstore::status_if_changed(&mut fw_status_seen) {
            mesh.app.board.fw_status = line;
        }
        mesh.app.board.mqtt_status = mqtt_status_line(&mesh.app.prefs, station.is_some());
        if mesh.app.uplink_enabled {
            let p = &mesh.app.prefs;
            let radio_str = uplink::radio_string(p.radio.freq_mhz, p.radio.bw_khz, p.radio.sf, p.radio.cr);
            let obs = uplink::Observer {
                name: &p.node_name,
                device_id: &device_id,
                model: &model,
                firmware_version: meshcore::FIRMWARE_VERSION,
                client_version: CLIENT_VERSION,
                radio: &radio_str,
            };
            let t = mesh.ctx.current_time();
            for up in core::mem::take(&mut mesh.app.uplink) {
                if let Some(pkt) = meshcore::Packet::parse(&up.raw) {
                    let json = uplink::packet_json(&obs, t, up.tx, &pkt, &up.raw, up.snr, up.rssi, up.score_duration);
                    uplink_pub.publish_immediate(alloc::sync::Arc::new(net::UplinkMsg { leaf: "packets", json, retain: false }));
                    if p.mqtt_raw {
                        let json = uplink::raw_json(&obs, t, &up.raw);
                        uplink_pub.publish_immediate(alloc::sync::Arc::new(net::UplinkMsg { leaf: "raw", json, retain: false }));
                    }
                }
            }
            let connected = net::BROKER_CONNECTED.swap(false, core::sync::atomic::Ordering::Relaxed);
            let send_now = core::mem::take(&mut mesh.app.effects.send_mqtt_status);
            if send_now || connected || (p.mqtt_status && now >= next_status) {
                next_status = now + uplink::STATUS_INTERVAL_SECS as u64 * 1000;
                let s = &mesh.ctx.stats;
                let stats = uplink::StatusStats {
                    battery_mv: mesh.app.board.batt_mv,
                    uptime_secs: (now / 1000) as u32,
                    errors: s.err_flags,
                    queue_len: mesh.ctx.outbound_len() as u16,
                    noise_floor: s.noise_floor,
                    tx_air_secs: (s.total_air_time_ms / 1000) as u32,
                    rx_air_secs: (s.rx_air_time_ms / 1000) as u32,
                    recv_errors: s.n_recv_errors,
                };
                let json = uplink::status_json(&obs, t, true, Some(&stats));
                uplink_pub.publish_immediate(alloc::sync::Arc::new(net::UplinkMsg { leaf: "status", json, retain: true }));
            }
        }

        // the hosted room server follows the repeater's clock (SNTP, `clock sync`, `time`)
        if let Some(r) = room.as_mut() {
            let t = mesh.ctx.current_time();
            if r.ctx.current_time().abs_diff(t) > 2 {
                r.ctx.set_current_time(t);
            }
        }

        prof.mark("clock");
        // persist the clock periodically, and right after it was set (`clock sync`, `time`);
        // the RTC RAM copy survives resets, the flash copy survives power loss
        let clock = mesh.ctx.current_time();
        let jumped = clock.abs_diff(last_clock) > 120;
        if clock != last_clock {
            clock_cell::store(clock);
        }
        last_clock = clock;
        if now >= next_clock_save || jumped {
            next_clock_save = now + CLOCK_SAVE_INTERVAL_MS;
            save(&store, Blob::Clock, &clock.to_le_bytes());
        }

        prof.mark("effects");
        // hardware side effects requested by the repeater
        let fx = mesh.app.take_effects();
        for line in &fx.console {
            println!("{line}");
        }
        if fx.save_prefs {
            let text = mesh.app.prefs.serialize();
            if !save(&store, Blob::Prefs, text.as_bytes()) {
                warn!("prefs save failed");
            }
        }
        if fx.save_acl {
            let acl = mesh.app.acl_serialize();
            if !save(&store, Blob::Acl, &acl) {
                warn!("ACL save failed");
            }
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
        if let Some(on) = fx.fem_lna {
            fem.set_lna(on);
        }
        if let Some(code) = fx.start_ota {
            if let (Some(stack), Some(s), Some(sl)) = (station, &store, slots) {
                let params = wifi_ota::Params {
                    ssid: alloc::format!("LAN {}", net::ipv4(&stack).unwrap_or_default()),
                    password: None,
                    node_name: mesh.app.prefs.node_name.clone(),
                    version: meshcore::FIRMWARE_VERSION,
                    code,
                    store: s.clone(),
                    slots: sl,
                    roots: mesh.app.ota.roots.clone(),
                    build: mesh.app.ota.build,
                };
                match wifi_ota::start_http(&spawner, stack, params) {
                    Ok(()) => println!("Wi-Fi OTA: upload page on the LAN address"),
                    Err(e) => println!("Wi-Fi OTA: {e}"),
                }
            } else {
            match (wifi.take(), &store, slots) {
                (Some(w), Some(s), Some(sl)) => {
                    let pw = mesh.app.prefs.password.clone();
                    let params = wifi_ota::Params {
                        ssid: ota_ssid(&mesh.app.prefs.node_name),
                        password: (pw.len() >= 8).then_some(pw),
                        node_name: mesh.app.prefs.node_name.clone(),
                        version: meshcore::FIRMWARE_VERSION,
                        code,
                        store: s.clone(),
                        slots: sl,
                        roots: mesh.app.ota.roots.clone(),
                        build: mesh.app.ota.build,
                    };
                    let seed = (Rng::new().random() as u64) << 32 | Rng::new().random() as u64;
                    match wifi_ota::start(&spawner, w, seed, params) {
                        Ok(()) => println!("Wi-Fi OTA started: connect and open http://192.168.4.1/"),
                        Err(e) => println!("Wi-Fi OTA failed: {e}"),
                    }
                }
                (None, ..) if bridge_radio => println!("Wi-Fi OTA unavailable while the bridge holds the radio (start ota ble)"),
                (None, ..) => {
                    wifi_ota::set_code(code);
                    println!("Wi-Fi OTA already running (new code set)");
                }
                _ => println!("Wi-Fi OTA unavailable"),
            }
            }
        }
        if let Some(pin) = fx.start_ble_ota {
            ble_until = Some(now + BLE_OTA_WINDOW_MINS as u64 * 60_000);
            ble_pin = Some(pin);
            next_view = 0;
            let params = ble::Params {
                name: ota_ssid(&mesh.app.prefs.node_name),
                address: ble::static_address(),
                // shared with the companion firmware: the same board is the same BLE device
                bonds: store.as_ref().and_then(|s| s.borrow_mut().comp_load(CompBlob::Bonds)).unwrap_or_default(),
                pin: Some(pin),
            };
            info!("BLE update window open, heap free {} bytes", esp_alloc::HEAP.free());
            // SAFETY: one BLE task at a time (`ble::start` checks), and an earlier one has dropped
            // its controller before clearing `BLE_RUNNING`
            ble::start(&spawner, unsafe { bt.clone_unchecked() }, params);
        }
        if ble_until.is_some_and(|t| now >= t) {
            if BLE_CONNECTED.load(core::sync::atomic::Ordering::Relaxed) {
                ble_until = Some(now + 60_000); // let a running upload finish
            } else {
                (ble_until, ble_pin) = (None, None);
                ble::stop();
                println!("BLE update window closed");
            }
        }
        if let Some(bonds) = BONDS_CHANGED.try_take()
            && let Some(s) = &store
        {
            s.borrow_mut().comp_save(CompBlob::Bonds, &bonds);
        }
        if fx.network_changed || fx.bridge_changed {
            reboot_at = Some(now + 3000);
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
            println!("Powering off...");
            let _ = radio.sleep();
            fem.shutdown();
            led.set_low();
            loop {
                Timer::after_secs(3600).await;
            }
        }
        prof.mark("end");
        if in_trial && now >= confirm_at {
            if let Some(s) = &store {
                ota::confirm(&mut s.borrow_mut());
            }
            in_trial = false;
            info!("OTA: new firmware confirmed");
        }

        if reboot_at.is_some_and(|t| now >= t) {
            save(&store, Blob::Clock, &mesh.ctx.current_time().to_le_bytes());
            reset::hard_reset();
        }
    }
}
