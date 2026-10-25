//! BLE companion interface on the nRF52840, as upstream's `SerialBLEInterface`: the Nordic UART
//! service, frames written to RX, answers notified on TX, over trouble-host with Nordic's
//! SoftDevice Controller (the SoftDevice in flash stays unused). Both characteristics need an
//! authenticated link: the phone pairs with a passkey (the app's PIN, else upstream's 123456),
//! and the bonds are kept in the store so a paired phone reconnects without asking again.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::num::NonZeroU128;
use core::sync::atomic::Ordering;

use embassy_futures::join::join;
use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Timer};
use log::{info, warn};
use meshcore::companion::MAX_FRAME_SIZE;
use meshcore_fw_core::frames::{FRAMES_IN, Link};
use meshcore_fw_core::links::{BLE_CONNECTED, BLE_ENABLED, BLE_OUT, BLE_PASSKEY, BONDS_CHANGED};
use nrf_sdc::SoftdeviceController;
use trouble_host::prelude::*;

/// Gap between notifications; each notify already waits until the host stack has taken it.
const NOTIFY_INTERVAL_MS: u64 = 20;
const BOND_RECORD: usize = 41;
const MAX_STORED_BONDS: usize = 8;
/// trouble-host's limit for the GAP device name (the advertised name may be longer).
const GAP_NAME_MAX: usize = 22;

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
    /// Serialized bonds from the store.
    pub bonds: Vec<u8>,
    /// Fixed pairing PIN; `None` = a random one per pairing (shown on the screen).
    pub pin: Option<u32>,
}

/// `[kind][addr 6][ltk 16][irk 16, 0 = none][bonded][level]` per bond (as on the ESP32 boards).
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

/// The chip's static random address (`FICR.DEVICEADDR`, as the SoftDevice uses), little-endian.
pub fn static_address() -> [u8; 6] {
    let ficr = embassy_nrf::pac::FICR;
    let lo = ficr.deviceaddr(0).read();
    let hi = ficr.deviceaddr(1).read();
    let mut a = [0u8; 6];
    a[..4].copy_from_slice(&lo.to_le_bytes());
    a[4..].copy_from_slice(&(hi as u16).to_le_bytes());
    a[5] |= 0xC0; // static random address
    a
}

/// IRK handed to phones when they bond: unique to the board (the default all-zero IRK makes Windows
/// refuse a second board) and stable. The board keeps its static address: nothing is resolved
/// with it.
fn identity_irk(address: &[u8; 6]) -> u128 {
    let mut h = [0u8; 32];
    meshcore::crypto::sha256(&mut h, &[b"MeshCore BLE IRK", address]);
    u128::from_le_bytes(h[..16].try_into().unwrap())
}

fn truncate(s: &str, max: usize) -> &str {
    let mut n = s.len().min(max);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

#[embassy_executor::task]
pub async fn ble_task(sdc: SoftdeviceController<'static>, params: Params) {
    let address = static_address();
    trouble_host::set_identity_irk(identity_irk(&address));
    let resources: &'static mut HostResources<DefaultPacketPool, 1, 3> = Box::leak(Box::new(HostResources::new()));
    let stack: &'static Stack<'static, SoftdeviceController<'static>, DefaultPacketPool> = Box::leak(Box::new(
        trouble_host::new(sdc, resources)
            .set_random_address(Address::random(address))
            .set_io_capabilities(IoCapabilities::DisplayOnly)
            .set_passkey(params.pin)
            .build(),
    ));
    for b in deserialize_bonds(&params.bonds) {
        let _ = stack.add_bond_information(b);
    }
    let name: &'static str = Box::leak(truncate(&params.name, GAP_NAME_MAX).into());
    let Ok(server) = Server::new_with_config(GapConfig::Peripheral(PeripheralConfig { name, appearance: &appearance::UNKNOWN }))
    else {
        warn!("BLE: GATT server setup failed");
        return;
    };
    let server: &'static Server<'static> = Box::leak(Box::new(server));
    let mut runner = stack.runner();
    let mut peripheral = stack.peripheral();

    let _ = join(
        async {
            loop {
                if let Err(e) = runner.run().await {
                    warn!("BLE: host error: {e:?}");
                    Timer::after_secs(1).await;
                }
            }
        },
        async {
            loop {
                while !BLE_ENABLED.load(Ordering::Relaxed) {
                    Timer::after_millis(500).await;
                }
                // switched off on the device: stop advertising, or drop the app
                match select(advertise(&params.name, &mut peripheral, server), switched_off()).await {
                    Either::First(Ok(conn)) => {
                        let _ = conn.raw().set_bondable(true);
                        while BLE_OUT.try_receive().is_ok() {} // stale frames for an earlier app
                        let link = select(events(server, &conn, stack), notifier(server, &conn));
                        if let Either::Second(()) = select(link, switched_off()).await {
                            conn.raw().disconnect();
                            Timer::after_millis(300).await;
                        }
                        BLE_CONNECTED.store(false, Ordering::Relaxed);
                        BLE_PASSKEY.store(0, Ordering::Relaxed);
                    }
                    Either::First(Err(e)) => {
                        warn!("BLE: advertising failed: {e:?}");
                        Timer::after_secs(5).await;
                    }
                    Either::Second(()) => {}
                }
            }
        },
    )
    .await;
}

async fn switched_off() {
    while BLE_ENABLED.load(Ordering::Relaxed) {
        Timer::after_millis(250).await;
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
async fn events<C: Controller>(
    server: &Server<'_>, conn: &GattConnection<'_, '_, DefaultPacketPool>, stack: &Stack<'_, C, DefaultPacketPool>,
) {
    let rx = server.uart.rx.handle;
    loop {
        // a bonded phone re-encrypts without a pairing event: poll the link's security level
        let authenticated = matches!(conn.raw().security_level(), Ok(SecurityLevel::EncryptedAuthenticated));
        BLE_CONNECTED.store(authenticated, Ordering::Relaxed);
        let event = match select(conn.next(), Timer::after(Duration::from_millis(250))).await {
            Either::First(e) => e,
            Either::Second(()) => continue,
        };
        match event {
            GattConnectionEvent::Disconnected { reason } => {
                info!("BLE: disconnected ({reason:?})");
                return;
            }
            GattConnectionEvent::PassKeyDisplay(key) => BLE_PASSKEY.store(key.value().max(1), Ordering::Relaxed),
            GattConnectionEvent::PairingComplete { security_level, bond } => {
                BLE_PASSKEY.store(0, Ordering::Relaxed);
                info!("BLE: paired ({security_level:?})");
                if bond.is_some() {
                    BONDS_CHANGED.signal(stack.with_bond_information(serialize_bonds));
                }
            }
            GattConnectionEvent::PairingFailed(e) => {
                BLE_PASSKEY.store(0, Ordering::Relaxed);
                warn!("BLE: pairing failed: {e:?}");
                conn.raw().disconnect();
            }
            GattConnectionEvent::Gatt { event } => {
                if let GattEvent::Write(w) = &event
                    && w.handle() == rx
                {
                    let frame = w.with_data(|_, data| data.to_vec());
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
