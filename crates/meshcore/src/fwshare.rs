//! Firmware passed along the mesh (this firmware's extension, off by default at both ends): a
//! repeater with an internet link (`set fw.serve on`) hands signed releases on to the companions
//! next to it, and a companion that pins a root key (`fw_pull`) asks it for an update by itself,
//! connected to nothing. Neither end trusts the other or the server the repeater fetched from: a
//! companion pulls only while it pins a root key, and installs only what that root signed (see
//! [`crate::release`], [`crate::ota`]).
//!
//! What travels is a delta file (`.mcfd`, made by the release CI with `ota-sign delta`): the signed
//! envelope of the new release, the image the delta starts from, and the delta, a zlib-compressed
//! bsdiff patch as LoRa updates use (see [`crate::ota::REQ_TYPE_OTA_BEGIN`]):
//!
//! ```text
//! envelope | base_len u32 | base_sha256 32 | patch
//! ```
//!
//! The requests are anonymous (ANON_REQ, kinds below the printable bytes a login's password starts
//! with) and direct only: they reach a neighbour, and are never flooded. Their data, after
//! upstream's `[tag u32][kind u8][reply path_len u8][reply path]` prologue, and the replies after
//! the reflected tag:
//!
//! - `FW_QUERY [board u8][role u8][counter u32][base_len u32][base_sha256 first 8]` -> `[status]`,
//!   and for `READY` `[len u32][id u32][counter u32][version_len u8][version]`, for `FETCHING`
//!   `[retry_secs u16]`
//! - `FW_GET [id u32][offset u32][len u8]` -> `[status][offset u32][data]`

use alloc::string::String;
use alloc::vec::Vec;

use crate::release::{self, Envelope, Peek};

pub const ANON_REQ_TYPE_FW_QUERY: u8 = 0x1C;
pub const ANON_REQ_TYPE_FW_GET: u8 = 0x1D;

/// Data in one FW_GET reply: with the tag, status and offset (9 bytes) it fills a RESPONSE
/// datagram, which takes at most 167 bytes (upstream's `createDatagram` bound); a multiple of 4
/// for flash writes.
pub const FW_PIECE: usize = 156;

/// Reply statuses.
pub mod status {
    /// FW_QUERY: an update is here (its length, id, release follow). FW_GET: the data follows.
    pub const READY: u8 = 0;
    /// The repeater is fetching it: ask again after `retry_secs`.
    pub const FETCHING: u8 = 1;
    /// No newer release with a delta from this image.
    pub const NONE: u8 = 2;
    /// The repeater can't get updates now (no internet link, or the server failed).
    pub const UNAVAILABLE: u8 = 3;
    /// FW_GET: the repeater holds another file now; query again.
    pub const CHANGED: u8 = 4;
    pub const BAD_REQUEST: u8 = 5;
}

/// What a companion asks for: an update for its board and role, from the image it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FwQuery {
    pub board: u8,
    pub role: u8,
    /// Its build's release number: only a newer release is an update.
    pub counter: u32,
    pub base_len: u32,
    pub base_sha8: [u8; 8],
}

impl FwQuery {
    pub const LEN: usize = 18;

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(Self::LEN);
        b.extend_from_slice(&[self.board, self.role]);
        b.extend_from_slice(&self.counter.to_le_bytes());
        b.extend_from_slice(&self.base_len.to_le_bytes());
        b.extend_from_slice(&self.base_sha8);
        b
    }

    pub fn parse(b: &[u8]) -> Option<Self> {
        let b = b.get(..Self::LEN)?;
        Some(Self {
            board: b[0],
            role: b[1],
            counter: u32::from_le_bytes(b[2..6].try_into().unwrap()),
            base_len: u32::from_le_bytes(b[6..10].try_into().unwrap()),
            base_sha8: b[10..18].try_into().unwrap(),
        })
    }
}

/// The start of a delta file: the release's envelope and the image the patch applies to.
#[derive(Clone, Debug, PartialEq)]
pub struct DeltaHeader {
    pub envelope: Envelope,
    /// Bytes of the envelope at the start of the file (fed to the update session as they are).
    pub envelope_len: usize,
    pub base_len: u32,
    pub base_sha256: [u8; 32],
}

/// After the envelope: `base_len u32` and `base_sha256`.
const BASE_LEN: usize = 36;

/// Where a file's header ends, if `head` holds enough of it to tell.
pub enum HeaderSize {
    Known(usize),
    /// At least this many bytes are needed to tell.
    NeedMore(usize),
    NotDelta,
}

impl DeltaHeader {
    pub fn size(head: &[u8]) -> HeaderSize {
        match release::peek(head) {
            Peek::Envelope { len } => HeaderSize::Known(len + BASE_LEN),
            Peek::NeedMore(n) => HeaderSize::NeedMore(n),
            Peek::NotEnvelope => HeaderSize::NotDelta,
        }
    }

