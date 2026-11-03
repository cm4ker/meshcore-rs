//! MeshCore repeater firmware for a Seeed XIAO ESP32-C6 with a Waveshare Core2021-HF (Semtech
//! LR2021), in Rust: the board around the shared repeater loop ([`meshcore_fw::repeater_app`]).
//!
//! Headless: no front end (the LR2021 switches its own antenna), no screen, no GPS and no battery
//! sense. The on-device interface is the USB console; firmware updates go over Wi-Fi, BLE or a
//! neighbour. On the ESP32-C6 esp-radio cannot run BLE next to a Wi-Fi station (coex is stubbed),
//! so BLE updates are only available while the Wi-Fi station is down (as the companion does).
//!
//! Wiring (XIAO pin -> GPIO): CLK D8 -> 19, MISO D9 -> 20, MOSI D10 -> 18, CS D3 -> 21,
//! DIO11/IRQ D1 -> 1, RESET D2 -> 2, BUSY D0 -> 0. LED on GPIO15 (active low), BOOT button GPIO9.

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::rc::Rc;
use core::cell::RefCell;

use embassy_executor::Spawner;
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::rng::{Trng, TrngSource};
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_storage::FlashStorage;
use log::warn;

use meshcore::gps::GpsReading;
use meshcore_fw::lr2021::Lr2021;
use meshcore_fw::ota::SharedStore;
use meshcore_fw::radio::EspIrq;
use meshcore_fw::repeater_app::{self, Config, Hardware, RepeaterBoard};
use meshcore_fw::{power::Sleeper, reset, storage};

esp_bootloader_esp_idf::esp_app_desc!();

const BUILD_EPOCH: &str = env!("BUILD_EPOCH");
const BUILD_DATE: &str = env!("BUILD_DATE");

const WATCHDOG_SECS: u64 = 30;
/// Waveshare Core2021 exposes DIO11 as the IRQ line.
const IRQ_DIO: u8 = 11;

/// The XIAO C6 repeater hardware: a user LED and the BOOT button, plus the light-sleep/power helper
/// (used only for power-off; the C6 does not light-sleep between packets here).
struct XiaoC6<'d> {
    /// The user LED, active low.
    led: Output<'d>,
    /// BOOT button (GPIO9).
    button: Input<'d>,
    sleeper: Sleeper<'d>,
}

impl RepeaterBoard for XiaoC6<'_> {
    fn name(&self) -> &str {
        "Xiao C6"
    }

    fn release_board(&self) -> u8 {
        meshcore::release::board::XIAO_C6
    }

    fn default_tx_dbm(&self) -> i8 {
        22
    }

    fn adc_multiplier(&self) -> Option<f32> {
        None
    }

    fn has_gps(&self) -> bool {
        false
    }

    fn has_screen(&self) -> bool {
        false
    }

    fn lna_controllable(&self) -> bool {
        false
    }

    fn powersaving_supported(&self) -> bool {
        false
    }

    fn bridge(&self) -> Option<&'static str> {
        None
    }

    fn fem_tx_mode(&mut self) {}

    fn fem_rx_mode(&mut self) {}

    fn fem_set_lna(&mut self, _on: bool) {}

    fn tx_led(&mut self, on: bool) {
        self.led.set_level(if on { Level::Low } else { Level::High });
    }

    fn led_toggle(&mut self) {
        self.led.toggle();
    }

    async fn battery_mv(&mut self, _multiplier: Option<f32>) -> u16 {
        0
    }

    fn set_gps(&mut self, _on: bool) {}

    fn gps_latest(&self) -> Option<GpsReading> {
        None
    }

    fn button_down(&self) -> bool {
        self.button.is_low()
    }

    async fn power_off(&mut self) {
        self.led.set_high(); // off (active low)
        // the radio is already asleep; deep sleep until the button is pressed
        self.sleeper.power_off(&mut self.button);
    }
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();
    let mut p = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 160 * 1024);

    let timg0 = TimerGroup::new(p.TIMG0);
    esp_rtos::start(timg0.timer0, p.FROM_CPU_INTR0);
    reset::install(esp_hal::rtc_cntl::Rtc::new(p.RTC_TIMER), WATCHDOG_SECS);

    let store: Option<SharedStore> = storage::open(FlashStorage::new(p.FLASH)).map(|s| Rc::new(RefCell::new(s)));
    if store.is_none() {
        warn!("no settings partition: settings will not persist");
    }

    // ---- radio (configured by the loop from the stored settings)
    let spi = Spi::new(p.SPI2, SpiConfig::default().with_frequency(Rate::from_mhz(8)))
        .unwrap()
        .with_sck(p.GPIO19)
        .with_mosi(p.GPIO18)
        .with_miso(p.GPIO20);
    let radio = Lr2021::new(
        spi,
        Output::new(p.GPIO21, Level::High, OutputConfig::default()),
        Output::new(p.GPIO2, Level::High, OutputConfig::default()),
        Input::new(p.GPIO0, InputConfig::default().with_pull(Pull::None)),
        EspIrq(Input::new(p.GPIO1, InputConfig::default().with_pull(Pull::Down))),
        IRQ_DIO,
        None,
    );

    // ---- entropy: hardware TRNG (the loop mixes in the radio's noise)
    let mut entropy = [0u8; 32];
    {
        let _source = TrngSource::new(p.RNG.reborrow(), p.ADC1.reborrow());
        if let Ok(trng) = Trng::try_new() {
            trng.read(&mut entropy);
        }
    }

    let board = XiaoC6 {
        led: Output::new(p.GPIO15, Level::High, OutputConfig::default()),
        button: Input::new(p.GPIO9, InputConfig::default().with_pull(Pull::Up)),
        sleeper: Sleeper::new(p.LPWR),
    };

    // ---- console over USB Serial/JTAG
    let usb_rx = meshcore_fw::usb::start(p.USB_DEVICE);
    spawner.spawn(repeater_app::usb_console_task(usb_rx).unwrap());

    let hw = Hardware { radio, store, wifi: p.WIFI, bt: p.BT, entropy };
    let cfg = Config { build_epoch: BUILD_EPOCH.parse().unwrap_or(0), build_date: BUILD_DATE };
    Box::pin(repeater_app::run(spawner, hw, board, cfg)).await
}
