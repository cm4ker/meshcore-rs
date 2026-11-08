//! The battery: a divider about 1:1 on GPIO20 (ADC2), switched on by GPIO19 low, as Meshtastic
//! has it for this board (upstream's `ADC_MULTIPLIER` of 8.4 reads several times the battery).

use embassy_time::Timer;
use esp_hal::Blocking;
use esp_hal::analog::adc::{Adc, AdcCalCurve, AdcConfig, AdcPin, Attenuation};
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::peripherals::{ADC2, GPIO19, GPIO20};

/// Over the calibrated ADC; `get adc.multiplier` shows it.
pub const ADC_MULTIPLIER: f32 = 2.0;

pub struct Battery {
    adc: Adc<'static, ADC2<'static>, Blocking>,
    pin: AdcPin<GPIO20<'static>, ADC2<'static>, AdcCalCurve<ADC2<'static>>>,
    ctrl: Output<'static>,
}

impl Battery {
    pub fn new(adc: ADC2<'static>, sense: GPIO20<'static>, ctrl: GPIO19<'static>) -> Self {
        let mut cfg = AdcConfig::new();
        let pin = cfg.enable_pin_with_cal::<_, AdcCalCurve<ADC2>>(sense, Attenuation::_11dB);
        Self { adc: Adc::new(adc, cfg), pin, ctrl: Output::new(ctrl, Level::High, OutputConfig::default()) }
    }

    /// Millivolts with `multiplier` (the board's own if none).
    pub async fn read_mv(&mut self, multiplier: Option<f32>) -> u16 {
        self.ctrl.set_low();
        Timer::after_millis(10).await;
        let mut sum = 0u32;
        for _ in 0..8 {
            sum += self.adc.read_blocking(&mut self.pin) as u32;
        }
        self.ctrl.set_high();
        ((sum / 8) as f32 * multiplier.filter(|m| *m > 0.0).unwrap_or(ADC_MULTIPLIER)) as u16
    }
}
