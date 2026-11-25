/* Heltec T114: the Pro Micro's MBR, SoftDevice S140 6.1.1 and Adafruit UF2 bootloader left in
   place, and the Pro Micro's layout (../promicro/memory.x), updates applied in place
   (../promicro/src/ota.rs):
     0x26000..0x8F000  the application (this), 420 KiB: an image of up to 388 KiB, and the 32 KiB
                       the installer moves the old one up by
     0x8F000..0xD6000  an update's plan and the plan back; the companion keeps its data there up
                       to 0xD1000, and lends it to its updates
     0xD6000..0xEA000  the sensors, the app's first page while installing, the install request,
                       the node's data
     0xEA000..0xED000  the update installer (../promicro/installer/)
     0xED000..0xF4000  upstream's file system, which the companion's data waits in during an
                       update */
MEMORY
{
  FLASH : ORIGIN = 0x00026000, LENGTH = 0x8F000 - 0x26000
  RAM   : ORIGIN = 0x20004000, LENGTH = 256K - 16K
}

SECTIONS
{
  .mcrs : ALIGN(4)
  {
    *(.mcrs .mcrs.*);
  } > FLASH
} INSERT AFTER .rodata;
