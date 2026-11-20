//! The T096 under the companion loop (`meshcore_fw_core::companion`): the Pro Micro's board
//! (`promicro::companion::ProMicro`) with the screen, turned with the button, and the GPS.

use alloc::string::String;
use alloc::vec::Vec;

use embassy_nrf::gpio::Output;
use meshcore_fw_core::companion::Board;
use meshcore_ui::{Page, View};
use promicro::companion::ProMicro;

use crate::board::{BATT_MV_PER_LSB, NAME, RELEASE_BOARD};
use crate::{gps, tft};

/// The interface's pages, the home page first.
const PAGES: &[Page] = &[
    Page::Home,
    Page::Messages,
    Page::Nearby,
    Page::Radio,
    Page::Bluetooth,
    Page::Gps,
    Page::Pull,
    Page::Advert,
    Page::PowerOff,
];

pub struct T096 {
    /// The LED, the button, the battery, the thermometer, the sensors: as on the Pro Micro.
    base: ProMicro,
    fem_ctx: Output<'static>,
    lna_enabled: bool,
}

impl T096 {
    pub fn new(fem_ctx: Output<'static>, base: ProMicro) -> Self {
        Self {
            base,
            fem_ctx,
            lna_enabled: false,
        }
    }
}

impl Board for T096 {
    fn manufacturer(&self) -> &str {
        NAME
    }

    fn release_board(&self) -> u8 {
        RELEASE_BOARD
    }

    fn max_tx_power_dbm(&self) -> i8 {
        22
    }

    fn on_tx_start(&mut self) {
        // With CTX high, SX1262 DIO2 selects TX or RX bypass.
        self.fem_ctx.set_high();
        self.base.on_tx_start();
    }

    fn on_tx_end(&mut self) {
        self.base.on_tx_end();
        self.set_lna(self.lna_enabled);
    }

    fn lna_controllable(&self) -> bool {
        true
    }

    fn set_lna(&mut self, on: bool) {
        self.lna_enabled = on;
        self.fem_ctx.set_level((!on).into());
    }

    fn power_off(&mut self) {
        crate::board::system_off();
    }

    async fn battery_mv(&mut self) -> u16 {
        self.base.battery.read_mv(BATT_MV_PER_LSB).await
    }

    fn adc_multiplier(&self) -> Option<f32> {
        Some(BATT_MV_PER_LSB)
    }

    async fn mcu_temp_c(&mut self) -> Option<f32> {
        self.base.mcu_temp_c().await
    }

    async fn sensor_lpp(&mut self) -> Vec<u8> {
        self.base.sensor_lpp().await
    }

    fn sensor_scan(&self) -> String {
        self.base.sensor_scan()
    }

    fn boot_log(&self) -> String {
        meshcore_fw_core::companion::Board::boot_log(&self.base)
    }

    fn button_down(&mut self) -> bool {
        self.base.button_down()
    }

    /// A random BLE pairing PIN, shown on the screen, as upstream's T096 with a display.
    fn has_screen(&self) -> bool {
        true
    }

    fn ui_pages(&self) -> &'static [Page] {
        PAGES
    }

    fn show_view(&mut self, view: &View) {
        tft::SCREEN.signal(Some(view.clone()));
    }

    fn screen_off(&mut self) {
        tft::SCREEN.signal(None);
    }

    fn has_gps(&self) -> bool {
        true
    }

    fn set_gps(&mut self, on: bool) {
        gps::GPS_ON.signal(on);
    }
}
