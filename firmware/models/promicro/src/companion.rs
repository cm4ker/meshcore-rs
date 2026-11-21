//! The Pro Micro under the companion loop (`meshcore_fw_core::companion`): the nRF52840 with the
//! SoftDevice running for BLE ([`Nrf`]), and the board around the radio ([`ProMicro`]). Its data
//! takes the staging area (`COMPANION_REGION`) and is moved aside while an update is there.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use embassy_executor::Spawner;
use embassy_nrf::gpio::{Input, Output};
use embassy_nrf::temp::Temp;
use embassy_nrf::wdt::WatchdogHandle;
use meshcore::companion::CompanionPrefs;
use meshcore::ota::FirmwareTarget;
use meshcore_fw_core::companion::{BleSetup, Board, Links, Platform};
use meshcore_fw_core::sensors::Sensors;
use meshcore_fw_core::storage::SharedStore;
use meshcore_ui::{Page, View};

use crate::board::{self, Battery, Led};
use crate::flash::Flash;
use crate::i2c::Bus;
use crate::usb_frames::{USB_HOST, USB_OUT};
use crate::{ble, buzzer, ota, sd};

/// The chip: the SoftDevice runs from the start (BLE), the USB port carries the boot log, then
/// the app's frames.
pub struct Nrf {
    pub watchdog: Option<WatchdogHandle>,
    /// State of a small generator seeded from the hardware RNG (which the SoftDevice owns).
    pub seed: u64,
    /// The store, whose companion region an update borrows (`ota::StagingTarget::lending`).
    pub store: SharedStore<Flash>,
    /// A fresh update runs on trial (`ota::check_trial`): the region holds the firmware to go
    /// back to until it is kept.
    pub in_trial: bool,
    /// The data could not be put back in the region: it stays aside until the next start.
    pub stays_aside: bool,
    pub updates: Option<&'static dyn ota::Updates>,
}

impl Platform for Nrf {
    type Flash = Flash;

    fn feed_watchdog(&mut self) {
        if let Some(w) = &mut self.watchdog {
            w.pet();
        }
    }

    fn reboot(&mut self) -> ! {
        if let Some(updates) = self.updates.filter(|u| u.install_pending()) {
            updates.hand_over();
        }
        cortex_m::peripheral::SCB::sys_reset()
    }

    fn ota_target(&mut self) -> Option<Box<dyn FirmwareTarget>> {
        self.updates?.target(self.store.clone(), true)
    }

    fn in_trial(&self) -> bool {
        self.in_trial
    }

    fn confirm_trial(&mut self) {
        if let Some(updates) = self.updates {
            updates.confirm(&self.store);
            self.in_trial = false;
            log::info!("OTA: new firmware confirmed");
        }
    }

    fn other_slot(&mut self) -> (bool, String, u32) {
        let (kept, prev, skip, _) = self.updates.map(|u| u.previous(&self.store)).unwrap_or_default();
        (kept, prev, skip)
    }

    fn rollback(&mut self) -> bool {
        self.updates.is_some_and(|u| u.rollback(&self.store))
    }

    fn poll_slot(&mut self, update: bool) -> bool {
        let mut store = self.store.borrow_mut();
        if !store.lent() || update || self.in_trial || self.stays_aside || self.updates.is_some_and(|u| u.install_pending()) {
            return false;
        }
        self.stays_aside = !store.take_back();
        !self.stays_aside
    }

    fn random(&mut self) -> u32 {
        // splitmix64
        self.seed = self.seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (z ^ (z >> 31)) as u32
    }

    fn ram_clock(&self) -> Option<u32> {
        board::clock_load()
    }

    fn set_ram_clock(&mut self, epoch: u32) {
        board::clock_store(epoch);
    }

    fn start_links(&mut self, spawner: &Spawner, _prefs: &CompanionPrefs, setup: BleSetup) -> Links {
        if !sd::enabled() {
            return Links::default();
        }
        // a board without a screen always has a fixed PIN (upstream's 123456 unless set); one
        // with a screen (the T114) shows a new one for each pairing
        let pin = setup.pin.unwrap_or(0);
        ble::start(ble::Params { name: setup.name, pin, role: ble::Role::Companion { bonds: setup.bonds } });
        spawner.spawn(ble::task().unwrap());
        Links { ble: true, tcp: false }
    }

