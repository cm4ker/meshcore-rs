//! MeshCore repeater firmware for a LilyGo LoRa32 T3-S3 (ESP32-S3FH4R2, 4 MB flash), in Rust: the
//! board around the shared repeater loop ([`meshcore_fw::repeater_app`]).
//!
//! Wiring (upstream `variants/lilygo_t3s3`): the SX1262 sits on SPI2 — SCK 5, MOSI 6, MISO 3,
//! NSS 7, RESET 8, BUSY 34, DIO1 33; DIO2 switches the antenna path, DIO3 feeds the 1.8 V TCXO.
//! TX LED 37 (active high), BOOT button 0, battery divider 100k/100k on GPIO1. USB goes to the
//! chip's own Serial/JTAG port (no bridge): it is the console, the app link and the flashing port.
//!
//! Headless: no screen, no GPS and no front-end module — the SX1262 drives the antenna itself, so
//! the output is the chip's own and `set radio.fem.rxgain` has nothing to switch. `bridge` is
//! absent too: this port wires no ESP-NOW partner.

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
use alloc::string::String;
use core::cell::RefCell;

use embassy_executor::Spawner;
use esp_backtrace as _;
use esp_hal::analog::adc::{Adc, AdcCalCurve, AdcConfig, Attenuation};
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Event, Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::peripherals::ADC1;
use esp_hal::rng::{Trng, TrngSource};
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::{Duration as HalDuration, Rate};
use esp_hal::timer::timg::TimerGroup;
use esp_storage::FlashStorage;
use log::warn;

use meshcore::gps::GpsReading;
use meshcore_fw::ota::SharedStore;
use meshcore_fw::radio::EspIrq;
use meshcore_fw::repeater_app::{self, Config, Hardware, RepeaterBoard, SleepWake};
use meshcore_fw::sx1262::Sx1262;
use meshcore_fw::{power::Sleeper, reset, storage};
use meshcore_ui::View;
use t3s3::battery::Battery;
use t3s3::display::Oled;

esp_bootloader_esp_idf::esp_app_desc!();

const BUILD_EPOCH: &str = env!("BUILD_EPOCH");
const BUILD_DATE: &str = env!("BUILD_DATE");

const WATCHDOG_SECS: u64 = 30;
/// The SX1262's own output; there is no front end after it.
const TX_DBM: i8 = 22;

/// The T3-S3's hardware: the TX LED, the battery sense, the optional OLED, the BOOT button and the
/// light-sleep/power helper.
struct T3S3<'d> {
    led: Output<'d>,
    battery: Battery<'d>,
    oled: Option<Oled<'d>>,
    button: Input<'d>,
    sleeper: Sleeper<'d>,
}

