//! The Security Manager of the peripheral ([Vol 3] Part H): LE Secure Connections pairing with a
//! passkey this device shows (or has printed on it) and the peer types in, then the exchange of
//! identity keys that makes a bond.
//!
//! No other pairing: legacy pairing can be listened in on, and without a passkey ("Just Works")
//! the link is not authenticated, which the service asks for.

use alloc::boxed::Box;

use crate::crypto::{self, Addr, KeyPair, flip};
use crate::{Bond, Event, Failure, Out};

/// The Security Manager's channel.
pub(crate) const CID: u16 = 6;

const PAIRING_REQUEST: u8 = 0x01;
const PAIRING_RESPONSE: u8 = 0x02;
const PAIRING_CONFIRM: u8 = 0x03;
const PAIRING_RANDOM: u8 = 0x04;
const PAIRING_FAILED: u8 = 0x05;
const IDENTITY_INFORMATION: u8 = 0x08;
const IDENTITY_ADDRESS_INFORMATION: u8 = 0x09;
const PAIRING_PUBLIC_KEY: u8 = 0x0C;
const PAIRING_DHKEY_CHECK: u8 = 0x0D;
const KEYPRESS_NOTIFICATION: u8 = 0x0E;

/// Why a pairing failed, as the peer is told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Reason {
    OobNotAvailable = 0x02,
    AuthenticationRequirements = 0x03,
    ConfirmValueFailed = 0x04,
    EncryptionKeySize = 0x06,
    CommandNotSupported = 0x07,
    Unspecified = 0x08,
    InvalidParameters = 0x0A,
    DhKeyCheckFailed = 0x0B,
}

const IO_DISPLAY_ONLY: u8 = 0x00;
const IO_KEYBOARD_ONLY: u8 = 0x02;
const IO_KEYBOARD_DISPLAY: u8 = 0x04;
const AUTH_BONDING: u8 = 0x01;
const AUTH_MITM: u8 = 0x04;
const AUTH_SECURE_CONNECTIONS: u8 = 0x08;
const KEY_IDENTITY: u8 = 0x02;
const KEY_SIZE: u8 = 16;
/// The bits of the passkey, each proven in a round of its own.
const ROUNDS: u8 = 20;
/// A pairing with no progress for this long is over, and no other may follow on the connection
/// ([Vol 3] Part H, Section 3.4).
pub(crate) const TIMEOUT_SECS: u32 = 30;
/// But the step a person takes, reading the passkey off the device and typing it in on the other
/// side, gets longer. Peers keep their question up past the 30 s: ended there by the device, the
/// pairing began anew as the peer came back, each time with a new passkey.
pub(crate) const PASSKEY_SECS: u32 = 120;

/// The Security Request ([Vol 3] Part H, Section 3.6.7) to a peer that has a key: it is to
/// encrypt the link with it, or to pair if it has none after all. `mitm`: the key is of a pairing
/// by passkey, and no lesser one will do.
pub(crate) fn security_request(mitm: bool) -> [u8; 2] {
    [0x0B, AUTH_BONDING | AUTH_SECURE_CONNECTIONS | if mitm { AUTH_MITM } else { 0 }]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// No pairing: before one, or after it ended either way.
    Idle,
    /// The response is sent: the peer's public key is due.
    PublicKey,
    /// The peer's commitment of a round is due.
    Confirm(u8),
    /// The nonce of that commitment is due.
    Random(u8),
    /// All rounds passed: the peer's check value is due.
    Check,
    /// Ours is sent: the peer starts encryption with the new key.
    Encryption,
    /// The link is encrypted and our identity sent: the peer's is due, key then address.
    IdentityInformation,
    IdentityAddress,
}

/// What a pairing in progress holds; the values of the toolbox are in its byte order.
struct Session {
    /// Features of the request and of the response, as sent: IO capability, OOB flag, AuthReq,
    /// key size, initiator's and responder's keys.
    request: [u8; 6],
    response: [u8; 6],
    keys: Option<KeyPair>,
    peer_key_x: [u8; 32],
    dh_key: [u8; 32],
    passkey: u32,
    /// The round's commitment of the peer and both nonces.
    confirm: [u8; 16],
    peer_nonce: [u8; 16],
    nonce: [u8; 16],
    /// The bond as far as it is known: complete when the pairing is.
    bond: Bond,
}

/// What the Security Manager needs of the connection and the device.
pub(crate) struct Env<'a> {
    pub out: &'a mut Out,
    pub random: fn(&mut [u8]),
    /// This device's address and the peer's, as the connection was made with them.
    pub local: Addr,
    pub peer: Addr,
    /// The identity resolving key this device gives a bonding peer, as on the air.
    pub irk: [u8; 16],
    /// The passkey, if a fixed one; else a random one per pairing.
    pub passkey: Option<u32>,
    /// Seconds on some clock.
    pub now: u32,
}