    /// The header at the start of `head`, if it is whole there.
    pub fn parse(head: &[u8]) -> Option<Self> {
        let (envelope, envelope_len) = Envelope::parse(head)?;
        let base = head.get(envelope_len..envelope_len + BASE_LEN)?;
        Some(Self {
            envelope,
            envelope_len,
            base_len: u32::from_le_bytes(base[..4].try_into().unwrap()),
            base_sha256: base[4..].try_into().unwrap(),
        })
    }

    /// Offset of the patch in the file.
    pub fn patch_offset(&self) -> usize {
        self.envelope_len + BASE_LEN
    }
}

/// A delta file: `envelope` (as it starts a `.mcfw` file), the base image, the zlib-compressed patch.
pub fn delta_file(envelope: &[u8], base_len: u32, base_sha256: &[u8; 32], patch: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(envelope.len() + BASE_LEN + patch.len());
    f.extend_from_slice(envelope);
    f.extend_from_slice(&base_len.to_le_bytes());
    f.extend_from_slice(base_sha256);
    f.extend_from_slice(patch);
    f
}

/// What a repeater can offer for a query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Offer {
    /// A delta file of `len` bytes for the release `counter`/`version`; `id` names it in FW_GET.
    Ready { len: u32, id: u32, counter: u32, version: String },
    Fetching { retry_secs: u16 },
    NoUpdate,
    Unavailable,
}

/// The updates a repeater hands on, kept by the firmware (fetched from the release server).
pub trait FwStore {
    /// The newest release for `q`'s board and role with a delta from `q`'s image, if the store
    /// holds it; a store that can fetch it starts doing so and answers `Fetching`.
    fn offer(&mut self, q: &FwQuery) -> Offer;
    /// Bytes of file `id` from `offset` into `buf`; `None` if the store holds another file now.
    fn read(&mut self, id: u32, offset: u32, buf: &mut [u8]) -> Option<usize>;
}

/// A repeater's answer to FW request `kind` with `data` (after the reply path), appended to
/// `reply`; false when there is nothing to answer.
pub fn answer(store: &mut dyn FwStore, kind: u8, data: &[u8], reply: &mut Vec<u8>) -> bool {
    match kind {
        ANON_REQ_TYPE_FW_QUERY => {
            let Some(q) = FwQuery::parse(data) else {
                reply.push(status::BAD_REQUEST);
                return true;
            };
            match store.offer(&q) {
                Offer::Ready { counter, .. } if counter <= q.counter => reply.push(status::NONE),
                Offer::Ready { len, id, counter, version } => {
                    reply.push(status::READY);
                    reply.extend_from_slice(&len.to_le_bytes());
                    reply.extend_from_slice(&id.to_le_bytes());
                    reply.extend_from_slice(&counter.to_le_bytes());
                    let v = &version.as_bytes()[..version.len().min(release::VERSION_MAX)];
                    reply.push(v.len() as u8);
                    reply.extend_from_slice(v);
                }
                Offer::Fetching { retry_secs } => {
                    reply.push(status::FETCHING);
                    reply.extend_from_slice(&retry_secs.to_le_bytes());
                }
                Offer::NoUpdate => reply.push(status::NONE),
                Offer::Unavailable => reply.push(status::UNAVAILABLE),
            }
            true
        }
        ANON_REQ_TYPE_FW_GET => {
            let (Some(id), Some(offset), Some(&len)) = (
                data.get(..4).map(|b| u32::from_le_bytes(b.try_into().unwrap())),
                data.get(4..8).map(|b| u32::from_le_bytes(b.try_into().unwrap())),
                data.get(8),
            ) else {
                reply.push(status::BAD_REQUEST);
                return true;
            };
            let mut buf = [0u8; FW_PIECE];
            let want = (len as usize).min(FW_PIECE);
            match store.read(id, offset, &mut buf[..want]) {
                Some(n) => {
                    reply.push(status::READY);
                    reply.extend_from_slice(&offset.to_le_bytes());
                    reply.extend_from_slice(&buf[..n]);
                }
                None => reply.push(status::CHANGED),
            }
            true
        }
        _ => false,
    }
}

/// A store holding one delta file in memory (tests, the companion simulator).
pub struct MemStore {
    pub file: Vec<u8>,
    pub id: u32,
    /// The board, role and base image the file is for.
    pub for_board: u8,
    pub for_role: u8,
    pub header: Option<DeltaHeader>,
}

impl MemStore {
    pub fn new(file: Vec<u8>, id: u32) -> Self {
        let header = DeltaHeader::parse(&file);
        let (for_board, for_role) = header.as_ref().map_or((0, 0), |h| (h.envelope.manifest.board, h.envelope.manifest.role));
        Self { file, id, for_board, for_role, header }
    }
}

