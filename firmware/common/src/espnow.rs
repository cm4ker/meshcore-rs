//! The bridge's medium on ESP32 boards: ESP-NOW broadcasts on one Wi-Fi channel, as upstream's
//! `ESPNowBridge`. Frames come and go here as bytes; their format and the duplicate checks are
//! `meshcore::bridge`'s, in the main loop.

use alloc::vec::Vec;

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use esp_radio::esp_now::{BROADCAST_ADDRESS, EspNow};

/// Frames from other bridges, for the main loop.
pub static IN: Channel<CriticalSectionRawMutex, Vec<u8>, 8> = Channel::new();
/// Frames to broadcast.
pub static OUT: Channel<CriticalSectionRawMutex, Vec<u8>, 16> = Channel::new();

pub fn spawn(spawner: &Spawner, esp_now: EspNow) -> Result<(), &'static str> {
    spawner.spawn(task(esp_now).map_err(|_| "task busy")?);
    Ok(())
}

#[embassy_executor::task]
async fn task(esp_now: EspNow) {
    let (_manager, mut sender, mut receiver) = esp_now.split();
    loop {
        match select(receiver.receive_async(), OUT.receive()).await {
            Either::First(rx) => {
                // a full queue drops the frame, as a busy radio would
                let _ = IN.try_send(rx.data().to_vec());
            }
            Either::Second(frame) => {
                if let Err(e) = sender.send_async(&BROADCAST_ADDRESS, &frame).await {
                    log::warn!("bridge: ESP-NOW send failed: {e:?}");
                }
            }
        }
    }
}
