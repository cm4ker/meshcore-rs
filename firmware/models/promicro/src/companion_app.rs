//! The companion on an nRF52840 board, up to the companion loop of the other boards
//! (`meshcore_fw_core::companion`): an app talks to it over BLE (the SoftDevice the bootloader
//! keeps, running all the time: `ble`, `sd`) or USB serial. Its data lives in the internal flash
//! where the repeater stages its updates (`COMPANION_REGION`) and moves aside while an update
//! comes in there (over BLE, USB or from a repeater, as on the other boards; `ota`). The board's
//! binary sets the chip up and puts the parts on their pins; its [`Board`] it makes of them.

use alloc::rc::Rc;
use core::cell::RefCell;

use embassy_executor::Spawner;
use embassy_nrf::gpio::{Input, Output};
use embassy_nrf::interrupt::typelevel::{self, Binding};
use embassy_nrf::nvmc::Nvmc;
use embassy_nrf::rng::Rng;
use embassy_nrf::temp::Temp;
use embassy_nrf::twim::Twim;
use embassy_nrf::usb::vbus_detect::SoftwareVbusDetect;
use embassy_nrf::usb::{self, Driver};
use embassy_nrf::wdt::{self, Watchdog};
use embassy_nrf::{Peri, pac, peripherals};
use embassy_time::Timer;
use embassy_usb::Builder;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use log::{info, warn};
use meshcore::radio::RadioParams;
use meshcore_fw_core::companion::{self, Board, Config};
use meshcore_fw_core::sensors::Sensors;
use meshcore_fw_core::storage::{Mount, Store};
use static_cell::StaticCell;

use crate::board::{self, Battery, Led, Parts};
use crate::companion::Nrf;
use crate::flash::Flash;
use crate::{COMPANION_ASIDE, COMPANION_REGION, COMPANION_REGION_BEFORE, DATA_REGION, Spec, console, ota, sd, usb_frames};

/// Without a pass of the main loop for this long, the watchdog restarts the board. Long, as the
/// watchdog keeps running into the UF2 bootloader (the 1200 baud touch is a soft reset), which
/// does not feed it: copying the next firmware has to fit in.
const WATCHDOG_SECS: u32 = 60;

/// The chip's peripherals the companion takes, and the binary's interrupts for USB.
pub struct Chip<I> {
    pub usbd: Peri<'static, peripherals::USBD>,
    pub irqs: I,
    pub wdt: Peri<'static, peripherals::WDT>,
    pub nvmc: Peri<'static, peripherals::NVMC>,
    pub rng: Peri<'static, peripherals::RNG>,
    pub temp: Temp<'static>,
}

/// What the board's [`Board`] is made of.
pub struct BoardParts {
    pub led: Led,
    pub rxen: Option<Output<'static>>,
    pub button: Input<'static>,
    pub battery: Battery,
    pub temp: Temp<'static>,
    pub sensors: Sensors<Twim<'static>>,
    /// The outputs that stay as they are (`Parts::hold`).
    pub hold: heapless::Vec<Output<'static>, 4>,
}

