//! State the app links share with the companion loop, whichever chip and BLE stack run them.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;

/// BLE is switched on (the on-device interface can switch it off until the next start).
pub static BLE_ENABLED: AtomicBool = AtomicBool::new(true);
/// A BLE app is connected (and its link is authenticated).
pub static BLE_CONNECTED: AtomicBool = AtomicBool::new(false);
/// Frames for the BLE app.
pub static BLE_OUT: Channel<CriticalSectionRawMutex, Vec<u8>, 32> = Channel::new();
/// Passkey to show while a phone pairs (0 = not pairing).
pub static BLE_PASSKEY: AtomicU32 = AtomicU32::new(0);
/// Bonds changed: their serialized form, for the main loop to store.
pub static BONDS_CHANGED: Signal<CriticalSectionRawMutex, Vec<u8>> = Signal::new();

/// Pairings begun in a row that did not end paired, and until when (seconds since start) no new
/// one is let in.
static PAIRING_FAILURES: AtomicU32 = AtomicU32::new(0);
static PAIRING_BLOCKED_UNTIL: AtomicU32 = AtomicU32::new(0);
/// Pairings that may fail in a row before the wait: a mistyped PIN or two.
const FREE_PAIRINGS: u32 = 3;

/// A phone starts pairing (the passkey is due): `false` if it must be turned away. An active
/// attacker learns a fixed passkey a bit per failed pairing (some 20 in all): after a few in a
/// row, the next waits a minute, then twice as long each time, up to an hour. Bonded phones
/// reconnect without pairing and are not held up.
pub fn pairing_begins() -> bool {
    let now = embassy_time::Instant::now().as_secs() as u32;
    if now < PAIRING_BLOCKED_UNTIL.load(Ordering::Relaxed) {
        return false;
    }
    // counted as failed until it completes
    let n = PAIRING_FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
    if n >= FREE_PAIRINGS {
        let wait = (60u32 << (n - FREE_PAIRINGS).min(6)).min(3600);
        PAIRING_BLOCKED_UNTIL.store(now + wait, Ordering::Relaxed);
    }
    true
}

/// A pairing completed with the passkey: the count starts over.
pub fn pairing_succeeded() {
    PAIRING_FAILURES.store(0, Ordering::Relaxed);
    PAIRING_BLOCKED_UNTIL.store(0, Ordering::Relaxed);
}
