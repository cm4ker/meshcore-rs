//! The repeater's BLE update window (`fw ble`, as on the Heltec boards) over the SoftDevice (`sd`),
//! which runs only while the window is open: the Nordic UART service, frames written to RX,
//! answers notified on TX (`Repeater::handle_link_frame` serves them: firmware updates and the
//! device query). Both need a link paired with the window's PIN, LE Secure Connections with a
//! passkey the board shows nowhere but in the reply to `fw ble`.
//!
//! Bonds live in RAM until the next restart: a PC that paired in an earlier window of this boot
//! reconnects without the PIN; after a restart the board knows no bond, so the PC's old one has to
//! go (`tools/ota-app/ota_app.py --pin` pairs afresh by itself).

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_futures::select::{Either4, select4};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::Timer;
use log::{info, warn};
use meshcore::ota::REQ_TYPE_OTA_BEGIN;
use meshcore_fw_core::links::{BLE_CONNECTED, BLE_OUT, pairing_begins, pairing_succeeded};
use p256::elliptic_curve::point::AffineCoordinates;
use p256::elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};
use p256::{AffinePoint, EncodedPoint, NonZeroScalar, ProjectivePoint};

use crate::sd::{self, NRF_ERROR_NO_MEM, NRF_ERROR_RESOURCES, NRF_SUCCESS, svc};

pub struct Params {
    /// Advertised name: `MeshCore-OTA-<node name>`.
    pub name: String,
    /// The window's pairing PIN (6 digits).
    pub pin: u32,
}

static START: Signal<CriticalSectionRawMutex, Params> = Signal::new();
static STOP: Signal<CriticalSectionRawMutex, ()> = Signal::new();
/// The window is open (the SoftDevice runs).
pub static RUNNING: AtomicBool = AtomicBool::new(false);
/// Frames from the app, for the main loop.
pub static FRAMES_IN: Channel<CriticalSectionRawMutex, Vec<u8>, 4> = Channel::new();

/// Open the window, or give an open one a new PIN.
pub fn start(params: Params) {
    START.signal(params);
}

/// Close the window: the link drops and the SoftDevice stops.
pub fn stop() {
    STOP.signal(());
}

// ---------------------------------------------------------------- the SoftDevice's interface

const SD_BLE_ENABLE: u8 = 0x60;
const SD_BLE_EVT_GET: u8 = 0x61;
const SD_BLE_UUID_VS_ADD: u8 = 0x62;
const SD_BLE_USER_MEM_REPLY: u8 = 0x66;
const SD_BLE_OPT_SET: u8 = 0x67;
const SD_BLE_CFG_SET: u8 = 0x69;
const SD_BLE_GAP_ADV_SET_CONFIGURE: u8 = 0x72;
const SD_BLE_GAP_ADV_START: u8 = 0x73;
const SD_BLE_GAP_CONN_PARAM_UPDATE: u8 = 0x75;
const SD_BLE_GAP_DISCONNECT: u8 = 0x76;
const SD_BLE_GAP_TX_POWER_SET: u8 = 0x77;
const SD_BLE_GAP_DEVICE_NAME_SET: u8 = 0x7C;
const SD_BLE_GAP_SEC_PARAMS_REPLY: u8 = 0x7F;
const SD_BLE_GAP_LESC_DHKEY_REPLY: u8 = 0x81;
const SD_BLE_GAP_SEC_INFO_REPLY: u8 = 0x86;
const SD_BLE_GAP_PHY_UPDATE: u8 = 0x8F;
const SD_BLE_GAP_DATA_LENGTH_UPDATE: u8 = 0x90;
const SD_BLE_GATTS_SERVICE_ADD: u8 = 0xA8;
const SD_BLE_GATTS_CHARACTERISTIC_ADD: u8 = 0xAA;
const SD_BLE_GATTS_HVX: u8 = 0xAE;
const SD_BLE_GATTS_RW_AUTHORIZE_REPLY: u8 = 0xB0;
const SD_BLE_GATTS_SYS_ATTR_SET: u8 = 0xB1;
const SD_BLE_GATTS_EXCHANGE_MTU_REPLY: u8 = 0xB5;

