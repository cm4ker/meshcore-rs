/* LilyGO T-Echo (nRF52840) with its Adafruit UF2 bootloader and SoftDevice S140 6.1.1 left in
   place: the MBR and the SoftDevice take the flash below 0x26000 and the bootloader the top
   (from 0xF4000); the application lives in between, up to the update installer at 0xED000
   (installer/, src/ota.rs). The SoftDevice is never enabled (the BLE controller is Nordic's
   SDC), so the RAM is ours apart from the MBR's first 8 bytes. */
MEMORY
{
  FLASH : ORIGIN = 0x00026000, LENGTH = 0xED000 - 0x26000
  RAM   : ORIGIN = 0x20000008, LENGTH = 256K - 8
}
