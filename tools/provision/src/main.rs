//! Build an image of the firmware's `meshcore` data partition, so a node boots straight into a
//! given identity and configuration (e.g. when moving an existing node to this firmware).
//!
//! ```text
//! provision --prv-file <key.txt> --prefs <prefs.txt> --out <image.bin>
//!           [--admin <64-hex>]... [--clock <epoch-secs>]
//! ```
//! `key.txt` holds the private key, 128 hex chars. `--prv <128-hex>` takes it on the command line,
//! which other users of the machine see in the process list and the shell keeps in its history.
//! `prefs.txt` uses the firmware's `key=value` prefs format (see `meshcore::prefs`).
//! Flash with: `espflash write-bin 0x400000 <image.bin>`.
//!
//! The record layout must match `firmware/fw-core/src/storage.rs`.

use std::process::exit;

use meshcore::identity::LocalIdentity;
use meshcore::prefs::NodePrefs;
use meshcore::util::{from_hex, to_hex};

const PARTITION_SIZE: usize = 0x10000;
const SECTOR: usize = 4096;
const MAGIC: u32 = 0x5352_434D; // "MCRS"

// blob slots, as in storage.rs
const BLOB_IDENTITY: usize = 0;
const BLOB_PREFS: usize = 1;
const BLOB_ACL: usize = 2;
const BLOB_CLOCK: usize = 3;

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// Write a record with sequence 1 into the first sector of `blob`.
fn put_record(image: &mut [u8], blob: usize, data: &[u8]) {
    assert!(data.len() <= SECTOR - 16, "blob too large");
    let off = blob * 2 * SECTOR;
    let rec = &mut image[off..off + SECTOR];
    rec[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    rec[4..8].copy_from_slice(&1u32.to_le_bytes());
    rec[8..12].copy_from_slice(&(data.len() as u32).to_le_bytes());
    rec[12..16].copy_from_slice(&crc32(data).to_le_bytes());
    rec[16..16 + data.len()].copy_from_slice(data);
}

fn die(msg: &str) -> ! {
    eprintln!("error: {msg}");
    exit(2)
}

fn main() {
    let mut prv_hex = None;
    let mut prefs_path = None;
    let mut out = None;
    let mut admins = Vec::new();
    let mut clock: Option<u32> = None;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| die(&format!("{a} needs a value")));
        match a.as_str() {
            "--prv" => prv_hex = Some(val()),
            "--prv-file" => {
                let path = val();
                prv_hex = Some(std::fs::read_to_string(&path).unwrap_or_else(|e| die(&format!("{path}: {e}"))));
            }
            "--prefs" => prefs_path = Some(val()),
            "--out" => out = Some(val()),
            "--admin" => admins.push(val()),
            "--clock" => clock = Some(val().parse().unwrap_or_else(|_| die("--clock must be epoch seconds"))),
            _ => die(&format!("unknown argument {a}")),
        }
    }
    let prv_hex = prv_hex.unwrap_or_else(|| die("--prv-file (or --prv) is required"));
    let out = out.unwrap_or_else(|| die("--out is required"));

    let mut prv = [0u8; 64];
    if !from_hex(&mut prv, prv_hex.trim()) {
        die("--prv must be 128 hex chars");
    }
    if !LocalIdentity::validate_private_key(&prv) {
        die("private key failed validation");
    }
    let id = LocalIdentity::from_private_key(&prv);

    let mut image = vec![0xFFu8; PARTITION_SIZE];

    let mut identity = prv.to_vec();
    identity.extend_from_slice(id.pub_key());
    put_record(&mut image, BLOB_IDENTITY, &identity);

    if let Some(path) = prefs_path {
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| die(&format!("{path}: {e}")));
        // normalise through the parser so the firmware sees sanitised values
        let prefs = NodePrefs::deserialize(&text);
        put_record(&mut image, BLOB_PREFS, prefs.serialize().as_bytes());
        println!(
            "prefs: name '{}', {} MHz BW{} SF{} CR{}, tx {} dBm",
            prefs.node_name, prefs.radio.freq_mhz, prefs.radio.bw_khz, prefs.radio.sf, prefs.radio.cr, prefs.tx_power_dbm
        );
    }

    if !admins.is_empty() {
        // `/s_contacts` record: pubkey(32) perms(1) sync_since(4) unused(2) out_path_len(1) out_path(64) secret(32)
        let mut acl = Vec::new();
        for hex in &admins {
            let mut key = [0u8; 32];
            if !from_hex(&mut key, hex.trim()) {
                die("--admin must be 64 hex chars");
            }
            acl.extend_from_slice(&key);
            acl.push(3); // PERM_ACL_ADMIN
            acl.extend_from_slice(&[0; 6]);
            acl.push(0xFF); // out path unknown
            acl.extend_from_slice(&[0; 64]);
            acl.extend_from_slice(&[0; 32]); // shared secret: recalculated at boot
        }
        put_record(&mut image, BLOB_ACL, &acl);
        println!("ACL: {} admin(s)", admins.len());
    }

    if let Some(t) = clock {
        put_record(&mut image, BLOB_CLOCK, &t.to_le_bytes());
        println!("clock: {t}");
    }

    std::fs::write(&out, &image).unwrap_or_else(|e| die(&format!("{out}: {e}")));
    println!("node ID: {}", to_hex(id.pub_key()));
    println!("wrote {out} ({} bytes) -> flash at 0x400000", image.len());
}
