/* ../rwtext_hook.x for lld (the RISC-V boards), where `.` inside a section is an address, not an
   offset from the section's start as in GNU ld: the code in RAM takes a room of fixed size,
   MC_RWTEXT_ROOM, so the data after it stays where it is when the code grows. See `layout` in the
   board's build.rs. When the code outgrows the room, lld stops with "unable to move location
   counter backward": raise MC_RWTEXT_ROOM there. */
. = _rwtext_start + MC_RWTEXT_ROOM;
