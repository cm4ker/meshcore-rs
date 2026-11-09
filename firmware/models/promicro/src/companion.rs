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
use embassy_nrf::twim::Twim;
use embassy_nrf::wdt::WatchdogHandle;
use meshcore::companion::CompanionPrefs;
use meshcore::ota::FirmwareTarget;
use meshcore_fw_core::companion::{BleSetup, Board, Links, Platform};
use meshcore_fw_core::sensors::Sensors;
use meshcore_fw_core::storage::SharedStore;

use crate::board::{self, Battery};
use crate::flash::Flash;
use crate::usb_frames::{USB_HOST, USB_OUT};
use crate::{ble, ota, sd};

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
}

impl Platform for Nrf {
    type Flash = Flash;

    fn feed_watchdog(&mut self) {
        if let Some(w) = &mut self.watchdog {
            w.pet();
        }
    }

    fn reboot(&mut self) -> ! {
        if ota::INSTALL_PENDING.load(Ordering::Relaxed) {
            ota::hand_over();
        }
        cortex_m::peripheral::SCB::sys_reset()
    }

    fn ota_target(&mut self) -> Option<Box<dyn FirmwareTarget>> {
        let target = ota::StagingTarget::lending(self.store.clone())?;
        Some(Box::new(target))
    }

    fn in_trial(&self) -> bool {
        self.in_trial
    }

    fn confirm_trial(&mut self) {
        ota::confirm(&self.store);
        self.in_trial = false;
        log::info!("OTA: new firmware confirmed");
    }

    fn other_slot(&mut self) -> (bool, String, u32) {
        let (kept, prev, skip, _) = ota::previous(&self.store);
        (kept, prev, skip)
    }

    fn rollback(&mut self) -> bool {
        ota::rollback(&self.store)
    }

    fn poll_slot(&mut self, update: bool) -> bool {
        let mut store = self.store.borrow_mut();
        if !store.lent() || update || self.in_trial || self.stays_aside || ota::INSTALL_PENDING.load(Ordering::Relaxed) {
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
        // a board without a screen always has a fixed PIN (upstream's 123456 unless set)
        let pin = setup.pin.unwrap_or(123_456);
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
}

/// The board around the radio (`board`): the LED lit while transmitting, the receive side of the
/// antenna switch, the button, the battery, the chip's thermometer, the sensors on the I2C.
pub struct ProMicro {
    pub led: Output<'static>,
    pub rxen: Output<'static>,
    pub button: Input<'static>,
    pub battery: Battery,
    /// Read while the SoftDevice is off; it has the TEMP peripheral while it runs.
    pub temp: Temp<'static>,
    pub sensors: Sensors<Twim<'static>>,
}

impl Board for ProMicro {
    fn manufacturer(&self) -> &str {
        "ProMicro DIY"
    }

    fn release_board(&self) -> u8 {
        meshcore::release::board::PROMICRO
    }

    fn max_tx_power_dbm(&self) -> i8 {
        22
    }

    fn on_tx_start(&mut self) {
        self.rxen.set_low();
        self.led.set_high();
    }

    fn on_tx_end(&mut self) {
        self.led.set_low();
        self.rxen.set_high();
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

    fn button_down(&mut self) -> bool {
        self.button.is_low()
    }
}
