//! BLE companion interface, as upstream `SerialBLEInterface`: the Nordic UART service, frames
//! written to RX, answers notified on TX. Both need an authenticated (MITM-protected) link:
//! pairing uses a passkey the phone asks for: a fixed PIN, or a random one shown on the screen
//! while pairing. Bonds are kept in flash so a paired phone reconnects without asking again.
//!
//! A repeater runs the same service only for a while (`start ota ble`): [`start`] and [`stop`].

use alloc::string::String;
use alloc::vec::Vec;
use core::num::NonZeroU128;
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use embassy_futures::join::join;
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer};
use esp_hal::peripherals::BT;
use esp_radio::ble::controller::BleConnector;
use log::{info, warn};
use meshcore::companion::MAX_FRAME_SIZE;
use meshcore::ota::REQ_TYPE_OTA_BEGIN;
use trouble_host::prelude::*;

use crate::frames::{FRAMES_IN, Link};
pub use meshcore_fw_core::links::{BLE_CONNECTED, BLE_OUT, BLE_PASSKEY, BONDS_CHANGED};
use meshcore_fw_core::links::{pairing_begins, pairing_succeeded};

/// The BLE task runs (and holds the controller).
pub static BLE_RUNNING: AtomicBool = AtomicBool::new(false);
static BLE_STOP: Signal<CriticalSectionRawMutex, ()> = Signal::new();
/// The GATT server, built on the first run of the BLE task: trouble-host keeps the attribute
/// storage in statics, so a second one cannot be built in the same boot.
static SERVER: AtomicPtr<Server<'static>> = AtomicPtr::new(core::ptr::null_mut());

/// Gap between notifications. Upstream uses 60 ms (`BLE_WRITE_MIN_INTERVAL`, Bluedroid's queue);
/// here each notify already waits until the host stack has taken it, so a short gap is enough and
/// a long contact list arrives before the apps' timeouts.
const NOTIFY_INTERVAL_MS: u64 = 20;
const BOND_RECORD: usize = 41;
/// trouble-host's limit for the GAP device name (the advertised name may be longer).
const GAP_NAME_MAX: usize = 22;

type Hci = ExternalController<BleConnector<'static>, 20>;
/// Connection interval asked for during a firmware upload: every chunk is a write request
/// answered in a later connection event, so the interval sets the transfer rate.
const UPLOAD_INTERVAL_MS: (u64, u64) = (8, 15);
const MAX_STORED_BONDS: usize = 8;

#[gatt_server]
struct Server {
    uart: UartService,
}

#[gatt_service(uuid = "6E400001-B5A3-F393-E0A9-E50E24DCCA9E")]
struct UartService {
    #[characteristic(uuid = "6E400002-B5A3-F393-E0A9-E50E24DCCA9E", write, write_without_response, permissions(write = authenticated))]
    rx: heapless::Vec<u8, MAX_FRAME_SIZE>,
    #[characteristic(uuid = "6E400003-B5A3-F393-E0A9-E50E24DCCA9E", read, notify, permissions(read = authenticated, cccd = authenticated))]
    tx: heapless::Vec<u8, MAX_FRAME_SIZE>,
}

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

/// `[kind][addr 6][ltk 16][irk 16, 0 = none][bonded][level]` per bond.
fn serialize_bonds(bonds: &[BondInformation]) -> Vec<u8> {
    let mut out = Vec::new();
    for b in bonds.iter().filter(|b| b.is_bonded).take(MAX_STORED_BONDS) {
        out.push(b.identity.addr.kind.as_raw());
        out.extend_from_slice(b.identity.addr.addr.raw());
        out.extend_from_slice(&b.ltk.0.to_le_bytes());
        out.extend_from_slice(&b.identity.irk.map_or(0, |k| k.0.get()).to_le_bytes());
        out.push(1);
        out.push(match b.security_level {
            SecurityLevel::NoEncryption => 0,
            SecurityLevel::Encrypted => 1,
            SecurityLevel::EncryptedAuthenticated => 2,
        });
    }
    out
}

