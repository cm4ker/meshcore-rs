//! The fundamental transmission unit and its wire format.
//!
//! Wire layout:
//! ```text
//! header(1) [transport_codes(2+2, LE)] path_len(1) path(N) payload(rest)
//! ```
//! `header` = `ver(2) | payload_type(4) | route_type(2)`.
//! `path_len` = `(hash_size-1)(2) | hash_count(6)`.

use sha2::{Digest, Sha256};

use crate::{MAX_HASH_SIZE, MAX_PACKET_PAYLOAD, MAX_PATH_SIZE, MAX_TRANS_UNIT};

pub const PH_ROUTE_MASK: u8 = 0x03;
pub const PH_TYPE_SHIFT: u8 = 2;
pub const PH_TYPE_MASK: u8 = 0x0F;
pub const PH_VER_SHIFT: u8 = 6;
pub const PH_VER_MASK: u8 = 0x03;

pub const ROUTE_TYPE_TRANSPORT_FLOOD: u8 = 0x00;
pub const ROUTE_TYPE_FLOOD: u8 = 0x01;
pub const ROUTE_TYPE_DIRECT: u8 = 0x02;
pub const ROUTE_TYPE_TRANSPORT_DIRECT: u8 = 0x03;

pub const PAYLOAD_TYPE_REQ: u8 = 0x00;
pub const PAYLOAD_TYPE_RESPONSE: u8 = 0x01;
pub const PAYLOAD_TYPE_TXT_MSG: u8 = 0x02;
pub const PAYLOAD_TYPE_ACK: u8 = 0x03;
pub const PAYLOAD_TYPE_ADVERT: u8 = 0x04;
pub const PAYLOAD_TYPE_GRP_TXT: u8 = 0x05;
pub const PAYLOAD_TYPE_GRP_DATA: u8 = 0x06;
pub const PAYLOAD_TYPE_ANON_REQ: u8 = 0x07;
pub const PAYLOAD_TYPE_PATH: u8 = 0x08;
pub const PAYLOAD_TYPE_TRACE: u8 = 0x09;
pub const PAYLOAD_TYPE_MULTIPART: u8 = 0x0A;
pub const PAYLOAD_TYPE_CONTROL: u8 = 0x0B;
pub const PAYLOAD_TYPE_RAW_CUSTOM: u8 = 0x0F;

pub const PAYLOAD_VER_1: u8 = 0x00;

#[derive(Clone)]
pub struct Packet {
    pub header: u8,
    pub transport_codes: [u16; 2],
    /// Encoded path length (hash size in the upper 2 bits, hash count in the lower 6).
    /// For DIRECT `TRACE` packets this is a plain count of SNR bytes.
    pub path_len: u8,
    pub path: [u8; MAX_PATH_SIZE],
    pub payload_len: usize,
    pub payload: [u8; MAX_PACKET_PAYLOAD],
    /// Received SNR, multiplied by 4.
    pub snr: i8,
}

impl Default for Packet {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for Packet {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Packet")
            .field("route", &self.route_type())
            .field("type", &self.payload_type())
            .field("path_len", &self.path_len)
            .field("payload_len", &self.payload_len)
            .finish()
    }
}

impl Packet {
    pub const fn new() -> Self {
        Self {
            header: 0,
            transport_codes: [0; 2],
            path_len: 0,
            path: [0; MAX_PATH_SIZE],
            payload_len: 0,
            payload: [0; MAX_PACKET_PAYLOAD],
            snr: 0,
        }
    }

    /// New packet of the given payload type (route type is set when sending).
    pub fn with_type(payload_type: u8) -> Self {
        let mut p = Self::new();
        p.header = (payload_type & PH_TYPE_MASK) << PH_TYPE_SHIFT;
        p
    }

    pub fn route_type(&self) -> u8 {
        self.header & PH_ROUTE_MASK
    }

    pub fn set_route_type(&mut self, route: u8) {
        self.header = (self.header & !PH_ROUTE_MASK) | (route & PH_ROUTE_MASK);
    }

    pub fn is_route_flood(&self) -> bool {
        matches!(self.route_type(), ROUTE_TYPE_FLOOD | ROUTE_TYPE_TRANSPORT_FLOOD)
    }

    pub fn is_route_direct(&self) -> bool {
        matches!(self.route_type(), ROUTE_TYPE_DIRECT | ROUTE_TYPE_TRANSPORT_DIRECT)
    }

    pub fn has_transport_codes(&self) -> bool {
        matches!(self.route_type(), ROUTE_TYPE_TRANSPORT_FLOOD | ROUTE_TYPE_TRANSPORT_DIRECT)
    }

