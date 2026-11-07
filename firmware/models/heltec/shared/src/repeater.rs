//! The MeshCore repeater on a Heltec board, in Rust: the board around the shared repeater loop
//! ([`meshcore_fw::repeater_app`]).

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use core::cell::RefCell;
use core::sync::atomic::Ordering;

use embassy_executor::Spawner;
use embassy_time::{Instant, Timer};
use esp_hal::analog::adc::{Adc, AdcCalCurve, AdcConfig, Attenuation};
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Event, Input, InputConfig, Level, Output, OutputConfig, Pin, Pull};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::peripherals::ADC1;
use esp_hal::rng::{Trng, TrngSource};
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::{Duration as HalDuration, Rate};
use esp_hal::timer::timg::TimerGroup;
use esp_storage::FlashStorage;
use log::warn;
use meshcore::gps::GpsReading;
use meshcore::prefs::NodePrefs;
use meshcore_fw::repeater_app::{self, CONSOLE_WOKE_MS, Config, Hardware, RepeaterBoard, SleepWake};
use meshcore_ui::View;

use crate::battery::{self, Battery};
use crate::display::Oled;
use crate::ota::SharedStore;
use crate::power::Sleeper;
use crate::radio::EspIrq;
use crate::sx1262::Sx1262;
use crate::{ConsolePins, FemPins, FrontEnd, Variant, gps, reset, storage};

/// Longest legitimate stall is one max-length SF12 transmission (~10 s).
const WATCHDOG_SECS: u64 = 30;

fn now_ms() -> u64 {
    Instant::now().as_millis()
}

fn load_prefs<V: Variant>(store: &Option<SharedStore<'_>>) -> NodePrefs {
    store
        .as_ref()
        .and_then(|s| s.borrow_mut().load(storage::Blob::Prefs))
        .map(|d| NodePrefs::deserialize(&String::from_utf8_lossy(&d)))
        .unwrap_or_else(|| NodePrefs { tx_power_dbm: V::DEFAULT_TX_DBM, ..Default::default() })
}

/// The Heltec hardware behind the shared repeater loop.
struct Heltec<'d, V: Variant> {
    fem: V::FrontEnd,
    led: Output<'d>,
    oled: Option<Oled<'d>>,
    battery: Battery<'d>,
    button: Input<'d>,
    sleeper: Sleeper<'d>,
}

impl<V: Variant> RepeaterBoard for Heltec<'_, V> {
    fn name(&self) -> &str {
        self.fem.name()
    }

    fn release_board(&self) -> u8 {
        V::RELEASE_BOARD
    }

    fn default_tx_dbm(&self) -> i8 {
        V::DEFAULT_TX_DBM
    }

    fn adc_multiplier(&self) -> Option<f32> {
        battery::ADC_MULTIPLIER
    }

    fn has_gps(&self) -> bool {
        true
    }

    fn has_screen(&self) -> bool {
        self.oled.is_some()
    }

    fn lna_controllable(&self) -> bool {
        self.fem.lna_controllable()
    }

    fn powersaving_supported(&self) -> bool {
        true
    }

    fn bridge(&self) -> Option<&'static str> {
        Some("espnow")
    }

    fn fem_tx_mode(&mut self) {
        self.fem.tx_mode();
    }

    fn fem_rx_mode(&mut self) {
        self.fem.rx_mode();
    }

    fn fem_set_lna(&mut self, on: bool) {
        self.fem.set_lna(on);
    }

    fn tx_led(&mut self, on: bool) {
        self.led.set_level(if on { Level::High } else { Level::Low });
    }

    fn led_toggle(&mut self) {
        self.led.toggle();
    }

    async fn battery_mv(&mut self, multiplier: Option<f32>) -> u16 {
        let scale = battery::battery_scale(multiplier);
        (self.battery.read_mv().await as f32 * scale) as u16
    }

    fn set_gps(&mut self, on: bool) {
        gps::GPS_ON.signal(on);
    }

    fn gps_latest(&self) -> Option<GpsReading> {
        gps::latest()
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
        self.oled.as_mut().map_or(true, |o| o.show(view))
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
        // console input woke it: the first bytes are lost, so ask for the line again
        if V::console_woke() {
            CONSOLE_WOKE_MS.store((now_ms() as u32).max(1), Ordering::Relaxed);
            woke.console = true;
        }
        Some(woke)
    }

    async fn power_off(&mut self) {
        self.fem.power_off();
        // the radio sleeps until NSS goes low: keep it high while the chip's pins are off
        let mut nss =
            Output::new(unsafe { esp_hal::peripherals::GPIO8::steal() }, Level::High, OutputConfig::default());
        nss.set_pad_hold(true);
        self.led.set_low();
        if let Some(o) = &mut self.oled {
            o.set_on(false);
        }
        // a button still down would end the sleep at once
        while self.button.is_low() {
            reset::feed();
            Timer::after_millis(50).await;
        }
        Timer::after_millis(100).await;
        self.sleeper.power_off(&mut self.button);
    }
}

