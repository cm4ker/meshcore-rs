//! BLE companion interface, as upstream `SerialBLEInterface`: the Nordic UART service, frames
//! written to RX, answers notified on TX. Both need an authenticated (MITM-protected) link:
//! pairing uses a passkey the phone asks for: a fixed PIN, or a random one shown on the screen
//! while pairing. Bonds are kept in flash so a paired phone reconnects without asking again.
//!
//! A repeater runs the same service only for a while (`start ota ble`): [`start`] and [`stop`].
//!
//! The host stack is `meshcore_ble`, which takes and gives HCI packets; here they are carried to
//! and from the chip's controller, and what the host reports is acted on.

use alloc::string::String;
use alloc::vec::Vec;
use core::future::pending;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_futures::select::{Either, Either3, select, select3};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use esp_hal::peripherals::BT;
use esp_hal::rng::Rng;
use esp_radio::ble::controller::BleConnector;
use log::{info, warn};
use meshcore::companion::MAX_FRAME_SIZE;
use meshcore::ota::REQ_TYPE_OTA_BEGIN;
use meshcore_ble::{Config, Event, Host, bonds_from_bytes, bonds_to_bytes};

use crate::frames::{FRAMES_IN, Link};
pub use meshcore_fw_core::links::{BLE_CONNECTED, BLE_OUT, BLE_PASSKEY, BONDS_CHANGED};
use meshcore_fw_core::links::{pairing_begins, pairing_succeeded};

/// The BLE task runs (and holds the controller).
pub static BLE_RUNNING: AtomicBool = AtomicBool::new(false);
static BLE_STOP: Signal<CriticalSectionRawMutex, ()> = Signal::new();
/// The pairing PIN of the running task (`NO_PIN` = a random one per pairing).
static PIN: AtomicU32 = AtomicU32::new(NO_PIN);
const NO_PIN: u32 = u32::MAX;

/// Gap between notifications. Upstream uses 60 ms (`BLE_WRITE_MIN_INTERVAL`, Bluedroid's queue);
/// here a frame is notified only once the controller has taken the one before, so a short gap is
/// enough and a long contact list arrives before the apps' timeouts.
const NOTIFY_INTERVAL: Duration = Duration::from_millis(20);
/// The largest HCI packet taken from the controller, with its type: an event of 255 bytes, or
/// data of the 251 a link layer packet carries.
const HCI_PACKET_MAX: usize = 260;

pub struct Params {
    /// Advertised name: `MeshCore-<node name>`.
    pub name: String,
    /// Static random address (from the MAC).
    pub address: [u8; 6],
    /// Serialized bonds from flash.
    pub bonds: Vec<u8>,
    /// Fixed pairing PIN; `None` = a random one per pairing (for boards with a screen).
    pub pin: Option<u32>,
}

/// Static random address derived from the MAC.
pub fn static_address() -> [u8; 6] {
    let mac = esp_hal::efuse::base_mac_address();
    let mut address = [0u8; 6];
    for (a, m) in address.iter_mut().zip(mac.as_bytes().iter().rev()) {
        *a = *m;
    }
    address[5] |= 0xC0; // static random address
    address
}

/// IRK handed to phones when they bond: unique to the board (an all-zero IRK makes Windows refuse
/// a second board) and stable, so both firmware roles on a board share their bonds. The board
/// keeps its static address: nothing is resolved with it.
fn identity_irk(address: &[u8; 6]) -> [u8; 16] {
    let mut h = [0u8; 32];
    meshcore::crypto::sha256(&mut h, &[b"MeshCore BLE IRK", address]);
    h[..16].try_into().unwrap()
}

/// Random bytes for keys: the chip's generator, fed by the radio's noise while BLE is on.
fn random(out: &mut [u8]) {
    Rng::new().read(out);
}

/// Spawn the BLE task unless it is already running (then only its PIN changes).
pub fn start(spawner: &Spawner, bt: BT<'static>, params: Params) {
    if BLE_RUNNING.swap(true, Ordering::AcqRel) {
        PIN.store(params.pin.unwrap_or(NO_PIN), Ordering::Relaxed);
        return;
    }
    match ble_task(bt, params) {
        Ok(task) => spawner.spawn(task),
        Err(_) => BLE_RUNNING.store(false, Ordering::Release),
    }
}

/// Shut a running BLE task down; the controller is released once `BLE_RUNNING` clears.
pub fn stop() {
    if BLE_RUNNING.load(Ordering::Acquire) {
        BLE_STOP.signal(());
    }
}

#[embassy_executor::task]
pub async fn ble_task(bt: BT<'static>, params: Params) {
    BLE_RUNNING.store(true, Ordering::Release);
    BLE_STOP.reset();
    // returns with the host stack and the controller dropped
    if let Either::Second(()) = select(serve(bt, params), BLE_STOP.wait()).await {
        info!("BLE: stopped");
    }
    BLE_CONNECTED.store(false, Ordering::Relaxed);
    BLE_PASSKEY.store(0, Ordering::Relaxed);
    BLE_RUNNING.store(false, Ordering::Release);
}

