//! The MeshCore companion radio on a Heltec board, in Rust: the board around the shared companion
//! loop (`meshcore_fw::companion_app`).

use alloc::boxed::Box;
use alloc::rc::Rc;
use core::cell::RefCell;

use embassy_executor::Spawner;
use esp_hal::analog::adc::{Adc, AdcCalCurve, AdcConfig, Attenuation};
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pin, Pull};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::peripherals::ADC1;
use esp_hal::rng::{Trng, TrngSource};
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use esp_storage::FlashStorage;
use log::warn;
use meshcore::radio::RadioParams;
use meshcore_fw::companion_app::{self, Board, Config, Hardware};
use meshcore_ui::refresh::oled::Oled as Policy;
use meshcore_ui::{Page, View};

use crate::battery::{self, Battery};
use crate::display::Oled;
use crate::power::Sleeper;
use crate::radio::EspIrq;
use crate::sx1262::Sx1262;
use crate::{FemPins, FrontEnd, Variant, gps, reset, storage};

const WATCHDOG_SECS: u64 = 30;
/// SX1262 output; the V4's FEM PA makes ~22 dBm of 10 dBm.
const MAX_TX_POWER_DBM: i8 = 22;
/// The on-device interface (no Bluetooth page: the ESP BLE task cannot be switched off yet). A
/// long press on the home page turns the board off, as on the repeater; the button turns it on.
const PAGES: &[Page] = &[Page::Home, Page::Messages, Page::Nearby, Page::Radio, Page::Gps, Page::Pull, Page::Advert];

struct Heltec<'d, V: Variant> {
    fem: V::FrontEnd,
    led: Output<'d>,
    battery: Battery<'d>,
    button: Input<'d>,
    sleeper: Sleeper<'d>,
    oled: Option<Oled<'d>>,
    oled_failures: u8,
    next_oled_try: u64,
    screen: Policy,
}

impl<V: Variant> Board for Heltec<'_, V> {
    fn manufacturer(&self) -> &str {
        self.fem.name()
    }

    fn release_board(&self) -> u8 {
        V::RELEASE_BOARD
    }

    fn max_tx_power_dbm(&self) -> i8 {
        MAX_TX_POWER_DBM
    }

    fn on_tx_start(&mut self) {
        self.fem.tx_mode();
        self.led.set_high();
    }

    fn on_tx_end(&mut self) {
        self.led.set_low();
        self.fem.rx_mode();
    }

    fn set_lna(&mut self, on: bool) {
        self.fem.set_lna(on);
    }

    /// As the repeater's: deep sleep until the button, the front end unpowered.
    fn power_off(&mut self) {
        gps::GPS_ON.signal(false);
        self.fem.power_off();
        // the radio sleeps until NSS goes low: keep it high while the chip's pins are off
        let mut nss =
            Output::new(unsafe { esp_hal::peripherals::GPIO8::steal() }, Level::High, OutputConfig::default());
        nss.set_pad_hold(true);
        self.led.set_low();
        if let Some(o) = self.oled.as_mut() {
            o.set_on(false);
        }
        self.sleeper.power_off(&mut self.button);
    }

    async fn battery_mv(&mut self) -> u16 {
        self.battery.read_mv().await
    }

    fn adc_multiplier(&self) -> Option<f32> {
        battery::ADC_MULTIPLIER
    }

    fn has_gps(&self) -> bool {
        true
    }

    fn set_gps(&mut self, on: bool) {
        gps::GPS_ON.signal(on);
    }

    fn button_down(&mut self) -> bool {
        self.button.is_low()
    }

    fn has_screen(&self) -> bool {
        self.oled.is_some()
    }

    fn ui_pages(&self) -> &'static [Page] {
        if self.oled.is_some() { PAGES } else { &[] }
    }

    fn show_view(&mut self, v: &View) {
        let now = embassy_time::Instant::now().as_millis();
        if now < self.next_oled_try || !self.screen.due(v, now) {
            return;
        }
        let Some(o) = self.oled.as_mut() else {
            return;
        };
        o.set_on(true);
        if o.show(v) {
            self.oled_failures = 0;
            self.screen.drawn(v.clone(), now);
        } else {
            // the next frame resets the controller first; back off if that keeps failing
            self.oled_failures += 1;
            self.next_oled_try = now + 1000 * self.oled_failures as u64;
            self.screen.forget();
            if self.oled_failures >= 10 {
                warn!("OLED: disabled after repeated bus errors");
                self.oled = None;
            }
        }
    }

    fn screen_off(&mut self) {
        if let Some(o) = self.oled.as_mut() {
            o.set_on(false);
        }
        self.screen.forget();
    }
}

