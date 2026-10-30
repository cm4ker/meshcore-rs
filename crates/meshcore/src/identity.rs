//! Node identities: Ed25519 keys in the orlp/ed25519 "expanded" format MeshCore stores.
//!
//! The 64-byte private key is `SHA-512(seed)` with the first half clamped, so signing goes
//! through the `hazmat` API. ECDH converts the peer's Ed25519 public key to Montgomery form
//! and runs X25519 with our clamped scalar (identical to orlp's `ed25519_key_exchange`).

use curve25519_dalek::edwards::{CompressedEdwardsY, EdwardsPoint};
use ed25519_dalek::hazmat::{ExpandedSecretKey, raw_sign, raw_verify};
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha512};

use crate::crypto::Secret;
use crate::{PATH_HASH_SIZE, PRV_KEY_SIZE, PUB_KEY_SIZE, SEED_SIZE, SIGNATURE_SIZE};

/// A party whose signatures we can verify. Its "hash" is simply a prefix of the public key.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Identity {
    pub pub_key: [u8; PUB_KEY_SIZE],
}

impl core::fmt::Debug for Identity {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Identity(")?;
        for b in &self.pub_key[..4] {
            write!(f, "{b:02X}")?;
        }
        write!(f, "..)")
    }
}

impl Identity {
    pub const fn new(pub_key: [u8; PUB_KEY_SIZE]) -> Self {
        Self { pub_key }
    }

    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        Some(Self { pub_key: bytes.get(..PUB_KEY_SIZE)?.try_into().ok()? })
    }

    /// The 1-byte hash used in V1 payloads.
    pub fn hash(&self) -> u8 {
        self.pub_key[0]
    }

    pub fn copy_hash_to(&self, dest: &mut [u8]) -> usize {
        let n = dest.len().min(PUB_KEY_SIZE);
        dest[..n].copy_from_slice(&self.pub_key[..n]);
        n
    }

    pub fn is_hash_match(&self, hash: &[u8]) -> bool {
        hash.len() <= PUB_KEY_SIZE && self.pub_key.starts_with(hash)
    }

    pub fn is_hash_match_v1(&self, hash: u8) -> bool {
        self.pub_key[..PATH_HASH_SIZE] == [hash]
    }

    pub fn verify(&self, sig: &[u8; SIGNATURE_SIZE], message: &[u8]) -> bool {
        let Ok(vk) = VerifyingKey::from_bytes(&self.pub_key) else {
            return false;
        };
        raw_verify::<Sha512>(&vk, message, &Signature::from_bytes(sig)).is_ok()
    }
}

/// An identity generated on this device (holds the private key).
#[derive(Clone)]
pub struct LocalIdentity {
    pub id: Identity,
    prv_key: [u8; PRV_KEY_SIZE],
}

impl core::fmt::Debug for LocalIdentity {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LocalIdentity").field("id", &self.id).finish_non_exhaustive()
    }
}

fn clamp(mut s: [u8; 32]) -> [u8; 32] {
    s[0] &= 248;
    s[31] &= 63;
    s[31] |= 64;
    s
}

fn derive_pub(prv: &[u8; PRV_KEY_SIZE]) -> [u8; PUB_KEY_SIZE] {
    let scalar: [u8; 32] = prv[..32].try_into().unwrap();
    EdwardsPoint::mul_base_clamped(scalar).compress().to_bytes()
}

/// ECDH between a raw 64-byte private key and an Ed25519 public key.
fn key_exchange(prv: &[u8; PRV_KEY_SIZE], other_pub: &[u8; PUB_KEY_SIZE]) -> Option<Secret> {
    let point = CompressedEdwardsY(*other_pub).decompress()?;
    let scalar: [u8; 32] = prv[..32].try_into().unwrap();
    let secret = point.to_montgomery().mul_clamped(scalar).to_bytes();
    // a point of small order gives all zeros, a "secret" everyone can compute
    (secret != [0; PUB_KEY_SIZE]).then_some(secret)
}

