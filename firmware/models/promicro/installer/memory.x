/* The installer lives in the 12 KiB between the node's data (ending at 0xEA000, where the Adafruit
   bootloader stops taking UF2 blocks) and upstream's file system (from 0xED000), which stays as
   it is. The firmware puts it there itself. It runs instead of the application (the
   application's first page then points here), so the RAM is all its own apart from the MBR's
   first 8 bytes. */
MEMORY
{
  FLASH : ORIGIN = 0x000EA000, LENGTH = 0xED000 - 0xEA000
  RAM   : ORIGIN = 0x20000008, LENGTH = 256K - 8
}