fn deserialize_bonds(data: &[u8]) -> Vec<BondInformation> {
    data.as_chunks::<BOND_RECORD>()
        .0
        .iter()
        .map(|r| {
            let addr = Address { kind: AddrKind::new(r[0]), addr: BdAddr::new(r[1..7].try_into().unwrap()) };
            let ltk = LongTermKey(u128::from_le_bytes(r[7..23].try_into().unwrap()));
            let irk = NonZeroU128::new(u128::from_le_bytes(r[23..39].try_into().unwrap())).map(IdentityResolvingKey);
            let level = match r[40] {
                0 => SecurityLevel::NoEncryption,
                1 => SecurityLevel::Encrypted,
                _ => SecurityLevel::EncryptedAuthenticated,
            };
            BondInformation::new(Identity { addr, irk }, ltk, level, r[39] != 0)
        })
        .collect()
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

/// The GATT server (named on its first use: a new name needs a reboot).
fn server(name: &str) -> Option<&'static Server<'static>> {
    let p = SERVER.load(Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: set below from a leaked box, never freed
        return Some(unsafe { &*p });
    }
    let name: &'static str = alloc::boxed::Box::leak(truncate(name, GAP_NAME_MAX).into());
    let server = Server::new_with_config(GapConfig::Peripheral(PeripheralConfig { name, appearance: &appearance::UNKNOWN })).ok()?;
    let server: &'static Server<'static> = alloc::boxed::Box::leak(alloc::boxed::Box::new(server));
    SERVER.store(core::ptr::from_ref(server).cast_mut(), Ordering::Release);
    Some(server)
}

fn truncate(s: &str, max: usize) -> &str {
    let mut n = s.len().min(max);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

/// IRK handed to phones when they bond: unique to the board (the default all-zero IRK makes Windows
/// refuse a second board) and stable, so both firmware roles on a board share their bonds. The
/// board keeps its static address: nothing is resolved with it.
fn identity_irk(address: &[u8; 6]) -> u128 {
    let mut h = [0u8; 32];
    meshcore::crypto::sha256(&mut h, &[b"MeshCore BLE IRK", address]);
    u128::from_le_bytes(h[..16].try_into().unwrap())
}

/// Spawn the BLE task unless it is already running (then only its PIN changes).
pub fn start(spawner: &Spawner, bt: BT<'static>, params: Params) {
    if BLE_RUNNING.swap(true, Ordering::AcqRel) {
        trouble_host::set_fixed_pass_key(params.pin);
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
    let connector = match BleConnector::new(bt, Default::default()) {
        Ok(c) => c,
        Err(e) => {
            warn!("BLE: controller init failed: {e:?}");
            return;
        }
    };
    let controller: Hci = ExternalController::new(connector);
    trouble_host::set_fixed_pass_key(params.pin);
    trouble_host::set_identity_irk(identity_irk(&params.address));
    let mut resources: HostResources<_, DefaultPacketPool, 1, 3> = HostResources::new();
    let stack = trouble_host::new(controller, &mut resources)
        .set_random_address(Address::random(params.address))
        .set_io_capabilities(IoCapabilities::DisplayOnly)
        .build();
    for b in deserialize_bonds(&params.bonds) {
        let _ = stack.add_bond_information(b);
    }
    let runner = stack.runner();
    let mut peripheral = stack.peripheral();
    let Some(server) = server(&params.name) else {
        warn!("BLE: GATT server setup failed");
        return;
    };

    let _ = join(run_host(runner), async {
        loop {
            match advertise(&params.name, &mut peripheral, server).await {
                Ok(conn) => {
                    let _ = conn.raw().set_bondable(true);
                    while BLE_OUT.try_receive().is_ok() {} // stale frames for an earlier app
                    select(events(server, &conn, &stack), notifier(server, &conn)).await;
                    BLE_CONNECTED.store(false, Ordering::Relaxed);
                    BLE_PASSKEY.store(0, Ordering::Relaxed);
                }
                Err(e) => {
                    warn!("BLE: advertising failed: {e:?}");
                    Timer::after_secs(5).await;
                }
            }
        }
    })
    .await;
}

async fn run_host<C: Controller, P: PacketPool>(mut runner: Runner<'_, C, P>) {
    loop {
        if let Err(e) = runner.run().await {
            warn!("BLE: host error: {e:?}");
            Timer::after_secs(1).await;
        }
    }
}

async fn advertise<'stack, 'server, C: Controller>(
    name: &str, peripheral: &mut Peripheral<'stack, C, DefaultPacketPool>, server: &'server Server<'_>,
) -> Result<GattConnection<'stack, 'server, DefaultPacketPool>, BleHostError<C::Error>> {
    let mut adv = [0u8; 31];
    let adv_len = AdStructure::encode_slice(
        &[
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
            AdStructure::CompleteServiceUuids128(&[[
                0x9E, 0xCA, 0xDC, 0x24, 0x0E, 0xE5, 0xA9, 0xE0, 0x93, 0xF3, 0xA3, 0xB5, 0x01, 0x00, 0x40, 0x6E,
            ]]),
        ],
        &mut adv[..],
    )?;
    let mut scan = [0u8; 31];
    let scan_len =
        AdStructure::encode_slice(&[AdStructure::CompleteLocalName(truncate(name, 29).as_bytes())], &mut scan[..])?;
    let advertiser = peripheral
        .advertise(
            &Default::default(),
            Advertisement::ConnectableScannableUndirected { adv_data: &adv[..adv_len], scan_data: &scan[..scan_len] },
        )
        .await?;
    let conn = advertiser.accept().await?.with_attribute_server(server)?;
    info!("BLE: connected");
    Ok(conn)
}

/// GATT and security events until the link drops.
async fn events(server: &Server<'_>, conn: &GattConnection<'_, '_, DefaultPacketPool>, stack: &Stack<'_, Hci, DefaultPacketPool>) {
    let rx = server.uart.rx.handle;
    loop {
        match conn.next().await {
            GattConnectionEvent::Disconnected { reason } => {
                info!("BLE: disconnected ({reason:?})");
                return;
            }
            GattConnectionEvent::PassKeyDisplay(key) => {
                if pairing_begins() {
                    BLE_PASSKEY.store(key.value().max(1), Ordering::Relaxed);
                } else {
                    warn!("BLE: pairing refused after failed ones, try later");
                    conn.raw().disconnect();
                }
            }
            GattConnectionEvent::PairingComplete { security_level, bond } => {
                BLE_PASSKEY.store(0, Ordering::Relaxed);
                info!("BLE: paired ({security_level:?})");
                if security_level == SecurityLevel::EncryptedAuthenticated {
                    pairing_succeeded();
                }
                BLE_CONNECTED.store(security_level == SecurityLevel::EncryptedAuthenticated, Ordering::Relaxed);
                if bond.is_some() {
                    BONDS_CHANGED.signal(stack.with_bond_information(serialize_bonds));
                }
            }
            GattConnectionEvent::Encrypted { security_level, .. } => {
                BLE_CONNECTED.store(security_level == SecurityLevel::EncryptedAuthenticated, Ordering::Relaxed);
            }
            GattConnectionEvent::PairingFailed(e) => {
                BLE_PASSKEY.store(0, Ordering::Relaxed);
                warn!("BLE: pairing failed: {e:?}");
                conn.raw().disconnect();
            }
            GattConnectionEvent::ConnectionParamsUpdated { conn_interval, peripheral_latency, .. } => {
                info!("BLE: interval {} ms, latency {peripheral_latency}", conn_interval.as_millis());
            }
            GattConnectionEvent::Gatt { event } => {
                if let GattEvent::Write(w) = &event
                    && w.handle() == rx
                {
                    let frame = w.with_data(|_, data| data.to_vec());
                    if frame.first() == Some(&REQ_TYPE_OTA_BEGIN) {
                        let fast = RequestedConnParams {
                            min_connection_interval: Duration::from_millis(UPLOAD_INTERVAL_MS.0),
                            max_connection_interval: Duration::from_millis(UPLOAD_INTERVAL_MS.1),
                            // committing stalls the stack for half a minute of flash work
                            supervision_timeout: Duration::from_secs(20),
                            ..Default::default()
                        };
                        if let Err(e) = conn.raw().update_connection_params(stack, &fast).await {
                            warn!("BLE: connection update failed: {e:?}");
                        }
                    }
                    if !frame.is_empty() {
                        // wait for room: the write is acknowledged only once the frame is queued,
                        // which paces a fast sender (firmware upload) to the main loop
                        FRAMES_IN.send((Link::Ble, frame)).await;
                    }
                }
                if let Ok(reply) = event.accept() {
                    reply.send().await;
                }
            }
            _ => {}
        }
    }
}

/// Notify queued frames to the app.
async fn notifier(server: &Server<'_>, conn: &GattConnection<'_, '_, DefaultPacketPool>) {
    loop {
        let frame = BLE_OUT.receive().await;
        if !BLE_CONNECTED.load(Ordering::Relaxed) {
            continue; // not authenticated yet: upstream drops pushes then too
        }
        match select(server.uart.tx.notify_raw(conn, &frame, false), Timer::after_secs(5)).await {
            Either::First(Ok(())) => {}
            Either::First(Err(e)) => warn!("BLE: notify failed: {e:?}"),
            Either::Second(()) => warn!("BLE: notify timed out"),
        }
        Timer::after_millis(NOTIFY_INTERVAL_MS).await;
    }
}
