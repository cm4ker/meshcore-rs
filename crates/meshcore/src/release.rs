//! Signed firmware releases, checked like a pinned TLS certificate.
//!
//! The release build signs a manifest naming the board, role and release number of an image and
//! its SHA-256. A device pins the fingerprints (SHA-256 of the public key) of the root keys it
//! trusts; once it has one, it installs only images whose manifest such a key signed, whoever
//! delivers them. With no pinned root it takes unsigned images as before.
//!
//! The envelope: `[manifest][n u8][(public key 32, signature 64) x n]`. A `.mcfw` file is the
//! envelope followed by the image. The manifest, little-endian:
//! `"MCFW" [format u8 = 1][board u8][role u8][flags u8][counter u32][image_len u32]
//! [image_sha256 32][issued u32][version_len u8][version]`. Signatures are Ed25519 over
//! `"MCFW-SIG" || manifest`, so they can't be mistaken for any other signature by the same key.

use alloc::string::String;
use alloc::vec::Vec;

use sha2::{Digest, Sha256};

use crate::identity::Identity;
use crate::{PUB_KEY_SIZE, SIGNATURE_SIZE};

pub const MAGIC: &[u8; 4] = b"MCFW";
pub const FORMAT: u8 = 1;
const SIGNED_PREFIX: &[u8] = b"MCFW-SIG";
/// The manifest up to its version string.
const FIXED_LEN: usize = 53;
pub const VERSION_MAX: usize = 31;
pub const MAX_SIGNATURES: usize = 8;
const SIGNATURE_ENTRY: usize = PUB_KEY_SIZE + SIGNATURE_SIZE;
/// Root keys a device pins at most.
pub const MAX_ROOTS: usize = 4;
pub const FINGERPRINT_SIZE: usize = 32;
pub type Fingerprint = [u8; FINGERPRINT_SIZE];

/// The board an image is built for.
pub mod board {
    pub const HELTEC_V4: u8 = 1;
    pub const HELTEC_V3: u8 = 2;
    pub const XIAO_C6: u8 = 3;
    pub const T_ECHO: u8 = 4;
}

/// What the image runs as.
pub mod role {
    pub const REPEATER: u8 = 1;
    pub const COMPANION: u8 = 2;
}

/// Bits of [`Manifest::flags`].
pub mod flags {
    /// A release of the dev channel: devices that update by themselves take it only when they
    /// follow that channel (`fw.channel dev`); a stable release has the bit clear.
    pub const DEV: u8 = 0x01;
}

/// The release channels a device that updates by itself may follow.
pub mod channel {
    pub const STABLE: u8 = 0;
    pub const DEV: u8 = 1;
}

pub fn channel_name(channel: u8) -> &'static str {
    if channel == channel::DEV { "dev" } else { "stable" }
}

pub fn channel_by_name(name: &str) -> Option<u8> {
    match name {
        "stable" => Some(channel::STABLE),
        "dev" => Some(channel::DEV),
        _ => None,
    }
}

/// Release number of this build, from `MCFW_COUNTER` at build time (0 for local builds).
pub const RELEASE_COUNTER: u32 = parse_u32(option_env!("MCFW_COUNTER"));

/// How a build names itself to people (`ver`, the app's device info, logs): a release by its
/// number, a local build by its date.
pub fn build_label(build_date: &str) -> String {
    match RELEASE_COUNTER {
        0 => alloc::format!("local build {build_date}"),
        n => alloc::format!("build #{n}"),
    }
}

const fn parse_u32(s: Option<&str>) -> u32 {
    let Some(s) = s else {
        return 0;
    };
    let b = s.as_bytes();
    let mut v = 0u32;
    let mut i = 0;
    while i < b.len() {
        if b[i] < b'0' || b[i] > b'9' {
            return 0;
        }
        v = v.saturating_mul(10).saturating_add((b[i] - b'0') as u32);
        i += 1;
    }
    v
}

pub fn board_name(board: u8) -> &'static str {
    match board {
        board::HELTEC_V4 => "heltec-v4",
        board::HELTEC_V3 => "heltec-v3",
        board::XIAO_C6 => "xiao-c6",
        board::T_ECHO => "t-echo",
        _ => "?",
    }
}