const BLE_EVT_USER_MEM_REQUEST: u16 = 0x01;
const BLE_GAP_EVT_CONNECTED: u16 = 0x10;
const BLE_GAP_EVT_DISCONNECTED: u16 = 0x11;
const BLE_GAP_EVT_CONN_PARAM_UPDATE: u16 = 0x12;
const BLE_GAP_EVT_SEC_PARAMS_REQUEST: u16 = 0x13;
const BLE_GAP_EVT_SEC_INFO_REQUEST: u16 = 0x14;
const BLE_GAP_EVT_LESC_DHKEY_REQUEST: u16 = 0x18;
const BLE_GAP_EVT_AUTH_STATUS: u16 = 0x19;
const BLE_GAP_EVT_CONN_SEC_UPDATE: u16 = 0x1A;
const BLE_GAP_EVT_PHY_UPDATE_REQUEST: u16 = 0x21;
const BLE_GAP_EVT_DATA_LENGTH_UPDATE_REQUEST: u16 = 0x23;
const BLE_GATTS_EVT_WRITE: u16 = 0x50;
const BLE_GATTS_EVT_RW_AUTHORIZE_REQUEST: u16 = 0x51;
const BLE_GATTS_EVT_SYS_ATTR_MISSING: u16 = 0x52;
const BLE_GATTS_EVT_EXCHANGE_MTU_REQUEST: u16 = 0x55;
const BLE_GATTS_EVT_TIMEOUT: u16 = 0x56;

const BLE_COMMON_CFG_VS_UUID: u32 = 0x01;
const BLE_CONN_CFG_GAP: u32 = 0x20;
const BLE_CONN_CFG_GATTS: u32 = 0x22;
const BLE_CONN_CFG_GATT: u32 = 0x23;
const BLE_GAP_CFG_ROLE_COUNT: u32 = 0x40;
const BLE_GAP_OPT_PASSKEY: u32 = 0x22;

const NO_CONN: u16 = 0xFFFF;
/// Our connection configuration (`BLE_CONN_CFG_TAG_DEFAULT` cannot be configured).
const CONN_TAG: u8 = 1;
const ATT_MTU: u16 = 247;
/// Connection event length, 1.25 ms units: 7.5 ms, for several packets per event.
const EVENT_LENGTH: u16 = 6;
const TX_DBM: i8 = 8;
/// Advertising interval, 0.625 ms units: 100 ms.
const ADV_INTERVAL: u32 = 160;
/// Connection interval asked for during a firmware upload (1.25 ms units: 7.5..15 ms), and a
/// supervision timeout as on the ESP32 boards (10 ms units).
static UPLOAD_PARAMS: [u16; 4] = [6, 12, 0, 2000];
const MAX_BONDS: usize = 4;

/// Security mode 1 level 3: encrypted, with MITM protection (the passkey).
const SEC_AUTHENTICATED: u8 = 0x31;
const SEC_OPEN: u8 = 0x11;
const SEC_NONE: u8 = 0x00;

const BLE_GATTS_OP_WRITE_REQ: u8 = 1;
const BLE_GATTS_AUTHORIZE_TYPE_WRITE: u8 = 2;
const BLE_GATT_STATUS_ATTERR_REQUEST_NOT_SUPPORTED: u16 = 0x0106;
const BLE_GAP_SEC_STATUS_PAIRING_NOT_SUPP: u8 = 0x85;
const BLE_HCI_REMOTE_USER_TERMINATED_CONNECTION: u8 = 0x13;

/// The Nordic UART service's base UUID, little-endian (bytes 12..14 take the 16-bit part).
static NUS_BASE: [u8; 16] = [0x9E, 0xCA, 0xDC, 0x24, 0x0E, 0xE5, 0xA9, 0xE0, 0x93, 0xF3, 0xA3, 0xB5, 0x00, 0x00, 0x40, 0x6E];
const NUS_SERVICE: u16 = 0x0001;
const NUS_RX: u16 = 0x0002;
const NUS_TX: u16 = 0x0003;

// The structures the calls take (ble_gatts.h, ble_gap.h); bit fields are bytes here, LSB first.

#[repr(C)]
struct Uuid {
    uuid: u16,
    kind: u8,
}

#[repr(C)]
struct AttrMd {
    read_perm: u8,
    write_perm: u8,
    /// vlen:1, vloc:2, rd_auth:1, wr_auth:1
    flags: u8,
}

#[repr(C)]
struct Attr {
    uuid: *const Uuid,
    md: *const AttrMd,
    init_len: u16,
    init_offs: u16,
    max_len: u16,
    value: *mut u8,
}

