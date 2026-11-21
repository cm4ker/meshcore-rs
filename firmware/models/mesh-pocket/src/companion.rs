use alloc::string::String;
use alloc::vec::Vec;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, with_timeout};
use meshcore_fw_core::companion::Board;
use meshcore_ui::{Body, Page, View};
use promicro::companion::ProMicro;

use crate::{board, epaper};

const PAGES: &[Page] = &[
    Page::Home,
    Page::Messages,
    Page::Nearby,
    Page::Radio,
    Page::Bluetooth,
    Page::Pull,
    Page::Advert,
    Page::PowerOff,
];

static POWER_OFF: Signal<CriticalSectionRawMutex, ()> = Signal::new();

#[embassy_executor::task]
pub async fn power_off_task() {
    POWER_OFF.wait().await;
    let _ = with_timeout(Duration::from_secs(10), epaper::OFF_DRAWN.wait()).await;
    board::system_off();
}

pub struct MeshPocket {
    pub base: ProMicro,
}

impl Board for MeshPocket {
    fn manufacturer(&self) -> &str {
        board::NAME
    }

    fn release_board(&self) -> u8 {
        board::RELEASE_BOARD
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

    fn tx_led(&mut self, on: bool) {
        self.base.tx_led(on);
    }

    fn power_off(&mut self) {
        POWER_OFF.signal(());
    }

    async fn battery_mv(&mut self) -> u16 {
        self.base.battery.read_mv(board::BATT_MV_PER_LSB).await
    }

    fn adc_multiplier(&self) -> Option<f32> {
        Some(board::BATT_MV_PER_LSB)
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
        Board::boot_log(&self.base)
    }

    fn button_down(&mut self) -> bool {
        self.base.button_down()
    }

    fn has_screen(&self) -> bool {
        true
    }

    fn always_on(&self) -> bool {
        true
    }

    fn ui_pages(&self) -> &'static [Page] {
        PAGES
    }

    fn show_view(&mut self, view: &View) {
        if matches!(&view.body, Body::Off { .. }) {
            epaper::OFF_DRAWN.reset();
        }
        epaper::SCREEN.signal(Some(view.clone()));
    }

    fn screen_off(&mut self) {
        epaper::SCREEN.signal(None);
    }
}