impl LocalIdentity {
    /// `ed25519_create_keypair()`: expand the seed with SHA-512, clamp, derive the public key.
    pub fn from_seed(seed: &[u8; SEED_SIZE]) -> Self {
        let h = Sha512::digest(seed);
        let mut prv = [0u8; PRV_KEY_SIZE];
        prv.copy_from_slice(&h);
        let scalar: [u8; 32] = prv[..32].try_into().unwrap();
        prv[..32].copy_from_slice(&clamp(scalar));
        let pub_key = derive_pub(&prv);
        Self { id: Identity::new(pub_key), prv_key: prv }
    }

    /// Restore from the 64-byte private key (public key re-derived).
    pub fn from_private_key(prv: &[u8; PRV_KEY_SIZE]) -> Self {
        Self { id: Identity::new(derive_pub(prv)), prv_key: *prv }
    }

    pub fn from_keys(prv: &[u8; PRV_KEY_SIZE], pub_key: &[u8; PUB_KEY_SIZE]) -> Self {
        Self { id: Identity::new(*pub_key), prv_key: *prv }
    }

    pub fn pub_key(&self) -> &[u8; PUB_KEY_SIZE] {
        &self.id.pub_key
    }

    pub fn private_key(&self) -> &[u8; PRV_KEY_SIZE] {
        &self.prv_key
    }

    pub fn sign(&self, message: &[u8]) -> [u8; SIGNATURE_SIZE] {
        let esk = ExpandedSecretKey::from_bytes(&self.prv_key);
        let Ok(vk) = VerifyingKey::from_bytes(&self.id.pub_key) else {
            return [0; SIGNATURE_SIZE];
        };
        raw_sign::<Sha512>(&esk, message, &vk).to_bytes()
    }

    /// ECDH shared secret with a peer (`None` if their key is not a valid curve point, or one of
    /// small order: no private key belongs to it).
    pub fn calc_shared_secret(&self, other_pub: &[u8; PUB_KEY_SIZE]) -> Option<Secret> {
        key_exchange(&self.prv_key, other_pub)
    }