    fn usb_write(&mut self, frame: &[u8]) {
        let _ = USB_OUT.try_send(Vec::from(frame));
    }

    fn usb_has_room(&self) -> bool {
        !USB_HOST.load(Ordering::Relaxed) || USB_OUT.free_capacity() > 0
    }

    fn quiet_usb_log(&mut self) {
        crate::usb_frames::silence_usb_log();
    }
}

/// The board around the radio (`board`): the LED lit while transmitting, the receive side of the
/// antenna switch, the button, the battery, the chip's thermometer, the sensors on the I2C.
pub struct ProMicro {
    pub led: Led,
    pub rxen: Option<Output<'static>>,
    pub button: Input<'static>,
    pub battery: Battery,
    /// Read while the SoftDevice is off; it has the TEMP peripheral while it runs.
    pub temp: Temp<'static>,
    pub sensors: Sensors<Bus>,
    /// An OLED answered on the I2C at boot: the interface's pages (`crate::oled`) are turned with
    /// the button, and the pairing PIN is shown on them.
    pub screen: bool,
    /// A speaker is wired on the tone pin (`crate::buzzer`): a new message and a boot sound.
    pub buzzer: bool,
    /// The outputs that stay as they are (`board::Parts::hold`).
    pub _hold: heapless::Vec<Output<'static>, 4>,
}

/// The companion's pages, the home page first. Without a screen none are shown and the button only
/// sends an advert (the firmware's long-stand behaviour).
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

impl Board for ProMicro {
    fn manufacturer(&self) -> &str {
        board::MANUFACTURER
    }

    fn release_board(&self) -> u8 {
        board::RELEASE_BOARD
    }

    fn max_tx_power_dbm(&self) -> i8 {
        22
    }

    fn on_tx_start(&mut self) {
        if let Some(r) = &mut self.rxen {
            r.set_low();
        }
    }

    fn on_tx_end(&mut self) {
        if let Some(r) = &mut self.rxen {
            r.set_high();
        }
    }

    fn tx_led(&mut self, on: bool) {
        if on {
            self.led.on();
        } else {
            self.led.off();
        }
    }

    fn power_off(&mut self) {
        board::system_off();
    }

    async fn battery_mv(&mut self) -> u16 {
        self.battery.read_mv(board::BATT_MV_PER_LSB).await
    }

    fn adc_multiplier(&self) -> Option<f32> {
        Some(board::BATT_MV_PER_LSB)
    }

    async fn mcu_temp_c(&mut self) -> Option<f32> {
        match sd::enabled() {
            true => sd::temp_c(),
            false => Some(self.temp.read().await.to_bits() as f32 / 4.0),
        }
    }

    async fn sensor_lpp(&mut self) -> Vec<u8> {
        let mut lpp = Vec::new();
        self.sensors.read_lpp(&mut lpp).await;
        lpp
    }

    fn sensor_scan(&self) -> String {
        self.sensors.scan.clone()
    }

    fn button_down(&mut self) -> bool {
        self.button.is_low()
    }

    fn has_screen(&self) -> bool {
        self.screen
    }

    fn ui_pages(&self) -> &'static [Page] {
        if self.screen { PAGES } else { &[] }
    }

    fn show_view(&mut self, view: &View) {
        crate::oled::SCREEN.signal(Some(view.clone()));
    }

    fn screen_off(&mut self) {
        crate::oled::SCREEN.signal(None);
    }

    fn has_buzzer(&self) -> bool {
        self.buzzer
    }

    fn boot_log(&self) -> String {
        crate::usb_frames::boot_log()
    }

    fn notify(&mut self, kind: meshcore_fw_core::companion::Notify) {
        use meshcore_fw_core::companion::Notify;
        let sound = match kind {
            Notify::Boot => buzzer::Sound::Startup,
            Notify::Message => buzzer::Sound::Message,
            Notify::Done => buzzer::Sound::Ack,
        };
        buzzer::SOUND.signal(sound);
    }
}
