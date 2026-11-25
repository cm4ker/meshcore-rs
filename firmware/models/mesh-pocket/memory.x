/* Stock S140 6.1.1 stays below the application. MeshCore's stock Companion
   filesystem starts at 0xD4000. The Rust program and its data fit below it. */
MEMORY
{
  FLASH : ORIGIN = 0x00026000, LENGTH = 0x8C000 - 0x26000
  RAM   : ORIGIN = 0x20004000, LENGTH = 256K - 16K
}

SECTIONS
{
  .mcrs : ALIGN(4)
  {
    *(.mcrs .mcrs.*);
  } > FLASH
} INSERT AFTER .rodata;