    /// Port of `LocalIdentity::validatePrivateKey()` used by `set prv.key`.
    pub fn validate_private_key(prv: &[u8; PRV_KEY_SIZE]) -> bool {
        const TEST_CLIENT_PRV: [u8; 64] = [
            0x70, 0x65, 0xe1, 0x8f, 0xd9, 0xfa, 0xbb, 0x70, 0xc1, 0xed, 0x90, 0xdc, 0xa1, 0x99, 0x07, 0xde,
            0x69, 0x8c, 0x88, 0xb7, 0x09, 0xea, 0x14, 0x6e, 0xaf, 0xd9, 0x3d, 0x9b, 0x83, 0x0c, 0x7b, 0x60,
            0xc4, 0x68, 0x11, 0x93, 0xc7, 0x9b, 0xbc, 0x39, 0x94, 0x5b, 0xa8, 0x06, 0x41, 0x04, 0xbb, 0x61,
            0x8f, 0x8f, 0xd7, 0xa8, 0x4a, 0x0a, 0xf6, 0xf5, 0x70, 0x33, 0xd6, 0xe8, 0xdd, 0xcd, 0x64, 0x71,
        ];
        const TEST_CLIENT_PUB: [u8; 32] = [
            0x1e, 0xc7, 0x71, 0x75, 0xb0, 0x91, 0x8e, 0xd2, 0x06, 0xf9, 0xae, 0x04, 0xec, 0x13, 0x6d, 0x6d,
            0x5d, 0x43, 0x15, 0xbb, 0x26, 0x30, 0x54, 0x27, 0xf6, 0x45, 0xb4, 0x92, 0xe9, 0x35, 0x0c, 0x10,
        ];
        let pub_key = derive_pub(prv);
        if pub_key[0] == 0x00 || pub_key[0] == 0xFF {
            return false;
        }
        let (Some(ss1), Some(ss2)) =
            (key_exchange(prv, &TEST_CLIENT_PUB), key_exchange(&TEST_CLIENT_PRV, &pub_key))
        else {
            return false;
        };
        ss1 == ss2 && ss1.iter().any(|&b| b != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc8032_test1_keypair_and_signature() {
        // RFC 8032 section 7.1, TEST 1 (empty message)
        let seed: [u8; 32] =
            hex::decode("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60").unwrap().try_into().unwrap();
        let id = LocalIdentity::from_seed(&seed);
        assert_eq!(
            hex::encode(id.pub_key()),
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
        );
        let sig = id.sign(b"");
        assert_eq!(
            hex::encode(sig),
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
        );
        assert!(id.id.verify(&sig, b""));
        assert!(!id.id.verify(&sig, b"x"));
    }

    #[test]
    fn test_client_keypair_is_consistent() {
        // the upstream test keypair must re-derive to its public key and validate
        let prv: [u8; 64] = hex::decode(
            "7065e18fd9fabb70c1ed90dca19907de698c88b709ea146eafd93d9b830c7b60c4681193c79bbc39945ba8064104bb618f8fd7a84a0af6f57033d6e8ddcd6471",
        )
        .unwrap()
        .try_into()
        .unwrap();
        let id = LocalIdentity::from_private_key(&prv);
        assert_eq!(
            hex::encode(id.pub_key()),
            "1ec77175b0918ed206f9ae04ec136d6d5d4315bb26305427f645b492e9350c10"
        );
    }

    /// Vectors produced by upstream `lib/ed25519` (orlp, as patched by MeshCore) compiled on a host.
    #[test]
    fn matches_upstream_c_implementation() {
        let seed_a: [u8; 32] = core::array::from_fn(|i| (i * 7 + 1) as u8);
        let seed_b: [u8; 32] = core::array::from_fn(|i| (255 - i * 3) as u8);
        let a = LocalIdentity::from_seed(&seed_a);
        let b = LocalIdentity::from_seed(&seed_b);
        assert_eq!(hex::encode(a.pub_key()), "e4030998cfd5ad1723c169f956aa0b9eb8619b5992bd612c2af428ebc79f8df0");
        assert_eq!(
            hex::encode(a.private_key()),
            "70bf4ad35f7569b718b6bb9fbd57d9df4e28a10b53504d455917a0893257c365bfa30c91fd639026b182f3fdd6cde66667783d8610fcc0082498f12d49e5e54f"
        );
        assert_eq!(hex::encode(b.pub_key()), "1ff449454787a63192c08dbbf6a97edb615aba0bb1618a3ee9b9a55d4631a05d");
        let sig = a.sign(b"MeshCore advert message");
        assert_eq!(
            hex::encode(sig),
            "4f1ff5acd016ed428f957b289d95185389f6878d7e16092b9d95e9321cd137d946df6b1b5eb11710f44e4eed6a2681fdc34a8386387c4469e0f3d1025bab0502"
        );
        let ss = a.calc_shared_secret(b.pub_key()).unwrap();
        assert_eq!(hex::encode(ss), "d8d4bde871ffa6bbd5b6ad71e067ba502546e3d62de63e47cb703f6e47a16658");
    }

    #[test]
    fn keys_nobody_holds_give_no_secret() {
        let a = LocalIdentity::from_seed(&[1; 32]);
        let mut identity = [0u8; PUB_KEY_SIZE];
        identity[0] = 1; // the neutral point, of order 1
        assert_eq!(a.calc_shared_secret(&identity), None);
        let off_curve = (2u8..).map(|k| [k; PUB_KEY_SIZE]).find(|k| CompressedEdwardsY(*k).decompress().is_none());
        assert_eq!(a.calc_shared_secret(&off_curve.unwrap()), None);
    }

    #[test]
    fn ecdh_is_symmetric() {
        let a = LocalIdentity::from_seed(&[1; 32]);
        let b = LocalIdentity::from_seed(&[2; 32]);
        let s1 = a.calc_shared_secret(b.pub_key()).unwrap();
        let s2 = b.calc_shared_secret(a.pub_key()).unwrap();
        assert_eq!(s1, s2);
        assert!(LocalIdentity::validate_private_key(a.private_key()) || a.pub_key()[0] == 0 || a.pub_key()[0] == 0xFF);
    }
}
