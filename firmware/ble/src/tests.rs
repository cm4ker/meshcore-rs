//! The host against a controller and a phone made up here: the phone's side of pairing is
//! written from the specification, with the same toolbox.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::crypto::{self, Addr, KeyPair, flip};
use crate::*;

const DEVICE: [u8; 6] = [0x11, 0x22, 0x33, 0x44, 0x55, 0xC6];
const DEVICE_IRK: [u8; 16] = [0xD0; 16];
const HANDLE: u16 = 0x0041;

static SEED: AtomicU64 = AtomicU64::new(0x2545_F491_4F6C_DD1D);

fn random(out: &mut [u8]) {
    let mut x = SEED.load(Ordering::Relaxed);
    for b in out {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = (x >> 24) as u8;
    }
    SEED.store(x, Ordering::Relaxed);
}

fn config(passkey: Option<u32>, bonds: Vec<Bond>) -> Config {
    Config {
        name: String::from("MeshCore-Тест 🎃 узел с длинным именем"),
        address: DEVICE,
        irk: DEVICE_IRK,
        passkey,
        bonds,
        frame_max: 176,
        random,
    }
}

/// The controller under the host, and what the phone hears through it.
struct Sim {
    host: Host,
    /// Commands the host gave: opcode and parameters.
    commands: Vec<(u16, Vec<u8>)>,
    /// PDUs for the phone: channel and payload.
    heard: VecDeque<(u16, Vec<u8>)>,
    partial: Vec<u8>,
    /// The size of a data buffer and their number.
    buffers: (u16, u8),
    /// Packets the controller holds, not yet said to be sent.
    held: usize,
    /// It says so by itself, as one that gets them on the air at once.
    prompt: bool,
    events: Vec<Event>,
}

impl Sim {
    fn new(config: Config, buffers: (u16, u8)) -> Self {
        let host = Host::new(config);
        let mut sim =
            Self { host, commands: vec![], heard: VecDeque::new(), partial: vec![], buffers, held: 0, prompt: true, events: vec![] };
        sim.run();
        sim
    }

    fn event(&mut self, code: u8, parameters: &[u8]) {
        let mut packet = vec![4, code, parameters.len() as u8];
        packet.extend_from_slice(parameters);
        self.host.receive(&packet);
    }

    /// Let the host say all it has to, the controller answering.
    fn run(&mut self) {
        loop {
            while let Some(event) = self.host.event() {
                self.events.push(event);
            }
            let Some(packet) = self.host.transmit() else { break };
            match packet[0] {
                1 => {
                    let opcode = u16::from_le_bytes([packet[1], packet[2]]);
                    assert_eq!(packet[3] as usize, packet.len() - 4);
                    self.commands.push((opcode, packet[4..].to_vec()));
                    let op = opcode.to_le_bytes();
                    match opcode {
                        LE_READ_BUFFER_SIZE => {
                            let size = self.buffers.0.to_le_bytes();
                            self.event(0x0E, &[1, op[0], op[1], 0, size[0], size[1], self.buffers.1]);
                        }
                        // these two are begun, and end with an event of their own
                        LE_CONNECTION_UPDATE => self.event(0x0F, &[0, 1, op[0], op[1]]),
                        DISCONNECT => {
                            self.event(0x0F, &[0, 1, op[0], op[1]]);
                            self.event(0x05, &[0, packet[4], packet[5], 0x16]);
                        }
                        _ => self.event(0x0E, &[1, op[0], op[1], 0]),
                    }
                }
                2 => {
                    let header = u16::from_le_bytes([packet[1], packet[2]]);
                    assert_eq!(header & 0x0FFF, HANDLE);
                    let data = &packet[5..];
                    assert_eq!(u16::from_le_bytes([packet[3], packet[4]]) as usize, data.len());
                    assert!(data.len() <= self.buffers.0 as usize, "more than a buffer takes");
                    self.held += 1;
                    assert!(self.held <= self.buffers.1 as usize, "more than the controller has buffers");
                    match header >> 12 {
                        0b00 => assert!(self.partial.is_empty(), "a PDU begun inside another"),
                        0b01 => assert!(!self.partial.is_empty(), "more of no PDU"),
                        flags => panic!("flags {flags}"),
                    }
                    self.partial.extend_from_slice(data);
                    if self.partial.len() >= 4 {
                        let length = u16::from_le_bytes([self.partial[0], self.partial[1]]) as usize;
                        assert!(self.partial.len() <= 4 + length);
                        if self.partial.len() == 4 + length {
                            let channel = u16::from_le_bytes([self.partial[2], self.partial[3]]);
                            self.heard.push_back((channel, self.partial.split_off(4)));
                            self.partial.clear();
                        }
                    }
                    if self.prompt {
                        self.sent(1);
                    }
                }
                kind => panic!("packet type {kind}"),
            }
        }
    }

