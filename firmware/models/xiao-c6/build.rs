fn main() {
    emit_build_time();
    // the build stamp follows every change of the firmware, as without any rerun-if-changed
    for f in ["src", "Cargo.toml", "build.rs"] {
        println!("cargo:rerun-if-changed={f}");
    }
    // make sure linkall.x is the last linker script
    println!("cargo:rustc-link-arg=-Tlinkall.x");
}

/// Build timestamp: seeds the wall clock (adverts must carry increasing timestamps) and `ver`.
/// `SOURCE_DATE_EPOCH` fixes it, as for reproducible builds: CI builds each image once more with a
/// fixed date and no release number, and the hash of that one changes only with the code.
fn emit_build_time() {
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    let secs = match std::env::var("SOURCE_DATE_EPOCH") {
        Ok(s) => s.parse().expect("SOURCE_DATE_EPOCH: seconds since 1970"),
        Err(_) => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
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
