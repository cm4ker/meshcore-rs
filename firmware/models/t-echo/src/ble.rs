//! BLE companion interface on the nRF52840, as upstream's `SerialBLEInterface`: the Nordic UART
//! service, frames written to RX, answers notified on TX, over Nordic's SoftDevice Controller
//! (the SoftDevice in flash stays unused). Both characteristics need an authenticated link: the
//! phone pairs with a passkey (the app's PIN, else a random one on the e-paper, as upstream), and
//! the bonds are kept in the store so a paired phone reconnects without asking again.
//!
//! The host stack is `meshcore_ble`, which takes and gives HCI packets. The controller has no
//! such packets for commands, only a function for each: [`command`] calls the one a packet names
//! and makes the event that answers it.

use alloc::string::String;
use alloc::vec::Vec;
use core::future::{pending, poll_fn};
use core::sync::atomic::Ordering;
use core::task::Poll;

use embassy_futures::select::{Either3, select3};
use embassy_time::{Duration, Instant, Timer};
use log::{info, warn};
use meshcore::companion::MAX_FRAME_SIZE;
use meshcore_ble::{Config, Event, Failure, Host, bonds_from_bytes, bonds_to_bytes};
use meshcore_fw_core::frames::{FRAMES_IN, Link};
use meshcore_fw_core::links::{
    BLE_CONNECTED, BLE_ENABLED, BLE_OUT, BLE_PASSKEY, BONDS_CHANGED, pairing_abandoned, pairing_begins, pairing_pause,
    pairing_succeeded,
};
use nrf_sdc::{SoftdeviceController, raw};

/// Gap between notifications; a frame is notified only once the controller has taken the one
/// before.
const NOTIFY_INTERVAL: Duration = Duration::from_millis(20);
/// The controller's largest message, behind the byte that says what it is.
const PACKET_MAX: usize = 1 + raw::HCI_MSG_BUFFER_MAX_SIZE as usize;
/// How often the switch on the device (BLE on or off) and the host's clock are looked at.
const TICK: Duration = Duration::from_millis(250);