pub fn board_by_name(name: &str) -> Option<u8> {
    [board::HELTEC_V4, board::HELTEC_V3, board::XIAO_C6, board::T_ECHO].into_iter().find(|&b| board_name(b) == name)
}

pub fn role_name(role: u8) -> &'static str {
    match role {
        role::REPEATER => "repeater",
        role::COMPANION => "companion",
        _ => "?",
    }
}

pub fn role_by_name(name: &str) -> Option<u8> {
    [role::REPEATER, role::COMPANION].into_iter().find(|&r| role_name(r) == name)
}

/// The fingerprint a device pins: SHA-256 of the root's public key.
pub fn fingerprint(public_key: &[u8; PUB_KEY_SIZE]) -> Fingerprint {
    Sha256::digest(public_key).into()
}

/// Lowercase hex, as fingerprints are shown and typed.
pub fn fingerprint_hex(fp: &Fingerprint) -> String {
    crate::util::to_hex(fp).to_ascii_lowercase()
}

pub fn join_fingerprints(fps: &[Fingerprint], sep: &str) -> String {
    fps.iter().map(fingerprint_hex).collect::<Vec<_>>().join(sep)
}

/// Fingerprints separated by commas, spaces or semicolons; `None` if any is not 64 hex digits.
pub fn parse_fingerprints(text: &str) -> Option<Vec<Fingerprint>> {
    let mut out = Vec::new();
    for word in text.split([',', ' ', ';']).filter(|w| !w.is_empty()) {
        let mut fp = [0u8; FINGERPRINT_SIZE];
        if word.len() != FINGERPRINT_SIZE * 2 || !crate::util::from_hex(&mut fp, word) {
            return None;
        }
        if !out.contains(&fp) {
            out.push(fp);
        }
    }
    (out.len() <= MAX_ROOTS).then_some(out)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub board: u8,
    pub role: u8,
    pub flags: u8,
    /// Release number: a device never installs a lower one than the build it runs.
    pub counter: u32,
    pub image_len: u32,
    pub image_sha256: [u8; 32],
    /// Unix time of the release, for people only (device clocks can't be trusted).
    pub issued: u32,
    pub version: String,
}

impl Manifest {
    /// A device following `channel` takes this release by itself: a dev device every release, a
    /// stable one only the stable ones.
    pub fn fits_channel(&self, channel: u8) -> bool {
        channel == channel::DEV || self.flags & flags::DEV == 0
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let version = &self.version.as_bytes()[..self.version.len().min(VERSION_MAX)];
        let mut b = Vec::with_capacity(FIXED_LEN + version.len());
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&[FORMAT, self.board, self.role, self.flags]);
        b.extend_from_slice(&self.counter.to_le_bytes());
        b.extend_from_slice(&self.image_len.to_le_bytes());
        b.extend_from_slice(&self.image_sha256);
        b.extend_from_slice(&self.issued.to_le_bytes());
        b.push(version.len() as u8);
        b.extend_from_slice(version);
        b
    }

    /// The manifest at the start of `b` and its length.
    fn parse(b: &[u8]) -> Option<(Self, usize)> {
        if b.len() < FIXED_LEN || &b[..4] != MAGIC || b[4] != FORMAT {
            return None;
        }
        let u32_at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        let vlen = b[52] as usize;
        if vlen > VERSION_MAX {
            return None;
        }
        let version = core::str::from_utf8(b.get(FIXED_LEN..FIXED_LEN + vlen)?).ok()?.into();
        let m = Self {
            board: b[5],
            role: b[6],
            flags: b[7],
            counter: u32_at(8),
            image_len: u32_at(12),
            image_sha256: b[16..48].try_into().unwrap(),
            issued: u32_at(48),
            version,
        };
        Some((m, FIXED_LEN + vlen))
    }
}

/// What the envelope at the start of some bytes is.
#[derive(Debug, PartialEq, Eq)]
pub enum Peek {
    /// The envelope is `len` bytes long.
    Envelope { len: usize },
    /// Not an envelope.
    NotEnvelope,
    /// At least this many bytes are needed to tell.
    NeedMore(usize),
}

