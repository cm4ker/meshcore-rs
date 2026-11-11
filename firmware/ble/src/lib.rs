//! A small Bluetooth LE host for the MeshCore firmware: the part of the stack above the
//! controller, for what the firmware does with it and no more.
//!
//! - one phone at a time, this device the peripheral, advertising when no phone is connected;
//! - the Nordic UART service: frames the app writes, frames it is notified of;
//! - LE Secure Connections pairing with a passkey the phone types in, and bonds: a paired phone
//!   comes back without pairing again, recognised behind its private address.
//!
//! The host does no I/O. It is handed the HCI packets of the controller ([`Host::receive`]) and
//! asked for the ones to send ([`Host::transmit`]), each as the UART transport frames them (the
//! packet type, then the packet), so the whole of it runs on any machine, a test included.

#![no_std]

extern crate alloc;

mod att;
pub mod crypto;
mod smp;
#[cfg(test)]
mod tests;

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

use att::Att;
use crypto::{Addr, flip};
use smp::Smp;

/// Bonds kept: the oldest goes when one more comes.
pub const MAX_BONDS: usize = 8;

/// A bond as kept in flash: `[kind][address 6][key 16][identity key 16][bonded][level]`.
pub const BOND_RECORD: usize = 41;

/// What a paired phone and this device keep of each other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bond {
    /// The phone's identity address: its type (0 public, 1 random) and the address as on the air.
    pub kind: u8,
    pub address: [u8; 6],
    /// The long term key, as the controller takes it.
    pub ltk: [u8; 16],
    /// The phone's identity resolving key, as on the air: what its private addresses are made
    /// with. Zero if it gave none.
    pub irk: [u8; 16],
    /// Paired with a passkey.
    pub authenticated: bool,
    /// Both sides wanted to keep it; else it lasts as long as the connection.
    pub bonded: bool,
}

impl Bond {
    /// The phone at `address` of type `kind`, as it connected, is this bond's.
    fn matches(&self, kind: u8, address: &[u8; 6]) -> bool {
        (self.kind == kind && self.address == *address)
            || (kind == 1 && self.irk != [0; 16] && crypto::resolves(&self.irk, address))
    }

    fn same_phone(&self, other: &Bond) -> bool {
        (self.kind == other.kind && self.address == other.address) || (self.irk != [0; 16] && self.irk == other.irk)
    }
}

/// The bonds to keep, as they go into flash.
pub fn bonds_to_bytes(bonds: &[Bond]) -> Vec<u8> {
    let mut out = Vec::new();
    for b in bonds.iter().filter(|b| b.bonded) {
        out.push(b.kind);
        out.extend_from_slice(&b.address);
        out.extend_from_slice(&b.ltk);
        out.extend_from_slice(&b.irk);
        out.push(1);
        out.push(if b.authenticated { 2 } else { 1 });
    }
    out
}

/// The bonds of [`bonds_to_bytes`].
pub fn bonds_from_bytes(data: &[u8]) -> Vec<Bond> {
    let records = data.as_chunks::<BOND_RECORD>().0.iter().map(|r| Bond {
        // an address the controller resolved has types 2 and 3 for 0 and 1
        kind: r[0] & 1,
        address: core::array::from_fn(|i| r[1 + i]),
        ltk: core::array::from_fn(|i| r[7 + i]),
        irk: core::array::from_fn(|i| r[23 + i]),
        authenticated: r[40] >= 2,
        bonded: true,
    });
    records.filter(|b| b.ltk != [0; 16]).collect()
}

/// What the device is and has.
pub struct Config {
    /// The name advertised; its first 22 bytes are the Device Name.
    pub name: String,
    /// The static random address.
    pub address: [u8; 6],
    /// The identity resolving key given to a bonding phone, as on the air: unique to the device
    /// (Windows refuses a second device with a key it has), though the address never changes.
    pub irk: [u8; 16],
    /// The passkey of pairing: a fixed one, or `None` for a random one per pairing, to be shown.
    pub passkey: Option<u32>,
    pub bonds: Vec<Bond>,
    /// The longest frame the app may write.
    pub frame_max: usize,
    /// Fills a buffer with random bytes, good enough for keys.
    pub random: fn(&mut [u8]),
}

