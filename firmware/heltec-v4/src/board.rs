//! Heltec WiFi LoRa 32 V4 specifics: LoRa front-end module (PA/LNA), TX LED, battery ADC.
//!
//! Pin map (from upstream `variants/heltec_v4`):
//! - SX1262: NSS 8, SCK 9, MOSI 10, MISO 11, RESET 12, BUSY 13, DIO1 14 (DIO2 = RF switch, DIO3 = TCXO 1.8 V)
//! - FEM LDO enable 7; GPIO2 = FEM chip enable (CSD), and its idle level tells the FEM type:
//!   pulled low = GC1109 (V4.2, PA mode on GPIO46), pulled high = KCT8103L (V4.3, CTX on GPIO5)
//! - TX LED 35, battery divider enable 37, battery sense 1

use embassy_time::Timer;
use esp_hal::gpio::{AnyPin, Flex, InputConfig, Level, Output, OutputConfig, Pull};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FemType {
    /// Heltec V4.2
    Gc1109,
    /// Heltec V4.3
    Kct8103l,
}

pub struct Fem<'d> {
    pub kind: FemType,
    _ldo: Output<'d>,
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
        let ldo = Output::new(ldo_pin, Level::High, OutputConfig::default());
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
        let mut fem = Self { kind, _ldo: ldo, csd, ctl, lna_enabled };
        fem.rx_mode();
        fem
    }

    /// Only the KCT8103L (V4.3) has a switchable LNA.
    pub fn lna_controllable(&self) -> bool {
        self.kind == FemType::Kct8103l
    }

    pub fn tx_mode(&mut self) {
        self.csd.set_high();
        self.ctl.set_high();
    }

    pub fn rx_mode(&mut self) {
        self.csd.set_high();
        match self.kind {
            FemType::Gc1109 => self.ctl.set_low(),
            FemType::Kct8103l => self.ctl.set_level(if self.lna_enabled { Level::Low } else { Level::High }),
        }
    }

    pub fn set_lna(&mut self, enabled: bool) {
        self.lna_enabled = enabled;
        self.rx_mode();
    }

    pub fn shutdown(&mut self) {
        self.csd.set_low();
        self.ctl.set_low();
    }

    pub fn name(&self) -> &'static str {
        match self.kind {
            FemType::Gc1109 => "Heltec V4 OLED",
            FemType::Kct8103l => "Heltec V4.3 OLED",
        }
    }
}

/// Battery voltage divider on the V4 is 390k/100k.
pub const BATTERY_DIVIDER: f32 = 4.9;
