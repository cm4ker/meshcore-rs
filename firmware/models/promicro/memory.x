/* nRF52840 Pro Micro (nice!nano-style) with its Adafruit UF2 bootloader and SoftDevice S140 6.1.1
   left in place: the MBR and the SoftDevice take the flash below 0x26000 and the bootloader the
   top (from 0xF4000). In between (src/ota.rs, installer/):
     0x26000..0x7B000  the application (this), 340 KiB
     0x7B000..0xD9000  an update being received (staging), then the install request at 0xD9000
     0xDA000..0xEA000  the node's data (src/lib.rs `DATA_REGION`), up to where the bootloader
                       stops taking UF2 blocks
     0xEA000..0xED000  the update installer
     0xED000..0xF4000  upstream's file system, left as it is: upstream's firmware finds its
                       identity and prefs again if it goes back on
   The SoftDevice is never enabled, so the RAM is ours apart from the MBR's first 8 bytes. */
MEMORY
{
  FLASH : ORIGIN = 0x00026000, LENGTH = 0x7B000 - 0x26000
  RAM   : ORIGIN = 0x20000008, LENGTH = 256K - 8
}
