//! The MeshCore companion radio on the Wireless Paper: the board around the shared companion loop
//! (`meshcore_fw::companion_app`), as `heltec_shared::companion` has it for the V3 and V4.

use alloc::boxed::Box;
use alloc::rc::Rc;
use core::cell::RefCell;

use embassy_executor::Spawner;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer, with_timeout};
use esp_hal::analog::adc::{Adc, AdcCalCurve, AdcConfig, AdcPin, Attenuation};
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::peripherals::{ADC2, GPIO20};
use esp_hal::rng::{Trng, TrngSource};
use esp_hal::spi::Mode as SpiMode;
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::uart::{Config as UartConfig, UartRx};
use esp_hal::Blocking;
use esp_println::println;
use esp_storage::FlashStorage;
use heltec_shared::power::Sleeper;
use heltec_shared::radio::EspIrq;
use heltec_shared::sx1262::Sx1262;
use heltec_shared::{reset, storage};
use meshcore::radio::RadioParams;
use meshcore_fw::companion_app::{self, Board, Config, Hardware, Serial};
use meshcore_ui::{Page, View};

use crate::epaper::{self, Epaper, SCREEN};

const WATCHDOG_SECS: u64 = 30;
const MAX_TX_POWER_DBM: i8 = 22;
/// The on-device interface, as the V3's without the GPS page (no GPS header on this board).
const PAGES: &[Page] = &[Page::Home, Page::Messages, Page::Nearby, Page::Radio, Page::Pull, Page::Advert];

/// The battery divider, about 1:1 as Meshtastic has it for this board (upstream's `ADC_MULTIPLIER`
/// of 8.4 reads several times the battery), over the calibrated ADC; `get adc.multiplier` shows it.
const ADC_MULTIPLIER: f32 = 2.0;

/// The e-paper task has drawn the "off" page.
pub static OFF_DRAWN: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Battery voltage through the divider on GPIO20, switched on by GPIO19 low.
struct Battery {
    adc: Adc<'static, ADC2<'static>, Blocking>,
    pin: AdcPin<GPIO20<'static>, ADC2<'static>, AdcCalCurve<ADC2<'static>>>,
    ctrl: Output<'static>,
}

impl Battery {
    async fn read_mv(&mut self) -> u16 {
        self.ctrl.set_low();
        Timer::after_millis(10).await;
        let mut sum = 0u32;
        for _ in 0..8 {
            sum += self.adc.read_blocking(&mut self.pin) as u32;
        }
        self.ctrl.set_high();
        ((sum / 8) as f32 * ADC_MULTIPLIER) as u16
    }
}

struct Paper {
    spawner: Spawner,
    led: Output<'static>,
    battery: Battery,
    /// Taken by the power-off task.
    button: Option<Input<'static>>,
    sleeper: Option<Sleeper<'static>>,
}

impl Board for Paper {
    fn manufacturer(&self) -> &str {
        "Heltec Wireless Paper"
    }

    fn release_board(&self) -> u8 {
        meshcore::release::board::HELTEC_PAPER
    }

    fn max_tx_power_dbm(&self) -> i8 {
        MAX_TX_POWER_DBM
    }

    fn on_tx_start(&mut self) {
        self.led.set_high();
    }

    fn on_tx_end(&mut self) {
        self.led.set_low();
    }

    /// The e-paper task draws the "off" page first; then deep sleep until the button.
    fn power_off(&mut self) {
        self.led.set_low();
        if let (Some(sleeper), Some(button)) = (self.sleeper.take(), self.button.take()) {
            self.spawner.spawn(power_off(sleeper, button).unwrap());
        }
    }

    async fn battery_mv(&mut self) -> u16 {
        self.battery.read_mv().await
    }

    fn adc_multiplier(&self) -> Option<f32> {
        Some(ADC_MULTIPLIER)
    }

    fn button_down(&mut self) -> bool {
        self.button.as_mut().is_some_and(|b| b.is_low())
    }

    fn has_screen(&self) -> bool {
        true
    }

    fn always_on(&self) -> bool {
        true
    }

