//! The nRF52840 under the companion loop: BLE (Nordic's SDC), the USB port, the watchdog, reboots.

use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use embassy_executor::Spawner;
use alloc::boxed::Box;
use embassy_nrf::qspi::Qspi;
use embassy_nrf::wdt::WatchdogHandle;
use meshcore::companion::CompanionPrefs;
use meshcore::ota::FirmwareTarget;
use meshcore_fw_core::companion::{BleSetup, Links, Platform};
use meshcore_fw_core::storage::SharedStore;
use nrf_sdc::SoftdeviceController;

use crate::ble;
use crate::ota::{self, StagingTarget};
use crate::usb::{USB_HOST, USB_OUT};

/// The USB port runs from the start (the boot log goes there), so the links to start are the
/// radio ones.
pub struct Nrf {
    /// The BLE controller, until `start_links` hands it to the BLE task.
    pub sdc: Option<SoftdeviceController<'static>>,
    /// State of a small generator seeded from the hardware RNG (which the BLE controller owns).
    pub seed: u64,
    pub watchdog: Option<WatchdogHandle>,
    /// The flash that stages updates.
    pub store: Option<SharedStore<Qspi<'static>>>,
}

impl Platform for Nrf {
    type Flash = Qspi<'static>;

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
        let target = StagingTarget::new(self.store.clone()?)?;
        Some(Box::new(target))
    }

    fn random(&mut self) -> u32 {
        // splitmix64
        self.seed = self.seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (z ^ (z >> 31)) as u32
    }

    fn start_links(&mut self, spawner: &Spawner, _prefs: &CompanionPrefs, setup: BleSetup) -> Links {
        let Some(sdc) = self.sdc.take() else {
            return Links::default();
        };
        let params = ble::Params { name: setup.name, bonds: setup.bonds, pin: setup.pin };
        spawner.spawn(ble::ble_task(sdc, params).unwrap());
        Links { ble: true, tcp: false }
    }

    fn usb_write(&mut self, frame: &[u8]) {
        let _ = USB_OUT.try_send(Vec::from(frame));
    }

    fn usb_has_room(&self) -> bool {
        !USB_HOST.load(Ordering::Relaxed) || USB_OUT.free_capacity() > 0
    }
}
