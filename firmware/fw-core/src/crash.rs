//! Compact crash error-code formatting for boards built with `-C panic=immediate-abort` (where
//! panics trap via Thumb-16 `udf #254` to `HardFault` instead of carrying ~15 KiB of panic text).
//!
//! All formatting runs into a fixed stack buffer (no heap allocation: the heap may be the cause).
//!
//! | Code | Meaning | Example |
//! |------|---------|---------|
//! | `E01` | Rust panic / assert / bounds trap (`udf #254`) | `E01 pc=0003a1c4 lr=0002e819 r123` |
//! | `E02` | Out of heap memory (last failed alloc size in bytes) | `E02 oom=43008 pc=0003a1c4 lr=0002e819 r123` |
//! | `E03` | MemManage fault (`mm=<MMFAR>` or `cf=<CFSR>`) | `E03 pc=0003a1c4 lr=0002e819 mm=20001234 r123` |
//! | `E04` | BusFault (`bf=<BFAR>` or `cf=<CFSR>`) | `E04 pc=0003a1c4 lr=0002e819 bf=40010000 r123` |
//! | `E05` | UsageFault: divide by zero | `E05 pc=0003a1c4 lr=0002e819 r123` |
//! | `E06` | UsageFault: unaligned / invalid instruction or state | `E06 pc=0003a1c4 lr=0002e819 cf=01000000 r123` |
//! | `E07` | HardFault (other / vector fetch) | `E07 pc=0003a1c4 lr=0002e819 hf=40000000 r123` |
//! | `E08` | SoftDevice fault (S140) | `E08 sd=0x1 pc=00012340 info=0x0 r123` |
//!
//! Resolve `pc` and `lr` to file and line against the release ELF (`debug = 2`):
//! `llvm-addr2line -f -i -e <elf> 0x<pc> 0x<lr>`

use core::fmt::{self, Write};

/// Captured ARMv7E-M fault state passed to [`format_fault`].
pub struct FaultRegs {
    pub pc: u32,
    pub lr: u32,
    pub cfsr: u32,
    pub hfsr: u32,
    pub mmfar: u32,
    pub bfar: u32,
    /// `true` when `UNDEFINSTR` fired on LLVM's Thumb-16 trap opcode (`0xDEFE`).
    pub is_trap: bool,
    /// Size of the last failed heap allocation (`0` if none since the last alloc/dealloc).
    pub oom: usize,
}

/// Fixed-capacity UTF-8 writer over a caller-supplied byte buffer (truncates cleanly at capacity).
struct StackBuf<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> StackBuf<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, len: 0 }
    }
}

impl Write for StackBuf<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let avail = self.buf.len() - self.len;
        let mut n = s.len().min(avail);
        while n > 0 && !s.is_char_boundary(n) {
            n -= 1;
        }
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

/// Format a captured fault as a compact error-code line (`E01`–`E07`) into `out`.
pub fn format_fault<'a>(r: &FaultRegs, release: u32, out: &'a mut [u8; 120]) -> &'a str {
    let mut w = StackBuf::new(out);
    if r.is_trap && r.oom != 0 {
        let _ = write!(w, "E02 oom={} pc={:08x} lr={:08x}", r.oom, r.pc, r.lr);
    } else {
        let (code, extra): (&str, Option<(&str, u32)>) = if r.is_trap {
            ("E01", None)
        } else if (r.cfsr & 0x0000_00FF) != 0 {
            let detail = if (r.cfsr & 0x80) != 0 { ("mm", r.mmfar) } else { ("cf", r.cfsr) };
            ("E03", Some(detail))
        } else if (r.cfsr & 0x0000_FF00) != 0 {
            let detail = if (r.cfsr & 0x8000) != 0 { ("bf", r.bfar) } else { ("cf", r.cfsr) };
            ("E04", Some(detail))
        } else if (r.cfsr & 0x0200_0000) != 0 {
            ("E05", None)
        } else if (r.cfsr & 0xFFFF_0000) != 0 {
            ("E06", Some(("cf", r.cfsr)))
        } else {
            ("E07", Some(("hf", r.hfsr)))
        };
        let _ = write!(w, "{code} pc={:08x} lr={:08x}", r.pc, r.lr);
        if let Some((tag, val)) = extra {
            let _ = write!(w, " {tag}={val:08x}");
        }
    }
    if release != 0 {
        let _ = write!(w, " r{release}");
    }
    let len = w.len;
    // SAFETY: `StackBuf` maintains the UTF-8 invariant on `out[..len]`.
    unsafe { core::str::from_utf8_unchecked(&out[..len]) }
}

