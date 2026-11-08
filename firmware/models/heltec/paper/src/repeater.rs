//! The MeshCore repeater on the Wireless Paper: the board around the shared repeater loop
//! (`meshcore_fw::repeater_app`), as `heltec_shared::repeater` has it for the V3 and V4, with the
//! pages on the e-paper in place of the OLED. The e-paper keeps its image unpowered, so the screen
//! never goes dark; its task redraws as the e-paper refresh policy says.

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use core::cell::RefCell;
use core::sync::atomic::Ordering;

use embassy_executor::Spawner;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Event, Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::rng::{Trng, TrngSource};
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::{Duration as HalDuration, Rate};
use esp_hal::timer::timg::TimerGroup;
use esp_storage::FlashStorage;
use heltec_shared::ota::SharedStore;
use heltec_shared::power::Sleeper;
use heltec_shared::radio::EspIrq;
use heltec_shared::sx1262::Sx1262;
use heltec_shared::{console, reset, storage};
use log::warn;
use meshcore::gps::GpsReading;
use meshcore_fw::repeater_app::{self, CONSOLE_WOKE_MS, Config, Hardware, RepeaterBoard, SleepWake};
use meshcore_ui::View;

use crate::battery::{ADC_MULTIPLIER, Battery};
use crate::epaper::{self, OFF_DRAWN, SCREEN};

/// Longest legitimate stall is one max-length SF12 transmission (~10 s).
const WATCHDOG_SECS: u64 = 30;
/// The radio drives the antenna itself, as on the V3.
const DEFAULT_TX_DBM: i8 = 22;

struct Paper {
    led: Output<'static>,
    battery: Battery,
    button: Input<'static>,
    sleeper: Sleeper<'static>,
}

impl RepeaterBoard for Paper {
    fn name(&self) -> &str {
        "Heltec Wireless Paper"
    }

    fn release_board(&self) -> u8 {
        meshcore::release::board::HELTEC_PAPER
    }

    fn default_tx_dbm(&self) -> i8 {
        DEFAULT_TX_DBM
    }

    fn adc_multiplier(&self) -> Option<f32> {
        Some(ADC_MULTIPLIER)
    }

    fn has_gps(&self) -> bool {
        false
    }

    fn has_screen(&self) -> bool {
        true
    }

    fn lna_controllable(&self) -> bool {
        false
    }

    fn powersaving_supported(&self) -> bool {
        true
    }

    fn bridge(&self) -> Option<&'static str> {
        Some("espnow")
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

    async fn battery_mv(&mut self, multiplier: Option<f32>) -> u16 {
        self.battery.read_mv(multiplier).await
    }

    fn set_gps(&mut self, _on: bool) {}

    fn gps_latest(&self) -> Option<GpsReading> {
        None
    }

    fn always_on(&self) -> bool {
        true
    }

    fn screen_is_on(&self) -> bool {
        true
    }

    fn screen_show(&mut self, view: &View) -> bool {
        SCREEN.signal(view.clone());
        true
    }

    fn button_down(&self) -> bool {
        self.button.is_low()
    }

    /// Never: the USB-C port leads to the CP2102, and the chip's own USB pins (19, 20) switch and
    /// read the battery divider, which its USB peripheral may take for a host's frames.
    fn usb_host_active(&mut self) -> bool {
        false
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
        // console input woke it: the first bytes are lost, so ask for the line again
        if console::uart0_woke() {
            CONSOLE_WOKE_MS.store((Instant::now().as_millis() as u32).max(1), Ordering::Relaxed);
            woke.console = true;
        }
        Some(woke)
    }

    /// The e-paper task draws the "off" page first; then the radio's and the screen's supply off,
    /// deep sleep until the button.
    async fn power_off(&mut self) {
        let _ = with_timeout(Duration::from_secs(10), OFF_DRAWN.wait()).await;
        // the radio sleeps until NSS goes low: keep it high while the chip's pins are off
        let mut nss =
            Output::new(unsafe { esp_hal::peripherals::GPIO8::steal() }, Level::High, OutputConfig::default());
        nss.set_pad_hold(true);
        let mut vext =
            Output::new(unsafe { esp_hal::peripherals::GPIO45::steal() }, Level::High, OutputConfig::default());
        vext.set_pad_hold(true);
        self.led.set_low();
        // a button still down would end the sleep at once
        while self.button.is_low() {
            reset::feed();
            Timer::after_millis(50).await;
        }
        Timer::after_millis(100).await;
        self.sleeper.power_off(&mut self.button);
    }
}

/// The repeater firmware, from reset; `build_epoch` and `build_date` are its build stamp
/// (`BUILD_EPOCH`, `BUILD_DATE` of the build script).
pub async fn run(spawner: Spawner, build_epoch: &str, build_date: &'static str) -> ! {
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

    // Vext feeds the radio and the screen (LOW = on); a power-off left it held off
    let mut vext = Output::new(p.GPIO45, Level::Low, OutputConfig::default());
    vext.set_pad_hold(false);
    Timer::after_millis(50).await;

    let led = Output::new(p.GPIO18, Level::Low, OutputConfig::default());

    // ---- radio (configured by the loop from the stored settings)
    let spi = Spi::new(p.SPI2, SpiConfig::default().with_frequency(Rate::from_mhz(8)))
        .unwrap()
        .with_sck(p.GPIO9)
        .with_mosi(p.GPIO10)
        .with_miso(p.GPIO11);
    let mut nss = Output::new(p.GPIO8, Level::High, OutputConfig::default());
    nss.set_pad_hold(false); // held high through a power-off
    let radio = Sx1262::new(
        spi,
        nss,
        Output::new(p.GPIO12, Level::High, OutputConfig::default()),
        Input::new(p.GPIO13, InputConfig::default().with_pull(Pull::None)),
        EspIrq(Input::new(p.GPIO14, InputConfig::default().with_pull(Pull::Down))),
    );

    // ---- entropy: hardware TRNG (the loop mixes in the radio's noise)
    let mut entropy = [0u8; 32];
    {
        let _source = TrngSource::new(p.RNG.reborrow(), p.ADC1.reborrow());
        if let Ok(trng) = Trng::try_new() {
            trng.read(&mut entropy);
        }
    }

    let battery = Battery::new(p.ADC2, p.GPIO20, p.GPIO19);
    epaper::start(spawner, p.SPI3, p.GPIO3, p.GPIO2, p.GPIO4, p.GPIO5, p.GPIO6, p.GPIO7);
    let button = Input::new(p.GPIO0, InputConfig::default().with_pull(Pull::Up));
    let sleeper = Sleeper::new(p.LPWR);

    // the USB port is a CP2102 bridge to UART0, as on the V3
    console::start_uart0(spawner, p.UART0, p.GPIO44);

    let board = Paper { led, battery, button, sleeper };
    let hw = Hardware { radio, store, wifi: p.WIFI, bt: p.BT, entropy };
    let cfg = Config { build_epoch: build_epoch.parse().unwrap_or(0), build_date };
    // the loop's state on the heap: in the main task's static it would grow with every change to
    // the loop and move the statics after it (see `layout` in build.rs)
    Box::pin(repeater_app::run(spawner, hw, board, cfg)).await
}
