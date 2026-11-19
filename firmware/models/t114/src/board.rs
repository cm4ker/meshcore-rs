//! The Heltec Mesh Node T114 around the radio, as upstream's `variants/heltec_t114` and
//! Meshtastic's `heltec_mesh_node_t114` have it: SX1262 on SPI (SCK P0.19, MOSI P0.22, MISO
//! P0.23, NSS P0.24), RESET P0.25, DIO1 P0.20, BUSY P0.17, DIO2 switching the antenna, a 1.8 V
//! TCXO on DIO3; the button P1.10 (low while pressed), the LED P1.03 (active low), the battery on
//! P0.04 (AIN2) behind a divider that P0.06 switches on; I2C on SDA P0.26, SCL P0.27; the ST7789
//! screen on SPIM2 ([`crate::tft`]); the GPS socket on UARTE0 ([`crate::gps`]); VEXT on P0.21
//! powering the screen, the GPS and the I2C connector ([`vext`]). The types, the clock and the rest are the Pro Micro's
//! (`promicro::board`): the same chip and bootloader.

use embassy_nrf::pac;
use promicro::Spec;

/// Upstream's divider of 4.9 over our 0..3.6 V range (upstream reads 0..3.0 V).
pub const BATT_MV_PER_LSB: f32 = 3600.0 / 4096.0 * 4.9;

/// The button as a pin number in P1.
const BUTTON_PIN: usize = 10;

/// The board as the repeater and the companion name it; `BUILD_EPOCH` and `BUILD_DATE` are the
/// binary's.
pub const fn spec(build_epoch: &'static str, build_date: &'static str) -> Spec {
    Spec {
        name: "Heltec T114",
        manufacturer: "Heltec T114",
        release_board: meshcore::release::board::HELTEC_T114,
        // upstream's IDs and product name for the board: the host keeps its COM port
        usb: (0x239A, 0x8029, "HT-n5262"),
        batt_mv_per_lsb: BATT_MV_PER_LSB,
        system_off,
        updates: Some(&promicro::ota::IN_PLACE),
        companion_regions: promicro::COMPANION_REGIONS,
        companion_tx_dbm: 22,
        companion_before: None,
        build_epoch,
        build_date,
    }
}

/// The T114's parts: the Pro Micro's set (`promicro::board::Parts`), the screen and the GPS,
/// moved out of `embassy_nrf::init`'s peripherals, with the interrupts bound to `$irqs` (TWISPI1
/// for the radio, SPI2 for the screen, the SAADC, TWISPI0 for the I2C). The screen is dark and
/// unpowered, the GPS too, until their tasks turn them on.
#[macro_export]
macro_rules! board_parts {
    ($p:ident, $irqs:expr) => {{
        use embassy_nrf::gpio::{Input, Level, Output, OutputDrive, Pull};
        use embassy_nrf::{saadc, spim, twim};
        // SPIM2 for the screen, not SPIM3 (the only one at 32 MHz): SPIM3 may send corrupted data
        // while the CPU uses the RAM its buffer is in (nRF52840 errata 198)
        let mut tcfg = spim::Config::default();
        tcfg.frequency = spim::Frequency::M8;
        // the screen dark and unpowered, as upstream's ST7789Display::turnOff
        let screen = $crate::tft::Tft {
            spi: spim::Spim::new_txonly($p.SPI2, $irqs, $p.P1_08, $p.P1_09, tcfg),
            cs: Output::new($p.P0_11, Level::Low, OutputDrive::Standard),
            dc: Output::new($p.P0_12, Level::Low, OutputDrive::Standard),
            reset: Output::new($p.P0_02, Level::Low, OutputDrive::Standard),
            power: Output::new($p.P0_03, Level::High, OutputDrive::Standard),
            light: Output::new($p.P0_15, Level::High, OutputDrive::Standard),
        };
        // VEXT off until the screen or the GPS wants it (`vext`): an output for good
        core::mem::forget(Output::new($p.P0_21, Level::Low, OutputDrive::Standard));
        let gps = $crate::gps::Gps {
            uarte: $p.UARTE0,
            rx: $p.P1_05,
            tx: $p.P1_07,
            reset: Output::new($p.P1_06, Level::Low, OutputDrive::Standard),
            wake: Output::new($p.P1_02, Level::Low, OutputDrive::Standard),
        };
        // upstream's speed (RadioLib's 2 MHz): some modules and boards do not take 8
        let mut scfg = spim::Config::default();
        scfg.frequency = spim::Frequency::M2;
        let spi = spim::Spim::new($p.TWISPI1, $irqs, $p.P0_19, $p.P0_23, $p.P0_22, scfg);
        let radio = meshcore_fw_core::sx1262::Sx1262::new(
            spi,
            Output::new($p.P0_24, Level::High, OutputDrive::Standard),
            Output::new($p.P0_25, Level::High, OutputDrive::Standard),
            Input::new($p.P0_17, Pull::None),
            meshcore_fw_core::radio::WaitIrq(Input::new($p.P0_20, Pull::Down)),
        )
        .without_rx_patch();
        let mut icfg = twim::Config::default();
        icfg.sda_pullup = true;
        icfg.scl_pullup = true;
        static TWIM_BUF: static_cell::StaticCell<[u8; 8]> = static_cell::StaticCell::new();
        let parts = promicro::board::Parts {
            radio,
            led: promicro::board::Led::new(Output::new($p.P1_03, Level::High, OutputDrive::Standard), true),
            rxen: None,
            button: Input::new($p.P1_10, Pull::Up),
            battery: promicro::board::Battery {
                adc: saadc::Saadc::new($p.SAADC, $irqs, saadc::Config::default(), [saadc::ChannelConfig::single_ended($p.P0_04)]),
                enable: Some(Output::new($p.P0_06, Level::Low, OutputDrive::Standard)),
            },
            i2c: twim::Twim::new($p.TWISPI0, $irqs, $p.P0_26, $p.P0_27, icfg, TWIM_BUF.init([0; 8])),
            hold: heapless::Vec::new(),
        };
        (parts, screen, gps)
    }};
}

/// VEXT (P0.21): the supply of the screen (its light at least: dark without it), the GPS and the
/// I2C connector. Meshtastic keeps it on; here it is on while the screen or the GPS is.
pub mod vext {
    use core::sync::atomic::{AtomicU8, Ordering};

    use embassy_nrf::pac;

    pub const SCREEN: u8 = 1;
    pub const GPS: u8 = 2;

    static WANTED: AtomicU8 = AtomicU8::new(0);

    /// `who` wants VEXT on, or no longer.
    pub fn want(who: u8, on: bool) {
        let wanted = if on { WANTED.fetch_or(who, Ordering::Relaxed) | who } else { WANTED.fetch_and(!who, Ordering::Relaxed) & !who };
        if wanted != 0 {
            pac::P0.outset().write(|w| w.set_pin(21, true));
        } else {
            pac::P0.outclr().write(|w| w.set_pin(21, true));
        }
    }
}

/// The chip's System OFF, which only the button ends: the LED off, the screen dark and
/// unpowered, the GPS too.
pub fn system_off() -> ! {
    promicro::board::system_off_with(BUTTON_PIN, || {
        pac::P1.outset().write(|w| w.set_pin(3, true)); // LED
        pac::P0.outset().write(|w| {
            w.set_pin(3, true); // screen supply
            w.set_pin(15, true); // screen light
        });
        pac::P0.outclr().write(|w| w.set_pin(21, true)); // GPS supply
    })
}