    pub fn payload_type(&self) -> u8 {
        (self.header >> PH_TYPE_SHIFT) & PH_TYPE_MASK
    }

    pub fn payload_ver(&self) -> u8 {
        (self.header >> PH_VER_SHIFT) & PH_VER_MASK
    }

    pub fn path_hash_size(&self) -> usize {
        ((self.path_len >> 6) + 1) as usize
    }

    pub fn path_hash_count(&self) -> usize {
        (self.path_len & 63) as usize
    }

    pub fn path_byte_len(&self) -> usize {
        self.path_hash_count() * self.path_hash_size()
    }

    pub fn set_path_hash_count(&mut self, n: usize) {
        self.path_len = (self.path_len & !63) | (n as u8 & 63);
    }

    pub fn set_path_hash_size_and_count(&mut self, size: usize, n: usize) {
        self.path_len = (((size as u8).wrapping_sub(1)) << 6) | (n as u8 & 63);
    }

    /// Byte length of the path described by an encoded `path_len`.
    pub fn path_bytes_for(path_len: u8) -> usize {
        ((path_len & 63) as usize) * (((path_len >> 6) + 1) as usize)
    }

    pub fn is_valid_path_len(path_len: u8) -> bool {
        let hash_size = (path_len >> 6) + 1;
        if hash_size == 4 {
            return false; // reserved for future
        }
        Self::path_bytes_for(path_len) <= MAX_PATH_SIZE
    }

    /// The encoded path (hashes only).
    pub fn path(&self) -> &[u8] {
        &self.path[..self.path_byte_len().min(MAX_PATH_SIZE)]
    }

    pub fn set_path(&mut self, path_len: u8, path: &[u8]) {
        let n = Self::path_bytes_for(path_len).min(MAX_PATH_SIZE).min(path.len());
        self.path[..n].copy_from_slice(&path[..n]);
        self.path_len = path_len;
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload[..self.payload_len]
    }

    pub fn set_payload(&mut self, data: &[u8]) -> bool {
        if data.len() > MAX_PACKET_PAYLOAD {
            return false;
        }
        self.payload[..data.len()].copy_from_slice(data);
        self.payload_len = data.len();
        true
    }

    pub fn mark_do_not_retransmit(&mut self) {
        self.header = 0xFF;
    }

    pub fn is_marked_do_not_retransmit(&self) -> bool {
        self.header == 0xFF
    }

    pub fn snr_db(&self) -> f32 {
        self.snr as f32 / 4.0
    }

    /// Length of the encoded frame.
    pub fn raw_length(&self) -> usize {
        2 + self.path_byte_len() + self.payload_len + if self.has_transport_codes() { 4 } else { 0 }
    }

    /// Hash used for duplicate detection: SHA-256 over type (+ path_len for TRACE) + payload, truncated.
    pub fn calculate_hash(&self) -> [u8; MAX_HASH_SIZE] {
        let mut sha = Sha256::new();
        let t = self.payload_type();
        sha.update([t]);
        if t == PAYLOAD_TYPE_TRACE {
            // upstream hashes `uint16_t path_len` in little-endian (TRACE packets revisit nodes)
            sha.update([self.path_len, 0]);
        }
        sha.update(self.payload());
        let digest = sha.finalize();
        let mut out = [0u8; MAX_HASH_SIZE];
        out.copy_from_slice(&digest[..MAX_HASH_SIZE]);
        out
    }

    /// Serialize to wire format. Returns the frame length, or `None` if it would exceed the MTU.
    pub fn write_to(&self, dest: &mut [u8; MAX_TRANS_UNIT]) -> Option<usize> {
        let path_bytes = Self::path_bytes_for(self.path_len);
        if path_bytes > MAX_PATH_SIZE {
            return None;
        }
        let mut i = 0;
        dest[i] = self.header;
        i += 1;
        if self.has_transport_codes() {
            dest[i..i + 2].copy_from_slice(&self.transport_codes[0].to_le_bytes());
            dest[i + 2..i + 4].copy_from_slice(&self.transport_codes[1].to_le_bytes());
            i += 4;
        }
        dest[i] = self.path_len;
        i += 1;
        if i + path_bytes + self.payload_len > MAX_TRANS_UNIT {
            return None;
        }
        dest[i..i + path_bytes].copy_from_slice(&self.path[..path_bytes]);
        i += path_bytes;
        dest[i..i + self.payload_len].copy_from_slice(self.payload());
        i += self.payload_len;
        Some(i)
    }

