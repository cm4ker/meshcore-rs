//! Heltec WiFi LoRa 32 V3: what sets it apart from the V4. No front-end module (the SX1262 reaches
//! the antenna through its DIO2 RF switch), 8 MB of flash (`partitions.csv`: the V4's layout with
//! smaller app slots), the console on UART0 through a CP2102 bridge, and a battery divider switch
//! whose polarity flipped with V3.2. The rest is [`heltec_shared`].

#![no_std]

use embassy_executor::Spawner;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::i2c::master::I2c;
use esp_hal::peripherals::GPIO37;
use esp_hal::uart::{Config, Uart, UartRx};
use heltec_shared::display::{SharedI2c, i2c_config};
use heltec_shared::gps::Gps;
use heltec_shared::{ConsolePins, ExtPins, FemPins, FrontEnd, Variant};
use meshcore_fw::companion_app::Serial;

pub struct HeltecV3;

impl Variant for HeltecV3 {
    const RELEASE_BOARD: u8 = meshcore::release::board::HELTEC_V3;
    /// The radio drives the antenna itself.
    const DEFAULT_TX_DBM: i8 = 22;

    type FrontEnd = NoFem;

    async fn front_end(_pins: FemPins, _lna: bool) -> NoFem {
        NoFem
    }

    /// The switch flipped polarity with V3.2; its pull resistor tells which (upstream
    /// HeltecV3Board: active = !digitalRead(PIN_ADC_CTRL)).
    fn divider_active(ctrl: GPIO37<'_>) -> Level {
        !Input::new(ctrl, InputConfig::default().with_pull(Pull::None)).level()
    }

    /// The USB port is a CP2102 bridge to UART0 (TX 43, RX 44); input on it wakes the chip.
    fn start_console(spawner: Spawner, pins: ConsolePins) {
        heltec_shared::console::start_uart0(spawner, pins.uart0, pins.rx);
    }

    fn console_woke() -> bool {
        heltec_shared::console::uart0_woke()
    }

    /// Over the CP2102 too: the chip's own USB port leads nowhere on the V3.
    fn app_serial(pins: ConsolePins) -> Serial {
        Serial::Uart(UartRx::new(pins.uart0, Config::default()).unwrap().with_rx(pins.rx).into_async())
    }

    /// V3 does not bring the OLED's I2C (GPIO17/18) out to the pin headers; external sensors sit
    /// on I2C1 (SDA GPIO41, SCL GPIO42). External GPS follows upstream `variants/heltec_v3`:
    /// UART1 RX 48 (receiver TX), TX 47 (receiver RX), EN 26 active HIGH.
    fn ext_peripherals(_oled_bus: Option<SharedI2c<'static>>, pins: ExtPins) -> (Option<SharedI2c<'static>>, Gps) {
        let sensor_bus = I2c::new(pins.i2c1, i2c_config())
            .ok()
            .map(|i2c| i2c.with_sda(pins.gpio41).with_scl(pins.gpio42))
            .map(SharedI2c::new);
        let (rx, tx) = Uart::new(pins.uart1, Config::default().with_baudrate(9600))
            .unwrap()
            .with_rx(pins.gpio48)
            .with_tx(pins.gpio47)
            .into_async()
            .split();
        let gps = Gps {
            rx,
            tx,
            enable: Output::new(pins.gpio26, Level::Low, OutputConfig::default()),
            enable_active: Level::High,
            reset: None,
        };
        (sensor_bus, gps)
    }
}

/// No front end: nothing to switch.
pub struct NoFem;

impl FrontEnd for NoFem {
    fn name(&self) -> &'static str {
        "Heltec V3"
    }

    fn lna_controllable(&self) -> bool {
        false
    }

    fn tx_mode(&mut self) {}

    fn rx_mode(&mut self) {}

    fn set_lna(&mut self, _on: bool) {}

    fn power_off(&mut self) {}
}
