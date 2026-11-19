/* Heltec T096, Adafruit UF2 bootloader and S140 6.1.1.
   0x26000..0x84000: Companion image including the 64-byte NRF trailer.
   0x84000..0x8C000: 32 KiB of working space for in-place updates and rollback.
   The rest of the layout is in ../promicro/ota_layout.rs. */
MEMORY
{
  FLASH : ORIGIN = 0x00026000, LENGTH = 0x84000 - 0x26000 - 64
  RAM   : ORIGIN = 0x20004000, LENGTH = 256K - 16K
}
