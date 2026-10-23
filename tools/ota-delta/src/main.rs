//! Make a delta firmware update for LoRa OTA: a bsdiff patch from the image running on the
//! repeater to the new one, in the format the firmware applies (`meshcore::ota`). The sender
//! (`tools/ota-lora/meshcore_ota.py`) zlib-compresses it.
//!
//!     ota-delta <base.bin> <new.bin> <patch.out>

use std::process::exit;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [base, new, out] = args.as_slice() else {
        eprintln!("usage: ota-delta <base.bin> <new.bin> <patch.out>");
        exit(2);
    };
    let read = |p: &str| {
        std::fs::read(p).unwrap_or_else(|e| {
            eprintln!("{p}: {e}");
            exit(1)
        })
    };
    let (base, new) = (read(base), read(new));
    let mut patch = Vec::new();
    bsdiff::diff(&base, &new, &mut patch).expect("diff");
    // self-check before anything goes on the air
    let mut check = Vec::with_capacity(new.len());
    bsdiff::patch(&base, &mut patch.as_slice(), &mut check).expect("patch");
    assert!(check == new, "patch does not reproduce the new image");
    std::fs::write(out, &patch).unwrap_or_else(|e| {
        eprintln!("{out}: {e}");
        exit(1)
    });
    println!("{} -> {} bytes: patch {} bytes", base.len(), new.len(), patch.len());
}
