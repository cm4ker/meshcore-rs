//! Regions ("scopes") and transport codes: port of upstream `RegionMap` / `TransportKey`.
//!
//! A flood packet can carry a transport code (HMAC of the payload under a region key). Repeaters
//! only re-flood scoped packets for regions they know and allow, which lets a mesh be zoned.
//! Region keys are public for `#hashtag` names (and plain names, treated as `#name`):
//! `key = SHA-256("#name")[..16]`. `$private` regions need a provisioned key.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::crypto;
use crate::packet::Packet;
use crate::util::atoi;

pub const MAX_REGION_ENTRIES: usize = 32;
pub const REGION_NAME_MAX: usize = 31;
pub const REGION_DENY_FLOOD: u8 = 0x01;
pub const REGION_DENY_DIRECT: u8 = 0x02; // reserved

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct TransportKey(pub [u8; 16]);

impl TransportKey {
    pub fn is_null(&self) -> bool {
        self.0 == [0; 16]
    }

    /// Public key of a `#hashtag` region.
    pub fn for_hashtag(name: &str) -> Self {
        let mut k = [0u8; 16];
        crypto::sha256(&mut k, &[name.as_bytes()]);
        Self(k)
    }

    /// `TransportKey::calcTransportCode`: codes 0000 and FFFF are reserved.
    pub fn transport_code(&self, pkt: &Packet) -> u16 {
        let mut out = [0u8; 2];
        crypto::hmac_sha256(&self.0, &mut out, &[&[pkt.payload_type()], pkt.payload()]);
        match u16::from_le_bytes(out) {
            0 => 1,
            0xFFFF => 0xFFFE,
            c => c,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionEntry {
    pub id: u16,
    pub parent: u16,
    pub flags: u8,
    pub name: String,
}

impl RegionEntry {
    pub fn is_wildcard(&self) -> bool {
        self.id == 0
    }

    pub fn allows_flood(&self) -> bool {
        self.flags & REGION_DENY_FLOOD == 0
    }
}

fn skip_hash(name: &str) -> &str {
    name.strip_prefix('#').unwrap_or(name)
}

/// Accept alphanumerics and accented characters, reject most punctuation.
pub fn is_name_char(c: u8) -> bool {
    c == b'-' || c == b'$' || c == b'#' || c.is_ascii_digit() || c >= b'A'
}

#[derive(Clone, Debug)]
pub struct RegionMap {
    next_id: u16,
    home_id: u16,
    default_id: u16,
    regions: Vec<RegionEntry>,
    wildcard: RegionEntry,
}

impl Default for RegionMap {
    fn default() -> Self {
        Self::new()
    }
}

impl RegionMap {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            home_id: 0,
            default_id: 0,
            regions: Vec::new(),
            wildcard: RegionEntry { id: 0, parent: 0, flags: 0, name: "*".into() },
        }
    }

    /// Empty map that keeps the id counter and settings (upstream `resetFrom`, for `region load`).
    pub fn cleared(mut self) -> Self {
        self.regions.clear();
        self
    }

    pub fn regions(&self) -> &[RegionEntry] {
        &self.regions
    }

    pub fn wildcard(&self) -> &RegionEntry {
        &self.wildcard
    }

    pub fn len(&self) -> usize {
        self.regions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    /// Create a region, or re-parent an existing one. New regions deny flood by default.
    pub fn put_region(&mut self, name: &str, parent_id: u16, id: u16) -> Option<u16> {
        if name.is_empty() || !name.bytes().all(is_name_char) {
            return None;
        }
        if let Some(i) = self.index_by_name(name) {
            if self.regions[i].id == parent_id {
                return None; // cannot be its own parent
            }
            self.regions[i].parent = parent_id;
            return Some(self.regions[i].id);
        }
        if id == 0 && self.regions.len() >= MAX_REGION_ENTRIES {
            return None;
        }
        let id = if id == 0 {
            self.next_id += 1;
            self.next_id - 1
        } else {
            self.next_id = self.next_id.max(id + 1);
            id
        };
        let mut n = String::new();
        for c in name.chars() {
            if n.len() + c.len_utf8() > REGION_NAME_MAX - 1 {
                break;
            }
            n.push(c);
        }
        self.regions.push(RegionEntry { id, parent: parent_id, flags: REGION_DENY_FLOOD, name: n });
        Some(id)
    }

    fn index_by_name(&self, name: &str) -> Option<usize> {
        let name = skip_hash(name);
        self.regions.iter().position(|r| skip_hash(&r.name) == name)
    }

    pub fn find_by_name(&self, name: &str) -> Option<&RegionEntry> {
        if name == "*" {
            return Some(&self.wildcard);
        }
        self.index_by_name(name).map(|i| &self.regions[i])
    }

    /// Exact match preferred, else the last region whose name starts with `prefix`.
    pub fn find_by_name_prefix(&self, prefix: &str) -> Option<&RegionEntry> {
        if prefix == "*" {
            return Some(&self.wildcard);
        }
        let prefix = skip_hash(prefix);
        let mut partial = None;
        for r in &self.regions {
            let n = skip_hash(&r.name);
            if n == prefix {
                return Some(r);
            }
            if n.starts_with(prefix) {
                partial = Some(r);
            }
        }
        partial
    }

    pub fn find_by_id(&self, id: u16) -> Option<&RegionEntry> {
        if id == 0 {
            return Some(&self.wildcard);
        }
        self.regions.iter().find(|r| r.id == id)
    }

    pub fn entry_mut(&mut self, id: u16) -> Option<&mut RegionEntry> {
        if id == 0 {
            return Some(&mut self.wildcard);
        }
        self.regions.iter_mut().find(|r| r.id == id)
    }

    pub fn home(&self) -> Option<&RegionEntry> {
        self.find_by_id(self.home_id)
    }

    pub fn set_home(&mut self, id: u16) {
        self.home_id = id;
    }

    pub fn default_region(&self) -> Option<&RegionEntry> {
        if self.default_id == 0 { None } else { self.find_by_id(self.default_id) }
    }

    pub fn set_default(&mut self, id: u16) {
        self.default_id = id;
    }

    /// Remove a leaf region (fails for the wildcard and for regions with children).
    pub fn remove(&mut self, id: u16) -> bool {
        if id == 0 || self.regions.iter().any(|r| r.parent == id) {
            return false;
        }
        match self.regions.iter().position(|r| r.id == id) {
            Some(i) => {
                self.regions.remove(i);
                true
            }
            None => false,
        }
    }

    /// Transport keys of a region (`getTransportKeysFor`). `$private` regions have none yet.
    pub fn transport_keys(&self, r: &RegionEntry) -> Vec<TransportKey> {
        if r.name.starts_with('$') {
            Vec::new()
        } else if r.name.starts_with('#') {
            alloc::vec![TransportKey::for_hashtag(&r.name)]
        } else {
            alloc::vec![TransportKey::for_hashtag(&format!("#{}", r.name))]
        }
    }

    pub fn first_key(&self, r: &RegionEntry) -> Option<TransportKey> {
        self.transport_keys(r).into_iter().next()
    }

    /// Region of a received transport-flood packet, among regions not blocked by `mask`.
    pub fn find_match(&self, pkt: &Packet, mask: u8) -> Option<&RegionEntry> {
        if pkt.transport_codes[0] == 0 {
            return Some(&self.wildcard);
        }
        self.regions.iter().filter(|r| r.flags & mask == 0).find(|r| {
            self.transport_keys(r).iter().any(|k| k.transport_code(pkt) == pkt.transport_codes[0])
        })
    }

    /// Indented tree, `F` marks flood-allowed, `^` the home region (`exportTo`).
    pub fn export_tree(&self) -> String {
        let mut out = String::new();
        self.export_children(0, &self.wildcard, &mut out);
        out
    }

    fn export_children(&self, indent: usize, parent: &RegionEntry, out: &mut String) {
        for _ in 0..indent {
            out.push(' ');
        }
        let home = if parent.id == self.home_id { "^" } else { "" };
        let flood = if parent.allows_flood() { " F" } else { "" };
        let _ = writeln!(out, "{}{}{}", skip_hash(&parent.name), home, flood);
        for r in self.regions.iter().filter(|r| r.parent == parent.id && r.id != parent.id) {
            self.export_children(indent + 1, r, out);
        }
    }

    /// Comma separated names; `invert` selects regions that DO have `mask` (`exportNamesTo`).
    pub fn export_names(&self, mask: u8, invert: bool, max_len: usize) -> String {
        let matches = |r: &RegionEntry| if invert { r.flags & mask != 0 } else { r.flags & mask == 0 };
        let mut out = String::new();
        if matches(&self.wildcard) {
            out.push_str("*,");
        }
        for r in self.regions.iter().filter(|r| matches(r)) {
            let n = skip_hash(&r.name);
            if out.len() + n.len() + 2 < max_len {
                out.push_str(n);
                out.push(',');
            }
        }
        out.pop(); // trailing comma
        out
    }

    /// Persistent text form: header `default,home,wildcard_flags,next_id`, then
    /// `id,parent,flags,name` per region.
    pub fn serialize(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "{},{},{},{}", self.default_id, self.home_id, self.wildcard.flags, self.next_id);
        for r in &self.regions {
            let _ = writeln!(s, "{},{},{},{}", r.id, r.parent, r.flags, r.name);
        }
        s
    }

    pub fn deserialize(text: &str) -> Self {
        let mut map = Self::new();
        let mut lines = text.lines();
        if let Some(h) = lines.next() {
            let f: Vec<&str> = h.split(',').collect();
            if f.len() == 4 {
                map.default_id = atoi(f[0]) as u16;
                map.home_id = atoi(f[1]) as u16;
                map.wildcard.flags = atoi(f[2]) as u8;
                map.next_id = (atoi(f[3]) as u16).max(1);
            }
        }
        for l in lines {
            let mut f = l.splitn(4, ',');
            let (Some(id), Some(parent), Some(flags), Some(name)) = (f.next(), f.next(), f.next(), f.next()) else {
                continue;
            };
            let id = atoi(id) as u16;
            if id == 0 || map.regions.len() >= MAX_REGION_ENTRIES {
                continue;
            }
            map.regions.push(RegionEntry { id, parent: atoi(parent) as u16, flags: atoi(flags) as u8, name: name.into() });
            map.next_id = map.next_id.max(id + 1);
        }
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::*;

    fn advert_packet() -> Packet {
        let mut p = Packet::with_type(PAYLOAD_TYPE_ADVERT);
        p.set_payload(&(0..120u8).collect::<alloc::vec::Vec<_>>());
        p
    }

    /// Vectors computed independently (Python hashlib/hmac) from the upstream C++ definitions.
    #[test]
    fn keys_and_codes_match_reference() {
        let k = TransportKey::for_hashtag("#omsk");
        assert_eq!(hex::encode(k.0), "4bc010baf2c9adf31fe30210d907155a");
        assert_eq!(k.transport_code(&advert_packet()), 0x934B);
    }

    #[test]
    fn match_and_tree() {
        let mut m = RegionMap::new();
        let ru = m.put_region("ru", 0, 0).unwrap();
        let omsk = m.put_region("#omsk", ru, 0).unwrap();
        m.entry_mut(omsk).unwrap().flags = 0; // allow flood
        assert!(m.put_region("bad,name", 0, 0).is_none());

        let mut p = advert_packet();
        p.set_route_type(ROUTE_TYPE_TRANSPORT_FLOOD);
        let key = m.first_key(m.find_by_id(omsk).unwrap()).unwrap();
        p.transport_codes = [key.transport_code(&p), 0];
        assert_eq!(m.find_match(&p, REGION_DENY_FLOOD).map(|r| r.id), Some(omsk));
        // "ru" denies flood: a packet scoped to it is not matched
        let ru_key = m.first_key(m.find_by_id(ru).unwrap()).unwrap();
        p.transport_codes = [ru_key.transport_code(&p), 0];
        assert!(m.find_match(&p, REGION_DENY_FLOOD).is_none());
        p.transport_codes = [0, 0];
        assert!(m.find_match(&p, REGION_DENY_FLOOD).unwrap().is_wildcard());

        m.set_home(omsk);
        assert_eq!(m.export_tree(), "* F\n ru\n  omsk^ F\n");
        assert_eq!(m.export_names(REGION_DENY_FLOOD, false, 160), "*,omsk");
        assert!(!m.remove(ru), "has children");
        let back = RegionMap::deserialize(&m.serialize());
        assert_eq!(back.export_tree(), m.export_tree());
        assert_eq!(back.find_by_name_prefix("om").map(|r| r.id), Some(omsk));
    }
}