#[repr(C)]
struct CharMd {
    props: u8,
    ext_props: u8,
    user_desc: *const u8,
    user_desc_max_size: u16,
    user_desc_size: u16,
    char_pf: *const u8,
    user_desc_md: *const AttrMd,
    cccd_md: *const AttrMd,
    sccd_md: *const AttrMd,
}

#[repr(C)]
#[derive(Default)]
struct CharHandles {
    value: u16,
    user_desc: u16,
    cccd: u16,
    sccd: u16,
}

#[repr(C)]
struct AdvData {
    adv: *const u8,
    adv_len: u16,
    scan: *const u8,
    scan_len: u16,
}

#[repr(C)]
struct AdvParams {
    kind: u8,
    props: u8,
    peer_addr: *const u8,
    interval: u32,
    duration: u16,
    max_adv_evts: u8,
    channel_mask: [u8; 5],
    filter_policy: u8,
    primary_phy: u8,
    secondary_phy: u8,
    set_id: u8,
}

#[repr(C)]
struct HvxParams {
    handle: u16,
    kind: u8,
    offset: u16,
    len: *mut u16,
    data: *const u8,
}

#[repr(C)]
struct AuthorizeReply {
    kind: u8,
    _pad: [u8; 3],
    gatt_status: u16,
    update: u8,
    offset: u16,
    len: u16,
    data: *const u8,
}

/// `ble_gap_enc_key_t`: the LTK and its flags (`ble_gap_enc_info_t`, 17 bytes), EDIV and Rand.
#[repr(C)]
#[derive(Clone, Copy)]
struct EncKey {
    enc_info: [u8; 17],
    _pad: u8,
    ediv: u16,
    rand: [u8; 8],
}

/// `ble_gap_id_key_t`: IRK and identity address (`ble_gap_addr_t`, 7 bytes).
#[repr(C)]
struct IdKey {
    irk: [u8; 16],
    addr: [u8; 7],
}

#[repr(C)]
struct KeySet {
    own_enc: *mut EncKey,
    own_id: *mut IdKey,
    own_sign: *mut u8,
    own_pk: *mut [u8; 64],
    peer_enc: *mut EncKey,
    peer_id: *mut IdKey,
    peer_sign: *mut u8,
    peer_pk: *mut [u8; 64],
}

const _: () = {
    assert!(size_of::<Attr>() == 20 && size_of::<CharMd>() == 28 && size_of::<AdvData>() == 16);
    assert!(size_of::<AdvParams>() == 24 && size_of::<HvxParams>() == 16 && size_of::<AuthorizeReply>() == 16);
    assert!(size_of::<EncKey>() == 28 && size_of::<KeySet>() == 32);
};

/// What the SoftDevice reads or writes after a call has returned: kept in place, used by the BLE
/// task only.
#[repr(C, align(4))]
struct Shared {
    /// One event (`BLE_EVT_LEN_MAX` of the ATT MTU fits).
    event: [u8; 512],
    adv: [u8; 31],
    scan: [u8; 31],
    passkey: [u8; 6],
    own_enc: EncKey,
    own_pk: [u8; 64],
    peer_id: IdKey,
    peer_pk: [u8; 64],
}

struct SharedCell(core::cell::UnsafeCell<Shared>);

// SAFETY: the BLE task alone touches it (`shared`)
unsafe impl Sync for SharedCell {}

static SHARED: SharedCell = SharedCell(core::cell::UnsafeCell::new(Shared {
    event: [0; 512],
    adv: [0; 31],
    scan: [0; 31],
    passkey: [0; 6],
    own_enc: EncKey { enc_info: [0; 17], _pad: 0, ediv: 0, rand: [0; 8] },
    own_pk: [0; 64],
    peer_id: IdKey { irk: [0; 16], addr: [0; 7] },
    peer_pk: [0; 64],
}));

fn shared() -> &'static mut Shared {
    // SAFETY: the BLE task only, one reference at a time
    unsafe { &mut *SHARED.0.get() }
}

/// A call with pointer arguments.
macro_rules! sd_call {
    ($n:expr $(, $a:expr)*) => {{
        let args: [u32; 4] = {
            let mut v = [0u32; 4];
            let given = [$($a as u32),*];
            v[..given.len()].copy_from_slice(&given);
            v
        };
        // SAFETY: the arguments are what the call takes, their memory outlives it (or is `Shared`)
        unsafe { svc::<{ $n }>(args[0], args[1], args[2], args[3]) }
    }};
}