pub(crate) struct Smp {
    stage: Stage,
    session: Option<Box<Session>>,
    deadline: u32,
    /// A pairing timed out: nothing more on this connection.
    dead: bool,
}

impl Smp {
    pub(crate) fn new() -> Self {
        Self { stage: Stage::Idle, session: None, deadline: 0, dead: false }
    }

    /// A PDU of the Security Manager's channel. Returns the bond when it completes a pairing.
    pub(crate) fn pdu(&mut self, pdu: &[u8], env: &mut Env<'_>) -> Option<Bond> {
        if self.dead {
            return None;
        }
        let (&code, payload) = pdu.split_first()?;
        match self.step(code, payload, env) {
            Ok(bond) => {
                if self.stage != Stage::Idle {
                    let time = if self.stage == Stage::Confirm(0) { PASSKEY_SECS } else { TIMEOUT_SECS };
                    self.deadline = env.now.wrapping_add(time);
                }
                bond
            }
            Err(reason) => {
                let failure = match reason {
                    Some(reason) => {
                        env.out.l2cap(CID, &[PAIRING_FAILED, reason as u8]);
                        Failure::Refused(reason as u8)
                    }
                    // the peer's own failure is not answered with another
                    None => Failure::Peer(payload.first().copied().unwrap_or(0)),
                };
                self.fail(env.out, failure);
                None
            }
        }
    }

    fn step(&mut self, code: u8, payload: &[u8], env: &mut Env<'_>) -> Result<Option<Bond>, Option<Reason>> {
        let size = match code {
            PAIRING_REQUEST | PAIRING_RESPONSE => 6,
            PAIRING_CONFIRM | PAIRING_RANDOM | PAIRING_DHKEY_CHECK | IDENTITY_INFORMATION | 0x06 | 0x0A => 16,
            PAIRING_FAILED | KEYPRESS_NOTIFICATION | 0x0B => 1,
            0x07 => 10,
            IDENTITY_ADDRESS_INFORMATION => 7,
            PAIRING_PUBLIC_KEY => 64,
            _ => return Err(Some(Reason::CommandNotSupported)),
        };
        if payload.len() != size {
            return Err(Some(Reason::InvalidParameters));
        }
        match (self.stage, code) {
            (_, PAIRING_FAILED) => return Err(None),
            (_, KEYPRESS_NOTIFICATION) => {}
            (Stage::Idle, PAIRING_REQUEST) => self.request(payload, env)?,
            (Stage::PublicKey, PAIRING_PUBLIC_KEY) => self.public_key(payload, env)?,
            (Stage::Confirm(round), PAIRING_CONFIRM) => self.confirm(round, payload, env)?,
            (Stage::Random(round), PAIRING_RANDOM) => self.random(round, payload, env)?,
            (Stage::Check, PAIRING_DHKEY_CHECK) => self.check(payload, env)?,
            (Stage::IdentityInformation, IDENTITY_INFORMATION) => {
                let session = self.session.as_mut().ok_or(Some(Reason::Unspecified))?;
                session.bond.irk.copy_from_slice(payload);
                self.stage = Stage::IdentityAddress;
            }
            (Stage::IdentityAddress, IDENTITY_ADDRESS_INFORMATION) => {
                let session = self.session.as_mut().ok_or(Some(Reason::Unspecified))?;
                if payload[0] > 1 {
                    return Err(Some(Reason::InvalidParameters));
                }
                session.bond.kind = payload[0];
                session.bond.address.copy_from_slice(&payload[1..]);
                return Ok(self.finish());
            }
            _ => return Err(Some(Reason::Unspecified)),
        }
        Ok(None)
    }