    /// The controller got packets on the air.
    fn sent(&mut self, packets: usize) {
        self.held -= packets;
        let (handle, count) = (HANDLE.to_le_bytes(), (packets as u16).to_le_bytes());
        self.event(0x13, &[1, handle[0], handle[1], count[0], count[1]]);
    }

    fn connect(&mut self, kind: u8, address: [u8; 6]) {
        let handle = HANDLE.to_le_bytes();
        let mut parameters = vec![0x01, 0, handle[0], handle[1], 1, kind];
        parameters.extend_from_slice(&address);
        parameters.extend_from_slice(&[24, 0, 0, 0, 0x48, 0, 1]);
        self.event(0x3E, &parameters);
        self.run();
    }

    fn disconnect(&mut self) {
        let handle = HANDLE.to_le_bytes();
        self.event(0x05, &[0, handle[0], handle[1], 0x13]);
        self.run();
    }

    /// The phone sends a PDU, in parts of at most `part` bytes.
    fn say_in_parts(&mut self, channel: u16, payload: &[u8], part: usize) {
        let mut pdu = (payload.len() as u16).to_le_bytes().to_vec();
        pdu.extend_from_slice(&channel.to_le_bytes());
        pdu.extend_from_slice(payload);
        for (i, data) in pdu.chunks(part).enumerate() {
            let header = (HANDLE | if i == 0 { 0b10 << 12 } else { 0b01 << 12 }).to_le_bytes();
            let mut packet = vec![2, header[0], header[1]];
            packet.extend_from_slice(&(data.len() as u16).to_le_bytes());
            packet.extend_from_slice(data);
            self.host.receive(&packet);
        }
        self.run();
    }

    fn say(&mut self, channel: u16, payload: &[u8]) {
        self.say_in_parts(channel, payload, 251);
    }

    /// What the phone heard next on a channel.
    fn hear(&mut self, channel: u16) -> Vec<u8> {
        let (on, pdu) = self.heard.pop_front().expect("the phone heard nothing");
        assert_eq!(on, channel, "{pdu:02x?}");
        pdu
    }

    /// An ATT request and its response.
    fn ask(&mut self, request: &[u8]) -> Vec<u8> {
        self.say(att::CID, request);
        self.hear(att::CID)
    }

    fn take_events(&mut self) -> Vec<Event> {
        core::mem::take(&mut self.events)
    }

    /// The last command of an opcode.
    fn commanded(&self, opcode: u16) -> Option<&[u8]> {
        self.commands.iter().rev().find(|c| c.0 == opcode).map(|c| c.1.as_slice())
    }

