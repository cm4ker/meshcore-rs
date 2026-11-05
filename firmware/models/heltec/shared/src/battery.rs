//! The battery reading, the same on the V3 and V4.

use embassy_time::Timer;
use esp_hal::Blocking;
use esp_hal::analog::adc::{Adc, AdcCalCurve, AdcPin};
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::peripherals::{ADC1, GPIO1, GPIO37};

/// Battery voltage divider on the V3 and V4 is 390k/100k.
pub const BATTERY_DIVIDER: f32 = 4.9;

/// Upstream's `ADC_MULTIPLIER` of the V3 and V4, which `get adc.multiplier` shows. Upstream sets
/// it only on the V4; the V3 has the same divider and default, so here it takes one too.
/// Upstream reads the battery as `multiplier × 3.3 V / 1024 × raw`, uncalibrated; this firmware
/// keeps its calibrated reading and scales it by `multiplier / 5.42`, so a value tuned on
/// upstream corrects it by as much.
pub const ADC_MULTIPLIER: Option<f32> = Some(5.42);

/// How much the multiplier in use (`Repeater::adc_multiplier`) scales the battery reading.
pub fn battery_scale(multiplier: Option<f32>) -> f32 {
    match (ADC_MULTIPLIER, multiplier) {
        (Some(own), Some(m)) if m > 0.0 => m / own,
        _ => 1.0,
    }
}

/// Battery voltage through the switched divider on GPIO1, enabled by GPIO37.
pub struct Battery<'d> {
    adc: Adc<'d, ADC1<'d>, Blocking>,
    pin: AdcPin<GPIO1<'d>, ADC1<'d>, AdcCalCurve<ADC1<'d>>>,
    ctrl: Output<'d>,
    /// GPIO37 level that switches the divider on ([`crate::Variant::divider_active`])
    active: Level,
}

impl<'d> Battery<'d> {
    pub fn new(
        adc: Adc<'d, ADC1<'d>, Blocking>, pin: AdcPin<GPIO1<'d>, ADC1<'d>, AdcCalCurve<ADC1<'d>>>, ctrl: GPIO37<'d>,
        active: Level,
    ) -> Self {
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
