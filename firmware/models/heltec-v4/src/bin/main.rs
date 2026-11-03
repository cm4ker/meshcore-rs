//! MeshCore repeater firmware for the Heltec WiFi LoRa 32 V4, in Rust: the board around the
//! shared repeater loop ([`meshcore_fw::repeater_app`]).

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
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::Ordering;

use embassy_executor::Spawner;
use embassy_time::{Instant, Timer};
use esp_backtrace as _;
#[cfg(feature = "heltec-v3")]
use esp_hal::Blocking;
use esp_hal::analog::adc::{Adc, AdcCalCurve, AdcConfig, Attenuation};
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Event, Input, InputConfig, Level, Output, OutputConfig, Pin, Pull};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::peripherals::ADC1;
use esp_hal::rng::{Trng, TrngSource};
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::{Duration as HalDuration, Rate};
use esp_hal::timer::timg::TimerGroup;
use esp_println::print;
use log::{info, warn};

use heltec_v4::board::Fem;
use heltec_v4::common::Battery;
use heltec_v4::display::Oled;
use heltec_v4::ota::SharedStore;
use heltec_v4::power::Sleeper;
use heltec_v4::radio::EspIrq;
use heltec_v4::reset;
use heltec_v4::storage;
use heltec_v4::sx1262::Sx1262;
use meshcore::gps::GpsReading;
use meshcore::prefs::NodePrefs;
use meshcore_fw::repeater_app::{self, CLI_LINES, CONSOLE_WOKE_MS, Config, Hardware, RepeaterBoard, SleepWake};
use meshcore_ui::View;
use esp_storage::FlashStorage;

esp_bootloader_esp_idf::esp_app_desc!();

/// esp-println's `println!` ends a line with LF only; the console's lines end with CR LF.
macro_rules! println {
    () => {
        print!("\r\n")
    };
    ($($arg:tt)*) => {
        print!("{}\r\n", format_args!($($arg)*))
    };
}

const BUILD_EPOCH: &str = env!("BUILD_EPOCH");
const BUILD_DATE: &str = env!("BUILD_DATE");

/// Longest legitimate stall is one max-length SF12 transmission (~10 s).
const WATCHDOG_SECS: u64 = 30;
/// Input closer than this to the wake, or to the input before it, is the rest of the cut line.
const CUT_LINE_GAP_MS: u32 = 500;

fn now_ms() -> u64 {
    Instant::now().as_millis()
}

fn load_prefs(store: &Option<SharedStore<'_>>) -> NodePrefs {
    store
        .as_ref()
        .and_then(|s| s.borrow_mut().load(heltec_v4::storage::Blob::Prefs))
        .map(|d| NodePrefs::deserialize(&String::from_utf8_lossy(&d)))
        .unwrap_or_else(|| NodePrefs { tx_power_dbm: heltec_v4::board::DEFAULT_TX_DBM, ..Default::default() })
}

// ---- console
/// The V4's console is the chip's USB Serial/JTAG port, read in its interrupt (`meshcore_fw::usb`).
#[cfg(not(feature = "heltec-v3"))]
type ConsoleRx = meshcore_fw::usb::Input;
/// The V3's USB port is a CP2102 bridge to UART0 (TX 43, RX 44).
#[cfg(feature = "heltec-v3")]
type ConsoleRx = esp_hal::uart::UartRx<'static, Blocking>;