/// What happened, for the firmware to act on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Connected,
    /// With the controller's reason: 0x08 the phone went silent, 0x13 it left, 0x16 this side left,
    /// 0x3D it has another key than this side.
    Disconnected(u8),
    /// A phone pairs: the passkey it asks its user for.
    Passkey(u32),
    PairingFailed,
    /// The phone paired, with the passkey. `bonded`: the bonds changed, keep them.
    Paired { bonded: bool },
    /// The link is encrypted; `true` if with a key of a pairing by passkey, which is what the
    /// service takes.
    Encrypted(bool),
    /// The app wrote a frame. Take it, then say so: [`Host::written`].
    Written(Vec<u8>),
}

/// What the layers above the link put out.
pub(crate) struct Out {
    /// L2CAP PDUs to send, each with its header.
    acl: VecDeque<Vec<u8>>,
    events: VecDeque<Event>,
}

impl Out {
    pub(crate) fn l2cap(&mut self, cid: u16, payload: &[u8]) {
        // a phone that asks and asks and takes no answer is not answered without end
        if self.acl.len() >= QUEUE_MAX {
            return;
        }
        let mut pdu = Vec::with_capacity(4 + payload.len());
        pdu.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        pdu.extend_from_slice(&cid.to_le_bytes());
        pdu.extend_from_slice(payload);
        self.acl.push_back(pdu);
    }

    pub(crate) fn event(&mut self, event: Event) {
        self.events.push_back(event);
    }
}

const COMMAND: u8 = 1;
const ACL: u8 = 2;
const EVENT: u8 = 4;

const DISCONNECT: u16 = 0x0406;
const SET_EVENT_MASK: u16 = 0x0C01;
const RESET: u16 = 0x0C03;
const READ_BUFFER_SIZE: u16 = 0x1005;
const LE_SET_EVENT_MASK: u16 = 0x2001;
const LE_READ_BUFFER_SIZE: u16 = 0x2002;
const LE_SET_RANDOM_ADDRESS: u16 = 0x2005;
const LE_SET_ADVERTISING_PARAMETERS: u16 = 0x2006;
const LE_SET_ADVERTISING_DATA: u16 = 0x2008;
const LE_SET_SCAN_RESPONSE_DATA: u16 = 0x2009;
const LE_SET_ADVERTISING_ENABLE: u16 = 0x200A;
const LE_CONNECTION_UPDATE: u16 = 0x2013;
const LE_LONG_TERM_KEY_REQUEST_REPLY: u16 = 0x201A;
const LE_LONG_TERM_KEY_REQUEST_NEGATIVE_REPLY: u16 = 0x201B;

const EVENT_DISCONNECTION_COMPLETE: u8 = 0x05;
const EVENT_ENCRYPTION_CHANGE: u8 = 0x08;
const EVENT_COMMAND_COMPLETE: u8 = 0x0E;
const EVENT_COMMAND_STATUS: u8 = 0x0F;
const EVENT_NUMBER_OF_COMPLETED_PACKETS: u8 = 0x13;
const EVENT_ENCRYPTION_KEY_REFRESH_COMPLETE: u8 = 0x30;
const EVENT_LE_META: u8 = 0x3E;
const LE_CONNECTION_COMPLETE: u8 = 0x01;
const LE_LONG_TERM_KEY_REQUEST: u8 = 0x05;

/// The L2CAP signalling channel of LE.
const SIGNALLING: u16 = 5;
/// The longest L2CAP PDU taken: an ATT PDU of the largest MTU, or a public key.
const PDU_MAX: usize = att::MTU_MAX;
/// The most data in a packet for the controller: what a link layer packet carries.
const ACL_MAX: usize = 251;
/// Commands, and PDUs for the phone, that may wait to be sent: far more than ever do.
const QUEUE_MAX: usize = 32;
/// The Device Name's length, as the table had it so far.
const NAME_MAX: usize = 22;

