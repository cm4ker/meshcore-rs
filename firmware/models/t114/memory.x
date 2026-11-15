/* Heltec T114: the Pro Micro's MBR, SoftDevice S140 6.1.1 and Adafruit UF2 bootloader left in
   place, and from 0xD6000 on the Pro Micro's layout (../promicro/memory.x). Updates are applied in
   place (src/ota.rs), so the app has the room the Pro Micro stages them in:
     0x26000..0x99000  the application (this), 460 KiB: an image of up to 428 KiB, and the 32 KiB
                       the installer moves the old one up by
     0x99000..0xD6000  an update's plan; the companion keeps its data there up to 0xD1000, and
                       lends it to its updates
     0xD6000..0xEA000  the sensors, the app's first page while installing, the install request,
                       the node's data
     0xEA000..0xED000  the update installer (installer/)
     0xED000..0xF4000  upstream's file system, which the companion's data waits in during an
                       update */
MEMORY
{
  FLASH : ORIGIN = 0x00026000, LENGTH = 0x99000 - 0x26000
  RAM   : ORIGIN = 0x20004000, LENGTH = 256K - 16K
}
