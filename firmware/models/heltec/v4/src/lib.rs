//! Heltec WiFi LoRa 32 V4 (V4.2, V4.3): what sets it apart from the V3. A LoRa front-end module
//! ([`fem`]) that turns the SX1262's 10 dBm into ~22 dBm, 16 MB of flash (`partitions.csv`), the
//! console on the chip's own USB port. The rest is [`heltec_shared`].

#![no_std]

pub mod fem;

use embassy_executor::Spawner;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::peripherals::GPIO37;
use esp_hal::uart::{Config, Uart};
use heltec_shared::display::SharedI2c;
use heltec_shared::gps::Gps;
use heltec_shared::{ConsolePins, ExtPins, FemPins, Variant};
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

    /// V4 wires its external I2C header to the OLED's I2C0 (GPIO17/18), and has a dedicated GNSS
    /// header on UART1 (RX 39, TX 38, EN 34 active LOW, RESET 42 active LOW).
    fn ext_peripherals(oled_bus: Option<SharedI2c<'static>>, pins: ExtPins) -> (Option<SharedI2c<'static>>, Gps) {
        let (rx, tx) = Uart::new(pins.uart1, Config::default().with_baudrate(9600))
            .unwrap()
            .with_rx(pins.gpio39)
            .with_tx(pins.gpio38)
            .into_async()
            .split();
        let gps = Gps {
            rx,
            tx,
            enable: Output::new(pins.gpio34, Level::High, OutputConfig::default()),
            enable_active: Level::Low,
            reset: Some(Output::new(pins.gpio42, Level::Low, OutputConfig::default())),
        };
        (oled_bus, gps)
    }
}

#[embassy_executor::task]
async fn console(rx: meshcore_fw::usb::Input) {
    heltec_shared::console::run(rx).await
}