fn check(what: &str, err: u32) -> Result<(), ()> {
    if err == NRF_SUCCESS {
        return Ok(());
    }
    warn!("BLE: {what} failed ({err:#x})");
    Err(())
}

// ---------------------------------------------------------------- the window

/// A peer that bonded during this boot: its address (`ble_gap_addr_t`) and the key its links use.
#[derive(Clone, Copy)]
struct Bond {
    addr: [u8; 7],
    key: EncKey,
}

struct Window {
    conn: u16,
    rx: u16,
    tx: u16,
    adv_handle: u8,
    peer: [u8; 7],
    authenticated: bool,
    secret: Option<NonZeroScalar>,
}

#[embassy_executor::task]
pub async fn task() -> ! {
    let mut bonds: Vec<Bond> = Vec::new();
    loop {
        let params = START.wait().await;
        STOP.reset();
        RUNNING.store(true, Ordering::Relaxed);
        let mut w = Window { conn: NO_CONN, rx: 0, tx: 0, adv_handle: 0xFF, peer: [0; 7], authenticated: false, secret: None };
        if open(&params, &mut w).is_ok() {
            info!("BLE: advertising as '{}'", params.name);
            serve(&mut w, &mut bonds).await;
        }
        sd::disable();
        BLE_CONNECTED.store(false, Ordering::Relaxed);
        RUNNING.store(false, Ordering::Relaxed);
        info!("BLE: off");
    }
}

fn cfg_set(id: u32, bytes: &[u8], ram: u32) -> Result<(), ()> {
    let mut cfg = [0u32; 4];
    // SAFETY: plain bytes into plain words
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), cfg.as_mut_ptr().cast::<u8>(), bytes.len()) };
    check("configuration", sd_call!(SD_BLE_CFG_SET, id, cfg.as_ptr(), ram))
}

/// The start of the app's RAM (memory.x): the SoftDevice's tables must end below it.
fn app_ram_base() -> u32 {
    unsafe extern "C" {
        static __sdata: u32;
    }
    // only its address is taken
    &raw const __sdata as u32
}

fn open(params: &Params, w: &mut Window) -> Result<(), ()> {
    if let Err(e) = sd::enable() {
        warn!("BLE: the SoftDevice did not start ({e:#x})");
        return Err(());
    }
    crate::console::record_crash("BLE step: configuration");
    let ram = app_ram_base();
    let [ev_lo, ev_hi] = EVENT_LENGTH.to_le_bytes();
    let [mtu_lo, mtu_hi] = ATT_MTU.to_le_bytes();
    cfg_set(BLE_CONN_CFG_GAP, &[CONN_TAG, 0, 1, 0, ev_lo, ev_hi], ram)?;
    cfg_set(BLE_CONN_CFG_GATT, &[CONN_TAG, 0, mtu_lo, mtu_hi], ram)?;
    cfg_set(BLE_CONN_CFG_GATTS, &[CONN_TAG, 0, 2], ram)?;
    // one advertiser, one peripheral link, no central role
    cfg_set(BLE_GAP_CFG_ROLE_COUNT, &[1, 1, 0, 0, 0], ram)?;
    cfg_set(BLE_COMMON_CFG_VS_UUID, &[1], ram)?;
    let mut need = ram;
    crate::console::record_crash("BLE step: ble enable");
    match sd_call!(SD_BLE_ENABLE, &raw mut need) {
        NRF_SUCCESS => {}
        NRF_ERROR_NO_MEM => {
            warn!("BLE: the SoftDevice needs RAM up to {need:#x}, the app starts at {ram:#x}");
            return Err(());
        }
        e => return check("enable", e),
    }

    crate::console::record_crash("BLE step: gatt");
    let mut kind = 0u8;
    check("UUID", sd_call!(SD_BLE_UUID_VS_ADD, NUS_BASE.as_ptr(), &raw mut kind))?;
    let mut service = 0u16;
    let uuid = Uuid { uuid: NUS_SERVICE, kind };
    check("service", sd_call!(SD_BLE_GATTS_SERVICE_ADD, 1 /* primary */, &raw const uuid, &raw mut service))?;
    // RX: write requests are held (authorized) until the main loop has room for the frame
    let rx_md = AttrMd { read_perm: SEC_NONE, write_perm: SEC_AUTHENTICATED, flags: 0b1_0011 };
    w.rx = add_char(service, kind, NUS_RX, 0b1100 /* write, write without response */, &rx_md, None)?;
    let tx_md = AttrMd { read_perm: SEC_AUTHENTICATED, write_perm: SEC_NONE, flags: 0b0011 };
    let cccd_md = AttrMd { read_perm: SEC_OPEN, write_perm: SEC_AUTHENTICATED, flags: 0b0010 };
    w.tx = add_char(service, kind, NUS_TX, 0b1_0010 /* read, notify */, &tx_md, Some(&cccd_md))?;

    let name = truncate(&params.name, 29);
    let no_access = SEC_NONE;
    check("name", sd_call!(SD_BLE_GAP_DEVICE_NAME_SET, &raw const no_access, name.as_ptr(), name.len()))?;
    set_passkey(params.pin);
    crate::console::record_crash("BLE step: lesc key");
    w.secret = lesc_keypair();
    if w.secret.is_none() {
        warn!("BLE: no key for LE Secure Connections");
        return Err(());
    }

    // flags, the service's UUID; the name in the scan response
    let sh = shared();
    sh.adv[..3].copy_from_slice(&[2, 0x01, 0x06]);
    sh.adv[3..5].copy_from_slice(&[17, 0x07]);
    sh.adv[5..21].copy_from_slice(&NUS_BASE);
    sh.adv[5 + 12..5 + 14].copy_from_slice(&NUS_SERVICE.to_le_bytes());
    sh.scan[0] = name.len() as u8 + 1;
    sh.scan[1] = 0x09;
    sh.scan[2..2 + name.len()].copy_from_slice(name.as_bytes());
    crate::console::record_crash("BLE step: advertise");
    let r = advertise(w, 21, 2 + name.len());
    let _ = crate::console::take_crash();
    r
}

