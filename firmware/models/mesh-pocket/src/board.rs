//! Wiring from the MeshCore `variants/mesh_pocket` board and Heltec's schematic.

use embassy_nrf::pac;
use promicro::Spec;
use promicro::ota::{COMPANION_REGION, TRIAL_REGION};

pub const NAME: &str = "Heltec MeshPocket";
pub const RELEASE_BOARD: u8 = meshcore::release::board::HELTEC_MESH_POCKET;
pub const BATT_MV_PER_LSB: f32 = 3600.0 / 4096.0 * 4.9;

const BUTTON_PIN: usize = 10; // P1.10, active low
const _: () = assert!(COMPANION_REGION.1 >= meshcore_fw_core::storage::MIN_COMPANION_SIZE);

pub const fn spec(build_epoch: &'static str, build_date: &'static str) -> Spec {
    Spec {
        name: NAME,
        manufacturer: NAME,
        release_board: RELEASE_BOARD,
        usb: (0x239A, 0x4405, "HT-n5262"),
        batt_mv_per_lsb: BATT_MV_PER_LSB,
        system_off,
        updates: Some(&promicro::ota::IN_PLACE),
        companion_regions: [TRIAL_REGION, COMPANION_REGION],
        companion_tx_dbm: 22,
        companion_before: None,
        build_epoch,
        build_date,
    }
}

#[macro_export]
macro_rules! board_parts {
    ($p:ident, $irqs:expr) => {{
        use embassy_nrf::gpio::{Input, Level, Output, OutputDrive, Pull};
        use embassy_nrf::{saadc, spim, twim};

        let mut radio_cfg = spim::Config::default();
        radio_cfg.frequency = spim::Frequency::M2;
        let spi = spim::Spim::new($p.TWISPI1, $irqs, $p.P0_04, $p.P1_09, $p.P0_05, radio_cfg);
        // The shared SX1262 driver enables the DIO2 RF switch and the 1.8 V DIO3 TCXO.
        let radio = meshcore_fw_core::sx1262::Sx1262::new(
            spi,
            Output::new($p.P0_26, Level::High, OutputDrive::Standard),
            Output::new($p.P0_12, Level::High, OutputDrive::Standard),
            Input::new($p.P0_15, Pull::None),
            meshcore_fw_core::radio::WaitIrq(Input::new($p.P0_16, Pull::Down)),
        )
        .without_rx_patch();

        let mut screen_cfg = spim::Config::default();
        screen_cfg.frequency = spim::Frequency::M4;
        let screen = $crate::epaper::Epaper {
            spi: spim::Spim::new_txonly($p.SPI2, $irqs, $p.P0_22, $p.P0_20, screen_cfg),
            cs: Output::new($p.P0_24, Level::High, OutputDrive::Standard),
            dc: Output::new($p.P0_31, Level::Low, OutputDrive::Standard),
            reset: Output::new($p.P1_04, Level::High, OutputDrive::Standard),
            busy: Input::new($p.P1_06, Pull::None),
        };

        let mut i2c_cfg = twim::Config::default();
        i2c_cfg.sda_pullup = true;
        i2c_cfg.scl_pullup = true;
        static I2C_BUF: static_cell::StaticCell<[u8; 8]> = static_cell::StaticCell::new();
        let i2c = twim::Twim::new(
            $p.TWISPI0,
            $irqs,
            $p.P1_15,
            $p.P1_13,
            i2c_cfg,
            I2C_BUF.init([0; 8]),
        );
        let parts = promicro::board::Parts {
            radio,
            led: promicro::board::Led::new(
                Output::new($p.P0_13, Level::High, OutputDrive::Standard),
                true,
            ),
            rxen: None,
            button: Input::new($p.P1_10, Pull::Up),
            battery: promicro::board::Battery {
                adc: saadc::Saadc::new(
                    $p.SAADC,
                    $irqs,
                    saadc::Config::default(),
                    [saadc::ChannelConfig::single_ended($p.P0_29)],
                ),
                enable: Some(Output::new($p.P1_02, Level::Low, OutputDrive::Standard)),
            },
            i2c: promicro::i2c::Bus::new(i2c),
            hold: heapless::Vec::new(),
        };
        (parts, screen)
    }};
}

pub fn system_off() -> ! {
    promicro::board::system_off_with(BUTTON_PIN, || {
        pac::P0.outset().write(|w| w.set_pin(13, true)); // LED off
        pac::P1.outclr().write(|w| w.set_pin(2, true)); // battery divider off
    })
}
