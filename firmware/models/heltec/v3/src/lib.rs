//! Heltec WiFi LoRa 32 V3: what sets it apart from the V4. No front-end module (the SX1262 reaches
//! the antenna through its DIO2 RF switch), 8 MB of flash (`partitions.csv`: the V4's layout with
//! smaller app slots), the console on UART0 through a CP2102 bridge, and a battery divider switch
//! whose polarity flipped with V3.2. The rest is [`heltec_shared`].

#![no_std]

use embassy_executor::Spawner;
use esp_hal::gpio::{Input, InputConfig, Level, Pull};
use esp_hal::peripherals::GPIO37;
use esp_hal::uart::{Config, UartRx};
use heltec_shared::{ConsolePins, FemPins, FrontEnd, Variant};
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
