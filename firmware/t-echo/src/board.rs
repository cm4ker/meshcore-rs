//! The LilyGO T-Echo around the radio: LEDs, the user button, the battery, power, the screen and
//! its light, the GPS.

use embassy_nrf::gpio::{Input, Level, Output};
use embassy_nrf::pac;
use embassy_nrf::saadc::Saadc;
use embassy_time::{Duration, Instant};
use meshcore_fw_core::companion::Board;
use meshcore_ui::{Page, View};

use crate::epaper::SCREEN;
use crate::gps::GPS_ON;

/// 12-bit samples of a 0..3 V range, behind a 1:2 divider (upstream `REAL_VBAT_MV_PER_LSB`).
const BATT_MV_PER_LSB: f32 = 3000.0 / 4096.0 * 2.0;

const PAGES: &[Page] =
    &[Page::Home, Page::Messages, Page::Nearby, Page::Radio, Page::Bluetooth, Page::Gps, Page::Advert, Page::PowerOff];

/// The button (P1.10) as a pin number in P1.
const BUTTON_PIN: usize = 10;

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

    fn release_board(&self) -> u8 {
        meshcore::release::board::T_ECHO
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

    /// The screen task finishes the job: it draws the "off" page, then calls [`system_off`]
    /// (the peripherals' supply has to stay on until the page is drawn).
    fn power_off(&mut self) {
        self.tx_led.set_high();
        GPS_ON.signal(false);
    }

    async fn battery_mv(&mut self) -> u16 {
        let mut sample = [0i16; 1];
        self.battery.sample(&mut sample).await;
        (sample[0].max(0) as f32 * BATT_MV_PER_LSB) as u16
    }

    fn button_down(&mut self) -> bool {
        self.button.is_low()
    }

    fn always_on(&self) -> bool {
        true
    }

    fn ui_pages(&self) -> &'static [Page] {
        PAGES
    }

    fn show_view(&mut self, view: &View) {
        SCREEN.signal(view.clone());
    }

    fn has_gps(&self) -> bool {
        true
    }

    fn set_gps(&mut self, on: bool) {
        GPS_ON.signal(on);
    }
}

/// Lights the screen while the touch pad above it is touched (it reads low then), as upstream.
#[embassy_executor::task]
pub async fn backlight_task(mut touch: Input<'static>, mut light: Output<'static>) {
    loop {
        light.set_level(if touch.is_low() { Level::High } else { Level::Low });
        touch.wait_for_any_edge().await;
    }
}

/// Everything off, then the chip's System OFF, which only the button ends (with a reset, so the
/// firmware starts over).
pub fn system_off() -> ! {
    // a pin already at its sense level would wake the chip at once: wait for the button to be let go
    let since = Instant::now();
    while !pac::P1.in_().read().pin(BUTTON_PIN) && since.elapsed() < Duration::from_secs(10) {}
    pac::P0.outset().write(|w| {
        for led in [13, 14, 15] {
            w.set_pin(led, true); // LEDs are active low
        }
    });
    pac::P0.outclr().write(|w| {
        w.set_pin(12, true); // peripherals' 3.3 V
        w.set_pin(21, true); // TCXO
    });
    pac::P1.outclr().write(|w| {
        w.set_pin(2, true); // GPS enable
        w.set_pin(5, true); // GPS reset
        w.set_pin(11, true); // screen light
    });
    for gps_uart in [8, 9] {
        pac::P1.pin_cnf(gps_uart).write(|_| {}); // disconnected: nothing to feed the unpowered GPS
    }
    pac::P1.pin_cnf(BUTTON_PIN).modify(|w| w.set_sense(pac::gpio::vals::Sense::Low));
    embassy_nrf::power::set_system_off();
    loop {
        cortex_m::asm::wfe();
    }
}