impl FwStore for MemStore {
    fn offer(&mut self, q: &FwQuery) -> Offer {
        let Some(h) = &self.header else { return Offer::Unavailable };
        if q.board != self.for_board || q.role != self.for_role || q.base_len != h.base_len || q.base_sha8 != h.base_sha256[..8] {
            return Offer::NoUpdate;
        }
        let m = &h.envelope.manifest;
        Offer::Ready { len: self.file.len() as u32, id: self.id, counter: m.counter, version: m.version.clone() }
    }

    fn read(&mut self, id: u32, offset: u32, buf: &mut [u8]) -> Option<usize> {
        if id != self.id {
            return None;
        }
        let from = (offset as usize).min(self.file.len());
        let n = buf.len().min(self.file.len() - from);
        buf[..n].copy_from_slice(&self.file[from..from + n]);
        Some(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::LocalIdentity;
    use crate::release::{Manifest, board, role};

    fn release_envelope(counter: u32, image: &[u8]) -> Vec<u8> {
        let mut sha = [0u8; 32];
        crate::crypto::sha256(&mut sha, &[image]);
        let manifest = Manifest {
            board: board::T_ECHO,
            role: role::COMPANION,
            flags: 0,
            counter,
            image_len: image.len() as u32,
            image_sha256: sha,
            issued: 1_792_900_000,
            version: "0.5.2".into(),
        };
        let mut env = Envelope { manifest, signatures: Vec::new() };
        let root = LocalIdentity::from_seed(&[7; 32]);
        let sig = root.sign(&Envelope::signed_message(&env.manifest));
        env.add_signature(*root.pub_key(), sig);
        env.to_bytes()
    }

    #[test]
    fn a_delta_file_names_its_release_and_base() {
        let env = release_envelope(5, b"new image");
        let base_sha = [9u8; 32];
        let file = delta_file(&env, 1234, &base_sha, b"patch bytes");
        match DeltaHeader::size(&file[..20]) {
            HeaderSize::NeedMore(n) => assert!(n > 20),
            _ => panic!("20 bytes can't tell"),
        }
        let HeaderSize::Known(n) = DeltaHeader::size(&file) else { panic!() };
        let h = DeltaHeader::parse(&file).unwrap();
        assert_eq!((n, h.patch_offset()), (env.len() + 36, env.len() + 36));
        assert_eq!((h.base_len, h.base_sha256), (1234, base_sha));
        assert_eq!(&file[h.patch_offset()..], b"patch bytes");
        assert!(matches!(DeltaHeader::size(b"not a release file at all, no"), HeaderSize::NotDelta));
    }

    #[test]
    fn a_store_offers_only_what_fits_the_query() {
        let env = release_envelope(5, b"new image");
        let mut base_sha = [0u8; 32];
        crate::crypto::sha256(&mut base_sha, &[b"old image".as_slice()]);
        let file = delta_file(&env, 9, &base_sha, &[0xAB; 300]);
        let mut store = MemStore::new(file.clone(), 77);
        let q = FwQuery { board: board::T_ECHO, role: role::COMPANION, counter: 4, base_len: 9, base_sha8: base_sha[..8].try_into().unwrap() };
        assert_eq!(FwQuery::parse(&q.to_bytes()), Some(q));

        let mut r = Vec::new();
        assert!(answer(&mut store, ANON_REQ_TYPE_FW_QUERY, &q.to_bytes(), &mut r));
        assert_eq!(r[0], status::READY);
        assert_eq!(u32::from_le_bytes(r[1..5].try_into().unwrap()), file.len() as u32);
        assert_eq!(u32::from_le_bytes(r[5..9].try_into().unwrap()), 77);
        assert_eq!(&r[14..], b"0.5.2");

        // the same release is no update; another image or board gets nothing
        for q in [FwQuery { counter: 5, ..q }, FwQuery { base_len: 10, ..q }, FwQuery { board: board::XIAO_C6, ..q }] {
            let mut r = Vec::new();
            answer(&mut store, ANON_REQ_TYPE_FW_QUERY, &q.to_bytes(), &mut r);
            assert_eq!(r, [status::NONE]);
        }

        let mut get = |id: u32, offset: u32, len: u8| {
            let mut req = id.to_le_bytes().to_vec();
            req.extend_from_slice(&offset.to_le_bytes());
            req.push(len);
            let mut r = Vec::new();
            answer(&mut store, ANON_REQ_TYPE_FW_GET, &req, &mut r);
            r
        };
        let r = get(77, 100, 200);
        assert_eq!(r[0], status::READY);
        assert_eq!(&r[5..], &file[100..100 + FW_PIECE], "capped at one piece");
        let tail = file.len() as u32 - 10;
        assert_eq!(&get(77, tail, 160)[5..], &file[tail as usize..]);
        assert_eq!(get(78, 0, 16), [status::CHANGED]);
    }
}
