//! Shared flash addresses for the running firmware and its in-place installer.

pub const APP_START: u32 = 0x26000;
pub const SHIFT: u32 = 32 * 1024;
const STOCK_COMPANION_FS: bool = cfg!(any(feature = "t096", feature = "mesh-pocket"));
pub const APP_END: u32 = if STOCK_COMPANION_FS { 0x8C000 } else { 0x8F000 };
pub const STAGING: u32 = if STOCK_COMPANION_FS { 0x9C000 } else { APP_END };
pub const STAGING_END: u32 = if STOCK_COMPANION_FS { 0xD4000 } else { 0xD6000 };
pub const SAVED_FIRST: u32 = if STOCK_COMPANION_FS { 0x8E000 } else { 0xD8000 };
pub const RECORD: u32 = SAVED_FIRST + 0x1000;
pub const INSTALLER: u32 = if STOCK_COMPANION_FS { APP_END } else { 0xEA000 };
pub const INSTALLER_END: u32 = if STOCK_COMPANION_FS { SAVED_FIRST } else { 0xED000 };

// On T096 and MeshPocket everything fits below the stock Companion filesystem at 0xD4000.
// The companion data starts at the same address as in the USB-only build.
#[cfg(any(feature = "t096", feature = "mesh-pocket"))]
pub const COMPANION_REGION: (u32, u32) = (STAGING, STAGING_END - STAGING);
#[cfg(any(feature = "t096", feature = "mesh-pocket"))]
pub const TRIAL_REGION: (u32, u32) = (RECORD + 0x1000, 0x2000);
#[cfg(any(feature = "t096", feature = "mesh-pocket"))]
pub const COMPANION_ASIDE: [(u32, u32); 2] = [
    (TRIAL_REGION.0 + TRIAL_REGION.1, STAGING - TRIAL_REGION.0 - TRIAL_REGION.1),
    (STAGING, 0),
];