/// The length of the envelope at the start of `b` (for a `.mcfw` file or a streamed upload).
pub fn peek(b: &[u8]) -> Peek {
    if b.len() < MAGIC.len() {
        return Peek::NeedMore(MAGIC.len());
    }
    if &b[..4] != MAGIC {
        return Peek::NotEnvelope;
    }
    let Some(&vlen) = b.get(FIXED_LEN - 1) else {
        return Peek::NeedMore(FIXED_LEN);
    };
    let count_at = FIXED_LEN + vlen as usize;
    match b.get(count_at) {
        None => Peek::NeedMore(count_at + 1),
        Some(&n) if n as usize > MAX_SIGNATURES || vlen as usize > VERSION_MAX => Peek::NotEnvelope,
        Some(&n) => Peek::Envelope { len: count_at + 1 + n as usize * SIGNATURE_ENTRY },
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub public_key: [u8; PUB_KEY_SIZE],
    pub signature: [u8; SIGNATURE_SIZE],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Envelope {
    pub manifest: Manifest,
    pub signatures: Vec<Signature>,
}

/// Why a device refuses a release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No valid signature by a pinned root key.
    Untrusted,
    /// Built for another board or role.
    WrongBoard,
    /// Older than the running build.
    Downgrade,
}

/// The build a device runs, which a release must match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Build {
    pub board: u8,
    pub role: u8,
    pub counter: u32,
}

impl Default for Build {
    fn default() -> Self {
        Self { board: 0, role: 0, counter: RELEASE_COUNTER }
    }
}

impl Envelope {
    /// The envelope at the start of `b` and its length.
    pub fn parse(b: &[u8]) -> Option<(Self, usize)> {
        let (manifest, mlen) = Manifest::parse(b)?;
        let n = *b.get(mlen)? as usize;
        if n > MAX_SIGNATURES {
            return None;
        }
        let mut signatures = Vec::with_capacity(n);
        let mut at = mlen + 1;
        for _ in 0..n {
            let e = b.get(at..at + SIGNATURE_ENTRY)?;
            signatures.push(Signature {
                public_key: e[..PUB_KEY_SIZE].try_into().unwrap(),
                signature: e[PUB_KEY_SIZE..].try_into().unwrap(),
            });
            at += SIGNATURE_ENTRY;
        }
        Some((Self { manifest, signatures }, at))
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = self.manifest.to_bytes();
        b.push(self.signatures.len() as u8);
        for s in &self.signatures {
            b.extend_from_slice(&s.public_key);
            b.extend_from_slice(&s.signature);
        }
        b
    }

    /// What the signatures sign.
    pub fn signed_message(manifest: &Manifest) -> Vec<u8> {
        [SIGNED_PREFIX, &manifest.to_bytes()].concat()
    }

    /// Add (or replace) the signature of `public_key`.
    pub fn add_signature(&mut self, public_key: [u8; PUB_KEY_SIZE], signature: [u8; SIGNATURE_SIZE]) {
        self.signatures.retain(|s| s.public_key != public_key);
        self.signatures.push(Signature { public_key, signature });
    }

    /// Fingerprints of the keys whose signatures are valid.
    pub fn valid_signers(&self) -> Vec<Fingerprint> {
        let message = Self::signed_message(&self.manifest);
        self.signatures
            .iter()
            .filter(|s| Identity::new(s.public_key).verify(&s.signature, &message))
            .map(|s| fingerprint(&s.public_key))
            .collect()
    }