pub struct Params {
    /// Advertised name: `MeshCore-<node name>`.
    pub name: String,
    /// Serialized bonds from the store.
    pub bonds: Vec<u8>,
    /// Fixed pairing PIN; `None` = a random one per pairing (shown on the screen).
    pub pin: Option<u32>,
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

/// IRK handed to phones when they bond: unique to the board (an all-zero IRK makes Windows refuse
/// a second board) and stable. The board keeps its static address: nothing is resolved with it.
fn identity_irk(address: &[u8; 6]) -> [u8; 16] {
    let mut h = [0u8; 32];
    meshcore::crypto::sha256(&mut h, &[b"MeshCore BLE IRK", address]);
    h[..16].try_into().unwrap()
}

/// Random bytes for keys, of the controller's generator (it owns the chip's): `false` if it gave
/// none.
fn try_random(out: &mut [u8]) -> bool {
    for chunk in out.chunks_mut(8) {
        let mut number = raw::sdc_hci_cmd_le_rand_return_t { random_number: 0 };
        // SAFETY: the controller is up for as long as the BLE task, the only caller, runs
        if unsafe { raw::sdc_hci_cmd_le_rand(&mut number) } != 0 {
            return false;
        }
        let number = number.random_number;
        chunk.copy_from_slice(&number.to_le_bytes()[..chunk.len()]);
    }
    true
}

fn random(out: &mut [u8]) {
    // it gave some when the task began; should it ever falter, asked again rather than keys made
    // of nothing
    while !try_random(out) {}
}

#[embassy_executor::task]
pub async fn ble_task(sdc: SoftdeviceController<'static>, params: Params) {
    if !try_random(&mut [0; 8]) {
        warn!("BLE: the controller gives no random numbers");
        return;
    }
    let address = static_address();
    let mut host = Host::new(Config {
        name: params.name,
        address,
        irk: identity_irk(&address),
        passkey: params.pin,
        bonds: bonds_from_bytes(&params.bonds),
        frame_max: MAX_FRAME_SIZE,
        random,
    });
    let mut packet = [0u8; PACKET_MAX];
    let mut connected = false;
    // the pairing under way was counted against the tries at the passkey
    let mut counted = false;
    let mut notify_at = Instant::now();
    loop {
        // switched off on the device: no advertising, and the app is dropped
        host.enable(BLE_ENABLED.load(Ordering::Relaxed));
        host.tick(Instant::now().as_secs() as u32);
        while let Some(out) = host.transmit() {
            trace(">", &out);
            if out[0] == 1 {
                let (answer, len) = command(&out);
                trace("<", &answer[..len]);
                host.receive(&answer[..len]);
            } else if let Err(e) = sdc.hci_data_put(&out[1..]) {
                // the host sends no more than the controller has said it has room for
                warn!("BLE: data not taken: {e:?}");
            }
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
                    let (failed, wait) = pairing_pause();
                    counted = pairing_begins();
                    if counted {
                        info!("BLE: pairing begins, {failed} before it did not end paired");
                        BLE_PASSKEY.store(key.max(1), Ordering::Relaxed);
                    } else {
                        warn!("BLE: pairing refused after {failed} that did not end paired, {wait} s to wait");
                        host.disconnect();
                    }
                }
                Event::Paired { bonded } => {
                    info!("BLE: paired");
                    BLE_PASSKEY.store(0, Ordering::Relaxed);
                    counted = false;
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
                Event::PairingFailed { why, passkey_entered } => {
                    warn!("BLE: pairing failed: {why:?}, passkey entered: {passkey_entered}");
                    BLE_PASSKEY.store(0, Ordering::Relaxed);
                    // one nobody typed a passkey into is no failed try at it
                    if counted && !passkey_entered {
                        pairing_abandoned();
                    }
                    counted = false;
                    // A phone that takes its own question back stays: it asks again when its
                    // user is there. (Dropped, it comes back within the second, asks and takes
                    // the question back at once, over and over.)
                    if !matches!(why, Failure::Peer(_)) || passkey_entered {
                        host.disconnect();
                    }
                }
                Event::Written(frame) => {
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
        match select3(read(&sdc, &mut packet), frame, Timer::after(TICK)).await {
            Either3::First(len) => {
                trace("<", &packet[..len]);
                host.receive(&packet[..len]);
            }
            // not authenticated yet: upstream drops pushes then too
            Either3::Second(frame) => {
                if BLE_CONNECTED.load(Ordering::Relaxed) && host.notify(&frame) {
                    notify_at = Instant::now() + NOTIFY_INTERVAL;
                }
            }
            Either3::Third(()) => {}
        }
    }
}

/// In a diagnostic build (MCFW_USB_LOG set when it is built), the log's line for a packet to (`>`)
/// or from (`<`) the controller: commands and events whole, of data the Security Manager's and
/// the attribute protocol's first bytes. Not what comes all the time: the counts of packets sent,
/// the app's frames (writes, their answers, notifications).
fn trace(way: &str, packet: &[u8]) {
    if option_env!("MCFW_USB_LOG").is_none() {
        return;
    }
    match packet {
        [4, 0x13, ..] | [2, _, _, _, _, _, _, 4, 0, 0x13, ..] => {}
        // a frame of the app's (written to RX) or for it (notified on TX): its code and length
        [2, _, _, _, _, len, _, 4, 0, 0x12 | 0x52, 9, 0, code, ..] | [2, _, _, _, _, len, _, 4, 0, 0x1B, 11, 0, code, ..] => {
            info!("BLE {way} frame {code:#04x}, {} bytes", len.saturating_sub(3))
        }
        [1 | 4, rest @ ..] => info!("BLE {way} hci {:02x?}", &rest[..rest.len().min(24)]),
        // the start of an L2CAP PDU: `[2][handle 2][length 2][PDU length 2][channel 2][PDU]`
        [2, _, flags, _, _, _, _, channel @ (4 | 6), 0, pdu @ ..] if flags >> 4 & 3 != 1 => {
            info!("BLE {way} {} {:02x?}", if *channel == 6 { "smp" } else { "att" }, &pdu[..pdu.len().min(8)])
        }
        _ => {}
    }
}

/// The controller's next event or data, as an HCI packet with its type first: the length of it
/// in `packet`. Nothing is taken while this waits, so the wait may be given up for something else.
async fn read(sdc: &SoftdeviceController<'_>, packet: &mut [u8; PACKET_MAX]) -> usize {
    poll_fn(|cx| {
        // the waker first, then the look: what comes in between wakes the task
        sdc.register_waker(cx.waker());
        let mut kind: raw::sdc_hci_msg_type_t = 0;
        // SAFETY: behind the type byte the buffer is as long as the controller's largest message
        if unsafe { raw::sdc_hci_get(packet[1..].as_mut_ptr(), (&mut kind) as *mut _ as *mut u8) } != 0 {
            return Poll::Pending;
        }
        let (indicator, len) = match kind {
            raw::SDC_HCI_MSG_TYPE_EVT => (4, 2 + packet[2] as usize),
            _ => (2, 4 + u16::from_le_bytes([packet[3], packet[4]]) as usize),
        };
        packet[0] = indicator;
        Poll::Ready(1 + len)
    })
    .await
}

/// A command of the host's (`[1][opcode 2][length][parameters]`), done by the controller's
/// function for it: the Command Complete or Command Status event that answers it, and its length.
fn command(packet: &[u8]) -> ([u8; 16], usize) {
    /// `call` with the parameters as the controller's structure for them, which has their layout
    /// on the wire; "invalid parameters" for too few of them.
    fn with<T>(parameters: &[u8], call: impl FnOnce(*const T) -> u8) -> u8 {
        if parameters.len() < size_of::<T>() { 0x12 } else { call(parameters.as_ptr().cast()) }
    }
    let opcode = u16::from_le_bytes([packet[1], packet[2]]);
    let p = &packet[4..];
    // what a command gives back beside its status, and how much of it
    let mut back = [0u8; 8];
    let mut back_len = 0;
    let mut complete = true;
    // SAFETY: each function gets a pointer to as many bytes as its structure has (`with`), or to
    // `back`, larger than any of the return structures; the structures are packed, so any
    // address does
    let status = unsafe {
        match opcode {
            0x0C03 => raw::sdc_hci_cmd_cb_reset(),
            0x0C01 => with(p, |p| raw::sdc_hci_cmd_cb_set_event_mask(p)),
            0x2001 => with(p, |p| raw::sdc_hci_cmd_le_set_event_mask(p)),
            0x2002 => {
                back_len = size_of::<raw::sdc_hci_cmd_le_read_buffer_size_return_t>();
                raw::sdc_hci_cmd_le_read_buffer_size(back.as_mut_ptr().cast())
            }
            0x2005 => with(p, |p| raw::sdc_hci_cmd_le_set_random_address(p)),
            0x2006 => with(p, |p| raw::sdc_hci_cmd_le_set_adv_params(p)),
            0x2008 => with(p, |p| raw::sdc_hci_cmd_le_set_adv_data(p)),
            0x2009 => with(p, |p| raw::sdc_hci_cmd_le_set_scan_response_data(p)),
            0x200A => with(p, |p| raw::sdc_hci_cmd_le_set_adv_enable(p)),
            0x201A => {
                back_len = size_of::<raw::sdc_hci_cmd_le_long_term_key_request_reply_return_t>();
                with(p, |p| raw::sdc_hci_cmd_le_long_term_key_request_reply(p, back.as_mut_ptr().cast()))
            }
            0x201B => {
                back_len = size_of::<raw::sdc_hci_cmd_le_long_term_key_request_negative_reply_return_t>();
                with(p, |p| raw::sdc_hci_cmd_le_long_term_key_request_negative_reply(p, back.as_mut_ptr().cast()))
            }
            // only begun: an event of its own ends it
            0x0406 => {
                complete = false;
                with(p, |p| raw::sdc_hci_cmd_lc_disconnect(p))
            }
            // LE Connection Update: not in the peripheral's build of the controller. Told so,
            // the host asks the phone over L2CAP instead.
            0x2013 => {
                complete = false;
                0x01
            }
            _ => 0x01, // unknown command
        }
    };
    let mut event = [0u8; 16];
    let op = opcode.to_le_bytes();
    if complete {
        event[..7].copy_from_slice(&[4, 0x0E, 4 + back_len as u8, 1, op[0], op[1], status]);
        event[7..7 + back_len].copy_from_slice(&back[..back_len]);
        (event, 7 + back_len)
    } else {
        event[..7].copy_from_slice(&[4, 0x0F, 4, status, 1, op[0], op[1]]);
        (event, 7)
    }
}