async fn serve(bt: BT<'static>, params: Params) {
    let mut controller = match BleConnector::new(bt, Default::default()) {
        Ok(c) => c,
        Err(e) => {
            warn!("BLE: controller init failed: {e:?}");
            return;
        }
    };
    PIN.store(params.pin.unwrap_or(NO_PIN), Ordering::Relaxed);
    let mut host = Host::new(Config {
        name: params.name,
        address: params.address,
        irk: identity_irk(&params.address),
        passkey: params.pin,
        bonds: bonds_from_bytes(&params.bonds),
        frame_max: MAX_FRAME_SIZE,
        random,
    });
    let mut packet = [0u8; HCI_PACKET_MAX];
    let mut connected = false;
    let mut notify_at = Instant::now();
    loop {
        let pin = PIN.load(Ordering::Relaxed);
        host.set_passkey((pin != NO_PIN).then_some(pin));
        host.tick(Instant::now().as_secs() as u32);
        while let Some(out) = host.transmit() {
            write(&mut controller, &out).await;
        }
        if let Some(event) = host.event() {
            match event {
                Event::Connected => {
                    info!("BLE: connected, {host:?}");
                    connected = true;
                    while BLE_OUT.try_receive().is_ok() {} // stale frames for an earlier app
                }
                Event::Disconnected(reason) => {
                    info!("BLE: disconnected ({reason:#04x}), {host:?}");
                    connected = false;
                    BLE_CONNECTED.store(false, Ordering::Relaxed);
                    BLE_PASSKEY.store(0, Ordering::Relaxed);
                }
                Event::Passkey(key) => {
                    if pairing_begins() {
                        BLE_PASSKEY.store(key.max(1), Ordering::Relaxed);
                    } else {
                        warn!("BLE: pairing refused after failed ones, try later");
                        host.disconnect();
                    }
                }
                Event::Paired { bonded } => {
                    info!("BLE: paired");
                    BLE_PASSKEY.store(0, Ordering::Relaxed);
                    pairing_succeeded();
                    BLE_CONNECTED.store(true, Ordering::Relaxed);
                    if bonded {
                        BONDS_CHANGED.signal(bonds_to_bytes(host.bonds()));
                    }
                }
                Event::Encrypted(authenticated) => {
                    info!("BLE: encrypted, {host:?}");
                    BLE_CONNECTED.store(authenticated, Ordering::Relaxed);
                }
                Event::PairingFailed => {
                    warn!("BLE: pairing failed");
                    BLE_PASSKEY.store(0, Ordering::Relaxed);
                    host.disconnect();
                }
                Event::Written(frame) => {
                    if frame.first() == Some(&REQ_TYPE_OTA_BEGIN) {
                        // every chunk of a firmware upload is a write answered in a later
                        // connection event, so the interval sets the transfer rate
                        host.hurry();
                    }
                    if !frame.is_empty() {
                        // wait for room: the write is acknowledged only once the frame is queued,
                        // which paces a fast sender (firmware upload) to the main loop
                        FRAMES_IN.send((Link::Ble, frame)).await;
                    }
                    host.written();
                }
            }
            continue;
        }
        // a frame for the app, once the controller has taken all that was for it before
        let take_frame = connected && host.idle();
        let frame = async {
            if !take_frame {
                pending::<()>().await;
            }
            Timer::at(notify_at).await;
            BLE_OUT.receive().await
        };
        match select3(read(&mut controller, &mut packet), frame, Timer::after_secs(1)).await {
            Either3::First(len) => host.receive(&packet[..len]),
            // not authenticated yet: upstream drops pushes then too
            Either3::Second(frame) => {
                if BLE_CONNECTED.load(Ordering::Relaxed) && host.notify(&frame) {
                    notify_at = Instant::now() + NOTIFY_INTERVAL;
                }
            }
            // the host is told the time: a pairing that stalls ends
            Either3::Third(()) => {}
        }
    }
}

/// The controller's next HCI packet, its type first: the length of it in `packet`. Nothing is
/// taken while this waits, so the wait may be given up for something else.
async fn read(controller: &mut BleConnector<'_>, packet: &mut [u8; HCI_PACKET_MAX]) -> usize {
    loop {
        if controller.read_async(&mut packet[..1]).await != Ok(1) {
            continue;
        }
        // a packet is whole in the controller's queue: the rest of it is there to read
        let header = match packet[0] {
            2 => 4, // data: handle and flags, length
            4 => 2, // an event: code, length
            _ => continue,
        };
        if controller.read(&mut packet[1..1 + header]) != Ok(header) {
            continue;
        }
        let body = match packet[0] {
            2 => u16::from_le_bytes([packet[3], packet[4]]) as usize,
            _ => packet[2] as usize,
        };
        let end = 1 + header + body;
        if end <= packet.len() {
            if controller.read(&mut packet[1 + header..end]) == Ok(body) {
                return end;
            }
            continue;
        }
        // longer than any the host takes: skipped, so the next read starts at a packet
        let mut left = body;
        while left > 0 {
            let part = left.min(packet.len());
            if controller.read(&mut packet[..part]) != Ok(part) {
                break;
            }
            left -= part;
        }
    }
}

async fn write(controller: &mut BleConnector<'_>, mut packet: &[u8]) {
    while !packet.is_empty() {
        match controller.write_async(packet).await {
            Ok(taken) if taken > 0 => packet = &packet[taken..],
            _ => return,
        }
    }
}