/// The repeater firmware of board `V`, from reset; `build_epoch` and `build_date` are its build
/// stamp (`BUILD_EPOCH`, `BUILD_DATE` of the build script).
pub async fn run<V: Variant>(spawner: Spawner, build_epoch: &str, build_date: &'static str) -> ! {
    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let mut p = esp_hal::init(config);

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
    // the FEM's initial LNA state follows the stored setting (the loop reloads the prefs itself)
    let prefs = load_prefs::<V>(&store);

    // ---- board: LED + front end
    let led = Output::new(p.GPIO35, Level::Low, OutputConfig::default());
    let fem_pins =
        FemPins { ldo: p.GPIO7.degrade(), csd: p.GPIO2.degrade(), cps: p.GPIO46.degrade(), ctx: p.GPIO5.degrade() };
    let fem = V::front_end(fem_pins, prefs.radio_fem_rxgain).await;

    // ---- radio (configured by the loop from the stored settings)
    let spi = Spi::new(p.SPI2, SpiConfig::default().with_frequency(Rate::from_mhz(8)))
        .unwrap()
        .with_sck(p.GPIO9)
        .with_mosi(p.GPIO10)
        .with_miso(p.GPIO11);
    let mut nss = Output::new(p.GPIO8, Level::High, OutputConfig::default());
    nss.set_pad_hold(false); // held high through a power-off
    let reset_pin = Output::new(p.GPIO12, Level::High, OutputConfig::default());
    let busy = Input::new(p.GPIO13, InputConfig::default().with_pull(Pull::None));
    let dio1 = Input::new(p.GPIO14, InputConfig::default().with_pull(Pull::Down));
    let radio = Sx1262::new(spi, nss, reset_pin, busy, EspIrq(dio1));

    // ---- entropy: hardware TRNG (the loop mixes in the radio's noise)
    let mut entropy = [0u8; 32];
    {
        let _source = TrngSource::new(p.RNG.reborrow(), p.ADC1.reborrow());
        if let Ok(trng) = Trng::try_new() {
            trng.read(&mut entropy);
        }
    }

    // ---- battery
    let mut adc_cfg = AdcConfig::new();
    let bat_pin = adc_cfg.enable_pin_with_cal::<_, AdcCalCurve<ADC1>>(p.GPIO1, Attenuation::_11dB);
    let active = V::divider_active(p.GPIO37.reborrow());
    let battery = Battery::new(Adc::new(p.ADC1, adc_cfg), bat_pin, p.GPIO37, active);

    // ---- OLED status screen (Vext powers it; GPIO36 LOW = on)
    let _vext = Output::new(p.GPIO36, Level::Low, OutputConfig::default());
    let oled_rst = Output::new(p.GPIO21, Level::High, OutputConfig::default());
    Timer::after_millis(10).await;
    let i2c_config = I2cConfig::default()
        .with_frequency(Rate::from_khz(400))
        .with_timeout(esp_hal::i2c::master::BusTimeout::Maximum)
        .with_software_timeout(esp_hal::i2c::master::SoftwareTimeout::Transaction(esp_hal::time::Duration::from_millis(5)));
    let oled = I2c::new(p.I2C0, i2c_config)
        .ok()
        .map(|i2c| i2c.with_sda(p.GPIO17).with_scl(p.GPIO18))
        .and_then(|i2c| Oled::new(i2c, oled_rst));

    let button = Input::new(p.GPIO0, InputConfig::default().with_pull(Pull::Up));
    let sleeper = Sleeper::new(p.LPWR);

    // ---- console
    V::start_console(spawner, ConsolePins { usb: p.USB_DEVICE, uart0: p.UART0, rx: p.GPIO44 });

    // ---- optional external GPS on the UART1 header (we read 39, write 38; EN 34, RESET 42)
    let gps_uart = esp_hal::uart::Uart::new(p.UART1, esp_hal::uart::Config::default().with_baudrate(9600))
        .unwrap()
        .with_rx(p.GPIO39)
        .with_tx(p.GPIO38)
        .into_async();
    let (gps_rx, gps_tx) = gps_uart.split();
    spawner.spawn(
        gps::task(gps::Gps {
            rx: gps_rx,
            tx: gps_tx,
            enable: Output::new(p.GPIO34, Level::High, OutputConfig::default()),
            reset: Output::new(p.GPIO42, Level::Low, OutputConfig::default()),
        })
        .unwrap(),
    );

    let board = Heltec::<V> { fem, led, oled, battery, button, sleeper };
    let hw = Hardware { radio, store, wifi: p.WIFI, bt: p.BT, entropy };
    let cfg = Config { build_epoch: build_epoch.parse().unwrap_or(0), build_date };
    // the loop's state on the heap: in the main task's static it would grow with every change to
    // the loop and move the statics after it (see `layout` in build.rs)
    Box::pin(repeater_app::run(spawner, hw, board, cfg)).await
}
