//! Symmetric crypto exactly as MeshCore does it:
//! AES-128-ECB with zero padding, then a truncated HMAC-SHA256 over the ciphertext.
//!
//! The 32-byte shared secret is used whole as the HMAC key; its first 16 bytes are the AES key.

use aes::Aes128;
use aes::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::{CIPHER_BLOCK_SIZE, CIPHER_KEY_SIZE, CIPHER_MAC_SIZE, PUB_KEY_SIZE};

pub type Secret = [u8; PUB_KEY_SIZE];

/// SHA-256 over the concatenation of `parts`, truncated into `out`.
pub fn sha256(out: &mut [u8], parts: &[&[u8]]) {
    let mut sha = Sha256::new();
    for p in parts {
        sha.update(p);
    }
    let digest = sha.finalize();
    let n = out.len().min(32);
    out[..n].copy_from_slice(&digest[..n]);
}

/// HMAC-SHA256 over the concatenation of `parts`, truncated into `out`.
pub fn hmac_sha256(key: &[u8], out: &mut [u8], parts: &[&[u8]]) {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC accepts any key length");
    for p in parts {
        mac.update(p);
    }
    let tag = mac.finalize().into_bytes();
    let n = out.len().min(32);
    out[..n].copy_from_slice(&tag[..n]);
}

fn cipher(secret: &[u8]) -> Aes128 {
    Aes128::new_from_slice(&secret[..CIPHER_KEY_SIZE]).expect("16-byte key")
}

/// Length of ciphertext produced for `len` plaintext bytes.
pub const fn padded_len(len: usize) -> usize {
    len.div_ceil(CIPHER_BLOCK_SIZE) * CIPHER_BLOCK_SIZE
}

/// AES-128-ECB encrypt, final block zero-padded. Returns bytes written (multiple of 16).
pub fn encrypt(secret: &[u8], dest: &mut [u8], src: &[u8]) -> usize {
    let aes = cipher(secret);
    let mut written = 0;
    for chunk in src.chunks(CIPHER_BLOCK_SIZE) {
        let mut block = [0u8; CIPHER_BLOCK_SIZE];
        block[..chunk.len()].copy_from_slice(chunk);
        let mut b = aes::Block::from(block);
        aes.encrypt_block(&mut b);
        dest[written..written + CIPHER_BLOCK_SIZE].copy_from_slice(&b);
        written += CIPHER_BLOCK_SIZE;
    }
    written
}

/// AES-128-ECB decrypt of whole blocks. Returns bytes written (trailing padding is kept).
pub fn decrypt(secret: &[u8], dest: &mut [u8], src: &[u8]) -> usize {
    let aes = cipher(secret);
    let mut written = 0;
    for chunk in src.as_chunks::<CIPHER_BLOCK_SIZE>().0 {
        let mut block = [0u8; CIPHER_BLOCK_SIZE];
        block.copy_from_slice(chunk);
        let mut b = aes::Block::from(block);
        aes.decrypt_block(&mut b);
        dest[written..written + CIPHER_BLOCK_SIZE].copy_from_slice(&b);
        written += CIPHER_BLOCK_SIZE;
    }
    written
}

/// Encrypt `src`, prefixing `dest` with a 2-byte MAC of the ciphertext. Returns total length.
pub fn encrypt_then_mac(secret: &Secret, dest: &mut [u8], src: &[u8]) -> usize {
    let enc_len = encrypt(secret, &mut dest[CIPHER_MAC_SIZE..], src);
    let (mac, ct) = dest.split_at_mut(CIPHER_MAC_SIZE);
    hmac_sha256(secret, mac, &[&ct[..enc_len]]);
    CIPHER_MAC_SIZE + enc_len
}

/// Check the leading MAC, then decrypt. Returns decrypted length (zero-padded), or `None`.
pub fn mac_then_decrypt(secret: &Secret, dest: &mut [u8], src: &[u8]) -> Option<usize> {
    if src.len() <= CIPHER_MAC_SIZE {
        return None;
    }
    let ct = &src[CIPHER_MAC_SIZE..];
    if !ct.len().is_multiple_of(CIPHER_BLOCK_SIZE) {
        return None;
    }
    let mut mac = [0u8; CIPHER_MAC_SIZE];
    hmac_sha256(secret, &mut mac, &[ct]);
    if mac != src[..CIPHER_MAC_SIZE] {
        return None;
    }
    Some(decrypt(secret, dest, ct))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes_matches_fips197() {
        // FIPS-197 appendix C.1
        let key = hex::decode("000102030405060708090a0b0c0d0e0f").unwrap();
        let pt = hex::decode("00112233445566778899aabbccddeeff").unwrap();
        let mut ct = [0u8; 16];
        assert_eq!(encrypt(&key, &mut ct, &pt), 16);
        assert_eq!(hex::encode(ct), "69c4e0d86a7b0430d8cdb78070b4c55a");
        let mut back = [0u8; 16];
        decrypt(&key, &mut back, &ct);
        assert_eq!(&back[..], &pt[..]);
    }

    #[test]
    fn hmac_matches_rfc4231() {
        // RFC 4231 test case 2
        let mut out = [0u8; 32];
        hmac_sha256(b"Jefe", &mut out, &[b"what do ya want ", b"for nothing?"]);
        assert_eq!(
            hex::encode(out),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn encrypt_then_mac_roundtrip() {
        let secret: Secret = core::array::from_fn(|i| i as u8 * 7);
        let msg = b"hello mesh, this is longer than one block";
        let mut buf = [0u8; 128];
        let n = encrypt_then_mac(&secret, &mut buf, msg);
        assert_eq!(n, 2 + padded_len(msg.len()));
        let mut out = [0u8; 128];
        let m = mac_then_decrypt(&secret, &mut out, &buf[..n]).unwrap();
        assert_eq!(&out[..msg.len()], msg);
        assert!(out[msg.len()..m].iter().all(|&b| b == 0));

        buf[5] ^= 1;
        assert!(mac_then_decrypt(&secret, &mut out, &buf[..n]).is_none());
    }
}
