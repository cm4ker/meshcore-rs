/* nRF52840 Pro Micro (nice!nano-style) with its Adafruit UF2 bootloader and SoftDevice S140 6.1.1
   left in place: the MBR and the SoftDevice take the flash below 0x26000 and the bootloader the
   top (from 0xF4000). The application lives in between, up to the node's data at 0xDA000
   (src/lib.rs `DATA_REGION`, 64 KiB, ending where the bootloader stops taking UF2 blocks);
   0xED000..0xF4000 stays as it is, so upstream's file system (identity, prefs) is still there
   if its firmware goes back on. The SoftDevice is never enabled,
   so the RAM is ours apart from the MBR's first 8 bytes. */
MEMORY
{
  FLASH : ORIGIN = 0x00026000, LENGTH = 0xDA000 - 0x26000
  RAM   : ORIGIN = 0x20000008, LENGTH = 256K - 8
}
