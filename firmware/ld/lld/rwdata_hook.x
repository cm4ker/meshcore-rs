/* ../rwdata_hook.x for lld (the RISC-V boards), where `.` inside a section is an address, not an
   offset from the section's start as in GNU ld: the switch tables and constants esp-hal places at
   the head of .data take a room of fixed size, MC_DATA_HEAD, and the statics after them another,
   up to MC_DATA_ROOM, so neither moves what comes after it when it grows. The statics are placed
   here, before esp-hal's own patterns for them, which then find none. See `layout` in the board's
   build.rs. When a part outgrows its room, lld stops with "unable to move location counter
   backward": raise MC_DATA_HEAD or MC_DATA_ROOM there. */
. = _data_start + MC_DATA_HEAD;
*(.sdata .sdata.* .sdata2 .sdata2.*);
*(.data .data.*);
*(.data1)
. = _data_start + MC_DATA_ROOM;
