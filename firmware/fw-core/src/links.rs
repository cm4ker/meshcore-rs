//! State the app links share with the companion loop, whichever chip and BLE stack run them.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;

/// A BLE app is connected (and its link is authenticated).
pub static BLE_CONNECTED: AtomicBool = AtomicBool::new(false);
/// Frames for the BLE app.
pub static BLE_OUT: Channel<CriticalSectionRawMutex, Vec<u8>, 32> = Channel::new();
/// Passkey to show while a phone pairs (0 = not pairing).
pub static BLE_PASSKEY: AtomicU32 = AtomicU32::new(0);
/// Bonds changed: their serialized form, for the main loop to store.
pub static BONDS_CHANGED: Signal<CriticalSectionRawMutex, Vec<u8>> = Signal::new();
