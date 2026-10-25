use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    emit_build_time();
    // memory.x next to the crate root, for cortex-m-rt's link.x
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    std::fs::copy("memory.x", out.join("memory.x")).unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rustc-link-arg=-Tlink.x");
    // the build stamp follows every change of the firmware, as without any rerun-if-changed
    for f in ["memory.x", "src", "Cargo.toml", "build.rs"] {
        println!("cargo:rerun-if-changed={f}");
    }
    build_installer(&out);
}

/// The update installer (installer/), built as its own program and carried as a flat image
/// (src/ota.rs puts it in place before an update).
fn build_installer(out: &Path) {
    for f in ["installer/src/main.rs", "installer/Cargo.toml", "installer/memory.x", "installer/build.rs"] {
        println!("cargo:rerun-if-changed={f}");
    }
    let target_dir = out.join("installer-target");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.current_dir("installer")
        .args(["build", "--release", "--target", "thumbv7em-none-eabihf", "--target-dir"])
        .arg(&target_dir);
    // our own build's settings are not the installer's
    for (k, _) in std::env::vars() {
        if k.starts_with("CARGO_") && k != "CARGO_HOME" || k == "RUSTFLAGS" || k.starts_with("RUSTC") {
            cmd.env_remove(k);
        }
    }
    let status = cmd.status().expect("cargo for the installer");
    assert!(status.success(), "building the installer failed");
    let elf = std::fs::read(target_dir.join("thumbv7em-none-eabihf/release/installer")).unwrap();
    let (start, image) = flat_image(&elf);
    assert_eq!(start, 0xED000, "the installer must start at 0xED000 (src/ota.rs)");
    assert!(image.len() <= 0xF4000 - 0xED000, "the installer does not fit below the bootloader");
    std::fs::write(out.join("installer.bin"), image).unwrap();
}

/// The loadable segments of a little-endian ELF32 as one image from the lowest address, gaps
/// filled with 0xFF.
fn flat_image(elf: &[u8]) -> (u32, Vec<u8>) {
    let u16_at = |i: usize| u16::from_le_bytes([elf[i], elf[i + 1]]) as usize;
    let u32_at = |i: usize| u32::from_le_bytes(elf[i..i + 4].try_into().unwrap());
    assert_eq!(&elf[..6], b"ELF", "a little-endian ELF32");
    let (phoff, phentsize, phnum) = (u32_at(28) as usize, u16_at(42), u16_at(44));
    let mut segs = Vec::new();
    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        let (kind, offset, paddr, filesz) = (u32_at(ph), u32_at(ph + 4) as usize, u32_at(ph + 12), u32_at(ph + 16) as usize);
        if kind == 1 && filesz > 0 {
            segs.push((paddr, &elf[offset..offset + filesz]));
        }
    }
    let start = segs.iter().map(|s| s.0).min().unwrap();
    let end = segs.iter().map(|s| s.0 + s.1.len() as u32).max().unwrap();
    let mut image = vec![0xFF; (end - start) as usize];
    for (addr, data) in segs {
        let at = (addr - start) as usize;
        image[at..at + data.len()].copy_from_slice(data);
    }
    (start, image)
}

/// Build timestamp: seeds the wall clock (adverts must carry increasing timestamps) and `ver`.
fn emit_build_time() {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86400) as i64;
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    println!("cargo:rustc-env=BUILD_EPOCH={secs}");
    let (hh, mm) = ((secs % 86400) / 3600, (secs % 3600) / 60);
    println!("cargo:rustc-env=BUILD_DATE={d} {} {y} {hh:02}:{mm:02}", MONTHS[(m - 1) as usize]);
}
