//! Heltec Mesh Node T096 v0.2 wiring, as in Heltec's schematic and MeshCore's heltec_t096 variant.

use embassy_nrf::pac;
use promicro::Spec;
use promicro::ota::{COMPANION_REGION, TRIAL_REGION};

pub const BATT_MV_PER_LSB: f32 = 3600.0 / 4096.0 * 4.9;
pub const NAME: &str = "Heltec T096";
pub const RELEASE_BOARD: u8 = meshcore::release::board::HELTEC_T096;

const BUTTON_PIN: usize = 10; // P1.10, active low
const _: () = assert!(COMPANION_REGION.1 >= meshcore_fw_core::storage::MIN_COMPANION_SIZE);

pub const fn spec(build_epoch: &'static str, build_date: &'static str) -> Spec {
    Spec {
        name: NAME,
        manufacturer: NAME,
        release_board: RELEASE_BOARD,
        usb: (0x239A, 0x8029, "HT-n5262G"),
        batt_mv_per_lsb: BATT_MV_PER_LSB,
        system_off,
        updates: Some(&promicro::ota::IN_PLACE),
        companion_regions: [TRIAL_REGION, COMPANION_REGION],
        // SX1262 output before the KCT8103L amplifier, as on stock MeshCore.
        companion_tx_dbm: 9,
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

        let mut tcfg = spim::Config::default();
        tcfg.frequency = spim::Frequency::M8;
        let screen = $crate::tft::Tft {
            spi: spim::Spim::new_txonly($p.SPI2, $irqs, $p.P0_20, $p.P0_17, tcfg),
            cs: Output::new($p.P0_22, Level::Low, OutputDrive::Standard),
            dc: Output::new($p.P0_15, Level::Low, OutputDrive::Standard),
            reset: Output::new($p.P0_13, Level::Low, OutputDrive::Standard),
            light: Output::new($p.P1_12, Level::High, OutputDrive::Standard),
        };
        // VEXT is shared by TFT and GNSS. Its output stays claimed for both tasks.
        core::mem::forget(Output::new($p.P0_26, Level::Low, OutputDrive::Standard));
        let gps = $crate::gps::Gps {
            uarte: $p.UARTE0,
            rx: $p.P0_23,
            tx: $p.P0_25,
            reset: Output::new($p.P1_14, Level::Low, OutputDrive::Standard),
            enable: Output::new($p.P0_06, Level::High, OutputDrive::Standard),
        };

        let mut scfg = spim::Config::default();
        scfg.frequency = spim::Frequency::M2;
        let spi = spim::Spim::new($p.TWISPI1, $irqs, $p.P1_08, $p.P0_14, $p.P0_11, scfg);
        let radio = meshcore_fw_core::sx1262::Sx1262::new(
            spi,
            Output::new($p.P0_05, Level::High, OutputDrive::Standard),
            Output::new($p.P0_16, Level::High, OutputDrive::Standard),
            Input::new($p.P0_19, Pull::None),
            meshcore_fw_core::radio::WaitIrq(Input::new($p.P0_21, Pull::Down)),
        )
        .without_rx_patch();

        let fem_power = Output::new($p.P0_30, Level::High, OutputDrive::Standard);
        embassy_time::block_for(embassy_time::Duration::from_millis(1));
        let fem_ctx = Output::new($p.P1_09, Level::High, OutputDrive::Standard);
        let fem_csd = Output::new($p.P0_12, Level::High, OutputDrive::Standard);
        let mut hold = heapless::Vec::new();
        hold.push(fem_power).ok();
        hold.push(fem_csd).ok();

        let mut icfg = twim::Config::default();
        icfg.sda_pullup = true;
        icfg.scl_pullup = true;
        static TWIM_BUF: static_cell::StaticCell<[u8; 8]> = static_cell::StaticCell::new();
        let parts = promicro::board::Parts {
            radio,
            led: promicro::board::Led::new(Output::new($p.P0_28, Level::Low, OutputDrive::Standard), false),
            rxen: None,
            button: Input::new($p.P1_10, Pull::Up),
            battery: promicro::board::Battery {
                adc: saadc::Saadc::new(
                    $p.SAADC,
                    $irqs,
                    saadc::Config::default(),
                    [saadc::ChannelConfig::single_ended($p.P0_03)],
                ),
                enable: Some(Output::new($p.P1_15, Level::Low, OutputDrive::Standard)),
            },
            // The connector's I²C bus; the internal bus on P0.07/P0.08 is separate.
            i2c: twim::Twim::new($p.TWISPI0, $irqs, $p.P0_04, $p.P0_27, icfg, TWIM_BUF.init([0; 8])),
            hold,
        };
        (parts, screen, gps, fem_ctx)
    }};
}

pub mod vext {
    use core::cell::Cell;
    use embassy_nrf::pac;
    use embassy_sync::blocking_mutex::CriticalSectionMutex;

    pub const SCREEN: u8 = 1;
    pub const GPS: u8 = 2;
    static WANTED: CriticalSectionMutex<Cell<u8>> = CriticalSectionMutex::new(Cell::new(0));

    pub fn want(who: u8, on: bool) {
        WANTED.lock(|bits| {
            let old = bits.get();
            let wanted = if on { old | who } else { old & !who };
            bits.set(wanted);
            if wanted != 0 {
                pac::P0.outset().write(|w| w.set_pin(26, true));
            } else {
                pac::P0.outclr().write(|w| w.set_pin(26, true));
            }
        });
    }
}

pub fn system_off() -> ! {
    promicro::board::system_off_with(BUTTON_PIN, || {
        pac::P0.outclr().write(|w| {
            w.set_pin(12, true); // FEM shutdown
            w.set_pin(13, true); // TFT reset
            w.set_pin(15, true); // TFT DC
            w.set_pin(22, true); // TFT CS
            w.set_pin(26, true); // VEXT
            w.set_pin(28, true); // TX LED
            w.set_pin(30, true); // FEM supply
        });
        // Disconnect peripheral pins that could source current into the unpowered TFT or GNSS.
        // In particular, UARTE's TX may still be driven high when the GNSS task was active.
        use pac::gpio::vals::Input;
        for pin in [6, 13, 15, 17, 20, 22, 23, 25] {
            pac::P0.pin_cnf(pin).write(|w| w.set_input(Input::Disconnect));
        }
        for pin in [12, 14] {
            pac::P1.pin_cnf(pin).write(|w| w.set_input(Input::Disconnect));
        }
        pac::P1.outclr().write(|w| {
            w.set_pin(9, true); // FEM CTX
            w.set_pin(14, true); // GNSS reset
            w.set_pin(15, true); // Battery divider
        });
    })
}
