#[allow(dead_code)]
#[path = "../ota_layout.rs"]
mod ota_layout;

fn main() {
    // memory.x next to the crate root, for cortex-m-rt's link.x
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let template = include_str!("memory.x.in");
    let memory = template.replace("@FLASH_ORIGIN@", &format!("{:#x}", ota_layout::INSTALLER))
        .replace("@FLASH_LENGTH@", &(ota_layout::INSTALLER_END - ota_layout::INSTALLER).to_string());
    std::fs::write(out.join("memory.x"), memory).unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rustc-link-arg-bins=-Tlink.x");
    println!("cargo:rerun-if-changed=memory.x.in");
    println!("cargo:rerun-if-changed=../ota_layout.rs");
}
