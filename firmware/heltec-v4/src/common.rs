//! Heltec V4 pieces both firmware roles (repeater, companion) share.

pub use meshcore_fw::clock_cell;
pub use meshcore_fw::noise::NoiseFloor;

use embassy_time::Timer;
use esp_hal::Blocking;
use esp_hal::analog::adc::{Adc, AdcCalCurve, AdcPin};
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::peripherals::{ADC1, GPIO1, GPIO37};

use crate::board::BATTERY_DIVIDER;

/// Battery voltage through the switched divider on GPIO1, enabled by GPIO37.
pub struct Battery<'d> {
    adc: Adc<'d, ADC1<'d>, Blocking>,
    pin: AdcPin<GPIO1<'d>, ADC1<'d>, AdcCalCurve<ADC1<'d>>>,
    ctrl: Output<'d>,
    /// GPIO37 level that switches the divider on
    active: Level,
}

impl<'d> Battery<'d> {
    #[allow(unused_mut)]
    pub fn new(
        adc: Adc<'d, ADC1<'d>, Blocking>, pin: AdcPin<GPIO1<'d>, ADC1<'d>, AdcCalCurve<ADC1<'d>>>, mut ctrl: GPIO37<'d>,
    ) -> Self {
        #[cfg(not(feature = "heltec-v3"))]
        let active = Level::High;
        // The V3's divider switch flipped polarity with V3.2; its pull resistor tells which
        // (upstream HeltecV3Board: active = !digitalRead(PIN_ADC_CTRL)).
        #[cfg(feature = "heltec-v3")]
        let active = {
            use esp_hal::gpio::{Input, InputConfig, Pull};
            let idle = Input::new(ctrl.reborrow(), InputConfig::default().with_pull(Pull::None)).level();
            !idle
        };
        let ctrl = Output::new(ctrl, !active, OutputConfig::default());
        Self { adc, pin, ctrl, active }
    }

    pub async fn read_mv(&mut self) -> u16 {
        self.ctrl.set_level(self.active);
        Timer::after_millis(10).await;
        let mut sum = 0u32;
        for _ in 0..8 {
            sum += self.adc.read_blocking(&mut self.pin) as u32;
        }
        self.ctrl.set_level(!self.active);
        ((sum / 8) as f32 * BATTERY_DIVIDER) as u16
    }
}