/// The connection parameters asked for during a firmware upload, where every chunk is a write
/// answered in a later connection event: 7.5 to 15 ms between events, and a supervision timeout
/// of 20 s, as committing stalls the node for half a minute of flash work.
const FAST_INTERVAL: (u16, u16) = (6, 12);
const FAST_TIMEOUT: u16 = 2000;

struct Connection {
    handle: u16,
    /// The phone's address as it connected: what pairing computes with.
    peer: Addr,
    /// The bond of the phone: found by its address, or made on this connection.
    bond: Option<Bond>,
    /// The key the controller was last given is of a pairing by passkey.
    key_authenticated: bool,
    /// The link is encrypted with such a key.
    authenticated: bool,
    att: Att,
    smp: Smp,
    /// An L2CAP PDU arriving in parts.
    incoming: Vec<u8>,
}

/// The host: see the crate's description.
pub struct Host {
    config: Config,
    out: Out,
    /// Commands to send, one at a time: the controller says when it takes the next.
    commands: VecDeque<Vec<u8>>,
    command_allowed: bool,
    /// The controller's buffers for data: the size of one, how many there are and how many of
    /// them are free.
    acl_size: usize,
    acl_buffers: usize,
    acl_free: usize,
    /// The L2CAP PDU going out in parts, and how much of it went.
    outgoing: Option<(Vec<u8>, usize)>,
    connection: Option<Connection>,
    /// What the last bonded phone asked of TX: it comes back expecting that kept.
    kept_cccd: Option<(Bond, u16)>,
    /// A signalling request's identifier: any but zero.
    identifier: u8,
    now: u32,
}

/// The state of the host in a line, for a log: the connection, and what waits to be sent.
impl core::fmt::Debug for Host {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match &self.connection {
            Some(c) => write!(
                f,
                "phone {:02x?} bond {} authenticated {} mtu {} cccd {}",
                c.peer,
                c.bond.is_some(),
                c.authenticated,
                c.att.mtu,
                c.att.cccd
            )?,
            None => f.write_str("no phone")?,
        }
        write!(
            f,
            ", buffers {}/{} of {}, commands {} (next {}), pdus {}",
            self.acl_free,
            self.acl_buffers,
            self.acl_size,
            self.commands.len(),
            if self.command_allowed { "taken" } else { "waits" },
            self.out.acl.len() + self.outgoing.is_some() as usize
        )
    }
}

impl Host {
    /// A host that starts the controller over and advertises.
    pub fn new(config: Config) -> Self {
        let mut host = Self {
            config,
            out: Out { acl: VecDeque::new(), events: VecDeque::new() },
            commands: VecDeque::new(),
            command_allowed: true,
            acl_size: 0,
            acl_buffers: 0,
            acl_free: 0,
            outgoing: None,
            connection: None,
            kept_cccd: None,
            identifier: 0,
            now: 0,
        };
        host.command(RESET, &[]);
        // the events of a connection: disconnection, encryption change and key refresh, and
        // those of LE, of which: connection complete, connection update, long term key request
        host.command(SET_EVENT_MASK, &[0x90, 0, 0, 0, 0, 0x80, 0, 0x20]);
        host.command(LE_SET_EVENT_MASK, &[0x15, 0, 0, 0, 0, 0, 0, 0]);
        host.command(LE_READ_BUFFER_SIZE, &[]);
        let address = host.config.address;
        host.command(LE_SET_RANDOM_ADDRESS, &address);
        host.advertise();
        host
    }

    fn command(&mut self, opcode: u16, parameters: &[u8]) {
        // a controller that takes no commands any more is not kept a pile of them
        if self.commands.len() >= QUEUE_MAX {
            return;
        }
        let mut packet = Vec::with_capacity(4 + parameters.len());
        packet.push(COMMAND);
        packet.extend_from_slice(&opcode.to_le_bytes());
        packet.push(parameters.len() as u8);
        packet.extend_from_slice(parameters);
        self.commands.push_back(packet);
    }