    fn ui_pages(&self) -> &'static [Page] {
        PAGES
    }

    fn show_view(&mut self, view: &View) {
        SCREEN.signal(view.clone());
    }
}

/// Once the "off" page is on the screen: the radio's and the screen's supply off, deep sleep.
#[embassy_executor::task]
async fn power_off(mut sleeper: Sleeper<'static>, mut button: Input<'static>) {
    let _ = with_timeout(Duration::from_secs(10), OFF_DRAWN.wait()).await;
    // the radio sleeps until NSS goes low: keep it high while the chip's pins are off
    let mut nss = Output::new(unsafe { esp_hal::peripherals::GPIO8::steal() }, Level::High, OutputConfig::default());
    nss.set_pad_hold(true);
    let mut vext = Output::new(unsafe { esp_hal::peripherals::GPIO45::steal() }, Level::High, OutputConfig::default());
    vext.set_pad_hold(true);
    sleeper.power_off(&mut button);
}

/// The companion firmware, from reset; `build_epoch` and `build_date` are its build stamp
/// (`BUILD_EPOCH`, `BUILD_DATE` of the build script).
pub async fn run(spawner: Spawner, build_epoch: &str, build_date: &'static str) -> ! {
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

    // Vext feeds the radio and the screen (LOW = on); a sleep may have left it held off
    let mut _vext = Output::new(p.GPIO45, Level::Low, OutputConfig::default());
    _vext.set_pad_hold(false);
    Timer::after_millis(50).await;

    let led = Output::new(p.GPIO18, Level::Low, OutputConfig::default());
    let spi = Spi::new(p.SPI2, SpiConfig::default().with_frequency(Rate::from_mhz(8)))
        .unwrap()
        .with_sck(p.GPIO9)
        .with_mosi(p.GPIO10)
        .with_miso(p.GPIO11);
    let mut nss = Output::new(p.GPIO8, Level::High, OutputConfig::default());
    nss.set_pad_hold(false);
    let radio = Sx1262::new(
        spi,
        nss,
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
    let pin = adc_cfg.enable_pin_with_cal::<_, AdcCalCurve<ADC2>>(p.GPIO20, Attenuation::_11dB);
    let battery = Battery {
        adc: Adc::new(p.ADC2, adc_cfg),
        pin,
        ctrl: Output::new(p.GPIO19, Level::High, OutputConfig::default()),
    };

    let epd_spi = Spi::new(p.SPI3, SpiConfig::default().with_frequency(Rate::from_mhz(4)).with_mode(SpiMode::_0))
        .unwrap()
        .with_sck(p.GPIO3)
        .with_mosi(p.GPIO2);
    let epd = Epaper {
        spi: epd_spi,
        cs: Output::new(p.GPIO4, Level::High, OutputConfig::default()),
        dc: Output::new(p.GPIO5, Level::High, OutputConfig::default()),
        reset: Output::new(p.GPIO6, Level::High, OutputConfig::default()),
        busy: Input::new(p.GPIO7, InputConfig::default().with_pull(Pull::None)),
    };
    spawner.spawn(epaper::task(epd).unwrap());

    let board = Paper {
        spawner,
        led,
        battery,
        button: Some(Input::new(p.GPIO0, InputConfig::default().with_pull(Pull::Up))),
        sleeper: Some(Sleeper::new(p.LPWR)),
    };
    // the USB port is a CP2102 bridge to UART0 (RX 44), as on the V3
    let serial = Serial::Uart(UartRx::new(p.UART0, UartConfig::default()).unwrap().with_rx(p.GPIO44).into_async());
    let hw = Hardware { radio, store, serial, wifi: p.WIFI, bt: p.BT, entropy };
    let cfg = Config {
        radio: RadioParams::default(),
        tx_power_dbm: MAX_TX_POWER_DBM,
        build_epoch: build_epoch.parse().unwrap_or(0),
        build_date,
    };
    // the main loop's state on the heap (see `heltec_shared::companion`)
    Box::pin(companion_app::run(spawner, hw, board, cfg)).await
}