/// The companion: the watchdog, the SoftDevice, USB with the boot log, the store, the board
/// `board` makes of its parts, then the companion loop for good.
pub async fn run<I, B: Board>(spawner: Spawner, chip: Chip<I>, parts: Parts, spec: &'static Spec, board: impl FnOnce(BoardParts) -> B) -> !
where
    I: Binding<typelevel::USBD, usb::InterruptHandler<peripherals::USBD>> + 'static,
{
    // a watchdog still running from before a soft reset is taken over with the same settings
    let mut wcfg = wdt::Config::default();
    wcfg.timeout_ticks = 32_768 * WATCHDOG_SECS;
    let watchdog = match Watchdog::try_new::<_, 1>(chip.wdt, wcfg) {
        Ok((_, [handle])) => Some(handle),
        Err(_) => None, // running with other settings: it restarts the board, and starts afresh
    };
    // before the SoftDevice's start notes its steps there
    let last_crash = console::take_crash();

    // ---- the SoftDevice, for good: BLE, and from here on the flash, RNG and VBUS through it
    let softdevice = if sd::present() { sd::enable() } else { Err(0) };
    let _ = console::take_crash(); // the steps it noted: it started
    static VBUS: StaticCell<SoftwareVbusDetect> = StaticCell::new();
    let vbus = VBUS.init(SoftwareVbusDetect::new(false, false));
    if softdevice.is_ok() {
        sd::usb_power_events(vbus);
    } else {
        // no VBUS events without it: USB as it is now
        let status = pac::POWER.usbregstatus().read();
        vbus.detected(status.vbusdetect());
        if status.outputrdy() {
            vbus.ready();
        }
    }

    // ---- USB: the boot log, then the app's frames
    let driver = Driver::new(chip.usbd, chip.irqs, &*vbus);
    // the IDs and serial number of upstream's firmware for the board: the host keeps its COM port
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
    spawner.spawn(usb_frames::device_task(builder.build()).unwrap());
    spawner.spawn(usb_frames::rx_task(rx, control).unwrap());
    spawner.spawn(usb_frames::tx_task(tx).unwrap());
    usb_frames::init_logger();
    Timer::after_millis(1500).await; // time for the host to open the port
    info!("MeshCore companion (Rust) {} {} for {}", meshcore::FIRMWARE_VERSION, meshcore::release::build_label(spec.build_date), spec.name);
    if let Some(crash) = last_crash {
        warn!("last reset: {crash}");
    }
    if let Err(e) = softdevice {
        warn!("no SoftDevice ({e:#x}): no BLE");
    }

    // ---- persistent state
    let flash = Flash::new(Nvmc::new(chip.nvmc));
    let mut store =
        Store::with_regions(flash, DATA_REGION, Some(COMPANION_REGION)).with_lending(COMPANION_ASIDE, COMPANION_REGION_BEFORE);
    // a page takes ~85 ms to erase: the region (~7 s) fits in the watchdog's time
    match store.mount() {
        Mount::Ready | Mount::Fresh => {}
        Mount::Lent => info!("companion data: aside, an update holds its region"),
        Mount::Moved => info!("companion data: moved to the advert ring"),
        Mount::Failed => warn!("companion data: the flash failed to take it"),
    }
    let store = Rc::new(RefCell::new(store));
    // an install request left behind (the installer only runs when the app hands over)
    ota::clear_request(&store);
    // count trial boots of a fresh update before anything can hang
    let in_trial = ota::check_trial(&store);
    // an update given up, or gone back from: the data goes back in place
    let lent = store.borrow().lent();
    if lent && !in_trial && !store.borrow_mut().take_back() {
        warn!("companion data: could not put it back, it stays aside");
    }

    // ---- the radio: its supply, the receive side of the antenna switch, the chip
    let Parts { radio, led, rxen, button, battery, i2c, hold } = parts;

    // ---- entropy: the hardware RNG (the SoftDevice's while it runs)
    let mut entropy = [0u8; 32];
    if sd::enabled() {
        // the radio's random numbers are mixed in by the companion loop, should this fail
        let _ = sd::random(&mut entropy);
    } else {
        Rng::new_blocking(chip.rng).blocking_fill_bytes(&mut entropy);
    }

    // ---- sensors on the I2C, for telemetry
    let sensors = Sensors::probe(i2c).await;
    let board = board(BoardParts { led, rxen, button, battery, temp: chip.temp, sensors, hold });
    let seed = u64::from_le_bytes(entropy[..8].try_into().unwrap()) ^ u64::from_le_bytes(entropy[8..16].try_into().unwrap());
    let platform = Nrf { watchdog, seed, store: store.clone(), in_trial, stays_aside: false };
    let cfg = Config {
        // the Omsk mesh, as on the T-Echo (the repeater firmware's settings win, if it ran here)
        radio: RadioParams { freq_mhz: 868.731, bw_khz: 62.5, sf: 7, cr: 7 },
        tx_power_dbm: 22,
        build_epoch: spec.build_epoch.parse().unwrap_or(0),
        build_date: spec.build_date,
    };
    companion::run(spawner, platform, radio, Some(store), entropy, board, cfg).await
}
