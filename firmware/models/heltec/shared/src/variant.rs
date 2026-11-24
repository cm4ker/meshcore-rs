//! What one Heltec board does its own way, for the shared firmware to ask.

use embassy_executor::Spawner;
use esp_hal::gpio::{AnyPin, Level};
use esp_hal::peripherals::{
    GPIO26, GPIO34, GPIO37, GPIO38, GPIO39, GPIO41, GPIO42, GPIO44, GPIO47, GPIO48, I2C1, UART0, UART1, USB_DEVICE,
};
use meshcore_fw::companion_app::Serial;

use crate::display::SharedI2c;
use crate::gps::Gps;

/// The pins of the V4's front end (FEM), handed to every board: on the V3 they lead nowhere.
pub struct FemPins {
    /// LDO enable (GPIO7)
    pub ldo: AnyPin<'static>,
    /// chip enable, CSD (GPIO2); its idle level tells the FEM type
    pub csd: AnyPin<'static>,
    /// GC1109's PA mode, CPS (GPIO46)
    pub cps: AnyPin<'static>,
    /// KCT8103L's CTX (GPIO5)
    pub ctx: AnyPin<'static>,
}

/// What the serial console (the repeater's CLI, the companion's app link) can run on: the chip's
/// USB port, or UART0 (RX 44).
pub struct ConsolePins {
    pub usb: USB_DEVICE<'static>,
    pub uart0: UART0<'static>,
    pub rx: GPIO44<'static>,
}

/// Peripherals for the board's external I2C sensors and optional GPS receiver (V3 and V4 wire
/// these pins differently: on V3 GPIO17/18 are not on the headers so sensors use I2C1 on 41/42
/// and GPS uses 47/48/26; on V4 sensors share I2C0 on 17/18 with the OLED and GPS uses 38/39/34/42).
pub struct ExtPins {
    pub i2c1: I2C1<'static>,
    pub uart1: UART1<'static>,
    pub gpio26: GPIO26<'static>,
    pub gpio34: GPIO34<'static>,
    pub gpio38: GPIO38<'static>,
    pub gpio39: GPIO39<'static>,
    pub gpio41: GPIO41<'static>,
    pub gpio42: GPIO42<'static>,
    pub gpio47: GPIO47<'static>,
    pub gpio48: GPIO48<'static>,
}

/// The radio's front end between the SX1262 and the antenna.
pub trait FrontEnd {
    /// The board's name, as `board` answers it.
    fn name(&self) -> &'static str;
    /// The front end has a switchable LNA (`set radio.fem.rxgain`).
    fn lna_controllable(&self) -> bool;
    fn tx_mode(&mut self);
    fn rx_mode(&mut self);
    fn set_lna(&mut self, on: bool);
    /// Unpowered until the next boot.
    fn power_off(&mut self);
}

/// One Heltec board, as the shared firmware sees it.
#[allow(async_fn_in_trait)]
pub trait Variant {
    /// The board a signed release for it must name (`meshcore::release::board`).
    const RELEASE_BOARD: u8;
    /// SX1262 output on a fresh node, repeater or companion.
    const DEFAULT_TX_DBM: i8;

    type FrontEnd: FrontEnd;

    /// Power the front end and put it into RX mode, its LNA on if `lna`.
    async fn front_end(pins: FemPins, lna: bool) -> Self::FrontEnd;

    /// The level of GPIO37 that switches the battery divider on.
    fn divider_active(ctrl: GPIO37<'_>) -> Level;

    /// Start reading the repeater's console, handing its lines to the loop
    /// ([`crate::console::run`]).
    fn start_console(spawner: Spawner, pins: ConsolePins);

    /// Console input woke the chip from light sleep (its first bytes are lost).
    fn console_woke() -> bool;

    /// The companion's serial link to the app.
    fn app_serial(pins: ConsolePins) -> Serial;

    /// Build the environment-sensor I2C bus and the external GPS receiver for this board variant.
    fn ext_peripherals(oled_bus: Option<SharedI2c<'static>>, pins: ExtPins) -> (Option<SharedI2c<'static>>, Gps);
}
