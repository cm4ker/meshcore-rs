/* Included at the end of esp-hal's .rwtext (ESP_HAL_CONFIG_USE_RWTEXT_LD_HOOK): the code in RAM
   takes a room of fixed size, MC_RWTEXT_ROOM, so the data after it stays where it is when the code
   grows. See `layout` in build.rs. (A value given to `.` here is an offset from the section's start,
   where _rwtext_start is.) When the code outgrows the room, ld stops at the next line with "cannot
   move location counter backwards": raise MC_RWTEXT_ROOM in build.rs. */
. = MC_RWTEXT_ROOM;