    /// The phone starts encryption: the key the host gives for it, if any.
    fn encrypt(&mut self) -> Option<[u8; 16]> {
        let handle = HANDLE.to_le_bytes();
        self.commands.clear();
        self.event(0x3E, &[0x05, handle[0], handle[1], 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        self.run();
        let key: [u8; 16] = match self.commands.as_slice() {
            [(LE_LONG_TERM_KEY_REQUEST_REPLY, p)] => {
                assert_eq!(p[..2], handle);
                p[2..].try_into().unwrap()
            }
            [(LE_LONG_TERM_KEY_REQUEST_NEGATIVE_REPLY, p)] => {
                assert_eq!(p[..], handle);
                return None;
            }
            other => panic!("{other:x?}"),
        };
        self.event(0x08, &[0, handle[0], handle[1], 1]);
        self.run();
        Some(key)
    }
}

/// A phone that pairs.
struct Phone {
    /// The address it connects with, as on the air, and its type.
    kind: u8,
    address: [u8; 6],
    /// Its identity: what it gives when bonding.
    identity: (u8, [u8; 6]),
    irk: [u8; 16],
    /// IO capability, OOB flag, AuthReq, key size, its keys, the device's keys.
    features: [u8; 6],
}

impl Phone {
    /// One with a keyboard and a display, bonding, behind a private address made of its key.
    fn new(n: u8) -> Self {
        let irk = [n; 16];
        Self { kind: 1, address: Self::private_address(&irk, n), identity: (0, [n, 2, 3, 4, 5, 6]), irk, features: [0x04, 0, 0x0D, 16, 0x02, 0x02] }
    }

    fn private_address(irk: &[u8; 16], n: u8) -> [u8; 6] {
        let prand = [0x40 | n & 0x3F, n, 0x5A];
        let hash = crypto::ah(&flip(*irk), prand);
        [hash[2], hash[1], hash[0], prand[2], prand[1], prand[0]]
    }

    fn addr(&self) -> Addr {
        let a = flip(self.address);
        [self.kind, a[0], a[1], a[2], a[3], a[4], a[5]]
    }

    /// Pair by typing `typed`: `Ok` with the key both sides have, `Err` with the reason the
    /// device gave for failing.
    fn pair(&self, sim: &mut Sim, typed: impl Fn(u32) -> u32) -> Result<[u8; 16], u8> {
        let failed = |pdu: &[u8]| match pdu {
            [0x05, reason] => Err(*reason),
            _ => Ok(()),
        };
        let device: Addr = {
            let a = flip(DEVICE);
            [1, a[0], a[1], a[2], a[3], a[4], a[5]]
        };
        let f = self.features;
        sim.say(smp::CID, &[0x01, f[0], f[1], f[2], f[3], f[4], f[5]]);
        let response = sim.hear(smp::CID);
        failed(&response)?;
        assert_eq!(response, [0x02, 0x00, 0, 0x0D, 16, f[4] & 2, f[5] & 2]);

        let mut secret = [0u8; 32];
        random(&mut secret);
        secret[0] &= 0x7F;
        let keys = KeyPair::new(secret).unwrap();
        let mut pdu = vec![0x0C];
        pdu.extend_from_slice(&flip::<32>(keys.public[..32].try_into().unwrap()));
        pdu.extend_from_slice(&flip::<32>(keys.public[32..].try_into().unwrap()));
        sim.say(smp::CID, &pdu);
        let theirs = sim.hear(smp::CID);
        failed(&theirs)?;
        assert_eq!((theirs[0], theirs.len()), (0x0C, 65));
        let mut device_key = [0u8; 64];
        device_key[..32].copy_from_slice(&flip::<32>(theirs[1..33].try_into().unwrap()));
        device_key[32..].copy_from_slice(&flip::<32>(theirs[33..].try_into().unwrap()));
        let dh_key = keys.dh_key(&device_key).expect("the device's key is on the curve");
        let (pka, pkb): ([u8; 32], [u8; 32]) = (keys.public[..32].try_into().unwrap(), device_key[..32].try_into().unwrap());

        // the device shows the passkey, the user types it
        let shown = sim.take_events().into_iter().find_map(|e| if let Event::Passkey(key) = e { Some(key) } else { None });
        let passkey = typed(shown.expect("a passkey to show"));
        let (mut na, mut nb) = ([0u8; 16], [0u8; 16]);
        for round in 0..20 {
            let z = 0x80 | (passkey >> round) as u8 & 1;
            random(&mut na);
            let mut pdu = vec![0x03];
            pdu.extend_from_slice(&flip(crypto::f4(&pka, &pkb, &na, z)));
            sim.say(smp::CID, &pdu);
            let confirm = sim.hear(smp::CID);
            failed(&confirm)?;
            assert_eq!(confirm[0], 0x03);
            let mut pdu = vec![0x04];
            pdu.extend_from_slice(&flip(na));
            sim.say(smp::CID, &pdu);
            let nonce = sim.hear(smp::CID);
            failed(&nonce)?;
            assert_eq!(nonce[0], 0x04);
            nb = flip(nonce[1..].try_into().unwrap());
            assert_eq!(confirm[1..], flip(crypto::f4(&pkb, &pka, &nb, z)), "round {round}");
        }

        let (mac_key, ltk) = crypto::f5(&dh_key, &na, &nb, &self.addr(), &device);
        let mut r = [0u8; 16];
        r[12..].copy_from_slice(&passkey.to_be_bytes());
        let ea = crypto::f6(&mac_key, &na, &nb, &r, &[f[2], f[1], f[0]], &self.addr(), &device);
        let mut pdu = vec![0x0D];
        pdu.extend_from_slice(&flip(ea));
        sim.say(smp::CID, &pdu);
        let check = sim.hear(smp::CID);
        failed(&check)?;
        let eb = crypto::f6(&mac_key, &nb, &na, &r, &[0x0D, 0, 0x00], &device, &self.addr());
        assert_eq!((check[0], &check[1..]), (0x0D, &flip(eb)[..]));

        // encryption with the new key, then the identities: the device's first
        assert_eq!(sim.encrypt(), Some(flip(ltk)), "one key on both sides");
        if f[5] & 2 != 0 {
            let mut information = vec![0x08];
            information.extend_from_slice(&DEVICE_IRK);
            assert_eq!(sim.hear(smp::CID), information);
            let mut address = vec![0x09, 1];
            address.extend_from_slice(&DEVICE);
            assert_eq!(sim.hear(smp::CID), address);
        }
        if f[4] & 2 != 0 {
            let mut pdu = vec![0x08];
            pdu.extend_from_slice(&self.irk);
            sim.say(smp::CID, &pdu);
            let mut pdu = vec![0x09, self.identity.0];
            pdu.extend_from_slice(&self.identity.1);
            sim.say(smp::CID, &pdu);
        }
        assert!(sim.heard.is_empty(), "{:02x?}", sim.heard);
        Ok(flip(ltk))
    }
}

#[test]
fn starts_the_controller_and_advertises() {
    let sim = Sim::new(config(None, vec![]), (251, 12));
    let opcodes: Vec<u16> = sim.commands.iter().map(|c| c.0).collect();
    assert_eq!(opcodes, [0x0C03, 0x0C01, 0x2001, 0x2002, 0x2005, 0x2006, 0x2008, 0x2009, 0x200A]);
    assert_eq!(sim.commanded(LE_SET_RANDOM_ADDRESS), Some(&DEVICE[..]));
    // as the firmware advertised so far
    assert_eq!(
        sim.commanded(LE_SET_ADVERTISING_PARAMETERS),
        Some(&[0x00, 0x01, 0x00, 0x01, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0x07, 0x00][..])
    );
    let data = sim.commanded(LE_SET_ADVERTISING_DATA).unwrap();
    assert_eq!(data.len(), 32);
    assert_eq!(data[..6], [21, 0x02, 0x01, 0x06, 0x11, 0x07]);
    assert_eq!(data[6..22], [0x9E, 0xCA, 0xDC, 0x24, 0x0E, 0xE5, 0xA9, 0xE0, 0x93, 0xF3, 0xA3, 0xB5, 0x01, 0x00, 0x40, 0x6E]);
    assert_eq!(data[22..], [0; 10]);
    // the name, cut at a whole character within the 29 bytes there are
    let name = "MeshCore-Тест 🎃 узе";
    let data = sim.commanded(LE_SET_SCAN_RESPONSE_DATA).unwrap();
    assert_eq!((data.len(), data[0] as usize, data[1] as usize, data[2]), (32, name.len() + 2, name.len() + 1, 0x09));
    assert_eq!(&data[3..3 + name.len()], name.as_bytes());
    assert_eq!(sim.commanded(LE_SET_ADVERTISING_ENABLE), Some(&[1][..]));
    assert_eq!(sim.host.acl_size, 251);
    assert_eq!(sim.host.acl_free, 12);
}

#[test]
fn buffers_shared_with_classic_bluetooth() {
    let mut sim = Sim::new(config(None, vec![]), (0, 0));
    assert_eq!(sim.commands.last().unwrap().0, READ_BUFFER_SIZE);
    sim.event(0x0E, &[1, 0x05, 0x10, 0, 0xFB, 0x00, 60, 8, 0, 4, 0]);
    assert_eq!((sim.host.acl_size, sim.host.acl_free), (251, 8));
}

#[test]
fn pairs_with_the_passkey_and_bonds() {
    let mut sim = Sim::new(config(None, vec![]), (251, 12));
    let phone = Phone::new(7);
    sim.connect(phone.kind, phone.address);
    assert_eq!(sim.take_events(), [Event::Connected]);

    // not paired: the service is closed, which is what makes a phone pair
    assert_eq!(sim.ask(&[0x12, 12, 0, 1, 0]), [0x01, 0x12, 12, 0, 0x05]);
    assert!(!sim.host.notify(&[1, 2, 3]));

    let key = phone.pair(&mut sim, |shown| shown).unwrap();
    assert_eq!(sim.take_events(), [Event::Encrypted(true), Event::Paired { bonded: true }]);
    let bond = Bond { kind: 0, address: phone.identity.1, ltk: key, irk: phone.irk, authenticated: true, bonded: true };
    assert_eq!(sim.host.bonds(), core::slice::from_ref(&bond));

    // the service is open
    assert_eq!(sim.ask(&[0x02, 0xF7, 0x00]), [0x03, 251, 0]);
    assert_eq!(sim.ask(&[0x12, 12, 0, 1, 0]), [0x13]);
    assert!(sim.host.notify(&[1, 2, 3]));
    sim.run();
    assert_eq!(sim.hear(att::CID), [0x1B, 11, 0, 1, 2, 3]);

    // a frame is acknowledged once the firmware has taken it
    sim.say(att::CID, &[0x12, 9, 0, 0xAA, 0xBB]);
    assert_eq!(sim.take_events(), [Event::Written(vec![0xAA, 0xBB])]);
    assert!(sim.heard.is_empty());
    sim.host.written();
    sim.run();
    assert_eq!(sim.hear(att::CID), [0x13]);

    // the phone comes back behind another private address: known by its key, and no pairing
    sim.disconnect();
    assert_eq!(sim.take_events(), [Event::Disconnected(0x13)]);
    assert_eq!(sim.commands.last().unwrap(), &(LE_SET_ADVERTISING_ENABLE, vec![1]));
    let address = Phone::private_address(&phone.irk, 99);
    assert_ne!(address, phone.address);
    sim.connect(1, address);
    assert!(!sim.host.notify(&[9]), "not before the link is encrypted");
    assert_eq!(sim.encrypt(), Some(key));
    assert_eq!(sim.take_events(), [Event::Connected, Event::Encrypted(true)]);
    // and its notifications are as it left them
    assert!(sim.host.notify(&[9]));
    sim.run();
    assert_eq!(sim.hear(att::CID), [0x1B, 11, 0, 9]);

    // the bonds survive a restart as they are stored
    let stored = bonds_to_bytes(sim.host.bonds());
    assert_eq!(stored.len(), BOND_RECORD);
    let mut sim = Sim::new(config(None, bonds_from_bytes(&stored)), (251, 12));
    sim.connect(1, Phone::private_address(&phone.irk, 5));
    assert_eq!(sim.encrypt(), Some(key));
    assert_eq!(sim.take_events(), [Event::Connected, Event::Encrypted(true)]);
    // after a restart the phone asks for its notifications anew
    assert!(!sim.host.notify(&[9]));

    // another phone is none of ours
    sim.disconnect();
    sim.connect(1, Phone::new(8).address);
    assert_eq!(sim.encrypt(), None);
    assert_eq!(sim.take_events(), [Event::Disconnected(0x13), Event::Connected]);
}

#[test]
fn a_fixed_passkey_and_a_wrong_one() {
    let mut sim = Sim::new(config(Some(123_456), vec![]), (251, 12));
    let phone = Phone::new(3);
    sim.connect(phone.kind, phone.address);
    // every bit of the passkey is checked in a round of its own: a wrong one fails there
    assert_eq!(phone.pair(&mut sim, |shown| shown ^ 0x10), Err(0x04));
    assert_eq!(sim.take_events(), [Event::PairingFailed]);
    assert_eq!(sim.host.bonds(), []);
    assert_eq!(sim.encrypt(), None);

    // the phone may try again
    phone.pair(&mut sim, |shown| {
        assert_eq!(shown, 123_456);
        shown
    })
    .unwrap();
    assert_eq!(sim.take_events(), [Event::Encrypted(true), Event::Paired { bonded: true }]);

    // a passkey of more than six digits is no fixed one
    sim.host.set_passkey(Some(1_000_000));
    sim.disconnect();
    sim.connect(phone.kind, phone.address);
    phone.pair(&mut sim, |shown| {
        assert!(shown <= 999_999 && shown != 123_456);
        shown
    })
    .unwrap();
    assert_eq!(sim.host.bonds().len(), 1, "the phone's bond is replaced, not added to");
}

#[test]
fn pairings_that_are_refused() {
    let mut sim = Sim::new(config(None, vec![]), (251, 12));
    let mut phone = Phone::new(4);
    sim.connect(phone.kind, phone.address);
    let mut refused = |features: [u8; 6]| {
        phone.features = features;
        let reason = phone.pair(&mut sim, |shown| shown).unwrap_err();
        assert_eq!(sim.take_events().last(), Some(&Event::PairingFailed));
        reason
    };
    assert_eq!(refused([0x04, 0, 0x05, 16, 2, 2]), 0x03, "legacy pairing");
    assert_eq!(refused([0x03, 0, 0x0D, 16, 2, 2]), 0x03, "no keyboard: no passkey");
    assert_eq!(refused([0x01, 0, 0x0D, 16, 2, 2]), 0x03, "a display and yes/no");
    assert_eq!(refused([0x04, 0, 0x0D, 7, 2, 2]), 0x06, "a short key");
    assert_eq!(refused([0x04, 0, 0x0D, 17, 2, 2]), 0x0A);
    assert_eq!(refused([0x04, 1, 0x0D, 16, 2, 2]), 0x02, "out of band data");

    // out of turn, cut short, unknown
    sim.say(smp::CID, &[0x03; 17]);
    assert_eq!(sim.hear(smp::CID), [0x05, 0x08]);
    sim.say(smp::CID, &[0x01, 0x04, 0, 0x0D, 16, 2]);
    assert_eq!(sim.hear(smp::CID), [0x05, 0x0A]);
    sim.say(smp::CID, &[0x0F, 0]);
    assert_eq!(sim.hear(smp::CID), [0x05, 0x07]);
    // the phone gives up: not answered
    sim.say(smp::CID, &[0x01, 0x04, 0, 0x0D, 16, 2, 2]);
    assert_eq!(sim.hear(smp::CID)[0], 0x02);
    sim.say(smp::CID, &[0x05, 0x08]);
    assert!(sim.heard.is_empty());

    // a public key off the curve would give the device's secret away
    sim.say(smp::CID, &[0x01, 0x04, 0, 0x0D, 16, 2, 2]);
    sim.hear(smp::CID);
    sim.say(smp::CID, &[0x0C; 65]);
    assert_eq!(sim.hear(smp::CID), [0x05, 0x0B]);
    assert_eq!(sim.host.bonds(), []);
}

#[test]
fn a_phone_that_keeps_no_bond_or_gives_no_identity() {
    let mut sim = Sim::new(config(None, vec![]), (251, 12));
    // pairs without bonding: good for the connection
    let mut phone = Phone::new(5);
    phone.features = [0x02, 0, 0x0C, 16, 0, 0];
    sim.connect(phone.kind, phone.address);
    let key = phone.pair(&mut sim, |shown| shown).unwrap();
    assert_eq!(sim.take_events(), [Event::Encrypted(true), Event::Paired { bonded: false }]);
    assert_eq!(sim.host.bonds(), []);
    assert_eq!(sim.encrypt(), Some(key), "the key is refreshed with the same");
    sim.disconnect();
    sim.connect(phone.kind, phone.address);
    assert_eq!(sim.encrypt(), None);

    // bonds under the address it has, a public one
    let mut phone = Phone::new(6);
    (phone.kind, phone.features) = (0, [0x04, 0, 0x0D, 16, 0, 2]);
    sim.disconnect();
    sim.connect(phone.kind, phone.address);
    let key = phone.pair(&mut sim, |shown| shown).unwrap();
    assert_eq!(sim.host.bonds(), [Bond { kind: 0, address: phone.address, ltk: key, irk: [0; 16], authenticated: true, bonded: true }]);
    sim.disconnect();
    sim.connect(1, phone.address);
    assert_eq!(sim.encrypt(), None, "the same address of the other type is another device");
    sim.disconnect();
    sim.connect(0, phone.address);
    assert_eq!(sim.encrypt(), Some(key));
}

#[test]
fn a_pairing_that_stalls() {
    let mut sim = Sim::new(config(None, vec![]), (251, 12));
    sim.host.tick(1000);
    sim.connect(1, Phone::new(9).address);
    sim.say(smp::CID, &[0x01, 0x04, 0, 0x0D, 16, 2, 2]);
    assert_eq!(sim.hear(smp::CID)[0], 0x02);
    sim.host.tick(1029);
    sim.run();
    assert_eq!(sim.take_events(), [Event::Connected]);
    sim.host.tick(1030);
    sim.run();
    assert_eq!(sim.take_events(), [Event::PairingFailed]);
    // and nothing more of it on this connection
    sim.say(smp::CID, &[0x01, 0x04, 0, 0x0D, 16, 2, 2]);
    assert!(sim.heard.is_empty());
    sim.host.tick(5000);
    sim.run();
    assert_eq!(sim.take_events(), []);
}

#[test]
fn long_frames_through_small_buffers() {
    // a controller of the least there may be: 27 bytes a packet, and three of them
    let mut sim = Sim::new(config(Some(1), vec![]), (27, 3));
    let phone = Phone::new(2);
    sim.connect(phone.kind, phone.address);
    phone.pair(&mut sim, |shown| shown).unwrap();
    assert_eq!(sim.ask(&[0x02, 0xF7, 0x00]), [0x03, 251, 0]);
    assert_eq!(sim.ask(&[0x12, 12, 0, 1, 0]), [0x13]);
    sim.take_events();

    let frame: Vec<u8> = (0..176).map(|i| i as u8).collect();
    let mut request = vec![0x12, 9, 0];
    request.extend_from_slice(&frame);
    // in parts of every size, the header of the PDU itself in pieces
    for part in [1, 2, 3, 5, 27, 100, 251] {
        sim.say_in_parts(att::CID, &request, part);
        assert_eq!(sim.take_events(), [Event::Written(frame.clone())], "parts of {part}");
        sim.host.written();
        sim.run();
        assert_eq!(sim.hear(att::CID), [0x13]);
    }
    // a part of no PDU, and a PDU begun anew, are not taken for one
    let lost = [2, 0x41, 0x10, 3, 0, 0x12, 9, 0];
    sim.host.receive(&lost);
    sim.say_in_parts(att::CID, &request[..40], 30);
    sim.host.receive(&[2, 0x41, 0x20, 6, 0, 3, 0, 4, 0, 0x0A, 3]);
    sim.say(att::CID, &[0x0A, 5, 0]);
    assert_eq!(sim.hear(att::CID), [0x0B, 0, 0]);
    assert_eq!(sim.take_events(), [Event::Written(request[3..40].to_vec())]);
    sim.host.written();
    sim.run();
    sim.heard.clear();

    // out: no more packets than the controller has room for, the rest when it says so
    sim.prompt = false;
    assert!(sim.host.idle());
    assert!(sim.host.notify(&frame));
    assert!(!sim.host.idle());
    sim.run();
    assert_eq!(sim.held, 3);
    assert!(sim.heard.is_empty());
    for _ in 0..3 {
        sim.sent(1);
        sim.run();
        assert_eq!(sim.held, 3);
    }
    sim.sent(3);
    sim.run();
    assert!(sim.host.idle());
    assert_eq!(sim.held, 1, "183 bytes: seven packets");
    let mut notification = vec![0x1B, 11, 0];
    notification.extend_from_slice(&frame);
    assert_eq!(sim.hear(att::CID), notification);

    // a connection lost with packets in the controller: its buffers are free for the next
    assert!(sim.host.notify(&frame));
    sim.run();
    sim.held = 0;
    sim.disconnect();
    assert!(sim.host.idle());
    assert_eq!(sim.host.acl_free, 3);
}

#[test]
fn the_signalling_channel_and_the_short_interval() {
    let mut sim = Sim::new(config(None, vec![]), (251, 12));
    sim.connect(1, Phone::new(1).address);
    // a channel of its own: none to be had
    sim.say(SIGNALLING, &[0x14, 7, 10, 0, 0x80, 0, 0x40, 0, 0x00, 1, 0x00, 1, 1, 0]);
    assert_eq!(sim.hear(SIGNALLING), [0x15, 7, 10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0]);
    // the phone has the say over the connection's parameters, and anything else is not known
    sim.say(SIGNALLING, &[0x12, 8, 8, 0, 6, 0, 12, 0, 0, 0, 0xD0, 0x07]);
    assert_eq!(sim.hear(SIGNALLING), [0x01, 8, 2, 0, 0, 0]);
    sim.say(SIGNALLING, &[0x0A, 9, 2, 0, 1, 0]);
    assert_eq!(sim.hear(SIGNALLING), [0x01, 9, 2, 0, 0, 0]);
    sim.say(SIGNALLING, &[0x13, 1, 2, 0, 0, 0]);
    sim.say(SIGNALLING, &[0x01]);
    sim.say(0x0040, &[1, 2, 3]);
    assert!(sim.heard.is_empty());

    // an upload: 7.5 to 15 ms, no latency, 20 s, by the controller
    sim.host.hurry();
    sim.run();
    assert_eq!(sim.commanded(LE_CONNECTION_UPDATE), Some(&[0x41, 0, 6, 0, 12, 0, 0, 0, 0xD0, 0x07, 0, 0, 0, 0][..]));
    assert!(sim.heard.is_empty());
    // or, if it cannot (the phone lacks the procedure), by asking the phone
    sim.event(0x0F, &[0x1A, 1, 0x13, 0x20]);
    sim.run();
    assert_eq!(sim.hear(SIGNALLING), [0x12, 1, 8, 0, 6, 0, 12, 0, 0, 0, 0xD0, 0x07]);

    sim.host.disconnect();
    sim.run();
    assert_eq!(sim.commanded(DISCONNECT), Some(&[0x41, 0, 0x13][..]));
    assert_eq!(sim.take_events(), [Event::Connected, Event::Disconnected(0x16)]);
}

#[test]
fn bonds_as_stored() {
    let bond = |n: u8| Bond { kind: n & 1, address: [n; 6], ltk: [n; 16], irk: [n ^ 0xFF; 16], authenticated: n != 2, bonded: true };
    let mut stored = bonds_to_bytes(&[bond(1), Bond { bonded: false, ..bond(7) }, bond(2)]);
    assert_eq!(stored.len(), 2 * BOND_RECORD);
    assert_eq!(stored[..7], [1, 1, 1, 1, 1, 1, 1]);
    assert_eq!(stored[39..41], [1, 2]);
    assert_eq!(stored[80..], [1, 1]);
    assert_eq!(bonds_from_bytes(&stored), [bond(1), bond(2)]);
    // as trouble-host wrote the type of an address the controller resolved
    stored[0] = 3;
    stored.extend_from_slice(&[0xEE; 20]);
    assert_eq!(bonds_from_bytes(&stored), [bond(1), bond(2)]);
    assert_eq!(bonds_from_bytes(&[0; BOND_RECORD]), [], "an erased record");

    // the oldest bond makes room
    let mut sim = Sim::new(config(Some(42), (10..18).map(bond).collect()), (251, 12));
    let phone = Phone::new(1);
    sim.connect(phone.kind, phone.address);
    phone.pair(&mut sim, |shown| shown).unwrap();
    let bonds = sim.host.bonds();
    assert_eq!((bonds.len(), &bonds[0], bonds[7].address), (MAX_BONDS, &bond(11), phone.identity.1));
}

/// Whatever the controller hands up, the host goes on: a panic would reset the node.
#[test]
fn any_packets_are_survived() {
    let mut sim = Sim::new(config(Some(7), vec![]), (27, 4));
    let phone = Phone::new(1);
    sim.connect(phone.kind, phone.address);
    phone.pair(&mut sim, |shown| shown).unwrap();
    let mut x = 0x0123_4567_89AB_CDEFu64;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for round in 0..300_000u32 {
        let mut packet = [0u8; 40];
        for b in &mut packet {
            let r = next();
            // mostly small values: opcodes, handles and lengths that exist
            *b = if r & 0x300 == 0 { (r >> 16) as u8 } else { (r >> 16) as u8 % 0x15 };
        }
        let r = next();
        let len = 5 + (r >> 8) as usize % 35;
        match r & 3 {
            // an event, a packet of data for the connection, each of the length it says
            0 => (packet[0], packet[2]) = (4, len as u8 - 3),
            1 => (packet[0], packet[1], packet[2], packet[3], packet[4]) = (2, 0x41, 0x20 >> (r >> 4 & 1), len as u8 - 5, 0),
            _ => {}
        }
        sim.host.receive(&packet[..len]);
        sim.host.tick(round / 100);
        if r & 0x30 == 0 {
            sim.host.written();
            sim.host.notify(&packet[..20]);
            sim.host.hurry();
        }
        // what it sends goes nowhere, but must come to an end
        for sent in 0.. {
            while sim.host.event().is_some() {}
            let Some(packet) = sim.host.transmit() else { break };
            assert!(sent < 500, "never done sending");
            match packet[0] {
                1 => sim.host.receive(&[4, 0x0E, 4, 1, packet[1], packet[2], 0]),
                _ => sim.host.receive(&[4, 0x13, 5, 1, 0x41, 0, 1, 0]),
            }
        }
        if sim.host.connection.is_none() {
            sim.host.receive(&[4, 0x3E, 19, 1, 0, 0x41, 0, 1, 1, 1, 2, 3, 4, 5, 0xC6, 24, 0, 0, 0, 0x48, 0, 1]);
        }
    }
    assert!(sim.host.commands.len() < 100 && sim.host.out.acl.len() < 100);
}
