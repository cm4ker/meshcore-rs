//! Heltec V4 pieces both firmware roles (repeater, companion) share.

pub use meshcore_fw::clock_cell;
pub use meshcore_fw::noise::NoiseFloor;

use embassy_time::Timer;
use esp_hal::Blocking;
use esp_hal::analog::adc::{Adc, AdcCalCurve, AdcPin};
use esp_hal::gpio::Output;
use esp_hal::peripherals::{ADC1, GPIO1};

use crate::board::BATTERY_DIVIDER;

/// Battery voltage through the switched divider on GPIO1.
pub struct Battery<'d> {
    pub adc: Adc<'d, ADC1<'d>, Blocking>,
    pub pin: AdcPin<GPIO1<'d>, ADC1<'d>, AdcCalCurve<ADC1<'d>>>,
    pub ctrl: Output<'d>,
}

impl Battery<'_> {
    pub async fn read_mv(&mut self) -> u16 {
        self.ctrl.set_high();
        Timer::after_millis(10).await;
        let mut sum = 0u32;
        for _ in 0..8 {
            sum += self.adc.read_blocking(&mut self.pin) as u32;
        }
        self.ctrl.set_low();
        ((sum / 8) as f32 * BATTERY_DIVIDER) as u16
    }
}
