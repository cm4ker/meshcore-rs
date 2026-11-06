/* nRF52840 Pro Micro (nice!nano-style) with its Adafruit UF2 bootloader and SoftDevice S140 6.1.1
   left in place: the MBR and the SoftDevice take the flash below 0x26000 and the bootloader the
   top (from 0xF4000). In between (src/ota.rs, installer/):
     0x26000..0x7B000  the application (this), 340 KiB
     0x7B000..0xD6000  an update being received (staging); once installed, the firmware it
                       replaced, to go back to. The companion keeps its data here, up to 0xD1000
                       (src/lib.rs `COMPANION_REGION`), and lends it to its updates: the data
                       waits in 0xD1000..0xD6000 and upstream's file system meanwhile
     0xD6000..0xD8000  the node's sensors (src/lib.rs `DATA_OVERFLOW`; staging before they came)
     0xD8000..0xD9000  the app's first page while the installer swaps the two
     0xD9000..0xDA000  the install request
     0xDA000..0xEA000  the node's data (src/lib.rs `DATA_REGION`), up to where the bootloader
                       stops taking UF2 blocks
     0xEA000..0xED000  the update installer
     0xED000..0xF4000  upstream's file system, left as it is by the repeater: upstream's
                       firmware finds its identity and prefs again if it goes back on
   The SoftDevice runs while a BLE update window is open, and all the time in the companion
   (src/sd.rs); the RAM below 0x20004000 is its own then (the MBR's 8 bytes and the SoftDevice's
   tables), and is left alone otherwise too: the SoftDevice keeps its interrupt forwarding there
   while it is disabled. */
MEMORY
{
  FLASH : ORIGIN = 0x00026000, LENGTH = 0x7B000 - 0x26000
  RAM   : ORIGIN = 0x20004000, LENGTH = 256K - 16K
}
