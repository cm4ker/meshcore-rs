//! Heltec WiFi LoRa 32 V3: what sets it apart from the V4. No front-end module (the SX1262 reaches
//! the antenna through its DIO2 RF switch), 8 MB of flash (`partitions.csv`: the V4's layout with
//! smaller app slots), the console on UART0 through a CP2102 bridge, and a battery divider switch
//! whose polarity flipped with V3.2. The rest is [`heltec_shared`].

#![no_std]

use embassy_executor::Spawner;
use embassy_time::Timer;
use esp_hal::Blocking;
use esp_hal::gpio::{Input, InputConfig, Level, Pull};
use esp_hal::peripherals::GPIO37;
use esp_hal::rtc_cntl::{WakeupSource, wakeup_cause};
use esp_hal::uart::{Config, UartRx, WakeupConfig};
use heltec_shared::console::ConsoleRx;
use heltec_shared::{ConsolePins, FemPins, FrontEnd, Variant};
use log::warn;

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
        let mut rx = UartRx::new(pins.uart0, Config::default()).unwrap().with_rx(pins.rx);
        if let Err(e) = rx.enable_wakeup(&WakeupConfig::default()) {
            warn!("console: no wake from sleep: {e}");
        }
        spawner.spawn(console(Uart0(rx)).unwrap());
    }

    fn console_woke() -> bool {
        wakeup_cause().contains(WakeupSource::Uart0)
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

/// The console's UART, polled: its FIFO holds what came while the task slept.
struct Uart0(UartRx<'static, Blocking>);

impl ConsoleRx for Uart0 {
    async fn read(&mut self, buf: &mut [u8]) -> usize {
        loop {
            let n = self.0.read_buffered(buf).unwrap_or(0);
            if n > 0 {
                return n;
            }
            Timer::after_millis(20).await;
        }
    }
}

#[embassy_executor::task]
async fn console(rx: Uart0) {
    heltec_shared::console::run(rx).await
}