/// The companion firmware of board `V`, from reset; `build_epoch` and `build_date` are its build
/// stamp (`BUILD_EPOCH`, `BUILD_DATE` of the build script).
pub async fn run<V: Variant>(spawner: Spawner, build_epoch: &str, build_date: &'static str) -> ! {
    esp_println::logger::init_logger_from_env();
    let mut p = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);
    esp_alloc::heap_allocator!(size: 150 * 1024);

    let timg0 = TimerGroup::new(p.TIMG0);
    esp_rtos::start(timg0.timer0, p.FROM_CPU_INTR0);
    reset::install(esp_hal::rtc_cntl::Rtc::new(p.RTC_TIMER), WATCHDOG_SECS);

    println!();
    println!("MeshCore companion (Rust) {} {}", meshcore::FIRMWARE_VERSION, meshcore::release::build_label(build_date));
    let store = storage::open(FlashStorage::new(p.FLASH)).map(|s| Rc::new(RefCell::new(s)));

    let led = Output::new(p.GPIO35, Level::Low, OutputConfig::default());
    // the LNA setting is applied from the stored settings once they are loaded
    let fem_pins =
        FemPins { ldo: p.GPIO7.degrade(), csd: p.GPIO2.degrade(), cps: p.GPIO46.degrade(), ctx: p.GPIO5.degrade() };
    let fem = V::front_end(fem_pins, true).await;
    let spi = Spi::new(p.SPI2, SpiConfig::default().with_frequency(Rate::from_mhz(8)))
        .unwrap()
        .with_sck(p.GPIO9)
        .with_mosi(p.GPIO10)
        .with_miso(p.GPIO11);
    let radio = Sx1262::new(
        spi,
        Output::new(p.GPIO8, Level::High, OutputConfig::default()),
        Output::new(p.GPIO12, Level::High, OutputConfig::default()),
        Input::new(p.GPIO13, InputConfig::default().with_pull(Pull::None)),
        EspIrq(Input::new(p.GPIO14, InputConfig::default().with_pull(Pull::Down))),
    );

    let mut entropy = [0u8; 32];
    {
        let _source = TrngSource::new(p.RNG.reborrow(), p.ADC1.reborrow());
        if let Ok(trng) = Trng::try_new() {
            trng.read(&mut entropy);
        }
    }
    let mut adc_cfg = AdcConfig::new();
    let bat_pin = adc_cfg.enable_pin_with_cal::<_, AdcCalCurve<ADC1>>(p.GPIO1, Attenuation::_11dB);
    let active = V::divider_active(p.GPIO37.reborrow());
    let battery = Battery::new(Adc::new(p.ADC1, adc_cfg), bat_pin, p.GPIO37, active);

    // OLED: Vext on with GPIO36 LOW (see the repeater firmware); bounded I2C transfers
    let _vext = Output::new(p.GPIO36, Level::Low, OutputConfig::default());
    let oled_rst = Output::new(p.GPIO21, Level::High, OutputConfig::default());
    embassy_time::Timer::after_millis(10).await;
    let i2c_config = I2cConfig::default()
        .with_frequency(Rate::from_khz(400))
        .with_timeout(esp_hal::i2c::master::BusTimeout::Maximum)
        .with_software_timeout(esp_hal::i2c::master::SoftwareTimeout::Transaction(esp_hal::time::Duration::from_millis(5)));
    let oled = I2c::new(p.I2C0, i2c_config)
        .ok()
        .map(|i2c| i2c.with_sda(p.GPIO17).with_scl(p.GPIO18))
        .and_then(|i2c| Oled::new(i2c, oled_rst));

    // optional external GPS on the UART1 header (RX 38, TX 39, EN 34, RESET 42)
    let gps_uart = esp_hal::uart::Uart::new(p.UART1, esp_hal::uart::Config::default().with_baudrate(9600))
        .unwrap()
        .with_rx(p.GPIO38)
        .with_tx(p.GPIO39)
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

    let board = Heltec::<V> {
        fem,
        led,
        battery,
        button: Input::new(p.GPIO0, InputConfig::default().with_pull(Pull::Up)),
        sleeper: Sleeper::new(p.LPWR),
        oled,
        oled_failures: 0,
        next_oled_try: 0,
        screen: Policy::default(),
    };
    let usb_rx = meshcore_fw::usb::start(p.USB_DEVICE);
    let hw = Hardware { radio, store, usb_rx, wifi: p.WIFI, bt: p.BT, entropy };
    let cfg = Config {
        radio: RadioParams::default(),
        tx_power_dbm: 10,
        build_epoch: build_epoch.parse().unwrap_or(0),
        build_date,
    };
    // the main loop's state on the heap: in the main task's static it would grow with every change
    // to the loop and move the statics after it (see `layout` in build.rs)
    Box::pin(companion_app::run(spawner, hw, board, cfg)).await
}