async fn console_read(rx: &mut ConsoleRx, buf: &mut [u8]) -> usize {
    #[cfg(not(feature = "heltec-v3"))]
    return rx.read(buf).await;
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
    let mut shown = 0;
    let show = |part: &[u8]| part.iter().for_each(|&c| print!("{}", c as char));
    let mut cut_line: Option<u32> = None;
    loop {
        let n = console_read(&mut rx, &mut buf).await;
        let typed = n <= 2;
        let woke = CONSOLE_WOKE_MS.swap(0, Ordering::Relaxed);
        if woke != 0 {
            (cut_line, shown) = (Some(woke), 0);
            line.clear();
        }
        for &c in &buf[..n] {
            if let Some(last) = cut_line {
                let t = now_ms() as u32;
                if t.wrapping_sub(last) < CUT_LINE_GAP_MS {
                    cut_line = (!matches!(c, b'\r' | b'\n')).then_some(t);
                    continue;
                }
                cut_line = None;
            }
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

/// The Heltec V4 hardware behind the shared repeater loop.
struct HeltecV4<'d> {
    fem: Fem<'d>,
    led: Output<'d>,
    oled: Option<Oled<'d>>,
    battery: Battery<'d>,
    button: Input<'d>,
    sleeper: Sleeper<'d>,
}

impl RepeaterBoard for HeltecV4<'_> {
    fn name(&self) -> &str {
        self.fem.name()
    }

    fn release_board(&self) -> u8 {
        heltec_v4::RELEASE_BOARD
    }

    fn default_tx_dbm(&self) -> i8 {
        heltec_v4::board::DEFAULT_TX_DBM
    }

    fn adc_multiplier(&self) -> Option<f32> {
        heltec_v4::board::ADC_MULTIPLIER
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
        let scale = heltec_v4::board::battery_scale(multiplier);
        (self.battery.read_mv().await as f32 * scale) as u16
    }

    fn set_gps(&mut self, on: bool) {
        heltec_v4::gps::GPS_ON.signal(on);
    }

    fn gps_latest(&self) -> Option<GpsReading> {
        heltec_v4::gps::latest()
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
        #[cfg(feature = "heltec-v3")]
        if esp_hal::rtc_cntl::wakeup_cause().contains(esp_hal::rtc_cntl::WakeupSource::Uart0) {
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

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
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
        warn!("no '{}' partition: settings will not persist", heltec_v4::storage::PARTITION_LABEL);
    }
    // the FEM's initial LNA state follows the stored setting (the loop reloads the prefs itself)
    let prefs = load_prefs(&store);

    // ---- board: LED + FEM
    let led = Output::new(p.GPIO35, Level::Low, OutputConfig::default());
    let fem = Fem::init(
        p.GPIO7.degrade(),
        p.GPIO2.degrade(),
        p.GPIO46.degrade(),
        p.GPIO5.degrade(),
        prefs.radio_fem_rxgain,
    )
    .await;
    info!("FEM: {:?}", fem.kind);

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
    let battery = Battery::new(Adc::new(p.ADC1, adc_cfg), bat_pin, p.GPIO37);

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
    #[cfg(not(feature = "heltec-v3"))]
    let console_rx = meshcore_fw::usb::start(p.USB_DEVICE);
    #[cfg(feature = "heltec-v3")]
    let mut console_rx = esp_hal::uart::UartRx::new(p.UART0, esp_hal::uart::Config::default()).unwrap().with_rx(p.GPIO44);
    #[cfg(feature = "heltec-v3")]
    if let Err(e) = console_rx.enable_wakeup(&esp_hal::uart::WakeupConfig::default()) {
        warn!("console: no wake from sleep: {e}");
    }
    spawner.spawn(console_task(console_rx).unwrap());

    // ---- optional external GPS on the V4's UART1 header (RX 38, TX 39, EN 34, RESET 42)
    let gps_uart = esp_hal::uart::Uart::new(p.UART1, esp_hal::uart::Config::default().with_baudrate(9600))
        .unwrap()
        .with_rx(p.GPIO38)
        .with_tx(p.GPIO39)
        .into_async();
    let (gps_rx, gps_tx) = gps_uart.split();
    spawner.spawn(
        heltec_v4::gps::task(heltec_v4::gps::Gps {
            rx: gps_rx,
            tx: gps_tx,
            enable: Output::new(p.GPIO34, Level::High, OutputConfig::default()),
            reset: Output::new(p.GPIO42, Level::Low, OutputConfig::default()),
        })
        .unwrap(),
    );

    let board = HeltecV4 { fem, led, oled, battery, button, sleeper };
    let hw = Hardware { radio, store, wifi: p.WIFI, bt: p.BT, entropy };
    let cfg = Config { build_epoch: BUILD_EPOCH.parse().unwrap_or(0), build_date: BUILD_DATE };
    // the loop's state on the heap: in the main task's static it would grow with every change to
    // the loop and move the statics after it (see `layout` in build.rs)
    Box::pin(repeater_app::run(spawner, hw, board, cfg)).await
}