    /// Parse a received frame (`Dispatcher::tryParsePacket` semantics, with strict bounds checks).
    pub fn parse(raw: &[u8]) -> Option<Packet> {
        let mut pkt = Packet::new();
        let mut i = 0;
        pkt.header = *raw.first()?;
        i += 1;
        if pkt.payload_ver() > PAYLOAD_VER_1 {
            return None; // unsupported version
        }
        if pkt.has_transport_codes() {
            if raw.len() < i + 4 {
                return None;
            }
            pkt.transport_codes[0] = u16::from_le_bytes([raw[i], raw[i + 1]]);
            pkt.transport_codes[1] = u16::from_le_bytes([raw[i + 2], raw[i + 3]]);
            i += 4;
        }
        pkt.path_len = *raw.get(i)?;
        i += 1;
        if pkt.path_len >> 6 == 3 {
            return None; // reserved path mode
        }
        let path_bytes = pkt.path_byte_len();
        if path_bytes > MAX_PATH_SIZE || i + path_bytes > raw.len() {
            return None; // partial or corrupt
        }
        pkt.path[..path_bytes].copy_from_slice(&raw[i..i + path_bytes]);
        i += path_bytes;

        let payload_len = raw.len() - i;
        if payload_len > MAX_PACKET_PAYLOAD {
            return None;
        }
        pkt.payload[..payload_len].copy_from_slice(&raw[i..]);
        pkt.payload_len = payload_len;
        Some(pkt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_flood() {
        let mut p = Packet::with_type(PAYLOAD_TYPE_TXT_MSG);
        p.set_route_type(ROUTE_TYPE_FLOOD);
        p.set_path_hash_size_and_count(1, 3);
        p.path[..3].copy_from_slice(&[0xAA, 0xBB, 0xCC]);
        p.set_payload(&[1, 2, 3, 4, 5]);
        let mut buf = [0u8; MAX_TRANS_UNIT];
        let n = p.write_to(&mut buf).unwrap();
        assert_eq!(&buf[..n], &[0x09, 0x03, 0xAA, 0xBB, 0xCC, 1, 2, 3, 4, 5]);
        assert_eq!(n, p.raw_length());
        let q = Packet::parse(&buf[..n]).unwrap();
        assert_eq!(q.header, p.header);
        assert_eq!(q.path(), p.path());
        assert_eq!(q.payload(), p.payload());
    }

    #[test]
    fn roundtrip_transport_codes() {
        let mut p = Packet::with_type(PAYLOAD_TYPE_ADVERT);
        p.set_route_type(ROUTE_TYPE_TRANSPORT_FLOOD);
        p.transport_codes = [0x1234, 0xABCD];
        p.set_path_hash_size_and_count(2, 1);
        p.path[..2].copy_from_slice(&[7, 8]);
        p.set_payload(&[9]);
        let mut buf = [0u8; MAX_TRANS_UNIT];
        let n = p.write_to(&mut buf).unwrap();
        assert_eq!(&buf[..n], &[0x10, 0x34, 0x12, 0xCD, 0xAB, 0x41, 7, 8, 9]);
        let q = Packet::parse(&buf[..n]).unwrap();
        assert_eq!(q.transport_codes, [0x1234, 0xABCD]);
        assert_eq!(q.path_hash_size(), 2);
        assert_eq!(q.path(), &[7, 8]);
    }

    #[test]
    fn parse_rejects_bad_frames() {
        assert!(Packet::parse(&[]).is_none());
        assert!(Packet::parse(&[0x09]).is_none());
        assert!(Packet::parse(&[0x49, 0x00, 1]).is_none()); // version 1
        assert!(Packet::parse(&[0x09, 0xC0, 1]).is_none()); // path mode 3
        assert!(Packet::parse(&[0x09, 0x05, 1, 2]).is_none()); // truncated path
        assert!(Packet::parse(&[0x00, 0x01]).is_none()); // truncated transport codes
        assert!(Packet::parse(&[0x09, 0x00]).is_some()); // empty payload is allowed
    }

    #[test]
    fn trace_hash_includes_path_len() {
        let mut a = Packet::with_type(PAYLOAD_TYPE_TRACE);
        a.set_payload(&[1, 2, 3]);
        let mut b = a.clone();
        b.path_len = 2;
        assert_ne!(a.calculate_hash(), b.calculate_hash());
        let mut c = Packet::with_type(PAYLOAD_TYPE_TXT_MSG);
        c.set_payload(&[1, 2, 3]);
        let mut d = c.clone();
        d.path_len = 2;
        assert_eq!(c.calculate_hash(), d.calculate_hash());
    }
}
