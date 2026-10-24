//! Bridge: packets carried between repeaters over another medium than LoRa, wire-compatible with
//! upstream's `BridgeBase` and `ESPNowBridge` (ESP-NOW broadcast between ESP32 boards).
//!
//! The repeater hands the bridge every packet it transmits (or, with `bridge.source rx`, every
//! packet it receives); the bridge sends each one once. Packets that come in over the bridge are
//! processed as if heard on LoRa, after `bridge.delay`, once each: a packet that went out over
//! the bridge does not come back, and one that came in is not sent back out when the repeater
//! retransmits it.
//!
//! A frame: the magic 0xC03E (big endian), then, XORed with the shared secret, a Fletcher-16
//! checksum of the packet (big endian) and the packet in its wire format. The XOR keeps bridges
//! with different secrets apart; it is no protection against anyone who listens.

use alloc::vec::Vec;

use crate::MAX_TRANS_UNIT;
use crate::packet::Packet;
use crate::tables::SeenTable;

pub const MAGIC: u16 = 0xC03E;
/// The largest ESP-NOW frame; larger packets are not bridged.
pub const MAX_FRAME: usize = 250;
const HEADER: usize = 4;

/// Fletcher-16 as upstream computes it.
pub fn fletcher16(data: &[u8]) -> u16 {
    let (mut a, mut b) = (0u16, 0u16);
    for &d in data {
        a = (a + d as u16) % 255;
        b = (b + a) % 255;
    }
    (b << 8) | a
}

pub struct Bridge {
    seen: SeenTable,
    secret: Vec<u8>,
}

impl Bridge {
    pub fn new(secret: &str) -> Self {
        Self { seen: SeenTable::new(), secret: secret.as_bytes().to_vec() }
    }

    pub fn set_secret(&mut self, secret: &str) {
        self.secret = secret.as_bytes().to_vec();
    }

    fn xor(&self, data: &mut [u8]) {
        // upstream divides by the secret's length: an empty secret would crash it, here it
        // leaves the frame as is
        if self.secret.is_empty() {
            return;
        }
        for (i, d) in data.iter_mut().enumerate() {
            *d ^= self.secret[i % self.secret.len()];
        }
    }

    /// The frame to send for `pkt`, unless it went over the bridge (either way) before.
    pub fn encode(&mut self, pkt: &Packet) -> Option<Vec<u8>> {
        if self.seen.was_seen(pkt) {
            return None;
        }
        self.seen.mark_seen(pkt);
        let mut raw = [0u8; MAX_TRANS_UNIT];
        let n = pkt.write_to(&mut raw)?;
        if HEADER + n > MAX_FRAME {
            return None;
        }
        let mut frame = Vec::with_capacity(HEADER + n);
        frame.extend_from_slice(&MAGIC.to_be_bytes());
        frame.extend_from_slice(&fletcher16(&raw[..n]).to_be_bytes());
        frame.extend_from_slice(&raw[..n]);
        self.xor(&mut frame[2..]);
        Some(frame)
    }

    /// The packet in a frame from the bridge, if the frame is ours (magic, secret, checksum) and
    /// the packet is new.
    pub fn decode(&mut self, frame: &[u8]) -> Option<Packet> {
        if frame.len() < HEADER || frame.len() > MAX_FRAME || frame[..2] != MAGIC.to_be_bytes() {
            return None;
        }
        let mut data = frame[2..].to_vec();
        self.xor(&mut data);
        let (sum, raw) = data.split_at(2);
        if u16::from_be_bytes([sum[0], sum[1]]) != fletcher16(raw) {
            return None;
        }
        let pkt = Packet::parse(raw)?;
        if self.seen.was_seen(&pkt) {
            return None;
        }
        self.seen.mark_seen(&pkt);
        Some(pkt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(payload: &[u8]) -> Packet {
        // a flood TXT_MSG with no path
        let mut raw = alloc::vec![0x09, 0x00];
        raw.extend_from_slice(payload);
        Packet::parse(&raw).unwrap()
    }

    #[test]
    fn frames_go_both_ways_once() {
        let mut a = Bridge::new("LVSITANOS");
        let mut b = Bridge::new("LVSITANOS");
        let p = packet(b"hello over the bridge");
        let frame = a.encode(&p).unwrap();
        assert_eq!(&frame[..2], &[0xC0, 0x3E]);
        assert!(a.encode(&p).is_none(), "sent once");
        let got = b.decode(&frame).unwrap();
        assert_eq!(got.payload[..got.payload_len], p.payload[..p.payload_len]);
        assert!(b.decode(&frame).is_none(), "taken once");
        assert!(b.encode(&got).is_none(), "not sent back when retransmitted");
    }

    #[test]
    fn other_secrets_and_damage_are_ignored() {
        let mut a = Bridge::new("LVSITANOS");
        let mut other = Bridge::new("other");
        let frame = a.encode(&packet(b"x")).unwrap();
        assert!(other.decode(&frame).is_none());
        let mut damaged = frame.clone();
        *damaged.last_mut().unwrap() ^= 1;
        assert!(Bridge::new("LVSITANOS").decode(&damaged).is_none());
        assert!(Bridge::new("LVSITANOS").decode(&frame).is_some());
    }

    #[test]
    fn checksum_matches_upstream() {
        // Fletcher-16 of "abcde" (the usual test vector)
        assert_eq!(fletcher16(b"abcde"), 0xC8F0);
    }
}
