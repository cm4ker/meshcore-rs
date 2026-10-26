//! Firmware update session, independent of the transport (LoRa admin requests, the companion app
//! link over BLE/USB/TCP, Wi-Fi upload).
//!
//! The image is received in fixed-size chunks, in any order, into the inactive app slot (on
//! chips with a single slot, a staging area the platform installs from). Sectors are erased
//! lazily before their first write, so no long blocking erase is needed. `commit` reads the whole
//! image back and checks it (see [`ImageFormat`]: the ESP image header and the SHA-256 the build
//! tool appends, or this firmware's trailer of an nRF52 image) and, when the sender supplied one,
//! the expected SHA-256; only then it asks the platform to boot it.
//!
//! Transfers can be zlib-compressed, and over LoRa also a delta: a zlib-compressed bsdiff patch
//! (format of the `bsdiff` crate) against the running firmware, which turns a typical update
//! from ~1 MB into tens of KB. The base is checked by SHA-256 before anything is transferred.
//!
//! Once the device pins a root key ([`OtaEndpoint::roots`]), a session only begins after the
//! envelope of a signed release (see [`crate::release`]) for this board and role, and for the
//! very image announced in BEGIN, has been accepted: whoever delivers it, only a release signed by
//! a pinned root is installed.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use sha2::{Digest, Sha256};

use crate::release::{Build, Envelope, Fingerprint, Manifest, Refusal};

pub const SECTOR_SIZE: u32 = 4096;
/// Largest chunk a companion can send as a LoRa admin request: its serial frames carry at most
/// 176 bytes, i.e. 143 bytes of request after the command and public key, minus the request
/// type and chunk index (see `repeater::REQ_TYPE_OTA_DATA`); a multiple of 4 for flash writes.
pub const LORA_CHUNK_SIZE: u16 = 140;
const ESP_IMAGE_MAGIC: u8 = 0xE9;
/// Chip ids in the ESP-IDF app image header.
pub const ESP_CHIP_ID_ESP32S3: u16 = 0x0009;
pub const ESP_CHIP_ID_ESP32C6: u16 = 0x000D;

/// The trailer of an nRF52 image ([`ImageFormat::Nrf`]), 64 bytes after the application's flash
/// contents: `[magic 8][app length u32][load address u32][chip u32][reserved 12][SHA-256 of the
/// app 32]`. It is flashed with the app, so the running firmware finds its own image by it.
pub const NRF_TRAILER_MAGIC: &[u8; 8] = b"MCRS-NRF";
pub const NRF_TRAILER_LEN: u32 = 64;
pub const NRF_CHIP_NRF52840: u32 = 0x52840;

/// What a target's images look like.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageFormat {
    /// An ESP-IDF app image (header, segments, appended SHA-256) for this chip (`ESP_CHIP_ID_*`).
    Esp(u16),
    /// The flash contents of an nRF52 application from `load` on, then the trailer.
    Nrf { chip: u32, load: u32 },
}

// Update requests: the first byte of a LoRa admin request, or the command byte of a companion
// app frame (the reply frame starts with the same byte). The layout is the same for both.

/// `[size u32][sha256 32][chunk_size u16]` (+ `[flags u8][raw_size u32]`, flags bit0 = zlib:
/// `size` is then the compressed size and `sha256` the hash of the inflated image;
/// + `[base_len u32][base_sha256 32]` with flags bit1 = delta: the zlib stream is a bsdiff patch
///   against that prefix of the running firmware; flags bit2 = every DATA request ends with a
///   CRC-32 of its index and data, for links that can corrupt frames) -> `[status][total_chunks u16]`
pub const REQ_TYPE_OTA_BEGIN: u8 = 0xF0;
/// `[index u16][data]` (+ `[crc32 u32]`, see BEGIN) -> no response (saves airtime; progress is
/// polled with STATUS)
pub const REQ_TYPE_OTA_DATA: u8 = 0xF1;
/// `[from u16]` -> `[status][received u16][total u16][n][missing u16 x n]`
pub const REQ_TYPE_OTA_STATUS: u8 = 0xF2;
/// verify + boot the new image on trial -> `[status]`
pub const REQ_TYPE_OTA_COMMIT: u8 = 0xF3;
pub const REQ_TYPE_OTA_ABORT: u8 = 0xF4;
/// -> `[has_target][capacity u32][version...][0][image_len u32][image_sha256 first 8][board u8]
/// [role u8][release counter u32][pinned roots u8][pinned owners u8]`: the running image, the base
/// a delta must be made against (zeroes if unknown), and what a signed release for this device
/// must name and who must sign it
pub const REQ_TYPE_OTA_INFO: u8 = 0xF5;
/// `[offset u16][total u16][len u8][piece of the envelope]` -> `[status][received u16]`: the
/// envelope of a signed release (see [`crate::release`]), in order, before BEGIN. Once whole it is
/// checked, and status 0 on the last piece means accepted. With no pinned root it is not needed
/// and is ignored.
pub const REQ_TYPE_OTA_MANIFEST: u8 = 0xF6;
/// Most missing chunks listed in one STATUS reply.
const OTA_MAX_MISSING: usize = 60;
/// Largest envelope: the manifest with the longest version and the most signatures.
const MAX_ENVELOPE: usize = 1024;

