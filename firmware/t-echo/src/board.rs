//! The LilyGO T-Echo around the radio: LEDs, the user button, the battery, power, the screen.

use embassy_nrf::gpio::{Input, Output};
use embassy_nrf::saadc::Saadc;
use meshcore_fw_core::companion::{Board, Status};

use crate::epaper::{SCREEN, Snapshot};

/// 12-bit samples of a 0..3 V range, behind a 1:2 divider (upstream `REAL_VBAT_MV_PER_LSB`).
const BATT_MV_PER_LSB: f32 = 3000.0 / 4096.0 * 2.0;

pub struct TEcho {
    /// Green LED, lit while transmitting (active low).
    pub tx_led: Output<'static>,
    /// The button by the screen, low while pressed.
    pub button: Input<'static>,
    pub battery: Saadc<'static, 1>,
    /// 3.3 V to the peripherals (radio, flash, screen, sensors).
    pub power: Output<'static>,
}

impl Board for TEcho {
    fn manufacturer(&self) -> &str {
        "LilyGo T-Echo"
    }

    fn max_tx_power_dbm(&self) -> i8 {
        22
    }

    fn on_tx_start(&mut self) {
        self.tx_led.set_low();
    }

    fn on_tx_end(&mut self) {
        self.tx_led.set_high();
    }

    fn power_off(&mut self) {
        self.tx_led.set_high();
        self.power.set_low();
    }

    async fn battery_mv(&mut self) -> u16 {
        let mut sample = [0i16; 1];
        self.battery.sample(&mut sample).await;
        (sample[0].max(0) as f32 * BATT_MV_PER_LSB) as u16
    }

    fn button_down(&mut self) -> bool {
        self.button.is_low()
    }

    fn show_status(&mut self, status: &Status<'_>) {
        SCREEN.signal(Snapshot::of(status));
    }

    fn always_on(&self) -> bool {
        true
    }
}
