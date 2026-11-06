//! MeshCore companion radio firmware for an nRF52840 Pro Micro with an SX1262 module, in Rust.
//!
//! The companion loop is the one of the other boards (`meshcore_fw_core::companion`): an app
//! talks to it over BLE (the SoftDevice the bootloader keeps, running all the time: `promicro::ble`,
//! `promicro::sd`) or USB serial. Its data lives in the internal flash where the repeater stages
//! its updates (`promicro::COMPANION_REGION`) and moves aside while an update comes in there
//! (over BLE, USB or from a repeater, as on the other boards; `promicro::ota`).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::rc::Rc;
use core::cell::RefCell;
use core::mem::MaybeUninit;
use core::panic::PanicInfo;

use embassy_executor::Spawner;
use embassy_nrf::gpio::{Input, Level, Output, OutputDrive, Pull};
use embassy_nrf::interrupt::{self, InterruptExt, Priority};
use embassy_nrf::nvmc::Nvmc;
use embassy_nrf::rng::Rng;
use embassy_nrf::saadc::{self, ChannelConfig, Saadc};
use embassy_nrf::spim::{self, Spim};
use embassy_nrf::temp::{self, Temp};
use embassy_nrf::twim::{self, Twim};
use embassy_nrf::usb::Driver;
use embassy_nrf::usb::vbus_detect::SoftwareVbusDetect;
use embassy_nrf::wdt::{self, Watchdog};
use embassy_nrf::{bind_interrupts, config, pac, peripherals, usb};
use embassy_time::Timer;
use embassy_usb::Builder;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embedded_alloc::LlffHeap as Heap;
use log::{info, warn};
use meshcore::radio::RadioParams;
use meshcore_fw_core::companion::{self, Config};
use meshcore_fw_core::radio::WaitIrq;
use meshcore_fw_core::storage::{Mount, Store};
use meshcore_fw_core::sx1262::Sx1262;
use promicro::board::{self, Battery};
use promicro::companion::{Nrf, ProMicro};
use promicro::flash::Flash;
use promicro::sensors::Sensors;
use promicro::{COMPANION_ASIDE, COMPANION_REGION, COMPANION_REGION_BEFORE, DATA_REGION, console, ota, sd, usb_frames};
use static_cell::StaticCell;

const BUILD_EPOCH: &str = env!("BUILD_EPOCH");
const BUILD_DATE: &str = env!("BUILD_DATE");

/// Without a pass of the main loop for this long, the watchdog restarts the board. Long, as the
/// watchdog keeps running into the UF2 bootloader (the 1200 baud touch is a soft reset), which
/// does not feed it: copying the next firmware has to fit in.
const WATCHDOG_SECS: u32 = 60;

const HEAP_SIZE: usize = 160 * 1024;