    /// Whether a device that pins `roots` and runs `build` may install this release.
    pub fn check(&self, roots: &[Fingerprint], build: &Build) -> Result<(), Refusal> {
        if !self.valid_signers().iter().any(|fp| roots.contains(fp)) {
            return Err(Refusal::Untrusted);
        }
        if self.manifest.board != build.board || self.manifest.role != build.role {
            return Err(Refusal::WrongBoard);
        }
        if self.manifest.counter < build.counter {
            return Err(Refusal::Downgrade);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LocalIdentity;

    fn manifest() -> Manifest {
        Manifest {
            board: board::HELTEC_V4,
            role: role::REPEATER,
            flags: 0,
            counter: 42,
            image_len: 1_043_216,
            image_sha256: [7; 32],
            issued: 1_792_800_000,
            version: "0.5.0".into(),
        }
    }

    fn signed(key: &LocalIdentity, m: Manifest) -> Envelope {
        let mut e = Envelope { manifest: m, signatures: Vec::new() };
        let sig = key.sign(&Envelope::signed_message(&e.manifest));
        e.add_signature(*key.pub_key(), sig);
        e
    }

    #[test]
    fn roundtrip_and_peek() {
        let root = LocalIdentity::from_seed(&[1; 32]);
        let other = LocalIdentity::from_seed(&[2; 32]);
        let mut e = signed(&root, manifest());
        let sig = other.sign(&Envelope::signed_message(&e.manifest));
        e.add_signature(*other.pub_key(), sig);
        let bytes = e.to_bytes();
        assert_eq!(bytes.len(), FIXED_LEN + 5 + 1 + 2 * SIGNATURE_ENTRY);
        let mut file = bytes.clone();
        file.extend_from_slice(b"image bytes");
        assert_eq!(Envelope::parse(&file), Some((e.clone(), bytes.len())));
        assert_eq!(peek(&file), Peek::Envelope { len: bytes.len() });
        assert_eq!(peek(&file[..10]), Peek::NeedMore(FIXED_LEN));
        assert_eq!(peek(&file[..FIXED_LEN]), Peek::NeedMore(FIXED_LEN + 5 + 1));
        assert_eq!(peek(&[0xE9, 3, 2, 0x20]), Peek::NotEnvelope, "an ESP image");
        assert_eq!(e.valid_signers(), alloc::vec![fingerprint(root.pub_key()), fingerprint(other.pub_key())]);
    }

    #[test]
    fn only_a_pinned_root_counts() {
        let root = LocalIdentity::from_seed(&[1; 32]);
        let stranger = LocalIdentity::from_seed(&[3; 32]);
        let pin = [fingerprint(root.pub_key())];
        let build = Build { board: board::HELTEC_V4, role: role::REPEATER, counter: 41 };
        assert_eq!(signed(&root, manifest()).check(&pin, &build), Ok(()));
        assert_eq!(signed(&stranger, manifest()).check(&pin, &build), Err(Refusal::Untrusted));
        assert_eq!(signed(&root, manifest()).check(&[], &build), Err(Refusal::Untrusted));

        // a signature over another manifest (the image hash changed afterwards)
        let mut forged = signed(&root, manifest());
        forged.manifest.image_sha256 = [8; 32];
        assert_eq!(forged.check(&pin, &build), Err(Refusal::Untrusted));

        let v3 = Manifest { board: board::HELTEC_V3, ..manifest() };
        assert_eq!(signed(&root, v3).check(&pin, &build), Err(Refusal::WrongBoard));
        let companion = Manifest { role: role::COMPANION, ..manifest() };
        assert_eq!(signed(&root, companion).check(&pin, &build), Err(Refusal::WrongBoard));
        let old = Manifest { counter: 40, ..manifest() };
        assert_eq!(signed(&root, old).check(&pin, &build), Err(Refusal::Downgrade));
        let same = Manifest { counter: 41, ..manifest() };
        assert_eq!(signed(&root, same).check(&pin, &build), Ok(()), "reinstalling the running release");
    }

    #[test]
    fn fingerprints_as_typed() {
        let fp = fingerprint(LocalIdentity::from_seed(&[1; 32]).pub_key());
        let hex = fingerprint_hex(&fp);
        assert_eq!(parse_fingerprints(&hex), Some(alloc::vec![fp]));
        assert_eq!(parse_fingerprints(&hex.to_ascii_uppercase()), Some(alloc::vec![fp]));
        assert_eq!(parse_fingerprints(&alloc::format!("{hex}, {hex}")), Some(alloc::vec![fp]), "duplicates collapse");
        assert_eq!(parse_fingerprints(""), Some(alloc::vec![]));
        assert_eq!(parse_fingerprints(&hex[..60]), None);
        assert_eq!(parse_fingerprints("zz"), None);
        assert_eq!(parse_u32(Some("42")), 42);
        assert_eq!(parse_u32(Some("4x")), 0);
        assert_eq!(parse_u32(None), 0);
    }
}
