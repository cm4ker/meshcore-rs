//! The companion radio on an ESP32: the chip side of `meshcore_fw_core::companion` (BLE, Wi-Fi
//! and TCP, the serial link, firmware slots, watchdog). Its data lives in the `companion`
//! partition, apart from the repeater firmware's; firmware updates go over the app link
//! (`tools/ota-app`), and on Wi-Fi come from the release server ([`fwstore`]), as a repeater's.
//!
//! The board crate creates the radio driver and a [`Board`] (front end, LED, screen, button,
//! battery), then hands over to [`run`].

use alloc::boxed::Box;
use alloc::string::String;
use core::sync::atomic::Ordering;

use embassy_executor::Spawner;
use embassy_net::Stack;
use esp_hal::Async;
use esp_hal::peripherals::{BT, WIFI};
use esp_hal::rng::Rng;
use esp_hal::uart::UartRx;
use esp_storage::FlashStorage;
use log::warn;
use meshcore::companion::CompanionPrefs;
use meshcore::fwshare::FwStore;
use meshcore::ota::FirmwareTarget;
use meshcore_fw_core::companion::{self, BleSetup, Links, Platform, TCP_PORT};
pub use meshcore_fw_core::companion::{Board, Config};

use crate::ota::{self, SharedStore, Slots};
use crate::radio::LoraRadio;
use crate::{ble, clock_cell, frames, fwstore, net, reset, usb};

/// The peripherals the companion loop owns.
pub struct Hardware<R> {
    /// Not initialised yet: [`run`] configures it from the stored settings.
    pub radio: R,
    pub store: Option<SharedStore<'static>>,
    pub serial: Serial,
    pub wifi: WIFI<'static>,
    pub bt: BT<'static>,
    /// From the hardware TRNG, mixed with the radio's own random numbers.
    pub entropy: [u8; 32],
}

/// What the app's serial link (`Link::Usb`) runs on.
pub enum Serial {
    /// The chip's USB Serial/JTAG port.
    Usb(usb::Input),
    /// A UART behind a USB bridge (the Heltec V3's CP2102 on UART0).
    Uart(UartRx<'static, Async>),
}

struct Esp {
    store: Option<SharedStore<'static>>,
    slots: Option<Slots>,
    in_trial: bool,
    /// Taken when the links start.
    serial: Option<Serial>,
    /// The serial link is a UART.
    uart: bool,
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

    fn other_slot(&mut self) -> (bool, String, u32) {
        match (&self.store, self.slots) {
            (Some(s), Some(sl)) => ota::other_slot(&mut s.borrow_mut(), sl),
            _ => (false, String::new(), 0),
        }
    }

    fn rollback(&mut self) -> bool {
        match (&self.store, self.slots) {
            (Some(s), Some(sl)) => ota::rollback(&mut s.borrow_mut(), sl),
            _ => false,
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

        match self.serial.take() {
            Some(Serial::Usb(rx)) => spawner.spawn(frames::usb_rx_task(rx).unwrap()),
            Some(Serial::Uart(rx)) => spawner.spawn(frames::uart_rx_task(rx).unwrap()),
            None => {}
        }
        Links { ble: ble_on, tcp: self.station.is_some() }
    }

    fn usb_write(&mut self, frame: &[u8]) {
        match self.uart {
            true => frames::uart_write(frame),
            false => frames::usb_write(frame),
        }
    }

    fn fw_store(&mut self, spawner: &Spawner, image_len: u32) -> Option<Box<dyn FwStore>> {
        let (Some(stack), Some(store), Some(slots)) = (self.station, &self.store, self.slots) else {
            return None;
        };
        let region = fwstore::region(&slots, image_len)?;
        spawner.spawn(fwstore::fetch_task(stack, store.clone(), region).unwrap());
        Some(Box::new(fwstore::FlashFwStore::new(store.clone(), region)))
    }

    fn poll_network(&mut self, fw_server: &str) -> (Option<u32>, Option<String>) {
        let ip = self.station.and_then(|s| net::ipv4(&s));
        if self.station.is_some() {
            fwstore::sync(net::WIFI_UP.load(Ordering::Relaxed) && ip.is_some(), fw_server);
        }
        (net::TIME_SYNC.try_take(), ip)
    }
}

pub async fn run<R: LoraRadio, B: Board>(spawner: Spawner, hw: Hardware<R>, board: B, cfg: Config) -> ! {
    let Hardware { radio, store, serial, wifi, bt, entropy } = hw;
    let slots = store.as_ref().and_then(|s| ota::slots(&mut s.borrow_mut()));
    let in_trial = match (&store, slots) {
        (Some(s), Some(sl)) => ota::check_trial(&mut s.borrow_mut(), sl.booted),
        _ => false,
    };
    let platform = Esp {
        store: store.clone(),
        slots,
        in_trial,
        uart: matches!(serial, Serial::Uart(_)),
        serial: Some(serial),
        wifi: Some(wifi),
        bt: Some(bt),
        station: None,
    };
    companion::run(spawner, platform, radio, store, entropy, board, cfg).await
}
