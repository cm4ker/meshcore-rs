//! The V4's LoRa front-end module (PA/LNA) between the SX1262 and the antenna.
//!
//! FEM LDO enable 7; GPIO2 = FEM chip enable (CSD), and its idle level tells the FEM type:
//! pulled low = GC1109 (V4.2, PA mode on GPIO46), pulled high = KCT8103L (V4.3, CTX on GPIO5).

use embassy_time::Timer;
use esp_hal::gpio::{AnyPin, Flex, InputConfig, Level, Output, OutputConfig, Pull};
use heltec_shared::FrontEnd;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FemType {
    /// Heltec V4.2
    Gc1109,
    /// Heltec V4.3
    Kct8103l,
}

pub struct Fem<'d> {
    pub kind: FemType,
    ldo: Output<'d>,
    csd: Flex<'d>,
    /// GC1109: CPS (high = full PA). KCT8103L: CTX (high = TX / LNA bypass).
    ctl: Output<'d>,
    lna_enabled: bool,
}

impl<'d> Fem<'d> {
    /// Power the FEM, detect its type and put it into RX mode.
    pub async fn init(
        ldo_pin: AnyPin<'d>, csd_pin: AnyPin<'d>, gc1109_cps: AnyPin<'d>, kct_ctx: AnyPin<'d>, lna_enabled: bool,
    ) -> Self {
        let mut ldo = Output::new(ldo_pin, Level::High, OutputConfig::default());
        ldo.set_pad_hold(false); // held low through a power-off
        Timer::after_millis(1).await; // FEM startup after cold power-on

        let mut csd = Flex::new(csd_pin);
        csd.apply_input_config(&InputConfig::default().with_pull(Pull::None));
        csd.set_input_enable(true);
        Timer::after_millis(1).await;
        let kind = if csd.is_high() { FemType::Kct8103l } else { FemType::Gc1109 };
        csd.set_level(Level::High);
        csd.apply_output_config(&OutputConfig::default());
        csd.set_output_enable(true);

        let ctl = match kind {
            FemType::Gc1109 => Output::new(gc1109_cps, Level::Low, OutputConfig::default()),
            FemType::Kct8103l => {
                Output::new(kct_ctx, if lna_enabled { Level::Low } else { Level::High }, OutputConfig::default())
            }
        };
        let mut fem = Self { kind, ldo, csd, ctl, lna_enabled };
        fem.rx_mode();
        fem
    }

    pub fn shutdown(&mut self) {
        self.csd.set_low();
        self.ctl.set_low();
    }
}

impl FrontEnd for Fem<'_> {
    fn name(&self) -> &'static str {
        match self.kind {
            FemType::Gc1109 => "Heltec V4 OLED",
            FemType::Kct8103l => "Heltec V4.3 OLED",
        }
    }

    /// Only the KCT8103L (V4.3) has a switchable LNA.
    fn lna_controllable(&self) -> bool {
        self.kind == FemType::Kct8103l
    }

    fn tx_mode(&mut self) {
        self.csd.set_high();
        self.ctl.set_high();
    }

    fn rx_mode(&mut self) {
        self.csd.set_high();
        match self.kind {
            FemType::Gc1109 => self.ctl.set_low(),
            FemType::Kct8103l => self.ctl.set_level(if self.lna_enabled { Level::Low } else { Level::High }),
        }
    }

    fn set_lna(&mut self, enabled: bool) {
        self.lna_enabled = enabled;
        self.rx_mode();
    }

    /// Unpowered until the next boot: the LDO stays off through deep sleep.
    fn power_off(&mut self) {
        self.shutdown();
        self.ldo.set_low();
        self.ldo.set_pad_hold(true);
    }
}