    fn request(&mut self, payload: &[u8], env: &mut Env<'_>) -> Result<(), Option<Reason>> {
        let request: [u8; 6] = payload.try_into().map_err(|_| Some(Reason::InvalidParameters))?;
        let [io, oob, auth, key_size, initiator_keys, responder_keys] = request;
        if !(7..=16).contains(&key_size) {
            return Err(Some(Reason::InvalidParameters));
        }
        if auth & AUTH_SECURE_CONNECTIONS == 0 {
            return Err(Some(Reason::AuthenticationRequirements));
        }
        if key_size < KEY_SIZE {
            return Err(Some(Reason::EncryptionKeySize));
        }
        if oob != 0 {
            return Err(Some(Reason::OobNotAvailable));
        }
        // the passkey is typed on the peer: one that cannot take it cannot pair
        if !matches!(io, IO_KEYBOARD_ONLY | IO_KEYBOARD_DISPLAY) {
            return Err(Some(Reason::AuthenticationRequirements));
        }
        // identity keys go the ways the peer asked for, and no other keys: the long term key of
        // LE Secure Connections is made, not sent
        let response = [
            IO_DISPLAY_ONLY,
            0,
            AUTH_BONDING | AUTH_MITM | AUTH_SECURE_CONNECTIONS,
            KEY_SIZE,
            initiator_keys & KEY_IDENTITY,
            responder_keys & KEY_IDENTITY,
        ];
        env.out.l2cap(CID, &[PAIRING_RESPONSE, response[0], response[1], response[2], response[3], response[4], response[5]]);
        self.session = Some(Box::new(Session {
            request,
            response,
            keys: None,
            peer_key_x: [0; 32],
            dh_key: [0; 32],
            passkey: 0,
            confirm: [0; 16],
            peer_nonce: [0; 16],
            nonce: [0; 16],
            bond: Bond {
                kind: env.peer[0],
                address: flip([env.peer[1], env.peer[2], env.peer[3], env.peer[4], env.peer[5], env.peer[6]]),
                ltk: [0; 16],
                irk: [0; 16],
                authenticated: true,
                bonded: auth & AUTH_BONDING != 0,
            },
        }));
        self.stage = Stage::PublicKey;
        Ok(())
    }

    fn public_key(&mut self, payload: &[u8], env: &mut Env<'_>) -> Result<(), Option<Reason>> {
        let session = self.session.as_mut().ok_or(Some(Reason::Unspecified))?;
        let mut peer = [0u8; 64];
        peer.copy_from_slice(payload);
        peer[..32].reverse();
        peer[32..].reverse();
        // a key pair for this pairing alone
        let keys = loop {
            let mut random = [0u8; 32];
            (env.random)(&mut random);
            if let Some(keys) = KeyPair::new(random) {
                break keys;
            }
        };
        session.dh_key = keys.dh_key(&peer).ok_or(Some(Reason::DhKeyCheckFailed))?;
        session.peer_key_x.copy_from_slice(&peer[..32]);
        let mut pdu = [0u8; 65];
        pdu[0] = PAIRING_PUBLIC_KEY;
        pdu[1..].copy_from_slice(&keys.public);
        pdu[1..33].reverse();
        pdu[33..].reverse();
        env.out.l2cap(CID, &pdu);
        session.keys = Some(keys);

        session.passkey = env.passkey.unwrap_or_else(|| loop {
            let mut random = [0u8; 4];
            (env.random)(&mut random);
            // the 20 low bits, of which those up to 999 999 are equally likely
            let key = u32::from_le_bytes(random) & 0xF_FFFF;
            if key <= 999_999 {
                break key;
            }
        });
        env.out.event(Event::Passkey(session.passkey));
        self.stage = Stage::Confirm(0);
        Ok(())
    }

    /// A round of the passkey: the peer commits to its nonce and the round's bit, then we do.
    fn confirm(&mut self, round: u8, payload: &[u8], env: &mut Env<'_>) -> Result<(), Option<Reason>> {
        let session = self.session.as_mut().ok_or(Some(Reason::Unspecified))?;
        let keys = session.keys.as_ref().ok_or(Some(Reason::Unspecified))?;
        session.confirm.copy_from_slice(payload);
        session.confirm.reverse();
        (env.random)(&mut session.nonce);
        let key_x: [u8; 32] = keys.public[..32].try_into().map_err(|_| Some(Reason::Unspecified))?;
        let confirm = crypto::f4(&key_x, &session.peer_key_x, &session.nonce, bit(session.passkey, round));
        send16(env.out, PAIRING_CONFIRM, &confirm);
        self.stage = Stage::Random(round);
        Ok(())
    }

    /// The peer opens its commitment: it holds only if the peer has the passkey's bit.
    fn random(&mut self, round: u8, payload: &[u8], env: &mut Env<'_>) -> Result<(), Option<Reason>> {
        let session = self.session.as_mut().ok_or(Some(Reason::Unspecified))?;
        let keys = session.keys.as_ref().ok_or(Some(Reason::Unspecified))?;
        session.peer_nonce.copy_from_slice(payload);
        session.peer_nonce.reverse();
        let key_x: [u8; 32] = keys.public[..32].try_into().map_err(|_| Some(Reason::Unspecified))?;
        if crypto::f4(&session.peer_key_x, &key_x, &session.peer_nonce, bit(session.passkey, round)) != session.confirm {
            return Err(Some(Reason::ConfirmValueFailed));
        }
        send16(env.out, PAIRING_RANDOM, &session.nonce);
        self.stage = if round + 1 == ROUNDS { Stage::Check } else { Stage::Confirm(round + 1) };
        Ok(())
    }

