//! Duplicate-packet detection (`SimpleMeshTables`): a ring of recently seen packet hashes.

use crate::MAX_HASH_SIZE;
use crate::packet::Packet;

pub const MAX_PACKET_HASHES: usize = 128 + 32;

pub struct SeenTable {
    hashes: [[u8; MAX_HASH_SIZE]; MAX_PACKET_HASHES],
    next_idx: usize,
    pub direct_dups: u32,
    pub flood_dups: u32,
}

impl Default for SeenTable {
    fn default() -> Self {
        Self::new()
    }
}

impl SeenTable {
    pub const fn new() -> Self {
        Self { hashes: [[0; MAX_HASH_SIZE]; MAX_PACKET_HASHES], next_idx: 0, direct_dups: 0, flood_dups: 0 }
    }

    pub fn was_seen(&mut self, pkt: &Packet) -> bool {
        let h = pkt.calculate_hash();
        if self.hashes.contains(&h) {
            if pkt.is_route_direct() {
                self.direct_dups += 1;
            } else {
                self.flood_dups += 1;
            }
            return true;
        }
        false
    }

    pub fn mark_seen(&mut self, pkt: &Packet) {
        self.hashes[self.next_idx] = pkt.calculate_hash();
        self.next_idx = (self.next_idx + 1) % MAX_PACKET_HASHES;
    }

    pub fn clear(&mut self, pkt: &Packet) {
        let h = pkt.calculate_hash();
        if let Some(slot) = self.hashes.iter_mut().find(|s| **s == h) {
            *slot = [0; MAX_HASH_SIZE];
        }
    }

    pub fn reset_stats(&mut self) {
        self.direct_dups = 0;
        self.flood_dups = 0;
    }
}
