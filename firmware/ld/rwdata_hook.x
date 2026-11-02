/* Included in esp-hal's .data (ESP_HAL_CONFIG_USE_RWDATA_LD_HOOK), right after the switch tables
   and constants it places there: they take a room of fixed size, MC_DATA_HEAD, and the statics after
   them another, up to MC_DATA_ROOM, so neither moves what comes after it when it grows. The statics
   are placed here, before esp-hal's own patterns for them, which then find none. See `layout` in
   build.rs. (A value given to `.` here is an offset from the section's start, where _data_start is.)
   When a part outgrows its room, ld stops at the line that ends it with "cannot move location
   counter backwards": raise MC_DATA_HEAD or MC_DATA_ROOM in build.rs. */
. = MC_DATA_HEAD;
*(.sdata .sdata.* .sdata2 .sdata2.*);
*(.data .data.*);
*(.data1)
. = MC_DATA_ROOM;