#[global_allocator]
static HEAP: Heap = Heap::empty();

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    // the SoftDevice's while it runs; VBUS events that come once it has stopped, right before a
    // reset, are taken here
    CLOCK_POWER => usb::vbus_detect::InterruptHandler;
    SPI2 => spim::InterruptHandler<peripherals::SPI2>;
    SAADC => saadc::InterruptHandler;
    TWISPI0 => twim::InterruptHandler<peripherals::TWISPI0>;
    TEMP => temp::InterruptHandler;
    EGU2_SWI2 => sd::SoftDeviceEvents;
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
    // the LDO, not the DC/DC converter upstream turns on (`NRF52BoardDCDC`): clones without its
    // inductor (a SuperMini seen 2026-11-06) lock up the moment it starts
    // priorities the SoftDevice leaves to the app (2, 3, 5..7); the critical sections mask them
    // all (`promicro::sd`)
    ncfg.time_interrupt_priority = Priority::P2;
    ncfg.gpiote_interrupt_priority = Priority::P2;
    let p = embassy_nrf::init(ncfg);
    for irq in [interrupt::USBD, interrupt::CLOCK_POWER, interrupt::SPI2, interrupt::SAADC, interrupt::TWISPI0, interrupt::TEMP] {
        irq.set_priority(Priority::P3);
    }
    // a watchdog still running from before a soft reset is taken over with the same settings
    let mut wcfg = wdt::Config::default();
    wcfg.timeout_ticks = 32_768 * WATCHDOG_SECS;
    let watchdog = match Watchdog::try_new::<_, 1>(p.WDT, wcfg) {
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
    let driver = Driver::new(p.USBD, Irqs, &*vbus);
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
    spawner.spawn(usb_frames::device_task(builder.build()).unwrap());
    spawner.spawn(usb_frames::rx_task(rx, control).unwrap());
    spawner.spawn(usb_frames::tx_task(tx).unwrap());
    usb_frames::init_logger();
    Timer::after_millis(1500).await; // time for the host to open the port
    info!("MeshCore companion (Rust) {} {} for ProMicro", meshcore::FIRMWARE_VERSION, meshcore::release::build_label(BUILD_DATE));
    if let Some(crash) = last_crash {
        warn!("last reset: {crash}");
    }
    if let Err(e) = softdevice {
        warn!("no SoftDevice ({e:#x}): no BLE");
    }

    // ---- persistent state
    let flash = Flash::new(Nvmc::new(p.NVMC));
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

    // ---- radio: its supply, the receive side of the antenna switch, the chip
    let led = Output::new(p.P0_15, Level::Low, OutputDrive::Standard);
    let _radio_power = Output::new(p.P0_13, Level::High, OutputDrive::Standard);
    let rxen = Output::new(p.P0_17, Level::High, OutputDrive::Standard);
    Timer::after_millis(10).await; // the radio powers up
    let mut scfg = spim::Config::default();
    // upstream's speed (RadioLib's 2 MHz): some modules and boards do not take 8
    scfg.frequency = spim::Frequency::M2;
    let spi = Spim::new(p.SPI2, Irqs, p.P1_11, p.P0_02, p.P1_15, scfg);
    let radio = Sx1262::new(
        spi,
        Output::new(p.P1_13, Level::High, OutputDrive::Standard),
        Output::new(p.P0_09, Level::High, OutputDrive::Standard),
        Input::new(p.P0_29, Pull::None),
        WaitIrq(Input::new(p.P0_10, Pull::Down)),
    )
    .without_rx_patch();

    // ---- entropy: the hardware RNG (the SoftDevice's while it runs)
    let mut entropy = [0u8; 32];
    if sd::enabled() {
        // the radio's random numbers are mixed in by the companion loop, should this fail
        let _ = sd::random(&mut entropy);
    } else {
        Rng::new_blocking(p.RNG).blocking_fill_bytes(&mut entropy);
    }

    let battery = Battery { adc: Saadc::new(p.SAADC, Irqs, saadc::Config::default(), [ChannelConfig::single_ended(p.P0_31)]) };
    // ---- sensors on the I2C, for telemetry
    let mut icfg = twim::Config::default();
    icfg.sda_pullup = true;
    icfg.scl_pullup = true;
    static TWIM_BUF: StaticCell<[u8; 8]> = StaticCell::new();
    let i2c = Twim::new(p.TWISPI0, Irqs, p.P1_04, p.P0_11, icfg, TWIM_BUF.init([0; 8]));
    let sensors = Sensors::probe(i2c).await;
    let board = ProMicro { led, rxen, button: Input::new(p.P1_00, Pull::Up), battery, temp: Temp::new(p.TEMP, Irqs), sensors };
    let seed = u64::from_le_bytes(entropy[..8].try_into().unwrap()) ^ u64::from_le_bytes(entropy[8..16].try_into().unwrap());
    let platform = Nrf { watchdog, seed, store: store.clone(), in_trial, stays_aside: false };
    let cfg = Config {
        // the Omsk mesh, as on the T-Echo (the repeater firmware's settings win, if it ran here)
        radio: RadioParams { freq_mhz: 868.731, bw_khz: 62.5, sf: 7, cr: 7 },
        tx_power_dbm: 22,
        build_epoch: BUILD_EPOCH.parse().unwrap_or(0),
        build_date: BUILD_DATE,
    };
    companion::run(spawner, platform, radio, Some(store), entropy, board, cfg).await
}