    /// Advertise, connectable by anyone: the service, and the name for those who ask.
    fn advertise(&mut self) {
        // every 160 ms on all three channels, from the random address
        self.command(LE_SET_ADVERTISING_PARAMETERS, &[0x00, 0x01, 0x00, 0x01, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0x07, 0x00]);
        let mut data = [0u8; 32];
        // flags: general discoverable, LE only; then the complete list of 128-bit services
        data[..6].copy_from_slice(&[21, 2, 0x01, 0x06, 17, 0x07]);
        data[6..22].copy_from_slice(&att::UART);
        self.command(LE_SET_ADVERTISING_DATA, &data);
        let name = truncate(&self.config.name, 29).as_bytes();
        let mut data = [0u8; 32];
        data[..3].copy_from_slice(&[name.len() as u8 + 2, name.len() as u8 + 1, 0x09]);
        data[3..3 + name.len()].copy_from_slice(name);
        self.command(LE_SET_SCAN_RESPONSE_DATA, &data);
        self.command(LE_SET_ADVERTISING_ENABLE, &[1]);
    }

    /// The next HCI packet for the controller, if it takes one now.
    pub fn transmit(&mut self) -> Option<Vec<u8>> {
        if self.command_allowed
            && let Some(command) = self.commands.pop_front()
        {
            self.command_allowed = false;
            return Some(command);
        }
        let handle = self.connection.as_ref()?.handle;
        if self.acl_free == 0 || self.acl_size == 0 {
            return None;
        }
        let (pdu, sent) = match self.outgoing.take() {
            Some(part) => part,
            None => (self.out.acl.pop_front()?, 0),
        };
        let end = pdu.len().min(sent + self.acl_size);
        // the packet boundary flag: the start of a PDU, or more of one
        let flags: u16 = if sent == 0 { 0b00 } else { 0b01 };
        let mut packet = Vec::with_capacity(5 + end - sent);
        packet.push(ACL);
        packet.extend_from_slice(&(handle | flags << 12).to_le_bytes());
        packet.extend_from_slice(&((end - sent) as u16).to_le_bytes());
        packet.extend_from_slice(&pdu[sent..end]);
        self.acl_free -= 1;
        if end < pdu.len() {
            self.outgoing = Some((pdu, end));
        }
        Some(packet)
    }

    /// The next thing that happened.
    pub fn event(&mut self) -> Option<Event> {
        self.out.events.pop_front()
    }

    /// Everything queued for the phone went to the controller: room for a notification.
    pub fn idle(&self) -> bool {
        self.out.acl.is_empty() && self.outgoing.is_none()
    }

    /// Notify the app of a frame. `false` if no phone is there to take it: none connected, or
    /// one that did not pair or did not ask for notifications.
    pub fn notify(&mut self, frame: &[u8]) -> bool {
        match &self.connection {
            Some(c) if c.authenticated && c.att.notifying() => {
                self.out.l2cap(att::CID, &c.att.notification(frame));
                true
            }
            _ => false,
        }
    }

    /// The frame of the last [`Event::Written`] is taken: the app may write the next.
    pub fn written(&mut self) {
        if let Some(reply) = self.connection.as_mut().and_then(|c| c.att.written()) {
            self.out.l2cap(att::CID, &reply);
        }
    }

    /// Ask the phone for the short connection interval of a firmware upload.
    pub fn hurry(&mut self) {
        if let Some(handle) = self.connection.as_ref().map(|c| c.handle) {
            let mut parameters = [0u8; 14];
            parameters[..2].copy_from_slice(&handle.to_le_bytes());
            parameters[2..10].copy_from_slice(&fast_link());
            self.command(LE_CONNECTION_UPDATE, &parameters);
        }
    }

    /// Drop the phone.
    pub fn disconnect(&mut self) {
        if let Some(handle) = self.connection.as_ref().map(|c| c.handle) {
            let handle = handle.to_le_bytes();
            // remote user terminated connection
            self.command(DISCONNECT, &[handle[0], handle[1], 0x13]);
        }
    }

