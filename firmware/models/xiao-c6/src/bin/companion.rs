//! MeshCore companion radio firmware for a Seeed XIAO ESP32-C6 with a Waveshare Core2021-HF
//! (Semtech LR2021), in Rust: the board around the shared companion loop.
//!
//! Wiring (XIAO pin -> GPIO): CLK D8 -> 19, MISO D9 -> 20, MOSI D10 -> 18, CS D3 -> 21,
//! DIO11/IRQ D1 -> 1, RESET D2 -> 2, BUSY D0 -> 0. The module has a crystal (no TCXO) and
//! switches its antenna path itself.

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

extern crate alloc;

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
use esp_println::println;
use esp_storage::FlashStorage;
use meshcore::radio::RadioParams;
use meshcore_fw::companion_app::{self, Board, Config, Hardware};
use meshcore_fw::lr2021::Lr2021;
use meshcore_fw::reset;
use meshcore_fw::radio::EspIrq;
use meshcore_fw::storage;

esp_bootloader_esp_idf::esp_app_desc!();

const BUILD_EPOCH: &str = env!("BUILD_EPOCH");
const BUILD_DATE: &str = env!("BUILD_DATE");

const WATCHDOG_SECS: u64 = 30;
/// Waveshare Core2021 exposes DIO11 as the IRQ line.
const IRQ_DIO: u8 = 11;

struct XiaoC6<'d> {
    /// The user LED, active low.
    led: Output<'d>,
    /// BOOT button (GPIO9), usable once the chip has started.
    button: Input<'d>,
}

impl Board for XiaoC6<'_> {
    fn manufacturer(&self) -> &str {
        "Xiao C6"
    }

    fn release_board(&self) -> u8 {
        meshcore::release::board::XIAO_C6
    }

    fn max_tx_power_dbm(&self) -> i8 {
        22
    }

    fn on_tx_start(&mut self) {
        self.led.set_low();
    }

    fn on_tx_end(&mut self) {
        self.led.set_high();
    }

    fn power_off(&mut self) {
        self.led.set_high();
    }

    fn button_down(&mut self) -> bool {
        self.button.is_low()
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

    println!();
    println!("MeshCore companion (Rust) {} build {} for XIAO C6 + LR2021", meshcore::FIRMWARE_VERSION, BUILD_DATE);
    let store = storage::open(FlashStorage::new(p.FLASH)).map(|s| Rc::new(RefCell::new(s)));

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
    };
    let usb_rx = meshcore_fw::usb::start(p.USB_DEVICE);
    let hw = Hardware { radio, store, usb_rx, wifi: p.WIFI, bt: p.BT, entropy };
    let cfg = Config {
        // the Omsk mesh this board was built for (upstream variant `LORA_FREQ` etc.)
        radio: RadioParams { freq_mhz: 868.731, bw_khz: 62.5, sf: 7, cr: 7 },
        tx_power_dbm: 22,
        build_epoch: BUILD_EPOCH.parse().unwrap_or(0),
        build_date: BUILD_DATE,
    };
    companion_app::run(spawner, hw, board, cfg).await
}
