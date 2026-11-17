//! The battery reading of the T3-S3: a 100k/100k divider on GPIO1 and no switch in front of it,
//! so the pin always sees half the battery (upstream reads `2 × analogReadMilliVolts`).

use esp_hal::Blocking;
use esp_hal::analog::adc::{Adc, AdcCalCurve, AdcPin};
use esp_hal::peripherals::{ADC1, GPIO1};

/// 100k over 100k: half the battery voltage reaches the pin.
pub const BATTERY_DIVIDER: f32 = 2.0;

pub struct Battery<'d> {
    adc: Adc<'d, ADC1<'d>, Blocking>,
    pin: AdcPin<GPIO1<'d>, ADC1<'d>, AdcCalCurve<ADC1<'d>>>,
}

impl<'d> Battery<'d> {
    pub fn new(adc: Adc<'d, ADC1<'d>, Blocking>, pin: AdcPin<GPIO1<'d>, ADC1<'d>, AdcCalCurve<ADC1<'d>>>) -> Self {
        Self { adc, pin }
    }

    /// Millivolts at the battery, averaged as upstream does (four reads there, eight here).
    pub async fn read_mv(&mut self) -> u16 {
        const SAMPLES: u32 = 8;

        let mut sum = 0u32;
        for _ in 0..SAMPLES {
            sum += self.adc.read_blocking(&mut self.pin) as u32;
        }

        ((sum / SAMPLES) as f32 * BATTERY_DIVIDER) as u16
    }
}
