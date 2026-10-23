//! The flash store (`meshcore_fw_core::storage`) on an ESP32: in the `meshcore` data partition,
//! plus the companion role's `companion` partition when the partition table has one.

use esp_bootloader_esp_idf::partitions;
use esp_storage::FlashStorage;
pub use meshcore_fw_core::storage::*;

pub const PARTITION_LABEL: &str = "meshcore";
pub const COMPANION_LABEL: &str = "companion";

pub type EspStore<'d> = Store<FlashStorage<'d>>;

/// Locate the partitions and open the store; `None` without a `meshcore` partition.
pub fn open(mut flash: FlashStorage<'_>) -> Option<EspStore<'_>> {
    let mut buf = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
    let table = partitions::read_partition_table(&mut flash, &mut buf).ok()?;
    let entry = table.iter().find(|p| p.label_as_str() == PARTITION_LABEL)?;
    let data = (entry.offset(), entry.len());
    let comp = table.iter().find(|p| p.label_as_str() == COMPANION_LABEL).map(|p| (p.offset(), p.len()));
    Some(Store::with_regions(flash, data, comp))
}
