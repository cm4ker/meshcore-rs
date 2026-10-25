//! The companion radio on an ESP32: the chip side of `meshcore_fw_core::companion` (BLE, Wi-Fi
//! and TCP, USB Serial/JTAG, firmware slots, watchdog). Its data lives in the `companion`
//! partition, apart from the repeater firmware's; firmware updates go over the app link
//! (`tools/ota-app`).
//!
//! The board crate creates the radio driver and a [`Board`] (front end, LED, screen, button,
//! battery), then hands over to [`run`].

use alloc::boxed::Box;
use alloc::string::String;

use embassy_executor::Spawner;
use embassy_net::Stack;
use esp_hal::peripherals::{BT, WIFI};
use esp_hal::rng::Rng;
use esp_storage::FlashStorage;
use log::warn;
use meshcore::companion::CompanionPrefs;
use meshcore::ota::FirmwareTarget;
use meshcore_fw_core::companion::{self, BleSetup, Links, Platform, TCP_PORT};
pub use meshcore_fw_core::companion::{Board, Config};

use crate::ota::{self, SharedStore, Slots};
use crate::radio::LoraRadio;
use crate::{ble, clock_cell, frames, net, reset, usb};

/// The peripherals the companion loop owns.
pub struct Hardware<R> {
    /// Not initialised yet: [`run`] configures it from the stored settings.
    pub radio: R,
    pub store: Option<SharedStore<'static>>,
    pub usb_rx: usb::Input,
    pub wifi: WIFI<'static>,
    pub bt: BT<'static>,
    /// From the hardware TRNG, mixed with the radio's own random numbers.
    pub entropy: [u8; 32],
}

struct Esp {
    store: Option<SharedStore<'static>>,
    slots: Option<Slots>,
    in_trial: bool,
    usb_rx: Option<usb::Input>,
    wifi: Option<WIFI<'static>>,
    bt: Option<BT<'static>>,
    station: Option<Stack<'static>>,
}

impl Platform for Esp {
    type Flash = FlashStorage<'static>;

    fn feed_watchdog(&mut self) {
        reset::feed();
    }

    fn reboot(&mut self) -> ! {
        reset::hard_reset()
    }

    fn random(&mut self) -> u32 {
        Rng::new().random()
    }

    fn ram_clock(&self) -> Option<u32> {
        clock_cell::load()
    }

    fn set_ram_clock(&mut self, epoch: u32) {
        clock_cell::store(epoch);
    }

    fn ota_target(&mut self) -> Option<Box<dyn FirmwareTarget>> {
        match (&self.store, self.slots) {
            (Some(s), Some(sl)) => Some(Box::new(ota::FlashTarget::new(s.clone(), sl))),
            _ => None,
        }
    }

    fn in_trial(&self) -> bool {
        self.in_trial
    }

    fn confirm_trial(&mut self) {
        if let Some(s) = &self.store {
            ota::confirm(&mut s.borrow_mut());
        }
    }

    fn start_links(&mut self, spawner: &Spawner, prefs: &CompanionPrefs, ble: BleSetup) -> Links {
        // Wi-Fi: the TCP companion interface
        if prefs.wifi_enabled
            && !prefs.wifi_ssid.is_empty()
            && let Some(wifi) = self.wifi.take()
        {
            let seed = (Rng::new().random() as u64) << 32 | Rng::new().random() as u64;
            let params = net::StationParams {
                ssid: prefs.wifi_ssid.clone(),
                password: prefs.wifi_pwd.clone(),
                power_save: false, // the app connects in
                esp_now: false,
            };
            match net::start(spawner, wifi, seed, params) {
                Ok((stack, _)) => {
                    self.station = Some(stack);
                    spawner.spawn(frames::tcp_server_task(stack, TCP_PORT).unwrap());
                }
                Err(e) => warn!("Wi-Fi station: {e}"),
            }
        }

        // BLE, next to Wi-Fi only where esp-radio coordinates the two radios (`coex`). On the C6
        // its BLE side is still stubbed out (esp-radio 1.0.0-beta.1): Wi-Fi starves while an app
        // is connected over BLE, so there Wi-Fi (TCP) replaces BLE as upstream's builds do.
        let ble_on = self.station.is_none() || cfg!(feature = "esp32s3");
        if ble_on && let Some(bt) = self.bt.take() {
            let params = ble::Params { name: ble.name, address: ble::static_address(), bonds: ble.bonds, pin: ble.pin };
            spawner.spawn(ble::ble_task(bt, params).unwrap());
        }

        if let Some(rx) = self.usb_rx.take() {
            spawner.spawn(frames::usb_rx_task(rx).unwrap());
        }
        Links { ble: ble_on, tcp: self.station.is_some() }
    }

    fn usb_write(&mut self, frame: &[u8]) {
        frames::usb_write(frame);
    }

    fn poll_network(&mut self) -> (Option<u32>, Option<String>) {
        (net::TIME_SYNC.try_take(), self.station.and_then(|s| net::ipv4(&s)))
    }
}

pub async fn run<R: LoraRadio, B: Board>(spawner: Spawner, hw: Hardware<R>, board: B, cfg: Config) -> ! {
    let Hardware { radio, store, usb_rx, wifi, bt, entropy } = hw;
    let slots = store.as_ref().and_then(|s| ota::slots(&mut s.borrow_mut()));
    let in_trial = match (&store, slots) {
        (Some(s), Some(sl)) => ota::check_trial(&mut s.borrow_mut(), sl.booted),
        _ => false,
    };
    let platform = Esp {
        store: store.clone(),
        slots,
        in_trial,
        usb_rx: Some(usb_rx),
        wifi: Some(wifi),
        bt: Some(bt),
        station: None,
    };
    companion::run(spawner, platform, radio, store, entropy, board, cfg).await
}