    /// Another passkey for the pairings to come.
    pub fn set_passkey(&mut self, passkey: Option<u32>) {
        self.config.passkey = passkey;
    }

    /// The bonds, the latest last.
    pub fn bonds(&self) -> &[Bond] {
        &self.config.bonds
    }

    /// The time, in seconds on any clock: to be told now and then, for a pairing that stalls.
    pub fn tick(&mut self, now: u32) {
        self.now = now;
        if let Some(c) = &mut self.connection {
            c.smp.tick(now, &mut self.out);
        }
    }

    /// An HCI packet of the controller.
    pub fn receive(&mut self, packet: &[u8]) {
        match packet {
            [EVENT, code, length, parameters @ ..] if parameters.len() == *length as usize => self.hci_event(*code, parameters),
            [ACL, a, b, c, d, data @ ..] if data.len() == u16::from_le_bytes([*c, *d]) as usize => {
                let header = u16::from_le_bytes([*a, *b]);
                self.acl(header & 0x0FFF, header >> 12 & 0b11, data);
            }
            _ => {}
        }
    }

    fn hci_event(&mut self, code: u8, p: &[u8]) {
        match (code, p) {
            (EVENT_COMMAND_COMPLETE, [allowed, a, b, results @ ..]) => {
                self.command_allowed = *allowed > 0;
                self.command_complete(u16::from_le_bytes([*a, *b]), results);
            }
            (EVENT_COMMAND_STATUS, [status, allowed, a, b]) => {
                self.command_allowed = *allowed > 0;
                // a controller that cannot ask the phone itself: ask over L2CAP
                if u16::from_le_bytes([*a, *b]) == LE_CONNECTION_UPDATE && *status != 0 && self.connection.is_some() {
                    self.identifier = self.identifier % 255 + 1;
                    let mut request = [0u8; 12];
                    request[..4].copy_from_slice(&[0x12, self.identifier, 8, 0]);
                    request[4..].copy_from_slice(&fast_link());
                    self.out.l2cap(SIGNALLING, &request);
                }
            }
            (EVENT_DISCONNECTION_COMPLETE, [0, a, b, reason]) => self.disconnected(u16::from_le_bytes([*a, *b]) & 0x0FFF, *reason),
            (EVENT_ENCRYPTION_CHANGE, [status, a, b, enabled]) => {
                self.encryption(u16::from_le_bytes([*a, *b]) & 0x0FFF, *status == 0 && *enabled != 0);
            }
            (EVENT_ENCRYPTION_KEY_REFRESH_COMPLETE, [status, a, b]) => {
                self.encryption(u16::from_le_bytes([*a, *b]) & 0x0FFF, *status == 0);
            }
            (EVENT_NUMBER_OF_COMPLETED_PACKETS, [_handles, counts @ ..]) => {
                for count in counts.as_chunks::<4>().0 {
                    let done = u16::from_le_bytes([count[2], count[3]]) as usize;
                    self.acl_free = (self.acl_free + done).min(self.acl_buffers);
                }
            }
            (EVENT_LE_META, [LE_CONNECTION_COMPLETE, status, a, b, role, kind, address @ ..]) if address.len() >= 6 => {
                if *status == 0 && *role == 1 {
                    let address: [u8; 6] = core::array::from_fn(|i| address[i]);
                    self.connected(u16::from_le_bytes([*a, *b]) & 0x0FFF, *kind & 1, address);
                } else {
                    self.advertise();
                }
            }
            (EVENT_LE_META, [LE_LONG_TERM_KEY_REQUEST, a, b, of_legacy_pairing @ ..]) if of_legacy_pairing.len() == 10 => {
                // a key of LE Secure Connections is asked for with zeros here
                let zeros = of_legacy_pairing.iter().all(|b| *b == 0);
                self.long_term_key_request(u16::from_le_bytes([*a, *b]) & 0x0FFF, zeros);
            }
            _ => {}
        }
    }

