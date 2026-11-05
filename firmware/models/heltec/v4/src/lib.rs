//! Heltec WiFi LoRa 32 V4 (V4.2, V4.3): what sets it apart from the V3. A LoRa front-end module
//! ([`fem`]) that turns the SX1262's 10 dBm into ~22 dBm, 16 MB of flash (`partitions.csv`), the
//! console on the chip's own USB port. The rest is [`heltec_shared`].

#![no_std]

pub mod fem;

use embassy_executor::Spawner;
use esp_hal::gpio::Level;
use esp_hal::peripherals::GPIO37;
use heltec_shared::{ConsolePins, FemPins, Variant};
use log::info;
use meshcore_fw::companion_app::Serial;

use crate::fem::Fem;

pub struct HeltecV4;

impl Variant for HeltecV4 {
    const RELEASE_BOARD: u8 = meshcore::release::board::HELTEC_V4;
    /// The FEM's PA turns 10 dBm into ~22 dBm.
    const DEFAULT_TX_DBM: i8 = 10;

    type FrontEnd = Fem<'static>;

    async fn front_end(pins: FemPins, lna: bool) -> Fem<'static> {
        let fem = Fem::init(pins.ldo, pins.csd, pins.cps, pins.ctx, lna).await;
        info!("FEM: {:?}", fem.kind);
        fem
    }

    fn divider_active(_ctrl: GPIO37<'_>) -> Level {
        Level::High
    }

    /// The chip's USB Serial/JTAG port, read in its interrupt (`meshcore_fw::usb`).
    fn start_console(spawner: Spawner, pins: ConsolePins) {
        spawner.spawn(console(meshcore_fw::usb::start(pins.usb)).unwrap());
    }

    fn console_woke() -> bool {
        false
    }

    fn app_serial(pins: ConsolePins) -> Serial {
        Serial::Usb(meshcore_fw::usb::start(pins.usb))
    }
}

#[embassy_executor::task]
async fn console(rx: meshcore_fw::usb::Input) {
    heltec_shared::console::run(rx).await
}
