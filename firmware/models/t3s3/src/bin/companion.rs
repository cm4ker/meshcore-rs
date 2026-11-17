//! MeshCore companion radio firmware for a LilyGo LoRa32 T3-S3 (ESP32-S3FH4R2, 4 MB flash), in
//! Rust: the board around the shared companion loop.
//!
//! Wiring (upstream `variants/lilygo_t3s3`): the SX1262 sits on SPI2 — SCK 5, MOSI 6, MISO 3,
//! NSS 7, RESET 8, BUSY 34, DIO1 33; DIO2 switches the antenna path, DIO3 feeds the 1.8 V TCXO.
//! TX LED 37 (active high), BOOT button 0, battery divider 100k/100k on GPIO1. USB goes to the
//! chip's own Serial/JTAG port: that is the app link, the BLE pairing needs nothing wired.

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
use esp_hal::analog::adc::{Adc, AdcCalCurve, AdcConfig, Attenuation};
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::peripherals::ADC1;
use esp_hal::rng::{Trng, TrngSource};
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use esp_storage::FlashStorage;

use meshcore::radio::RadioParams;
use meshcore_fw::companion_app::{self, Board, Config, Hardware, Serial};
use meshcore_fw::radio::EspIrq;
use meshcore_fw::sx1262::Sx1262;
use meshcore_fw::{reset, storage};
use t3s3::battery::Battery;

esp_bootloader_esp_idf::esp_app_desc!();

const BUILD_EPOCH: &str = env!("BUILD_EPOCH");
const BUILD_DATE: &str = env!("BUILD_DATE");

const WATCHDOG_SECS: u64 = 30;
/// The SX1262's own output; there is no front end after it.
const TX_DBM: i8 = 22;

struct T3S3<'d> {
    led: Output<'d>,
    battery: Battery<'d>,
    button: Input<'d>,
}

impl Board for T3S3<'_> {
    fn manufacturer(&self) -> &str {
        "LilyGo T3-S3"
    }

    fn release_board(&self) -> u8 {
        meshcore::release::board::T3S3
    }

    fn max_tx_power_dbm(&self) -> i8 {
        TX_DBM
    }

    fn on_tx_start(&mut self) {
        self.led.set_high();
    }

    fn on_tx_end(&mut self) {
        self.led.set_low();
    }

    async fn battery_mv(&mut self) -> u16 {
        self.battery.read_mv().await
    }

    fn button_down(&mut self) -> bool {
        self.button.is_low()
    }

    fn power_off(&mut self) {
        self.led.set_low();
    }
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();
    let mut p = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);
    esp_alloc::heap_allocator!(size: 150 * 1024);

    let timg0 = TimerGroup::new(p.TIMG0);
    esp_rtos::start(timg0.timer0, p.FROM_CPU_INTR0);
    reset::install(esp_hal::rtc_cntl::Rtc::new(p.RTC_TIMER), WATCHDOG_SECS);

    println!();
    println!(
        "MeshCore companion (Rust) {} {} for LilyGo T3-S3",
        meshcore::FIRMWARE_VERSION,
        meshcore::release::build_label(BUILD_DATE)
    );
    let store = storage::open(FlashStorage::new(p.FLASH)).map(|s| Rc::new(RefCell::new(s)));

    let spi = Spi::new(p.SPI2, SpiConfig::default().with_frequency(Rate::from_mhz(8)))
        .unwrap()
        .with_sck(p.GPIO5)
        .with_mosi(p.GPIO6)
        .with_miso(p.GPIO3);
    let mut nss = Output::new(p.GPIO7, Level::High, OutputConfig::default());
    nss.set_pad_hold(false); // held high through a power-off
    let reset_pin = Output::new(p.GPIO8, Level::High, OutputConfig::default());
    let busy = Input::new(p.GPIO34, InputConfig::default().with_pull(Pull::None));
    let dio1 = Input::new(p.GPIO33, InputConfig::default().with_pull(Pull::Down));
    let radio = Sx1262::new(spi, nss, reset_pin, busy, EspIrq(dio1)).without_rx_patch();

    let mut entropy = [0u8; 32];
    {
        let _source = TrngSource::new(p.RNG.reborrow(), p.ADC1.reborrow());
        if let Ok(trng) = Trng::try_new() {
            trng.read(&mut entropy);
        }
    }

    let mut adc_cfg = AdcConfig::new();
    let bat_pin = adc_cfg.enable_pin_with_cal::<_, AdcCalCurve<ADC1<'_>>>(p.GPIO1, Attenuation::_11dB);

    let board = T3S3 {
        led: Output::new(p.GPIO37, Level::Low, OutputConfig::default()),
        battery: Battery::new(Adc::new(p.ADC1, adc_cfg), bat_pin),
        button: Input::new(p.GPIO0, InputConfig::default().with_pull(Pull::Up)),
    };

    let serial = Serial::Usb(meshcore_fw::usb::start(p.USB_DEVICE));
    let hw = Hardware { radio, store, serial, wifi: p.WIFI, bt: p.BT, entropy };
    let cfg = Config {
        // the Omsk mesh this board was built for (upstream variant `LORA_FREQ` etc.)
        radio: RadioParams { freq_mhz: 868.731, bw_khz: 62.5, sf: 7, cr: 7 },
        tx_power_dbm: TX_DBM,
        build_epoch: BUILD_EPOCH.parse().unwrap_or(0),
        build_date: BUILD_DATE,
    };
    // the main loop's state on the heap: in the main task's static it would grow with every change
    // to the loop and move the statics after it (see `layout` in build.rs)
    Box::pin(companion_app::run(spawner, hw, board, cfg)).await
}