    fn command_complete(&mut self, opcode: u16, results: &[u8]) {
        match (opcode, results) {
            // no buffers for LE alone: they are shared with BR/EDR, and counted there
            (LE_READ_BUFFER_SIZE, [0, 0, 0, _]) | (LE_READ_BUFFER_SIZE, [0, _, _, 0]) => self.command(READ_BUFFER_SIZE, &[]),
            (LE_READ_BUFFER_SIZE, [0, a, b, count]) => self.buffers(u16::from_le_bytes([*a, *b]), *count as u16),
            (READ_BUFFER_SIZE, [0, a, b, _, c, d, ..]) => self.buffers(u16::from_le_bytes([*a, *b]), u16::from_le_bytes([*c, *d])),
            _ => {}
        }
    }

    fn buffers(&mut self, size: u16, count: u16) {
        // no more than goes on the air in one piece, whatever the controller would take
        self.acl_size = (size as usize).min(ACL_MAX);
        self.acl_buffers = count as usize;
        self.acl_free = count as usize;
    }

    fn connected(&mut self, handle: u16, kind: u8, address: [u8; 6]) {
        if self.connection.is_some() {
            return; // one phone at a time, and the controller advertises to no second
        }
        let bond = self.config.bonds.iter().find(|b| b.matches(kind, &address)).cloned();
        // a bonded phone that comes back finds its notifications as it left them
        let cccd = match (&bond, &self.kept_cccd) {
            (Some(bond), Some((kept, cccd))) if bond.same_phone(kept) => *cccd,
            _ => 0,
        };
        let address = flip(address);
        self.connection = Some(Connection {
            handle,
            peer: [kind, address[0], address[1], address[2], address[3], address[4], address[5]],
            bond,
            key_authenticated: false,
            authenticated: false,
            att: Att::new(cccd),
            smp: Smp::new(),
            incoming: Vec::new(),
        });
        self.out.event(Event::Connected);
    }

    fn disconnected(&mut self, handle: u16, reason: u8) {
        let Some(c) = self.connection.take_if(|c| c.handle == handle) else {
            return;
        };
        self.kept_cccd = c.bond.filter(|b| b.bonded).map(|b| (b, c.att.cccd));
        // what was on its way is gone with the connection, and the controller's buffers are free
        self.out.acl.clear();
        self.outgoing = None;
        self.acl_free = self.acl_buffers;
        self.out.event(Event::Disconnected(reason));
        self.advertise();
    }

    /// The phone starts encryption and the controller asks for the key: that of the pairing just
    /// done, or that of the bond.
    fn long_term_key_request(&mut self, handle: u16, of_secure_connections: bool) {
        let Some(c) = self.connection.as_mut().filter(|c| c.handle == handle) else {
            return;
        };
        let key = match (c.smp.ltk(), &c.bond) {
            _ if !of_secure_connections => None,
            (Some(key), _) => Some((key, true)),
            (None, Some(bond)) => Some((bond.ltk, bond.authenticated)),
            (None, None) => None,
        };
        let handle = handle.to_le_bytes();
        match key {
            Some((key, authenticated)) => {
                c.key_authenticated = authenticated;
                let mut parameters = [0u8; 18];
                parameters[..2].copy_from_slice(&handle);
                parameters[2..].copy_from_slice(&key);
                self.command(LE_LONG_TERM_KEY_REQUEST_REPLY, &parameters);
            }
            // the phone is told the key is missing, and may pair again
            None => self.command(LE_LONG_TERM_KEY_REQUEST_NEGATIVE_REPLY, &handle),
        }
    }

    fn encryption(&mut self, handle: u16, on: bool) {
        let Some(c) = self.connection.as_mut().filter(|c| c.handle == handle) else {
            return;
        };
        c.authenticated = on && c.key_authenticated;
        if on {
            self.out.event(Event::Encrypted(c.authenticated));
        }
        let mut env = env(&self.config, c.peer, &mut self.out, self.now);
        if let Some(bond) = c.smp.encrypted(on, &mut env) {
            self.paired(bond);
        }
    }

