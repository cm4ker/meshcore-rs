/* The installer lives between the application and the Adafruit bootloader (from 0xF4000), in the
   28 KiB the bootloader leaves for an Arduino file system, which this firmware does not use. It
   runs instead of the application (the application's first page then points here), so the RAM
   is all its own apart from the MBR's first 8 bytes. */
MEMORY
{
  FLASH : ORIGIN = 0x000ED000, LENGTH = 0xF4000 - 0xED000
  RAM   : ORIGIN = 0x20000008, LENGTH = 256K - 8
}
