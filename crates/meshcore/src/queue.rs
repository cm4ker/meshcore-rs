//! Priority + schedule queue (`PacketQueue` from `StaticPoolPacketManager`).

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::packet::Packet;

struct Entry {
    packet: Box<Packet>,
    priority: u8,
    scheduled_for: u64,
}

pub struct PacketQueue {
    entries: Vec<Entry>,
    capacity: usize,
}

impl PacketQueue {
    pub fn new(capacity: usize) -> Self {
        Self { entries: Vec::with_capacity(capacity), capacity }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the packet back if the queue is full.
    pub fn add(&mut self, packet: Box<Packet>, priority: u8, scheduled_for: u64) -> Result<(), Box<Packet>> {
        if self.entries.len() >= self.capacity {
            return Err(packet);
        }
        self.entries.push(Entry { packet, priority, scheduled_for });
        Ok(())
    }

    /// Number of entries whose scheduled time has arrived.
    pub fn count_due(&self, now: u64) -> usize {
        self.entries.iter().filter(|e| e.scheduled_for <= now).count()
    }

    /// Earliest scheduled time of any entry.
    pub fn next_due(&self) -> Option<u64> {
        self.entries.iter().map(|e| e.scheduled_for).min()
    }

    /// Take the most important (lowest priority value) due entry; ties keep insertion order.
    pub fn get(&mut self, now: u64) -> Option<Box<Packet>> {
        let mut best: Option<(usize, u8)> = None;
        for (i, e) in self.entries.iter().enumerate() {
            if e.scheduled_for > now {
                continue;
            }
            if best.is_none_or(|(_, p)| e.priority < p) {
                best = Some((i, e.priority));
            }
        }
        best.map(|(i, _)| self.entries.remove(i).packet)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Packet> {
        self.entries.iter().map(|e| &*e.packet)
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(tag: u8) -> Box<Packet> {
        let mut p = Box::new(Packet::new());
        p.set_payload(&[tag]);
        p
    }

    #[test]
    fn priority_and_schedule() {
        let mut q = PacketQueue::new(4);
        q.add(pkt(1), 3, 0).unwrap();
        q.add(pkt(2), 1, 100).unwrap();
        q.add(pkt(3), 2, 0).unwrap();
        q.add(pkt(4), 2, 0).unwrap();
        assert!(q.add(pkt(5), 0, 0).is_err());
        assert_eq!(q.count_due(0), 3);
        assert_eq!(q.get(0).unwrap().payload(), &[3]);
        assert_eq!(q.get(0).unwrap().payload(), &[4]);
        assert_eq!(q.get(0).unwrap().payload(), &[1]);
        assert!(q.get(50).is_none());
        assert_eq!(q.next_due(), Some(100));
        assert_eq!(q.get(100).unwrap().payload(), &[2]);
    }
}