/// The inactive firmware slot, provided by the platform.
pub trait FirmwareTarget {
    fn capacity(&self) -> u32;
    /// Erase `[offset, offset + len)`, sector-aligned.
    fn erase(&mut self, offset: u32, len: u32) -> bool;
    /// Write at a 4-byte aligned offset.
    fn write(&mut self, offset: u32, data: &[u8]) -> bool;
    fn read(&mut self, offset: u32, buf: &mut [u8]) -> bool;
    /// Select the verified `size`-byte image for the next boot (on trial, where the platform can
    /// roll back).
    fn activate(&mut self, size: u32) -> bool;
    /// Size of the running firmware's slot (the base of delta updates); 0 if it can't be read.
    fn running_capacity(&self) -> u32 {
        0
    }
    fn read_running(&mut self, _offset: u32, _buf: &mut [u8]) -> bool {
        false
    }
    /// Chip id an ESP image must carry in its header (see `ESP_CHIP_ID_*`).
    fn chip_id(&self) -> u16 {
        ESP_CHIP_ID_ESP32S3
    }
    fn format(&self) -> ImageFormat {
        ImageFormat::Esp(self.chip_id())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum OtaError {
    NoTarget = 1,
    TooLarge = 2,
    BadParams = 3,
    NoSession = 4,
    FlashError = 5,
    Incomplete = 6,
    HashMismatch = 7,
    BadImage = 8,
    /// The running firmware is not the base the delta was made against.
    BaseMismatch = 9,
    /// A root is pinned and no signed release for this image was accepted.
    Unsigned = 10,
    /// The release is not signed by a pinned root.
    Untrusted = 11,
    /// The release is for another board or role.
    WrongBoard = 12,
    /// The release is older than the running build.
    Downgrade = 13,
    /// The envelope can't be read.
    BadEnvelope = 14,
    /// Owner keys are pinned and none of them signed the release.
    NotOwner = 15,
}

impl From<Refusal> for OtaError {
    fn from(r: Refusal) -> Self {
        match r {
            Refusal::Untrusted => Self::Untrusted,
            Refusal::NotOwner => Self::NotOwner,
            Refusal::WrongBoard => Self::WrongBoard,
            Refusal::Downgrade => Self::Downgrade,
        }
    }
}

pub struct OtaSession {
    /// Bytes being transferred (compressed size for compressed sessions).
    size: u32,
    sha256: Option<[u8; 32]>,
    chunk_size: u32,
    /// Where the transferred bytes go in the slot (end of the slot for compressed sessions).
    base: u32,
    /// Size of the decompressed image, for zlib-compressed transfers.
    raw_size: Option<u32>,
    /// Delta transfers: length of the base (a prefix of the running slot).
    base_len: Option<u32>,
    received: Vec<u8>,
    received_count: u32,
    erased: Vec<u8>,
    /// DATA requests carry a CRC-32 (BEGIN flags bit2).
    crc_chunks: bool,
}

/// CRC-32 (IEEE 802.3, as zlib's `crc32`).
fn crc32(data: &[u8]) -> u32 {
    !data.iter().fold(!0u32, |mut c, &b| {
        c ^= b as u32;
        for _ in 0..8 {
            c = (c >> 1) ^ (0xEDB8_8320 & (c & 1).wrapping_neg());
        }
        c
    })
}

fn bit(v: &[u8], i: usize) -> bool {
    v[i / 8] & (1 << (i % 8)) != 0
}

fn set_bit(v: &mut [u8], i: usize) {
    v[i / 8] |= 1 << (i % 8);
}

fn sector_align(n: u32) -> u32 {
    n.div_ceil(SECTOR_SIZE) * SECTOR_SIZE
}

/// Write `data` at a sector-aligned `offset`, erasing the sectors first (padding to whole words).
fn write_fresh(target: &mut dyn FirmwareTarget, offset: u32, data: &[u8]) -> Result<(), OtaError> {
    if !target.erase(offset, sector_align(data.len() as u32)) {
        return Err(OtaError::FlashError);
    }
    let ok = if data.len().is_multiple_of(4) {
        target.write(offset, data)
    } else {
        let mut padded = data.to_vec();
        padded.resize(data.len().next_multiple_of(4), 0xFF);
        target.write(offset, &padded)
    };
    if ok { Ok(()) } else { Err(OtaError::FlashError) }
}

impl OtaSession {
    /// `sha256`: expected hash of the whole image, if the transport provides one.
    pub fn begin(
        target: &dyn FirmwareTarget, size: u32, sha256: Option<[u8; 32]>, chunk_size: u16,
    ) -> Result<Self, OtaError> {
        if size <= 64 {
            return Err(OtaError::BadParams);
        }
        if size > target.capacity() {
            return Err(OtaError::TooLarge);
        }
        Self::new(size, sha256, chunk_size, 0, None)
    }

    /// A zlib-compressed transfer of `size` bytes that inflates to a `raw_size` image.
    /// The compressed stream is staged at the end of the slot, then inflated to its start.
    pub fn begin_compressed(
        target: &dyn FirmwareTarget, size: u32, raw_size: u32, sha256: Option<[u8; 32]>, chunk_size: u16,
    ) -> Result<Self, OtaError> {
        if size == 0 || raw_size <= 64 {
            return Err(OtaError::BadParams);
        }
        if sector_align(size) + sector_align(raw_size) > target.capacity() {
            return Err(OtaError::TooLarge);
        }
        let base = target.capacity() / SECTOR_SIZE * SECTOR_SIZE - sector_align(size);
        Self::new(size, sha256, chunk_size, base, Some(raw_size))
    }

    /// A zlib-compressed bsdiff patch of `size` bytes that turns the first `base_len` bytes of
    /// the running firmware (which must hash to `base_sha256`) into a `raw_size` image.
    pub fn begin_delta(
        target: &mut dyn FirmwareTarget, size: u32, raw_size: u32, sha256: [u8; 32], chunk_size: u16, base_len: u32,
        base_sha256: [u8; 32],
    ) -> Result<Self, OtaError> {
        if base_len == 0 || base_len > target.running_capacity() {
            return Err(OtaError::BaseMismatch);
        }
        if running_hash(target, base_len)? != base_sha256 {
            return Err(OtaError::BaseMismatch);
        }
        let mut s = Self::begin_compressed(target, size, raw_size, Some(sha256), chunk_size)?;
        s.base_len = Some(base_len);
        Ok(s)
    }

    fn new(size: u32, sha256: Option<[u8; 32]>, chunk_size: u16, base: u32, raw_size: Option<u32>) -> Result<Self, OtaError> {
        if chunk_size == 0 || !chunk_size.is_multiple_of(4) {
            return Err(OtaError::BadParams);
        }
        let chunks = size.div_ceil(chunk_size as u32) as usize;
        let sectors = size.div_ceil(SECTOR_SIZE) as usize;
        Ok(Self {
            size,
            sha256,
            chunk_size: chunk_size as u32,
            base,
            raw_size,
            base_len: None,
            received: vec![0; chunks.div_ceil(8)],
            received_count: 0,
            erased: vec![0; sectors.div_ceil(8)],
            crc_chunks: false,
        })
    }

    pub fn total_chunks(&self) -> u32 {
        self.size.div_ceil(self.chunk_size)
    }

    pub fn received_chunks(&self) -> u32 {
        self.received_count
    }

    /// Bytes being transferred.
    pub fn size(&self) -> u32 {
        self.size
    }

    pub fn chunk_size(&self) -> u32 {
        self.chunk_size
    }

    pub fn is_complete(&self) -> bool {
        self.received_count == self.total_chunks()
    }

    /// Store chunk `index`. Duplicates are accepted and ignored.
    pub fn write_chunk(&mut self, target: &mut dyn FirmwareTarget, index: u32, data: &[u8]) -> Result<(), OtaError> {
        if index >= self.total_chunks() {
            return Err(OtaError::BadParams);
        }
        let offset = index * self.chunk_size;
        let expected = (self.size - offset).min(self.chunk_size) as usize;
        if data.len() != expected {
            return Err(OtaError::BadParams);
        }
        if bit(&self.received, index as usize) {
            return Ok(());
        }
        // erase every sector this chunk touches, once (`base` is sector-aligned)
        let first = offset / SECTOR_SIZE;
        let last = (offset + data.len() as u32 - 1) / SECTOR_SIZE;
        for s in first..=last {
            if !bit(&self.erased, s as usize) {
                if !target.erase(self.base + s * SECTOR_SIZE, SECTOR_SIZE) {
                    return Err(OtaError::FlashError);
                }
                set_bit(&mut self.erased, s as usize);
            }
        }
        // writes must be whole words: pad the final chunk with erased-flash bytes
        let ok = if data.len().is_multiple_of(4) {
            target.write(self.base + offset, data)
        } else {
            let mut padded = data.to_vec();
            padded.resize(data.len().next_multiple_of(4), 0xFF);
            target.write(self.base + offset, &padded)
        };
        if !ok {
            return Err(OtaError::FlashError);
        }
        set_bit(&mut self.received, index as usize);
        self.received_count += 1;
        Ok(())
    }

    /// SHA-256 of the transferred bytes as stored in the slot (diagnostics).
    pub fn staged_sha256(&self, target: &mut dyn FirmwareTarget) -> Option<[u8; 32]> {
        let mut h = Sha256::new();
        let mut buf = [0u8; 1024];
        let mut offset = 0;
        while offset < self.size {
            let n = ((self.size - offset) as usize).min(buf.len());
            if !target.read(self.base + offset, &mut buf[..n]) {
                return None;
            }
            h.update(&buf[..n]);
            offset += n as u32;
        }
        Some(h.finalize().into())
    }

    /// Up to `max` chunk indexes still missing, starting from `from`.
    pub fn missing(&self, from: u32, max: usize) -> Vec<u32> {
        (from..self.total_chunks()).filter(|&i| !bit(&self.received, i as usize)).take(max).collect()
    }

    /// Verify the received image (inflating it first if compressed) and select it for the next boot.
    pub fn commit(&self, target: &mut dyn FirmwareTarget) -> Result<(), OtaError> {
        if !self.is_complete() {
            return Err(OtaError::Incomplete);
        }
        let image_size = match (self.raw_size, self.base_len) {
            (Some(raw), None) => {
                inflate(target, self.base, self.size, &mut ImageWriter::new(raw))?;
                raw
            }
            (Some(raw), Some(base_len)) => {
                inflate(target, self.base, self.size, &mut Patcher::new(raw, base_len))?;
                raw
            }
            (None, _) => self.size,
        };
        verify_image(target, image_size, self.sha256)?;
        if !target.activate(image_size) {
            return Err(OtaError::FlashError);
        }
        Ok(())
    }
}

/// Consumer of an inflated stream.
trait Sink {
    fn put(&mut self, target: &mut dyn FirmwareTarget, data: &[u8]) -> Result<(), OtaError>;
    /// End of stream: flush, and check everything expected arrived.
    fn finish(&mut self, target: &mut dyn FirmwareTarget) -> Result<(), OtaError>;
}

/// Writes a byte stream to the start of the slot, a whole sector at a time.
struct ImageWriter {
    size: u32,
    written: u32,
    sector: Vec<u8>,
}

impl ImageWriter {
    fn new(size: u32) -> Self {
        Self { size, written: 0, sector: Vec::with_capacity(SECTOR_SIZE as usize) }
    }
}

impl Sink for ImageWriter {
    fn put(&mut self, target: &mut dyn FirmwareTarget, mut data: &[u8]) -> Result<(), OtaError> {
        if self.written as usize + self.sector.len() + data.len() > self.size as usize {
            return Err(OtaError::BadImage);
        }
        while !data.is_empty() {
            let n = data.len().min(SECTOR_SIZE as usize - self.sector.len());
            self.sector.extend_from_slice(&data[..n]);
            data = &data[n..];
            if self.sector.len() == SECTOR_SIZE as usize {
                write_fresh(target, self.written, &self.sector)?;
                self.written += SECTOR_SIZE;
                self.sector.clear();
            }
        }
        Ok(())
    }

    fn finish(&mut self, target: &mut dyn FirmwareTarget) -> Result<(), OtaError> {
        if !self.sector.is_empty() {
            write_fresh(target, self.written, &self.sector)?;
            self.written += self.sector.len() as u32;
            self.sector.clear();
        }
        if self.written == self.size { Ok(()) } else { Err(OtaError::BadImage) }
    }
}

enum PatchState {
    /// Collecting the 24-byte control record `[mix u64][copy u64][seek i64 sign-magnitude]`.
    Control { record: [u8; 24], have: usize },
    /// `left` bytes to add to the base, then `copy` literal bytes, then move in the base by `seek`.
    Mix { left: u64, copy: u64, seek: i64 },
    Copy { left: u64, seek: i64 },
}

/// Applies a bsdiff patch stream against the running firmware, writing the new image.
struct Patcher {
    out: ImageWriter,
    base_len: u32,
    base_pos: u32,
    state: PatchState,
    old: Vec<u8>,
}

impl Patcher {
    fn new(size: u32, base_len: u32) -> Self {
        Self {
            out: ImageWriter::new(size),
            base_len,
            base_pos: 0,
            state: PatchState::Control { record: [0; 24], have: 0 },
            old: vec![0; 256],
        }
    }
}

impl Sink for Patcher {
    fn put(&mut self, target: &mut dyn FirmwareTarget, mut data: &[u8]) -> Result<(), OtaError> {
        while !data.is_empty() {
            match &mut self.state {
                PatchState::Control { record, have } => {
                    let n = data.len().min(24 - *have);
                    record[*have..*have + n].copy_from_slice(&data[..n]);
                    *have += n;
                    data = &data[n..];
                    if *have == 24 {
                        let field = |i: usize| u64::from_le_bytes(record[i..i + 8].try_into().unwrap());
                        let (mix, copy, raw_seek) = (field(0), field(8), field(16));
                        let seek = if raw_seek >> 63 == 0 { raw_seek as i64 } else { -((raw_seek & !(1 << 63)) as i64) };
                        if self.base_pos as u64 + mix > self.base_len as u64 {
                            return Err(OtaError::BadImage);
                        }
                        self.state = PatchState::Mix { left: mix, copy, seek };
                    }
                }
                PatchState::Mix { left: 0, copy, seek } => {
                    self.state = PatchState::Copy { left: *copy, seek: *seek };
                }
                PatchState::Mix { left, .. } => {
                    let n = data.len().min(self.old.len()).min(*left as usize);
                    *left -= n as u64;
                    if !target.read_running(self.base_pos, &mut self.old[..n]) {
                        return Err(OtaError::FlashError);
                    }
                    for (o, d) in self.old[..n].iter_mut().zip(data) {
                        *o = o.wrapping_add(*d);
                    }
                    self.base_pos += n as u32;
                    data = &data[n..];
                    self.out.put(target, &self.old[..n])?;
                }
                PatchState::Copy { left: 0, seek } => {
                    let pos = self.base_pos as i64 + *seek;
                    if !(0..=self.base_len as i64).contains(&pos) {
                        return Err(OtaError::BadImage);
                    }
                    self.base_pos = pos as u32;
                    self.state = PatchState::Control { record: [0; 24], have: 0 };
                }
                PatchState::Copy { left, .. } => {
                    let n = data.len().min(*left as usize);
                    *left -= n as u64;
                    self.out.put(target, &data[..n])?;
                    data = &data[n..];
                }
            }
        }
        Ok(())
    }

    fn finish(&mut self, target: &mut dyn FirmwareTarget) -> Result<(), OtaError> {
        // the stream may end right after a record with nothing left to add or copy
        match self.state {
            PatchState::Control { have: 0, .. }
            | PatchState::Mix { left: 0, copy: 0, .. }
            | PatchState::Copy { left: 0, .. } => self.out.finish(target),
            _ => Err(OtaError::BadImage),
        }
    }
}

/// What an update request did besides its reply.
#[derive(Debug, PartialEq, Eq)]
pub enum OtaOutcome {
    None,
    /// The new image is verified and selected: reboot (after the reply has gone out).
    Committed,
    /// Commit refused; a line for the log.
    CommitFailed(String),
}

/// The update requests (`REQ_TYPE_OTA_*`) for one firmware slot, shared by the transports.
#[derive(Default)]
pub struct OtaEndpoint {
    /// The inactive firmware slot, if the platform supports updates.
    pub target: Option<Box<dyn FirmwareTarget>>,
    session: Option<OtaSession>,
    /// Requests handled so far: a platform can tell from it when an update was last active.
    pub requests: u32,
    /// Length and hash of the running image, computed on the first INFO.
    running_image: Option<(u32, [u8; 32])>,
    /// Fingerprints of the root keys whose signed releases this device installs; empty = unsigned
    /// images are accepted (as before roots existed).
    pub roots: Vec<Fingerprint>,
    /// Fingerprints of the owner keys one of which must sign a release as well (with a root
    /// pinned); empty = the root's signature is enough.
    pub owners: Vec<Fingerprint>,
    /// This build: a release must be for its board and role, and not older.
    pub build: Build,
    /// The envelope being received (MANIFEST requests).
    envelope: Vec<u8>,
    /// The signed release accepted for the next session.
    release: Option<Manifest>,
}

impl OtaEndpoint {
    /// An endpoint of a device in `role` (see [`crate::release::role`]) that installs only
    /// releases signed by one of `roots` (and by one of `owners`, if any), or anything while
    /// `roots` is empty. The firmware still sets the target and [`Build::board`].
    pub fn new(roots: Vec<Fingerprint>, owners: Vec<Fingerprint>, role: u8) -> Self {
        Self { roots, owners, build: Build { role, ..Build::default() }, ..Self::default() }
    }

    pub fn is_request(kind: u8) -> bool {
        (REQ_TYPE_OTA_BEGIN..=REQ_TYPE_OTA_MANIFEST).contains(&kind)
    }

    /// Check the received envelope; on success its release may begin a session.
    fn accept_envelope(&mut self) -> Result<(), OtaError> {
        let (envelope, len) = Envelope::parse(&self.envelope).ok_or(OtaError::BadEnvelope)?;
        if len != self.envelope.len() {
            return Err(OtaError::BadEnvelope);
        }
        if self.roots.is_empty() {
            return Ok(());
        }
        envelope.check(&self.roots, &self.owners, &self.build)?;
        self.release = Some(envelope.manifest);
        Ok(())
    }

    /// With a pinned root, a session may only begin for the image of the accepted release.
    fn check_release(&self, sha256: Option<[u8; 32]>, image_len: u32) -> Result<(), OtaError> {
        if self.roots.is_empty() {
            return Ok(());
        }
        match &self.release {
            None => Err(OtaError::Unsigned),
            Some(m) if Some(m.image_sha256) == sha256 && m.image_len == image_len => Ok(()),
            Some(_) => Err(OtaError::HashMismatch),
        }
    }

    /// Length and SHA-256 of the running image (the base of a delta), if the target can tell.
    pub fn running(&mut self) -> Option<(u32, [u8; 32])> {
        if self.running_image.is_none() {
            #[allow(clippy::redundant_closure)]
            let image = self.target.as_deref_mut().and_then(|t| running_image(t));
            self.running_image = image;
        }
        self.running_image
    }

    /// An update has begun and is neither committed nor aborted.
    pub fn busy(&self) -> bool {
        self.session.is_some()
    }

    /// Handle one request (`payload[0]` is its type); the reply body, if one is due.
    pub fn handle(&mut self, payload: &[u8], firmware_version: &str) -> (Option<Vec<u8>>, OtaOutcome) {
        self.requests = self.requests.wrapping_add(1);
        let read_u32 = |i: usize| payload.get(i..i + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
        let u16_at = |i: usize| payload.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
        let status = |r: Result<(), OtaError>| match r {
            Ok(()) => 0u8,
            Err(e) => e as u8,
        };
        match payload.first().copied().unwrap_or(0) {
            REQ_TYPE_OTA_INFO => {
                let mut out = Vec::new();
                out.push(self.target.is_some() as u8);
                out.extend_from_slice(&self.target.as_ref().map_or(0, |t| t.capacity()).to_le_bytes());
                out.extend_from_slice(firmware_version.as_bytes());
                if self.running_image.is_none() {
                    // a closure: the fn item would demand a `'static` borrow of `self`
                    #[allow(clippy::redundant_closure)]
                    let image = self.target.as_deref_mut().and_then(|t| running_image(t));
                    self.running_image = image;
                }
                out.push(0);
                let (len, sha) = self.running_image.unwrap_or((0, [0; 32]));
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(&sha[..8]);
                out.extend_from_slice(&[self.build.board, self.build.role]);
                out.extend_from_slice(&self.build.counter.to_le_bytes());
                out.push(self.roots.len() as u8);
                out.push(self.owners.len() as u8);
                (Some(out), OtaOutcome::None)
            }
            REQ_TYPE_OTA_MANIFEST => {
                let offset = u16_at(1).unwrap_or(0) as usize;
                let total = u16_at(3).unwrap_or(0) as usize;
                let len = payload.get(5).copied().unwrap_or(0) as usize;
                if offset == 0 {
                    self.envelope.clear();
                    self.release = None;
                }
                // pieces come in order, a repeated one is ignored; LoRa pads payloads: cut to `len`
                if let Some(piece) = payload.get(6..6 + len)
                    && offset == self.envelope.len()
                    && offset + len <= total
                    && total <= MAX_ENVELOPE
                {
                    self.envelope.extend_from_slice(piece);
                }
                let res = if total == 0 || total > MAX_ENVELOPE {
                    Err(OtaError::BadParams)
                } else if self.envelope.len() < total {
                    Ok(())
                } else {
                    self.accept_envelope()
                };
                let mut out = vec![status(res)];
                out.extend_from_slice(&(self.envelope.len() as u16).to_le_bytes());
                (Some(out), OtaOutcome::None)
            }
            REQ_TYPE_OTA_BEGIN => {
                let size = read_u32(1);
                let sha: Option<[u8; 32]> = payload.get(5..37).and_then(|b| b.try_into().ok());
                let chunk = u16_at(37).unwrap_or(0);
                let flags = payload.get(39).copied().unwrap_or(0);
                let raw_size = read_u32(40);
                let base_len = read_u32(44);
                let base_sha: Option<[u8; 32]> = payload.get(48..80).and_then(|b| b.try_into().ok());
                self.session = None;
                let image_len = if flags & 1 != 0 { raw_size } else { size };
                let res = self.check_release(sha, image_len).and_then(|()| match (self.target.as_deref_mut(), sha) {
                    (None, _) => Err(OtaError::NoTarget),
                    (_, None) => Err(OtaError::BadParams),
                    (Some(t), Some(sha)) if flags & 3 == 3 => match base_sha {
                        Some(base_sha) => OtaSession::begin_delta(t, size, raw_size, sha, chunk, base_len, base_sha),
                        None => Err(OtaError::BadParams),
                    },
                    (Some(t), Some(sha)) if flags & 1 != 0 => OtaSession::begin_compressed(t, size, raw_size, Some(sha), chunk),
                    (Some(t), Some(sha)) => OtaSession::begin(t, size, Some(sha), chunk),
                })
                .map(|mut s| {
                    s.crc_chunks = flags & 4 != 0;
                    self.session = Some(s);
                });
                let total = self.session.as_ref().map_or(0, |s| s.total_chunks() as u16);
                let mut out = vec![status(res)];
                out.extend_from_slice(&total.to_le_bytes());
                (Some(out), OtaOutcome::None)
            }
            REQ_TYPE_OTA_DATA => {
                if let (Some(session), Some(target), Some(index)) =
                    (self.session.as_mut(), self.target.as_deref_mut(), u16_at(1))
                {
                    // LoRa payloads are zero-padded to the cipher block: cut to the chunk length
                    let index = index as u32;
                    let chunk = session.chunk_size();
                    let len = session.size().saturating_sub(index * chunk).min(chunk) as usize;
                    // a corrupted request is dropped: the chunk stays missing and is sent again
                    let intact = !session.crc_chunks
                        || payload.get(3 + len..3 + len + 4).is_some_and(|c| {
                            u32::from_le_bytes(c.try_into().unwrap()) == crc32(&payload[1..3 + len])
                        });
                    if let Some(data) = payload.get(3..3 + len).filter(|_| intact) {
                        let _ = session.write_chunk(target, index, data);
                    }
                }
                (None, OtaOutcome::None)
            }
            REQ_TYPE_OTA_STATUS => {
                let mut out = Vec::new();
                match self.session.as_ref() {
                    None => out.push(OtaError::NoSession as u8),
                    Some(s) => {
                        let from = u16_at(1).unwrap_or(0) as u32;
                        let missing = s.missing(from, OTA_MAX_MISSING);
                        out.push(0);
                        out.extend_from_slice(&(s.received_chunks() as u16).to_le_bytes());
                        out.extend_from_slice(&(s.total_chunks() as u16).to_le_bytes());
                        out.push(missing.len() as u8);
                        for m in missing {
                            out.extend_from_slice(&(m as u16).to_le_bytes());
                        }
                    }
                }
                (Some(out), OtaOutcome::None)
            }
            REQ_TYPE_OTA_COMMIT => {
                let res = match (self.session.as_ref(), self.target.as_deref_mut()) {
                    (None, _) => Err(OtaError::NoSession),
                    (_, None) => Err(OtaError::NoTarget),
                    (Some(s), Some(t)) => s.commit(t),
                };
                let outcome = match (res, self.session.as_ref(), self.target.as_deref_mut()) {
                    (Ok(()), ..) => {
                        self.session = None;
                        self.release = None;
                        OtaOutcome::Committed
                    }
                    (Err(e), Some(s), Some(t)) => {
                        let staged = s.staged_sha256(t).map(|h| crate::util::to_hex(&h[..8]));
                        OtaOutcome::CommitFailed(alloc::format!(
                            "OTA: commit failed: {e:?} (received {} bytes, sha256 {staged:?})",
                            s.size()
                        ))
                    }
                    (Err(e), ..) => OtaOutcome::CommitFailed(alloc::format!("OTA: commit failed: {e:?}")),
                };
                (Some(vec![status(res)]), outcome)
            }
            REQ_TYPE_OTA_ABORT => {
                self.session = None;
                self.envelope.clear();
                self.release = None;
                (Some(vec![0]), OtaOutcome::None)
            }
            _ => (None, OtaOutcome::None),
        }
    }
}

/// SHA-256 of the first `len` bytes of the running firmware slot.
fn running_hash(target: &mut dyn FirmwareTarget, len: u32) -> Result<[u8; 32], OtaError> {
    let mut h = Sha256::new();
    let mut buf = [0u8; 1024];
    let mut offset = 0;
    while offset < len {
        let n = ((len - offset) as usize).min(buf.len());
        if !target.read_running(offset, &mut buf[..n]) {
            return Err(OtaError::FlashError);
        }
        h.update(&buf[..n]);
        offset += n as u32;
    }
    Ok(h.finalize().into())
}

/// Length of the image at the start of the running slot and its SHA-256: what a delta for this
/// device must be made against.
pub fn running_image(target: &mut dyn FirmwareTarget) -> Option<(u32, [u8; 32])> {
    let len = match target.format() {
        ImageFormat::Esp(_) => running_esp_len(target)?,
        ImageFormat::Nrf { chip, load } => find_nrf_trailer(target, chip, load)? + NRF_TRAILER_LEN,
    };
    Some((len, running_hash(target, len).ok()?))
}

/// An ESP image: its segments, the checksum block and the appended hash.
fn running_esp_len(target: &mut dyn FirmwareTarget) -> Option<u32> {
    let cap = target.running_capacity();
    let mut header = [0u8; 24];
    if cap < 24 || !target.read_running(0, &mut header) || header[0] != ESP_IMAGE_MAGIC {
        return None;
    }
    let mut offset = 24u32;
    for _ in 0..header[1] {
        let mut seg = [0u8; 8];
        if !target.read_running(offset, &mut seg) {
            return None;
        }
        offset = offset.checked_add(8 + u32::from_le_bytes(seg[4..8].try_into().ok()?))?;
        if offset > cap {
            return None;
        }
    }
    // the checksum byte ends a 16-byte aligned block, then comes the optional SHA-256
    let len = (offset / 16 + 1) * 16 + if header[23] == 1 { 32 } else { 0 };
    (len <= cap).then_some(len)
}

fn read_u32_le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// The trailer of an image whose app is `app_len` bytes long, for this chip and load address.
fn nrf_trailer_ok(t: &[u8], app_len: u32, chip: u32, load: u32) -> bool {
    t.len() >= NRF_TRAILER_LEN as usize
        && &t[..8] == NRF_TRAILER_MAGIC
        && read_u32_le(t, 8) == app_len
        && read_u32_le(t, 12) == load
        && read_u32_le(t, 16) == chip
}

/// Offset of the running image's trailer: the first 4-byte aligned place holding a trailer that
/// names its own offset as the app length (the magic alone also turns up in the firmware's data).
fn find_nrf_trailer(target: &mut dyn FirmwareTarget, chip: u32, load: u32) -> Option<u32> {
    const STEP: u32 = 1024;
    let cap = target.running_capacity();
    let mut window = [0u8; STEP as usize + 8];
    let mut base = 0u32;
    while base + NRF_TRAILER_LEN <= cap {
        // 8 bytes of overlap: a magic across the step is found in this window
        let n = (cap - base).min(STEP + 8) as usize;
        if !target.read_running(base, &mut window[..n]) {
            return None;
        }
        for i in (0..n.saturating_sub(7)).step_by(4).take_while(|&i| i < STEP as usize) {
            if &window[i..i + 8] == NRF_TRAILER_MAGIC {
                let at = base + i as u32;
                let mut t = [0u8; NRF_TRAILER_LEN as usize];
                if at + NRF_TRAILER_LEN <= cap && target.read_running(at, &mut t) && nrf_trailer_ok(&t, at, chip, load) {
                    return Some(at);
                }
            }
        }
        base += STEP;
    }
    None
}

/// Inflate the zlib stream at `[src, src + len)` of the slot into `sink`.
fn inflate(target: &mut dyn FirmwareTarget, src: u32, len: u32, sink: &mut dyn Sink) -> Result<(), OtaError> {
    use miniz_oxide::inflate::TINFLStatus;
    use miniz_oxide::inflate::core::inflate_flags::{TINFL_FLAG_HAS_MORE_INPUT, TINFL_FLAG_PARSE_ZLIB_HEADER};
    use miniz_oxide::inflate::core::{DecompressorOxide, decompress_with_limit};

    const WINDOW: usize = 32 * 1024; // deflate dictionary, wrapping output buffer
    // straight on the heap: `Box::new(DecompressorOxide::new())` builds its ~11 KB on the stack
    // first, and the firmware commits from its main loop, whose stack overflowed with it
    // SAFETY: every field of the decompressor is an integer or an array of integers, and the
    // state enum's first variant (Start) is 0, so all zeroes is a valid value; `init` resets it
    let mut state: alloc::boxed::Box<DecompressorOxide> = unsafe { alloc::boxed::Box::new_zeroed().assume_init() };
    state.init();
    let mut window = vec![0u8; WINDOW];
    let mut input = [0u8; 1024];
    let (mut in_read, mut in_pos, mut in_len) = (0u32, 0usize, 0usize);
    let (mut produced, mut flushed) = (0usize, 0usize);

    loop {
        if in_pos == in_len && in_read < len {
            in_len = ((len - in_read) as usize).min(input.len());
            if !target.read(src + in_read, &mut input[..in_len]) {
                return Err(OtaError::FlashError);
            }
            in_read += in_len as u32;
            in_pos = 0;
        }
        let more = if in_read < len { TINFL_FLAG_HAS_MORE_INPUT } else { 0 };
        let room = WINDOW - (produced - flushed);
        let (status, consumed, written) = decompress_with_limit(
            &mut state,
            &input[in_pos..in_len],
            &mut window,
            produced % WINDOW,
            room,
            TINFL_FLAG_PARSE_ZLIB_HEADER | more,
        );
        in_pos += consumed;
        produced += written;
        let done = status == TINFLStatus::Done;
        // hand whole sectors (and the tail at the end) out of the window, in contiguous runs
        while produced - flushed >= SECTOR_SIZE as usize || (done && produced > flushed) {
            let start = flushed % WINDOW;
            let n = (produced - flushed).min(SECTOR_SIZE as usize).min(WINDOW - start);
            sink.put(target, &window[start..start + n])?;
            flushed += n;
        }
        match status {
            TINFLStatus::Done => break,
            TINFLStatus::NeedsMoreInput if in_read >= len && in_pos == in_len => return Err(OtaError::BadImage),
            TINFLStatus::NeedsMoreInput | TINFLStatus::HasMoreOutput => {}
            _ => return Err(OtaError::BadImage),
        }
    }
    sink.finish(target)
}

/// Check the image (its own hash) and the expected hash of the whole image.
fn verify_image(target: &mut dyn FirmwareTarget, size: u32, expected: Option<[u8; 32]>) -> Result<(), OtaError> {
    match target.format() {
        ImageFormat::Esp(chip) => verify_esp(target, size, expected, chip),
        ImageFormat::Nrf { chip, load } => verify_nrf(target, size, expected, chip, load),
    }
}

/// An nRF52 image: the trailer, the app's hash in it, and that it fits the app's flash.
fn verify_nrf(
    target: &mut dyn FirmwareTarget, size: u32, expected: Option<[u8; 32]>, chip: u32, load: u32,
) -> Result<(), OtaError> {
    let max = target.running_capacity();
    if max != 0 && size > max {
        return Err(OtaError::TooLarge);
    }
    let Some(app_len) = size.checked_sub(NRF_TRAILER_LEN) else {
        return Err(OtaError::BadImage);
    };
    let mut trailer = [0u8; NRF_TRAILER_LEN as usize];
    if !target.read(app_len, &mut trailer) {
        return Err(OtaError::FlashError);
    }
    if !nrf_trailer_ok(&trailer, app_len, chip, load) {
        return Err(OtaError::BadImage);
    }
    let mut whole = Sha256::new();
    let mut app = Sha256::new();
    let mut buf = [0u8; 1024];
    let mut offset = 0u32;
    while offset < size {
        let n = ((size - offset) as usize).min(buf.len());
        if !target.read(offset, &mut buf[..n]) {
            return Err(OtaError::FlashError);
        }
        whole.update(&buf[..n]);
        if offset < app_len {
            app.update(&buf[..(app_len.min(offset + n as u32) - offset) as usize]);
        }
        offset += n as u32;
    }
    if app.finalize()[..] != trailer[32..] {
        return Err(OtaError::HashMismatch);
    }
    match expected {
        Some(e) if whole.finalize()[..] != e => Err(OtaError::HashMismatch),
        _ => Ok(()),
    }
}

/// The ESP image header, its appended SHA-256 and the expected hash of the whole image.
fn verify_esp(
    target: &mut dyn FirmwareTarget, size: u32, expected: Option<[u8; 32]>, chip_id: u16,
) -> Result<(), OtaError> {
    // whole-image hash, and the hash of everything but the last 32 bytes (the appended one)
    let body_len = size - 32;
    let mut whole = Sha256::new();
    let mut body = Sha256::new();
    let mut buf = [0u8; 1024];
    let mut header = [0u8; 24];
    let mut tail = [0u8; 32];
    let mut offset = 0u32;
    while offset < size {
        let n = ((size - offset) as usize).min(buf.len());
        if !target.read(offset, &mut buf[..n]) {
            return Err(OtaError::FlashError);
        }
        let chunk = &buf[..n];
        if offset == 0 {
            header.copy_from_slice(&chunk[..24]);
        }
        whole.update(chunk);
        let end = offset + n as u32;
        if offset < body_len {
            body.update(&chunk[..(body_len.min(end) - offset) as usize]);
        }
        if end > body_len {
            let from = body_len.max(offset);
            tail[(from - body_len) as usize..(end - body_len) as usize].copy_from_slice(&chunk[(from - offset) as usize..]);
        }
        offset = end;
    }
    let chip = u16::from_le_bytes([header[12], header[13]]);
    if header[0] != ESP_IMAGE_MAGIC || chip != chip_id {
        return Err(OtaError::BadImage);
    }
    let hash_appended = header[23] == 1;
    if hash_appended && body.finalize()[..] != tail {
        return Err(OtaError::HashMismatch);
    }
    match expected {
        Some(e) if whole.finalize()[..] != e => Err(OtaError::HashMismatch),
        None if !hash_appended => Err(OtaError::BadImage), // nothing to verify against
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RamTarget {
        mem: Vec<u8>,
        erases: usize,
        activated: bool,
        /// The running slot (base of delta updates).
        running: Vec<u8>,
        format: ImageFormat,
    }

    impl RamTarget {
        fn new(size: usize, fill: u8, running: Vec<u8>) -> Self {
            Self { mem: vec![fill; size], erases: 0, activated: false, running, format: ImageFormat::Esp(9) }
        }
    }

    impl FirmwareTarget for RamTarget {
        fn capacity(&self) -> u32 {
            self.mem.len() as u32
        }
        fn erase(&mut self, offset: u32, len: u32) -> bool {
            self.erases += 1;
            self.mem[offset as usize..(offset + len) as usize].fill(0xFF);
            true
        }
        fn write(&mut self, offset: u32, data: &[u8]) -> bool {
            assert_eq!(offset % 4, 0);
            assert_eq!(data.len() % 4, 0);
            let dst = &mut self.mem[offset as usize..offset as usize + data.len()];
            // NOR flash can only clear bits
            for (d, s) in dst.iter_mut().zip(data) {
                *d &= *s;
            }
            true
        }
        fn read(&mut self, offset: u32, buf: &mut [u8]) -> bool {
            buf.copy_from_slice(&self.mem[offset as usize..offset as usize + buf.len()]);
            true
        }
        fn activate(&mut self, _size: u32) -> bool {
            self.activated = true;
            true
        }
        fn format(&self) -> ImageFormat {
            self.format
        }
        fn running_capacity(&self) -> u32 {
            self.running.len() as u32
        }
        fn read_running(&mut self, offset: u32, buf: &mut [u8]) -> bool {
            buf.copy_from_slice(&self.running[offset as usize..offset as usize + buf.len()]);
            true
        }
    }

    #[test]
    fn corrupted_chunks_are_dropped_when_they_carry_a_crc() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        let img = image(1000);
        let sha: [u8; 32] = Sha256::digest(&img).into();
        let mut ep = OtaEndpoint { target: Some(Box::new(RamTarget::new(8192, 0, Vec::new()))), ..Default::default() };
        let mut begin = vec![REQ_TYPE_OTA_BEGIN];
        begin.extend_from_slice(&1000u32.to_le_bytes());
        begin.extend_from_slice(&sha);
        begin.extend_from_slice(&168u16.to_le_bytes());
        begin.push(4); // CRCs, not compressed
        assert_eq!(ep.handle(&begin, "v").0.unwrap()[0], 0);
        let data = |i: u16, flip: bool| {
            let mut f = vec![REQ_TYPE_OTA_DATA];
            f.extend_from_slice(&i.to_le_bytes());
            f.extend_from_slice(&img[i as usize * 168..((i as usize + 1) * 168).min(1000)]);
            let crc = crc32(&f[1..]);
            if flip {
                f[10] ^= 0x40;
            }
            f.extend_from_slice(&crc.to_le_bytes());
            f
        };
        for i in 0..6 {
            ep.handle(&data(i, i == 2), "v");
        }
        let status = ep.handle(&[REQ_TYPE_OTA_STATUS, 0, 0], "v").0.unwrap();
        assert_eq!(&status[..8], &[0, 5, 0, 6, 0, 1, 2, 0], "chunk 2 is missing");
        ep.handle(&data(2, false), "v");
        assert_eq!(ep.handle(&[REQ_TYPE_OTA_COMMIT], "v").0.unwrap()[0], 0);
    }

    /// Fake nRF52840 image: app bytes, then the trailer, like tools/uf2/elf2uf2.py writes it.
    fn nrf_image(app: &[u8]) -> Vec<u8> {
        let mut img = app.to_vec();
        img.extend_from_slice(NRF_TRAILER_MAGIC);
        img.extend_from_slice(&(app.len() as u32).to_le_bytes());
        img.extend_from_slice(&0x26000u32.to_le_bytes());
        img.extend_from_slice(&NRF_CHIP_NRF52840.to_le_bytes());
        img.extend_from_slice(&[0; 12]);
        img.extend_from_slice(&Sha256::digest(app));
        img
    }

    #[test]
    fn nrf_images_and_the_running_one() {
        let nrf = ImageFormat::Nrf { chip: NRF_CHIP_NRF52840, load: 0x26000 };
        // the app's own data holds the magic too: it must not pass for the trailer
        let mut app: Vec<u8> = (0..30_000u32).map(|i| (i * 13 % 251) as u8).collect();
        app[4096..4104].copy_from_slice(NRF_TRAILER_MAGIC);
        let img = nrf_image(&app);
        let mut running = img.clone();
        running.extend_from_slice(&nrf_image(&[7; 2000])); // stale bytes of an older, longer build
        running.resize(64 * 1024, 0xFF);
        let mut t = RamTarget::new(64 * 1024, 0x55, running);
        t.format = nrf;
        assert_eq!(running_image(&mut t), Some((img.len() as u32, Sha256::digest(&img).into())));

        let upload = |t: &mut RamTarget, img: &[u8]| {
            let sha: [u8; 32] = Sha256::digest(img).into();
            let mut s = OtaSession::begin(t, img.len() as u32, Some(sha), 1024)?;
            for (i, c) in img.chunks(1024).enumerate() {
                s.write_chunk(t, i as u32, c)?;
            }
            s.commit(t)
        };
        let mut corrupt = img.clone();
        corrupt[100] ^= 1;
        assert_eq!(upload(&mut t, &corrupt), Err(OtaError::HashMismatch));
        let mut other_chip = img.clone();
        other_chip[app.len() + 16] = 0x32;
        assert_eq!(upload(&mut t, &other_chip), Err(OtaError::BadImage));
        assert_eq!(upload(&mut t, &image(5000)), Err(OtaError::BadImage), "an ESP image");
        assert!(!t.activated);
        assert_eq!(upload(&mut t, &img), Ok(()));
        assert!(t.activated);
        // bigger than the app's flash (the running slot)
        let big = nrf_image(&vec![1; 70 * 1024]);
        let mut t = RamTarget::new(128 * 1024, 0x55, vec![0xFF; 64 * 1024]);
        t.format = nrf;
        assert_eq!(upload(&mut t, &big), Err(OtaError::TooLarge));
        assert_eq!(running_image(&mut t), None, "no trailer: flashed some other way");
    }

    /// Fake ESP32-S3 image with the SHA-256 appended, like espflash produces.
    fn image(len: usize) -> Vec<u8> {
        let mut img: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
        img[0] = 0xE9;
        img[12..14].copy_from_slice(&9u16.to_le_bytes());
        img[23] = 1;
        let h: [u8; 32] = Sha256::digest(&img[..len - 32]).into();
        img[len - 32..].copy_from_slice(&h);
        img
    }

    #[test]
    fn out_of_order_with_duplicates_and_resume() {
        let img = image(10_001);
        let sha: [u8; 32] = Sha256::digest(&img).into();
        let mut t = RamTarget::new(64 * 1024, 0x55, Vec::new());
        let mut s = OtaSession::begin(&t, img.len() as u32, Some(sha), LORA_CHUNK_SIZE).unwrap();
        let n = s.total_chunks();
        let chunk = |i: u32| {
            let a = (i * LORA_CHUNK_SIZE as u32) as usize;
            &img[a..(a + LORA_CHUNK_SIZE as usize).min(img.len())]
        };
        // send odd chunks first, then even, one duplicate
        for i in (1..n).step_by(2) {
            s.write_chunk(&mut t, i, chunk(i)).unwrap();
        }
        assert_eq!(s.missing(0, 3), vec![0, 2, 4]);
        assert_eq!(s.commit(&mut t), Err(OtaError::Incomplete));
        for i in (0..n).step_by(2) {
            s.write_chunk(&mut t, i, chunk(i)).unwrap();
        }
        s.write_chunk(&mut t, 3, chunk(3)).unwrap();
        assert!(s.is_complete());
        assert_eq!(t.erases, 3, "each sector erased exactly once");
        s.commit(&mut t).unwrap();
        assert!(t.activated);
        assert_eq!(&t.mem[..img.len()], &img[..]);
    }

    #[test]
    fn rejects_bad_hash_bad_image_and_bad_chunks() {
        let img = image(1000);
        let mut t = RamTarget::new(8192, 0, Vec::new());
        let mut s = OtaSession::begin(&t, 1000, Some([0; 32]), 152).unwrap();
        assert_eq!(s.write_chunk(&mut t, 0, &img[..100]), Err(OtaError::BadParams));
        assert_eq!(s.write_chunk(&mut t, 99, &img[..152]), Err(OtaError::BadParams));
        for i in 0..s.total_chunks() {
            let a = (i * 152) as usize;
            s.write_chunk(&mut t, i, &img[a..(a + 152).min(1000)]).unwrap();
        }
        assert_eq!(s.commit(&mut t), Err(OtaError::HashMismatch));

        let mut wrong = image(1000);
        wrong[12] = 5; // another chip
        let sha: [u8; 32] = Sha256::digest(&wrong).into();
        let mut s = OtaSession::begin(&t, 1000, Some(sha), 152).unwrap();
        for i in 0..s.total_chunks() {
            let a = (i * 152) as usize;
            s.write_chunk(&mut t, i, &wrong[a..(a + 152).min(1000)]).unwrap();
        }
        assert_eq!(s.commit(&mut t), Err(OtaError::BadImage));
        assert!(!t.activated);
        assert!(OtaSession::begin(&t, 9000, None, 152).is_err());
    }

    /// zlib-compressed transfer: staged at the end of the slot, inflated to its start.
    #[test]
    fn compressed_transfer() {
        let img = image(40_000);
        let packed = miniz_oxide::deflate::compress_to_vec_zlib(&img, 9);
        assert!(packed.len() < img.len());
        let sha: [u8; 32] = Sha256::digest(&img).into();
        let mut t = RamTarget::new(128 * 1024, 0x55, Vec::new());
        let mut s = OtaSession::begin_compressed(&t, packed.len() as u32, img.len() as u32, Some(sha), 152).unwrap();
        for i in (0..s.total_chunks()).rev() {
            let a = (i * 152) as usize;
            s.write_chunk(&mut t, i, &packed[a..(a + 152).min(packed.len())]).unwrap();
        }
        s.commit(&mut t).unwrap();
        assert!(t.activated);
        assert_eq!(&t.mem[..img.len()], &img[..]);

        // a corrupted stream is rejected, not activated
        let mut bad = packed.clone();
        bad[100] ^= 0xFF;
        t.activated = false;
        let mut s = OtaSession::begin_compressed(&t, bad.len() as u32, img.len() as u32, Some(sha), 152).unwrap();
        for i in 0..s.total_chunks() {
            let a = (i * 152) as usize;
            s.write_chunk(&mut t, i, &bad[a..(a + 152).min(bad.len())]).unwrap();
        }
        assert!(s.commit(&mut t).is_err());
        assert!(!t.activated);
        // does not fit
        assert!(OtaSession::begin_compressed(&t, 100_000, 100_000, None, 152).is_err());
    }

    /// Wi-Fi uploads carry no hash: the one appended to the image must be intact.
    #[test]
    fn embedded_hash_only() {
        let img = image(5000);
        let mut t = RamTarget::new(8192, 0, Vec::new());
        let upload = |t: &mut RamTarget, img: &[u8]| {
            let mut s = OtaSession::begin(t, img.len() as u32, None, 1024).unwrap();
            for (i, c) in img.chunks(1024).enumerate() {
                s.write_chunk(t, i as u32, c).unwrap();
            }
            s.commit(t)
        };
        let mut corrupt = img.clone();
        corrupt[2000] ^= 1;
        assert_eq!(upload(&mut t, &corrupt), Err(OtaError::HashMismatch));
        let mut no_hash = img.clone();
        no_hash[23] = 0;
        assert_eq!(upload(&mut t, &no_hash), Err(OtaError::BadImage));
        assert_eq!(upload(&mut t, &img), Ok(()));
        assert!(t.activated);
    }

    /// Fake image in the real layout: header, segments, checksum block, appended SHA-256.
    fn segmented_image(segments: &[Vec<u8>]) -> Vec<u8> {
        let mut img = vec![0u8; 24];
        img[0] = 0xE9;
        img[1] = segments.len() as u8;
        img[12..14].copy_from_slice(&9u16.to_le_bytes());
        img[23] = 1;
        for (i, seg) in segments.iter().enumerate() {
            img.extend_from_slice(&(0x4200_0000u32 + i as u32 * 0x10000).to_le_bytes());
            img.extend_from_slice(&(seg.len() as u32).to_le_bytes());
            img.extend_from_slice(seg);
        }
        img.resize((img.len() / 16 + 1) * 16, 0);
        let h: [u8; 32] = Sha256::digest(&img).into();
        img.extend_from_slice(&h);
        img
    }

    fn send_all(s: &mut OtaSession, t: &mut RamTarget, data: &[u8]) {
        for i in 0..s.total_chunks() {
            let a = (i * 152) as usize;
            s.write_chunk(t, i, &data[a..(a + 152).min(data.len())]).unwrap();
        }
    }

    #[test]
    fn delta_update_against_running_firmware() {
        let code: Vec<u8> = (0..60_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        let data: Vec<u8> = (0..9_000u32).map(|i| (i * 7 % 253) as u8).collect();
        let old = segmented_image(&[code.clone(), data.clone()]);
        // new build: a few bytes changed, a block inserted, the data segment grown
        let mut code2 = code.clone();
        code2[1000] ^= 0x5A;
        code2.splice(30_000..30_000, (0..700u32).map(|i| i as u8));
        let mut data2 = data.clone();
        data2.extend_from_slice(b"new string table entry");
        let new = segmented_image(&[code2, data2]);

        let mut running = old.clone();
        running.resize(128 * 1024, 0xFF); // the rest of the slot
        let mut t = RamTarget::new(256 * 1024, 0x55, running);
        let (base_len, base_sha) = running_image(&mut t).unwrap();
        assert_eq!(base_len as usize, old.len());
        assert_eq!(base_sha, <[u8; 32]>::from(Sha256::digest(&old)));

        let mut patch = Vec::new();
        bsdiff::diff(&old, &new, &mut patch).unwrap();
        let packed = miniz_oxide::deflate::compress_to_vec_zlib(&patch, 9);
        assert!(packed.len() < new.len() / 10, "delta {} vs image {}", packed.len(), new.len());
        let sha: [u8; 32] = Sha256::digest(&new).into();

        // a patch for another base is refused before anything is sent
        let wrong = OtaSession::begin_delta(&mut t, packed.len() as u32, new.len() as u32, sha, 152, base_len, [0; 32]);
        assert_eq!(wrong.err(), Some(OtaError::BaseMismatch));

        let mut s =
            OtaSession::begin_delta(&mut t, packed.len() as u32, new.len() as u32, sha, 152, base_len, base_sha).unwrap();
        send_all(&mut s, &mut t, &packed);
        s.commit(&mut t).unwrap();
        assert!(t.activated);
        assert_eq!(&t.mem[..new.len()], &new[..]);

        // a corrupted patch never activates
        let mut bad = patch.clone();
        bad[40] ^= 1; // inside the first diff block
        let bad = miniz_oxide::deflate::compress_to_vec_zlib(&bad, 9);
        t.activated = false;
        let mut s = OtaSession::begin_delta(&mut t, bad.len() as u32, new.len() as u32, sha, 152, base_len, base_sha).unwrap();
        send_all(&mut s, &mut t, &bad);
        assert!(s.commit(&mut t).is_err());
        assert!(!t.activated);
    }

    /// With a pinned root only the image of an accepted signed release can begin a session.
    #[test]
    fn pinned_root_requires_a_signed_release() {
        use crate::LocalIdentity;
        use crate::release::{self, Envelope, Manifest};

        let img = image(3000);
        let sha: [u8; 32] = Sha256::digest(&img).into();
        let root = LocalIdentity::from_seed(&[9; 32]);
        let stranger = LocalIdentity::from_seed(&[8; 32]);
        let build = release::Build { board: release::board::HELTEC_V4, role: release::role::REPEATER, counter: 5 };
        let mut ep = OtaEndpoint {
            target: Some(Box::new(RamTarget::new(8192, 0, Vec::new()))),
            roots: vec![release::fingerprint(root.pub_key())],
            build,
            ..Default::default()
        };
        let envelope = |key: &LocalIdentity, m: Manifest| {
            let mut e = Envelope { manifest: m, signatures: Vec::new() };
            e.add_signature(*key.pub_key(), key.sign(&Envelope::signed_message(&e.manifest)));
            e.to_bytes()
        };
        // in 100-byte pieces with cipher padding, as over LoRa; the last reply's status
        let send = |ep: &mut OtaEndpoint, env: &[u8]| {
            let mut status = 0xFF;
            for (k, piece) in env.chunks(100).enumerate() {
                let mut req = vec![REQ_TYPE_OTA_MANIFEST];
                req.extend_from_slice(&((k * 100) as u16).to_le_bytes());
                req.extend_from_slice(&(env.len() as u16).to_le_bytes());
                req.push(piece.len() as u8);
                req.extend_from_slice(piece);
                req.extend_from_slice(&[0; 7]);
                status = ep.handle(&req, "v").0.unwrap()[0];
            }
            status
        };
        let mut begin = vec![REQ_TYPE_OTA_BEGIN];
        begin.extend_from_slice(&3000u32.to_le_bytes());
        begin.extend_from_slice(&sha);
        begin.extend_from_slice(&500u16.to_le_bytes());
        let begin_status = |ep: &mut OtaEndpoint| ep.handle(&begin, "v").0.unwrap()[0];
        let manifest = Manifest {
            board: build.board,
            role: build.role,
            flags: 0,
            counter: 6,
            image_len: 3000,
            image_sha256: sha,
            issued: 0,
            version: "test".into(),
        };

        assert_eq!(begin_status(&mut ep), OtaError::Unsigned as u8);
        assert_eq!(send(&mut ep, &envelope(&stranger, manifest.clone())), OtaError::Untrusted as u8);
        assert_eq!(begin_status(&mut ep), OtaError::Unsigned as u8);
        let old = Manifest { counter: 4, ..manifest.clone() };
        assert_eq!(send(&mut ep, &envelope(&root, old)), OtaError::Downgrade as u8);
        let v3 = Manifest { board: release::board::HELTEC_V3, ..manifest.clone() };
        assert_eq!(send(&mut ep, &envelope(&root, v3)), OtaError::WrongBoard as u8);
        let other_image = Manifest { image_sha256: [1; 32], ..manifest.clone() };
        assert_eq!(send(&mut ep, &envelope(&root, other_image)), 0);
        assert_eq!(begin_status(&mut ep), OtaError::HashMismatch as u8, "signed, but for another image");

        assert_eq!(send(&mut ep, &envelope(&root, manifest.clone())), 0);
        assert_eq!(begin_status(&mut ep), 0);
        for (i, c) in img.chunks(500).enumerate() {
            let mut d = vec![REQ_TYPE_OTA_DATA];
            d.extend_from_slice(&(i as u16).to_le_bytes());
            d.extend_from_slice(c);
            ep.handle(&d, "v");
        }
        assert_eq!(ep.handle(&[REQ_TYPE_OTA_COMMIT], "v").0.unwrap()[0], 0);
        assert_eq!(begin_status(&mut ep), OtaError::Unsigned as u8, "a release is good for one session");

        let info = ep.handle(&[REQ_TYPE_OTA_INFO], "v").0.unwrap();
        assert_eq!(&info[info.len() - 8..], &[1, 1, 5, 0, 0, 0, 1, 0], "board, role, counter, roots, owners");

        // an owner key pinned: the root's signature alone is not enough
        let owner = LocalIdentity::from_seed(&[7; 32]);
        ep.owners = vec![release::fingerprint(owner.pub_key())];
        assert_eq!(send(&mut ep, &envelope(&root, manifest.clone())), OtaError::NotOwner as u8);
        assert_eq!(begin_status(&mut ep), OtaError::Unsigned as u8);
        let mut cosigned = Envelope { manifest: manifest.clone(), signatures: Vec::new() };
        for key in [&root, &owner] {
            cosigned.add_signature(*key.pub_key(), key.sign(&Envelope::signed_message(&manifest)));
        }
        assert_eq!(send(&mut ep, &cosigned.to_bytes()), 0);
        assert_eq!(begin_status(&mut ep), 0);
    }
}