    /// A pairing completed: its bond is the phone's from now on.
    fn paired(&mut self, bond: Bond) {
        if bond.bonded {
            self.config.bonds.retain(|b| !b.same_phone(&bond));
            self.config.bonds.push(bond.clone());
            if self.config.bonds.len() > MAX_BONDS {
                self.config.bonds.remove(0);
            }
        }
        self.out.event(Event::Paired { bonded: bond.bonded });
        if let Some(c) = &mut self.connection {
            c.bond = Some(bond);
        }
    }

    /// Data of the phone: an L2CAP PDU, or a part of one.
    fn acl(&mut self, handle: u16, boundary: u16, data: &[u8]) {
        let Some(c) = self.connection.as_mut().filter(|c| c.handle == handle) else {
            return;
        };
        match boundary {
            0b10 => c.incoming.clear(),
            // more of a PDU, of which the start must be there
            0b01 if !c.incoming.is_empty() => {}
            _ => return,
        }
        if c.incoming.len() + data.len() > 4 + PDU_MAX {
            c.incoming.clear();
            return;
        }
        c.incoming.extend_from_slice(data);
        let [a, b, low, high, payload @ ..] = c.incoming.as_slice() else {
            return;
        };
        let length = u16::from_le_bytes([*a, *b]) as usize;
        if payload.len() < length {
            return;
        }
        let channel = u16::from_le_bytes([*low, *high]);
        let pdu = core::mem::take(&mut c.incoming);
        let pdu = &pdu[4..4 + length];
        match channel {
            att::CID => {
                let name = truncate(&self.config.name, NAME_MAX).as_bytes();
                let outcome = c.att.pdu(pdu, name, c.authenticated, self.config.frame_max);
                if let Some(reply) = outcome.reply {
                    self.out.l2cap(att::CID, &reply);
                }
                if let Some(frame) = outcome.frame {
                    self.out.event(Event::Written(frame));
                }
            }
            smp::CID => {
                let mut env = env(&self.config, c.peer, &mut self.out, self.now);
                if let Some(bond) = c.smp.pdu(pdu, &mut env) {
                    self.paired(bond);
                }
            }
            SIGNALLING => self.signalling(pdu),
            _ => {}
        }
    }

    /// The signalling channel: nothing this host takes part in, but requests are owed an answer.
    fn signalling(&mut self, pdu: &[u8]) {
        let &[code, identifier, ..] = pdu else {
            return;
        };
        match code {
            // answers, and credits of channels there are none of
            0x01 | 0x07 | 0x13 | 0x15 | 0x16 => {}
            // an LE credit based connection: no such service here
            0x14 => self.out.l2cap(SIGNALLING, &[0x15, identifier, 10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x02, 0x00]),
            // command not understood
            _ => self.out.l2cap(SIGNALLING, &[0x01, identifier, 2, 0, 0, 0]),
        }
    }
}

/// What the Security Manager works with.
fn env<'a>(config: &Config, peer: Addr, out: &'a mut Out, now: u32) -> smp::Env<'a> {
    let address = flip(config.address);
    smp::Env {
        out,
        random: config.random,
        local: [1, address[0], address[1], address[2], address[3], address[4], address[5]],
        peer,
        irk: config.irk,
        passkey: config.passkey.filter(|key| *key <= 999_999),
        now,
    }
}

/// Connection interval (least, most), latency and supervision timeout of [`Host::hurry`].
fn fast_link() -> [u8; 8] {
    let (least, most, timeout) = (FAST_INTERVAL.0.to_le_bytes(), FAST_INTERVAL.1.to_le_bytes(), FAST_TIMEOUT.to_le_bytes());
    [least[0], least[1], most[0], most[1], 0, 0, timeout[0], timeout[1]]
}

/// The start of `s`, at most `max` bytes of whole characters.
fn truncate(s: &str, max: usize) -> &str {
    let mut n = s.len().min(max);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}
