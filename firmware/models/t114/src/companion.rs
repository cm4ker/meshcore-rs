//! The T114 under the companion loop (`meshcore_fw_core::companion`): the Pro Micro's board
//! (`promicro::companion::ProMicro`) with the screen, turned with the button, and the GPS.

use alloc::string::String;
use alloc::vec::Vec;

use meshcore_fw_core::companion::Board;
use meshcore_ui::{Page, View};
use promicro::companion::ProMicro;

use crate::board::BATT_MV_PER_LSB;
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

pub struct T114 {
    /// The LED, the button, the battery, the thermometer, the sensors: as on the Pro Micro.
    pub base: ProMicro,
}

impl Board for T114 {
    fn manufacturer(&self) -> &str {
        "Heltec T114"
    }

    fn release_board(&self) -> u8 {
        meshcore::release::board::HELTEC_T114
    }

    fn max_tx_power_dbm(&self) -> i8 {
        22
    }

    fn on_tx_start(&mut self) {
        self.base.on_tx_start();
    }

    fn on_tx_end(&mut self) {
        self.base.on_tx_end();
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
        self.base.boot_log()
    }

    fn button_down(&mut self) -> bool {
        self.base.button_down()
    }

    /// A random BLE pairing PIN, shown on the screen, as upstream's T114 with a display.
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