    /// The check values: each side proves it has the Diffie-Hellman key, with all that was
    /// exchanged; the long term key comes of the same.
    fn check(&mut self, payload: &[u8], env: &mut Env<'_>) -> Result<(), Option<Reason>> {
        let session = self.session.as_mut().ok_or(Some(Reason::Unspecified))?;
        let (mac_key, ltk) = crypto::f5(&session.dh_key, &session.peer_nonce, &session.nonce, &env.peer, &env.local);
        let mut passkey = [0u8; 16];
        passkey[12..].copy_from_slice(&session.passkey.to_be_bytes());
        // AuthReq, OOB flag, IO capability
        let io = |f: &[u8; 6]| [f[2], f[1], f[0]];
        let theirs =
            crypto::f6(&mac_key, &session.peer_nonce, &session.nonce, &passkey, &io(&session.request), &env.peer, &env.local);
        let mut given = [0u8; 16];
        given.copy_from_slice(payload);
        if flip(given) != theirs {
            return Err(Some(Reason::DhKeyCheckFailed));
        }
        let ours =
            crypto::f6(&mac_key, &session.nonce, &session.peer_nonce, &passkey, &io(&session.response), &env.local, &env.peer);
        send16(env.out, PAIRING_DHKEY_CHECK, &ours);
        session.bond.ltk = flip(ltk);
        self.stage = Stage::Encryption;
        Ok(())
    }

    /// The key of a pairing that has come to starting encryption, as the controller takes it.
    pub(crate) fn ltk(&self) -> Option<[u8; 16]> {
        match (self.stage, &self.session) {
            (Stage::Encryption, Some(session)) => Some(session.bond.ltk),
            _ => None,
        }
    }

    /// The link's encryption changed. Returns the bond when that completes a pairing.
    pub(crate) fn encrypted(&mut self, on: bool, env: &mut Env<'_>) -> Option<Bond> {
        if self.stage != Stage::Encryption {
            return None;
        }
        if !on {
            self.fail(env.out, Failure::NotEncrypted);
            return None;
        }
        let session = self.session.as_ref()?;
        let [_, _, _, _, initiator_keys, responder_keys] = session.response;
        if responder_keys & KEY_IDENTITY != 0 {
            let mut pdu = [0u8; 17];
            pdu[0] = IDENTITY_INFORMATION;
            pdu[1..].copy_from_slice(&env.irk);
            env.out.l2cap(CID, &pdu);
            // the address this device is known by: the one it connects with
            let mut pdu = [0u8; 8];
            pdu[0] = IDENTITY_ADDRESS_INFORMATION;
            pdu[1..].copy_from_slice(&env.local);
            pdu[2..].reverse();
            env.out.l2cap(CID, &pdu);
        }
        if initiator_keys & KEY_IDENTITY != 0 {
            self.stage = Stage::IdentityInformation;
            self.deadline = env.now.wrapping_add(TIMEOUT_SECS);
            None
        } else {
            self.finish()
        }
    }

    fn finish(&mut self) -> Option<Bond> {
        self.stage = Stage::Idle;
        self.session.take().map(|session| session.bond.clone())
    }

    fn fail(&mut self, out: &mut Out, why: Failure) {
        // the peer asks for the passkey before the first round, and goes on once it has one
        let passkey_entered = !matches!(self.stage, Stage::Idle | Stage::PublicKey | Stage::Confirm(0));
        self.stage = Stage::Idle;
        self.session = None;
        out.event(Event::PairingFailed { why, passkey_entered });
    }

    /// The connection is gone, and a pairing under way with it.
    pub(crate) fn link_lost(&mut self, out: &mut Out) {
        if self.stage != Stage::Idle {
            self.fail(out, Failure::LinkLost);
        }
    }

    /// The clock moved on: a pairing that stalled is over.
    pub(crate) fn tick(&mut self, now: u32, out: &mut Out) {
        // wrapping: the clock may be one that starts over
        if self.stage != Stage::Idle && now.wrapping_sub(self.deadline) < u32::MAX / 2 {
            self.dead = true;
            self.fail(out, Failure::Stalled);
        }
    }
}

/// What a round commits to beside the keys: a bit of the passkey.
fn bit(passkey: u32, round: u8) -> u8 {
    0x80 | (passkey >> round) as u8 & 1
}

/// A PDU of a 16-byte value of the toolbox.
fn send16(out: &mut Out, code: u8, value: &[u8; 16]) {
    let mut pdu = [0u8; 17];
    pdu[0] = code;
    pdu[1..].copy_from_slice(&flip(*value));
    out.l2cap(CID, &pdu);
}