fn truncate(s: &str, max: usize) -> &str {
    let mut n = s.len().min(max);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

fn add_char(service: u16, kind: u8, id: u16, props: u8, md: &AttrMd, cccd: Option<&AttrMd>) -> Result<u16, ()> {
    let uuid = Uuid { uuid: id, kind };
    let char_md = CharMd {
        props,
        ext_props: 0,
        user_desc: core::ptr::null(),
        user_desc_max_size: 0,
        user_desc_size: 0,
        char_pf: core::ptr::null(),
        user_desc_md: core::ptr::null(),
        cccd_md: cccd.map_or(core::ptr::null(), |c| c as *const AttrMd),
        sccd_md: core::ptr::null(),
    };
    let attr = Attr { uuid: &uuid, md, init_len: 1, init_offs: 0, max_len: ATT_MTU - 3, value: core::ptr::null_mut() };
    let mut handles = CharHandles::default();
    check("characteristic", sd_call!(SD_BLE_GATTS_CHARACTERISTIC_ADD, service, &raw const char_md, &raw const attr, &raw mut handles))?;
    Ok(handles.value)
}

fn set_passkey(pin: u32) {
    let sh = shared();
    let mut digits = pin % 1_000_000;
    for d in sh.passkey.iter_mut().rev() {
        *d = b'0' + (digits % 10) as u8;
        digits /= 10;
    }
    let opt = [sh.passkey.as_ptr() as u32, 0, 0, 0];
    let _ = check("passkey", sd_call!(SD_BLE_OPT_SET, BLE_GAP_OPT_PASSKEY, opt.as_ptr()));
}

fn advertise(w: &mut Window, adv_len: usize, scan_len: usize) -> Result<(), ()> {
    let sh = shared();
    let data = AdvData { adv: sh.adv.as_ptr(), adv_len: adv_len as u16, scan: sh.scan.as_ptr(), scan_len: scan_len as u16 };
    let params = AdvParams {
        kind: 0x01, // connectable, scannable, undirected
        props: 0,
        peer_addr: core::ptr::null(),
        interval: ADV_INTERVAL,
        duration: 0,
        max_adv_evts: 0,
        channel_mask: [0; 5],
        filter_policy: 0,
        primary_phy: 1,
        secondary_phy: 1,
        set_id: 0,
    };
    check("advertising", sd_call!(SD_BLE_GAP_ADV_SET_CONFIGURE, &raw mut w.adv_handle, &raw const data, &raw const params))?;
    let _ = sd_call!(SD_BLE_GAP_TX_POWER_SET, 1 /* advertiser */, w.adv_handle, TX_DBM);
    check("advertising start", sd_call!(SD_BLE_GAP_ADV_START, w.adv_handle, CONN_TAG))
}

/// Advertise again after a link has gone (the SoftDevice let go of the data buffers).
fn readvertise(w: &mut Window) {
    let sh = shared();
    let scan_len = sh.scan[0] as usize + 1;
    let _ = advertise(w, 21, scan_len);
}

async fn serve(w: &mut Window, bonds: &mut Vec<Bond>) {
    let mut pending: Option<Vec<u8>> = None;
    loop {
        while let Some(id) = next_event() {
            handle(w, bonds, id).await;
        }
        if let Some(frame) = pending.take()
            && !notify(w, &frame)
        {
            pending = Some(frame);
        }
        let can_send = pending.is_none() && w.conn != NO_CONN;
        let out = async {
            if can_send { BLE_OUT.receive().await } else { core::future::pending().await }
        };
        match select4(sd::EVENTS.wait(), STOP.wait(), START.wait(), out).await {
            Either4::First(()) => {}
            Either4::Second(()) => {
                if w.conn != NO_CONN {
                    let _ = sd_call!(SD_BLE_GAP_DISCONNECT, w.conn, BLE_HCI_REMOTE_USER_TERMINATED_CONNECTION);
                    Timer::after_millis(200).await; // the termination goes out
                }
                return;
            }
            Either4::Third(params) => set_passkey(params.pin),
            // an unauthenticated link gets nothing (upstream drops pushes then too)
            Either4::Fourth(frame) => {
                if w.authenticated && !notify(w, &frame) {
                    pending = Some(frame);
                }
            }
        }
    }
}

/// The next event into `Shared::event`: its ID.
fn next_event() -> Option<u16> {
    let sh = shared();
    let mut len = sh.event.len() as u16;
    (sd_call!(SD_BLE_EVT_GET, sh.event.as_mut_ptr(), &raw mut len) == NRF_SUCCESS).then(|| u16::from_le_bytes([sh.event[0], sh.event[1]]))
}

/// Notify a frame: `false` if the SoftDevice's queue is full (try again after it has sent some).
fn notify(w: &Window, frame: &[u8]) -> bool {
    let mut len = frame.len() as u16;
    let p = HvxParams { handle: w.tx, kind: 1 /* notification */, offset: 0, len: &raw mut len, data: frame.as_ptr() };
    match sd_call!(SD_BLE_GATTS_HVX, w.conn, &raw const p) {
        NRF_ERROR_RESOURCES => false,
        NRF_SUCCESS => true,
        e => {
            warn!("BLE: notify failed ({e:#x})");
            true
        }
    }
}

async fn handle(w: &mut Window, bonds: &mut Vec<Bond>, id: u16) {
    let ev = &shared().event;
    let u16_at = |i: usize| u16::from_le_bytes([ev[i], ev[i + 1]]);
    // GAP events: the connection at 4, the parameters from 8; GATTS events: from 6
    let conn = u16_at(4);
    match id {
        BLE_GAP_EVT_CONNECTED => {
            w.conn = conn;
            w.authenticated = false;
            w.peer.copy_from_slice(&ev[8..15]);
            while BLE_OUT.try_receive().is_ok() {} // frames for an earlier app
            let _ = sd_call!(SD_BLE_GAP_TX_POWER_SET, 3 /* connection */, conn, TX_DBM);
            info!("BLE: connected");
        }
        BLE_GAP_EVT_DISCONNECTED => {
            info!("BLE: disconnected (reason {:#04x})", ev[8]);
            w.conn = NO_CONN;
            w.authenticated = false;
            BLE_CONNECTED.store(false, Ordering::Relaxed);
            readvertise(w);
        }
        BLE_GAP_EVT_CONN_PARAM_UPDATE => {
            info!("BLE: interval {} x 1.25 ms, latency {}", u16_at(10), u16_at(12));
        }
        BLE_GAP_EVT_SEC_PARAMS_REQUEST => {
            if !pairing_begins() {
                warn!("BLE: pairing refused after failed ones, try later");
                let _ = sd_call!(SD_BLE_GAP_SEC_PARAMS_REPLY, conn, BLE_GAP_SEC_STATUS_PAIRING_NOT_SUPP, 0, 0);
                let _ = sd_call!(SD_BLE_GAP_DISCONNECT, conn, BLE_HCI_REMOTE_USER_TERMINATED_CONNECTION);
                return;
            }
            // bond, MITM, LE Secure Connections, display only; our LTK (legacy pairing) and the
            // peer's identity are exchanged
            let params: [u8; 5] = [0b0000_0111, 7, 16, 0b01, 0b10];
            let sh = shared();
            let keys = KeySet {
                own_enc: &raw mut sh.own_enc,
                own_id: core::ptr::null_mut(),
                own_sign: core::ptr::null_mut(),
                own_pk: &raw mut sh.own_pk,
                peer_enc: core::ptr::null_mut(),
                peer_id: &raw mut sh.peer_id,
                peer_sign: core::ptr::null_mut(),
                peer_pk: &raw mut sh.peer_pk,
            };
            // the key set has to stay put while pairing: the pointers in it do, it is copied
            let _ = check("pairing", sd_call!(SD_BLE_GAP_SEC_PARAMS_REPLY, conn, 0, params.as_ptr(), &raw const keys));
        }
        BLE_GAP_EVT_SEC_INFO_REQUEST => {
            let (addr, ediv, rand) = (&ev[8..15], u16_at(16), &ev[18..26]);
            let legacy = ediv != 0 || rand.iter().any(|&b| b != 0);
            let bond = bonds.iter().find(|b| {
                if legacy { b.key.ediv == ediv && b.key.rand == *rand } else { b.addr[1..] == addr[1..] && b.addr[0] >> 1 == addr[0] >> 1 }
            });
            let enc = bond.map_or(core::ptr::null(), |b| b.key.enc_info.as_ptr());
            let _ = sd_call!(SD_BLE_GAP_SEC_INFO_REPLY, conn, enc, 0, 0);
        }
        BLE_GAP_EVT_LESC_DHKEY_REQUEST => {
            // SAFETY: the SoftDevice points into the key set it was given (`peer_pk`)
            let peer = unsafe { *(u32::from_le_bytes(ev[8..12].try_into().unwrap()) as *const [u8; 64]) };
            // an invalid key gets a key that fails the check that follows
            let key = w.secret.as_ref().and_then(|s| dhkey(s, &peer)).unwrap_or([0; 32]);
            let _ = check("DH key", sd_call!(SD_BLE_GAP_LESC_DHKEY_REPLY, conn, key.as_ptr()));
        }
        BLE_GAP_EVT_AUTH_STATUS => {
            let (status, flags, levels, kdist_peer) = (ev[8], ev[9], ev[10], ev[13]);
            if status != 0 {
                warn!("BLE: pairing failed ({status:#04x})");
                return;
            }
            // levels: lv1..lv4 bits of security mode 1; 3 and 4 are authenticated
            if levels & 0b1100 != 0 {
                pairing_succeeded();
            }
            info!("BLE: paired{}", if flags & 0b1000 != 0 { " (LE Secure Connections)" } else { "" });
            if flags & 0b100 != 0 {
                let sh = shared();
                let addr = if kdist_peer & 0b10 != 0 { sh.peer_id.addr } else { w.peer };
                bonds.retain(|b| b.addr != addr);
                if bonds.len() >= MAX_BONDS {
                    bonds.remove(0);
                }
                bonds.push(Bond { addr, key: sh.own_enc });
            }
        }
        BLE_GAP_EVT_CONN_SEC_UPDATE => {
            // security mode 1 (low nibble), level 3 or 4 (high nibble): authenticated
            w.authenticated = ev[8] & 0x0F == 1 && ev[8] >> 4 >= 3;
            BLE_CONNECTED.store(w.authenticated, Ordering::Relaxed);
        }
        BLE_GAP_EVT_PHY_UPDATE_REQUEST => {
            let auto = [0u8, 0];
            let _ = sd_call!(SD_BLE_GAP_PHY_UPDATE, conn, auto.as_ptr());
        }
        BLE_GAP_EVT_DATA_LENGTH_UPDATE_REQUEST => {
            let _ = sd_call!(SD_BLE_GAP_DATA_LENGTH_UPDATE, conn, 0, 0);
        }
        BLE_EVT_USER_MEM_REQUEST => {
            // no queued (long) writes
            let _ = sd_call!(SD_BLE_USER_MEM_REPLY, conn, 0);
        }
        BLE_GATTS_EVT_SYS_ATTR_MISSING => {
            let _ = sd_call!(SD_BLE_GATTS_SYS_ATTR_SET, conn, 0, 0, 0);
        }
        BLE_GATTS_EVT_EXCHANGE_MTU_REQUEST => {
            let _ = sd_call!(SD_BLE_GATTS_EXCHANGE_MTU_REPLY, conn, ATT_MTU);
        }
        BLE_GATTS_EVT_TIMEOUT => {
            let _ = sd_call!(SD_BLE_GAP_DISCONNECT, conn, BLE_HCI_REMOTE_USER_TERMINATED_CONNECTION);
        }
        BLE_GATTS_EVT_WRITE => {
            // a write without response (the commit goes so): handle 6, length 16, data 18
            if u16_at(6) == w.rx {
                let frame = ev[18..18 + (u16_at(16) as usize).min(ev.len() - 18)].to_vec();
                take_frame(w, frame).await;
            }
        }
        BLE_GATTS_EVT_RW_AUTHORIZE_REQUEST => {
            // a write request: type 6, handle 8, op 14, offset 16, length 18, data 20
            if ev[6] != BLE_GATTS_AUTHORIZE_TYPE_WRITE {
                return;
            }
            let (op, offset, len) = (ev[14], u16_at(16), (u16_at(18) as usize).min(ev.len() - 20));
            let ok = op == BLE_GATTS_OP_WRITE_REQ && offset == 0 && u16_at(8) == w.rx;
            let frame = ev[20..20 + len].to_vec();
            // the write is answered once the frame is queued: a fast sender (a firmware upload)
            // goes at the main loop's pace
            if ok {
                take_frame(w, frame.clone()).await;
            }
            let reply = AuthorizeReply {
                kind: BLE_GATTS_AUTHORIZE_TYPE_WRITE,
                _pad: [0; 3],
                gatt_status: if ok { 0 } else { BLE_GATT_STATUS_ATTERR_REQUEST_NOT_SUPPORTED },
                update: ok as u8,
                offset,
                len: frame.len() as u16,
                data: frame.as_ptr(),
            };
            let _ = check("write reply", sd_call!(SD_BLE_GATTS_RW_AUTHORIZE_REPLY, conn, &raw const reply));
        }
        _ => {}
    }
}

async fn take_frame(w: &Window, frame: Vec<u8>) {
    if frame.first() == Some(&REQ_TYPE_OTA_BEGIN) {
        let _ = sd_call!(SD_BLE_GAP_CONN_PARAM_UPDATE, w.conn, UPLOAD_PARAMS.as_ptr());
    }
    if !frame.is_empty() {
        FRAMES_IN.send(frame).await;
    }
}

// ---------------------------------------------------------------- LE Secure Connections

/// A fresh P-256 key pair from the chip's RNG: the secret, and the public key as the SoftDevice
/// takes it (X then Y, each little-endian).
fn lesc_keypair() -> Option<NonZeroScalar> {
    let mut seed = [0u8; 32];
    for _ in 0..4 {
        if !sd::random(&mut seed) {
            return None;
        }
        let Some(secret) = Option::<NonZeroScalar>::from(NonZeroScalar::from_repr(seed.into())) else {
            continue; // not below the group order: rare
        };
        let point = (ProjectivePoint::GENERATOR * *secret).to_affine().to_encoded_point(false);
        let pk = &mut shared().own_pk;
        pk[..32].copy_from_slice(point.x()?);
        pk[32..].copy_from_slice(point.y()?);
        pk[..32].reverse();
        pk[32..].reverse();
        return Some(secret);
    }
    None
}

/// The Diffie-Hellman key with the peer's public key (little-endian halves), little-endian.
fn dhkey(secret: &NonZeroScalar, peer: &[u8; 64]) -> Option<[u8; 32]> {
    let mut x: [u8; 32] = peer[..32].try_into().unwrap();
    let mut y: [u8; 32] = peer[32..].try_into().unwrap();
    x.reverse();
    y.reverse();
    let point = EncodedPoint::from_affine_coordinates(&x.into(), &y.into(), false);
    let point = Option::<AffinePoint>::from(AffinePoint::from_encoded_point(&point))?;
    let mut key: [u8; 32] = (ProjectivePoint::from(point) * **secret).to_affine().x().into();
    key.reverse();
    Some(key)
}