/// Format a SoftDevice fault (`E08`) into `out`.
pub fn format_softdevice_fault(id: u32, pc: u32, info: u32, release: u32, out: &mut [u8; 120]) -> &str {
    let mut w = StackBuf::new(out);
    let _ = write!(w, "E08 sd={id:#x} pc={pc:08x} info={info:#x}");
    if release != 0 {
        let _ = write!(w, " r{release}");
    }
    let len = w.len;
    // SAFETY: `StackBuf` maintains the UTF-8 invariant on `out[..len]`.
    unsafe { core::str::from_utf8_unchecked(&out[..len]) }
}

/// Read Cortex-M4 SCB fault registers, classify the fault (`E01`–`E07`), and pass the formatted
/// line to `record`. `flash_range` guards instruction inspection at `pc` so a corrupt `pc` can
/// never cause a secondary bus fault.
#[cfg(target_arch = "arm")]
#[inline(never)]
pub fn record_arm_hard_fault(
    pc: u32,
    lr: u32,
    flash_range: core::ops::Range<u32>,
    oom: usize,
    record: impl FnOnce(&str),
) {
    const SCB_CFSR: *const u32 = 0xE000_ED28 as *const u32;
    const SCB_HFSR: *const u32 = 0xE000_ED2C as *const u32;
    const SCB_MMFAR: *const u32 = 0xE000_ED34 as *const u32;
    const SCB_BFAR: *const u32 = 0xE000_ED38 as *const u32;
    const UDF_TRAP: u16 = 0xDEFE; // LLVM Thumb-16 `udf #254` (`core::intrinsics::abort`)

    // SAFETY: ARMv7E-M SCS registers are always mapped and readable in handler mode.
    let (cfsr, hfsr, mmfar, bfar) = unsafe {
        (
            SCB_CFSR.read_volatile(),
            SCB_HFSR.read_volatile(),
            SCB_MMFAR.read_volatile(),
            SCB_BFAR.read_volatile(),
        )
    };
    let is_trap = (cfsr & 0x0001_0000) != 0
        && (pc & 1) == 0
        && flash_range.contains(&pc)
        && unsafe { (pc as *const u16).read_volatile() } == UDF_TRAP;

    let regs = FaultRegs { pc, lr, cfsr, hfsr, mmfar, bfar, is_trap, oom };
    let mut buf = [0u8; 120];
    record(format_fault(&regs, meshcore::release::release_counter(), &mut buf));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> FaultRegs {
        FaultRegs { pc: 0x0003_a1c4, lr: 0x0002_e819, cfsr: 0, hfsr: 0, mmfar: 0, bfar: 0, is_trap: false, oom: 0 }
    }

    #[test]
    fn classifies_all_crash_codes() {
        let mut buf = [0u8; 120];

        let r = FaultRegs { is_trap: true, cfsr: 0x0001_0000, ..base() };
        assert_eq!(format_fault(&r, 123, &mut buf), "E01 pc=0003a1c4 lr=0002e819 r123");

        let r = FaultRegs { is_trap: true, cfsr: 0x0001_0000, oom: 43008, ..base() };
        assert_eq!(format_fault(&r, 123, &mut buf), "E02 oom=43008 pc=0003a1c4 lr=0002e819 r123");

        let r = FaultRegs { cfsr: 0x82, mmfar: 0x2000_1234, ..base() };
        assert_eq!(format_fault(&r, 0, &mut buf), "E03 pc=0003a1c4 lr=0002e819 mm=20001234");

        let r = FaultRegs { cfsr: 0x02, ..base() };
        assert_eq!(format_fault(&r, 0, &mut buf), "E03 pc=0003a1c4 lr=0002e819 cf=00000002");

        let r = FaultRegs { cfsr: 0x8200, bfar: 0x4001_0000, ..base() };
        assert_eq!(format_fault(&r, 5, &mut buf), "E04 pc=0003a1c4 lr=0002e819 bf=40010000 r5");

        let r = FaultRegs { cfsr: 0x0200_0000, ..base() };
        assert_eq!(format_fault(&r, 0, &mut buf), "E05 pc=0003a1c4 lr=0002e819");

        let r = FaultRegs { cfsr: 0x0100_0000, ..base() };
        assert_eq!(format_fault(&r, 0, &mut buf), "E06 pc=0003a1c4 lr=0002e819 cf=01000000");

        let r = FaultRegs { hfsr: 0x4000_0000, ..base() };
        assert_eq!(format_fault(&r, 0, &mut buf), "E07 pc=0003a1c4 lr=0002e819 hf=40000000");

        assert_eq!(format_softdevice_fault(1, 0x0001_2340, 0, 9, &mut buf), "E08 sd=0x1 pc=00012340 info=0x0 r9");
    }
}