impl RepeaterBoard for T3S3<'_> {
    fn name(&self) -> &str {
        "LilyGo T3-S3"
    }

    fn release_board(&self) -> u8 {
        meshcore::release::board::T3S3
    }

    fn default_tx_dbm(&self) -> i8 {
        TX_DBM
    }

    /// Upstream keeps no multiplier for this board (it reads `2 ×` the pin), so there is none to
    /// calibrate `set adc.multiplier` against.
    fn adc_multiplier(&self) -> Option<f32> {
        None
    }

    fn has_gps(&self) -> bool {
        false
    }

    fn has_screen(&self) -> bool {
        self.oled.is_some()
    }

    fn lna_controllable(&self) -> bool {
        false
    }

    /// The S3 light-sleeps between packets, as the Heltec boards do: the radio's IRQ and the button
    /// are the wake sources, and the loop skips the sleep while a USB host is attached.
    fn powersaving_supported(&self) -> bool {
        true
    }

    fn bridge(&self) -> Option<&'static str> {
        None
    }

    fn fem_tx_mode(&mut self) {}

    fn fem_rx_mode(&mut self) {}

    fn fem_set_lna(&mut self, _on: bool) {}

    fn tx_led(&mut self, on: bool) {
        self.led.set_level(if on { Level::High } else { Level::Low });
    }

    fn led_toggle(&mut self) {
        self.led.toggle();
    }

    async fn battery_mv(&mut self, _multiplier: Option<f32>) -> u16 {
        self.battery.read_mv().await
    }

    fn set_gps(&mut self, _on: bool) {}

    fn gps_latest(&self) -> Option<GpsReading> {
        None
    }

    fn screen_is_on(&self) -> bool {
        self.oled.as_ref().is_some_and(|o| o.is_on())
    }

    fn screen_set_on(&mut self, on: bool) {
        if let Some(o) = &mut self.oled {
            o.set_on(on);
        }
    }

    fn screen_show(&mut self, view: &View) -> bool {
        self.oled.as_mut().is_none_or(|o| o.show(view))
    }

    fn screen_bus_errors(&self) -> u32 {
        self.oled.as_ref().map_or(0, |o| o.bus_errors())
    }

    fn screen_disable(&mut self) {
        self.oled = None;
    }

    fn button_down(&self) -> bool {
        self.button.is_low()
    }

    fn usb_host_active(&mut self) -> bool {
        self.sleeper.usb_host_active()
    }

    fn power_summary(&mut self) -> String {
        self.sleeper.summary()
    }

    fn asleep_pct(&self) -> u8 {
        self.sleeper.asleep_pct()
    }

    fn light_sleep(&mut self, span_ms: u64) -> Option<SleepWake> {
        self.button.listen(Event::LowLevel);
        self.sleeper.light_sleep(HalDuration::from_millis(span_ms));
        self.button.unlisten();
        let mut woke = SleepWake::default();
        if self.button.is_low() {
            woke.button = true;
        }
        Some(woke)
    }

    async fn power_off(&mut self) {
        self.led.set_low(); // off (active high)
        if let Some(o) = &mut self.oled {
            o.set_on(false);
        }
        // the radio is already asleep; deep sleep until the button is pressed
        self.sleeper.power_off(&mut self.button);
    }
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();
    let mut p = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);
    esp_alloc::heap_allocator!(size: 126 * 1024);

    let timg0 = TimerGroup::new(p.TIMG0);
    esp_rtos::start(timg0.timer0, p.FROM_CPU_INTR0);
    // an unattended repeater must recover from hangs: full reset if the main loop stops feeding
    reset::install(esp_hal::rtc_cntl::Rtc::new(p.RTC_TIMER), WATCHDOG_SECS);

    let store: Option<SharedStore> = storage::open(FlashStorage::new(p.FLASH)).map(|s| Rc::new(RefCell::new(s)));
    if store.is_none() {
        warn!("no '{}' partition: settings will not persist", storage::PARTITION_LABEL);
    }

    // ---- radio (configured by the loop from the stored settings)
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
    // no Heltec RX register patch: upstream applies it only where the variant asks for it
    let radio = Sx1262::new(spi, nss, reset_pin, busy, EspIrq(dio1)).without_rx_patch();

    // ---- entropy: hardware TRNG (the loop mixes in the radio's noise)
    let mut entropy = [0u8; 32];
    {
        let _source = TrngSource::new(p.RNG.reborrow(), p.ADC1.reborrow());
        if let Ok(trng) = Trng::try_new() {
            trng.read(&mut entropy);
        }
    }

    // ---- battery (the divider has no switch, so the pin is always wired)
    let mut adc_cfg = AdcConfig::new();
    let bat_pin = adc_cfg.enable_pin_with_cal::<_, AdcCalCurve<ADC1<'_>>>(p.GPIO1, Attenuation::_11dB);
    let battery = Battery::new(Adc::new(p.ADC1, adc_cfg), bat_pin);

    // ---- OLED status screen (optional on the board: `None` when nothing answers on the bus)
    let oled_rst = Output::new(p.GPIO21, Level::High, OutputConfig::default());
    let i2c_config = I2cConfig::default()
        .with_frequency(Rate::from_khz(400))
        .with_timeout(esp_hal::i2c::master::BusTimeout::Maximum)
        .with_software_timeout(esp_hal::i2c::master::SoftwareTimeout::Transaction(esp_hal::time::Duration::from_millis(5)));
    let oled = I2c::new(p.I2C0, i2c_config)
        .ok()
        .map(|i2c| i2c.with_sda(p.GPIO18).with_scl(p.GPIO17))
        .and_then(|i2c| Oled::new(i2c, oled_rst));
    if oled.is_none() {
        warn!("no OLED on the I2C bus: running headless");
    }

    let board = T3S3 {
        led: Output::new(p.GPIO37, Level::Low, OutputConfig::default()),
        battery,
        oled,
        button: Input::new(p.GPIO0, InputConfig::default().with_pull(Pull::Up)),
        sleeper: Sleeper::new(p.LPWR),
    };

    // ---- console and app link over the chip's own USB Serial/JTAG
    let usb_rx = meshcore_fw::usb::start(p.USB_DEVICE);
    spawner.spawn(repeater_app::usb_console_task(usb_rx).unwrap());

    let hw = Hardware { radio, store, wifi: p.WIFI, bt: p.BT, entropy };
    let cfg = Config { build_epoch: BUILD_EPOCH.parse().unwrap_or(0), build_date: BUILD_DATE };
    // the loop's state on the heap: in the main task's static it would grow with every change to
    // the loop and move the statics after it (see `layout` in build.rs)
    Box::pin(repeater_app::run(spawner, hw, board, cfg)).await
}
