//! Nordic's SoftDevice S140 6.1.1, which the Adafruit bootloader keeps in the flash below the app
//! (memory.x). The repeater runs it only while a BLE update window is open (`ble`), the companion
//! all the time: [`enable`] and [`disable`]. While it runs it owns the radio, the clocks, the TEMP
//! and RNG peripherals and the flash controller: flash writes go through it ([`flash_erase`],
//! [`flash_write`]), the temperature is asked of it ([`temp_c`]), nothing may write the POWER
//! registers, and VBUS comes as its events ([`usb_power_events`]).
//!
//! Its functions are supervisor calls into it (numbers and structures from its 6.1.1 headers,
//! `nrf_sdm.h`, `nrf_soc.h`, `ble*.h`). Calls go through the MBR's vector table: while the
//! SoftDevice is off, interrupts come straight to the app's own (cortex-m-rt `set-vtor`).
//!
//! The critical sections here mask the application's interrupts (priority 2 and lower, which all
//! of ours are) by BASEPRI: the SoftDevice's own (0, 1) must run on time, or it asserts. No call
//! into the SoftDevice may come from inside one: it takes calls at a priority BASEPRI holds, and
//! the call becomes a hard fault (the first `fw ble` on Wan10 rebooted it so).

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use embassy_nrf::interrupt::{self, InterruptExt, Priority};
use embassy_nrf::pac;
use embassy_nrf::usb::vbus_detect::SoftwareVbusDetect;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant};

/// Where the app's vector table is (memory.x), for the SoftDevice to forward interrupts to.
const APP_VECTORS: u32 = 0x26000;
/// The SoftDevice's info structure: MBR size + `SOFTDEVICE_INFO_STRUCT_OFFSET`.
const INFO: u32 = 0x3000;
const INFO_MAGIC: u32 = 0x51B1_E5DB;
/// `sd_fwid` of S140 6.1.1, the one the board's bootloader ships with.
const FWID_S140_6_1_1: u16 = 0x00B6;

/// Interrupt priority bits of the nRF52840 (`__NVIC_PRIO_BITS`): BASEPRI masks priority 2 and lower.
const APP_MASK: u8 = 2 << 5;

pub const NRF_SUCCESS: u32 = 0;
pub const NRF_ERROR_NOT_FOUND: u32 = 5;
pub const NRF_ERROR_NO_MEM: u32 = 4;
pub const NRF_ERROR_RESOURCES: u32 = 19;
const NRF_ERROR_SOC_RAND_NOT_ENOUGH_VALUES: u32 = 0x2007;
const NRF_EVT_FLASH_OPERATION_SUCCESS: u32 = 2;
const NRF_EVT_FLASH_OPERATION_ERROR: u32 = 3;
const NRF_EVT_POWER_USB_POWER_READY: u32 = 9;
const NRF_EVT_POWER_USB_DETECTED: u32 = 10;
const NRF_EVT_POWER_USB_REMOVED: u32 = 11;

// supervisor call numbers
const SD_SOFTDEVICE_ENABLE: u8 = 0x10;
const SD_SOFTDEVICE_DISABLE: u8 = 0x11;
const SD_SOFTDEVICE_VECTOR_TABLE_BASE_SET: u8 = 0x13;
const SD_FLASH_PAGE_ERASE: u8 = 0x28;
const SD_FLASH_WRITE: u8 = 0x29;
const SD_RAND_APPLICATION_VECTOR_GET: u8 = 0x31;
const SD_POWER_SYSTEM_OFF: u8 = 0x33;
const SD_CLOCK_HFCLK_REQUEST: u8 = 0x42;
const SD_EVT_GET: u8 = 0x4B;
const SD_TEMP_GET: u8 = 0x4C;
const SD_POWER_USBPWRRDY_ENABLE: u8 = 0x4D;
const SD_POWER_USBDETECTED_ENABLE: u8 = 0x4E;
const SD_POWER_USBREMOVED_ENABLE: u8 = 0x4F;
const SD_POWER_USBREGSTATUS_GET: u8 = 0x50;

static ENABLED: AtomicBool = AtomicBool::new(false);
/// The SoftDevice has events (BLE or SoC) to take: its event interrupt, SWI2.
pub static EVENTS: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// A supervisor call into the MBR or the SoftDevice: arguments in r0..r3, the result in r0.
///
/// # Safety
/// The arguments must be what call `N` takes; pointers stay valid as long as it says.
#[inline(always)]
pub unsafe fn svc<const N: u8>(a: u32, b: u32, c: u32, d: u32) -> u32 {
    let ret: u32;
    // SAFETY: the caller's; the SoftDevice keeps to the AAPCS
    unsafe {
        asm!("svc {n}", n = const N, inout("r0") a => ret, inout("r1") b => _, inout("r2") c => _,
            inout("r3") d => _, out("r12") _, out("lr") _);
    }
    ret
}

/// The SoftDevice the app knows how to drive is in the flash.
pub fn present() -> bool {
    // SAFETY: the MBR/SoftDevice flash, memory-mapped
    let word = |a: u32| unsafe { core::ptr::read_volatile(a as *const u32) };
    word(INFO + 4) == INFO_MAGIC && word(INFO + 8) == APP_VECTORS && word(INFO + 0x0C) as u16 == FWID_S140_6_1_1
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// The SoftDevice asserted (or a protected memory was touched): note it for the next start.
extern "C" fn fault(id: u32, pc: u32, info: u32) {
    let mut s: heapless::String<120> = heapless::String::new();
    let _ = core::fmt::write(&mut s, format_args!("SoftDevice fault {id:#x} at pc {pc:#x} info {info:#x}"));
    crate::console::record_crash(&s);
    cortex_m::peripheral::SCB::sys_reset()
}

fn set_vtor(addr: u32) {
    // SAFETY: both tables forward every interrupt (the MBR's through the SoftDevice to ours)
    unsafe { (*cortex_m::peripheral::SCB::PTR).vtor.write(addr) }
}

/// The interrupts the SoftDevice takes over (`nrf_sdm.h`): none may be enabled when it starts.
fn softdevice_irqs() -> [interrupt::Interrupt; 9] {
    use interrupt::Interrupt::*;
    [CLOCK_POWER, RADIO, RTC0, TIMER0, RNG, ECB, AAR_CCM, TEMP, EGU5_SWI5]
}

/// Start the SoftDevice: returns its error code if it does not.
pub fn enable() -> Result<(), u32> {
    if enabled() {
        return Ok(());
    }
    for irq in softdevice_irqs() {
        irq.disable();
    }
    // VBUS events wait for the SoftDevice to stop (`disable`): it takes the POWER interrupt
    pac::POWER.intenclr().write(|w| {
        w.set_usbdetected(true);
        w.set_usbremoved(true);
        w.set_usbpwrrdy(true);
    });
    // calls and interrupts go through the MBR from here on: the bootloader has pointed it at the
    // SoftDevice, and the SoftDevice at us. Never a call inside a critical section: the
    // SoftDevice runs calls at its own priority, which BASEPRI would hold
    crate::console::record_crash("BLE step: vtor");
    set_vtor(0);
    crate::console::record_crash("BLE step: vector base");
    // SAFETY: the address of our vector table
    unsafe { svc::<SD_SOFTDEVICE_VECTOR_TABLE_BASE_SET>(APP_VECTORS, 0, 0, 0) };
    crate::console::record_crash("BLE step: sd enable");
    // the board has no 32 kHz crystal: the RC, calibrated every 4 s (Nordic's recommendation)
    let lfclk: [u8; 4] = [0 /* RC */, 16, 2, 1 /* 500 ppm */];
    // SAFETY: the clock settings are copied; the fault handler lives forever
    let err = unsafe { svc::<SD_SOFTDEVICE_ENABLE>(lfclk.as_ptr() as u32, fault as *const () as u32, 0, 0) };
    if err != NRF_SUCCESS {
        after_softdevice();
        return Err(err);
    }
    ENABLED.store(true, Ordering::Relaxed);
    // USB runs on the crystal, which the SoftDevice would otherwise start for its radio only
    // SAFETY: no arguments
    unsafe { svc::<SD_CLOCK_HFCLK_REQUEST>(0, 0, 0, 0) };
    let swi = interrupt::Interrupt::EGU2_SWI2;
    swi.set_priority(Priority::P6);
    swi.unpend();
    // SAFETY: its handler is `SoftDeviceEvents`
    unsafe { swi.enable() };
    Ok(())
}

/// Stop the SoftDevice (if running): the app has the chip to itself again.
pub fn disable() {
    if !ENABLED.swap(false, Ordering::Relaxed) {
        return;
    }
    interrupt::Interrupt::EGU2_SWI2.disable();
    // SAFETY: no arguments
    unsafe { svc::<SD_SOFTDEVICE_DISABLE>(0, 0, 0, 0) };
    after_softdevice();
}

/// Back to the app's own setup: its vector table, the crystal, and the interrupts the SoftDevice
/// had (VBUS events that came meanwhile are taken now).
fn after_softdevice() {
    set_vtor(APP_VECTORS);
    let clock = pac::CLOCK;
    clock.events_hfclkstarted().write_value(0);
    clock.tasks_hfclkstart().write_value(1);
    let since = Instant::now();
    while clock.events_hfclkstarted().read() == 0 && since.elapsed() < Duration::from_millis(10) {}
    pac::POWER.intenset().write(|w| {
        w.set_usbdetected(true);
        w.set_usbremoved(true);
        w.set_usbpwrrdy(true);
    });
    for irq in [interrupt::Interrupt::CLOCK_POWER, interrupt::Interrupt::TEMP] {
        irq.set_priority(Priority::P3);
        // SAFETY: their handlers are the USB VBUS and TEMP drivers', bound at start
        unsafe { irq.enable() };
    }
}

/// The SoftDevice's event interrupt (SWI2): wakes whoever takes its events.
pub struct SoftDeviceEvents;

impl interrupt::typelevel::Handler<interrupt::typelevel::EGU2_SWI2> for SoftDeviceEvents {
    unsafe fn on_interrupt() {
        EVENTS.signal(());
    }
}

/// Where VBUS events go (the USB driver of the companion, which keeps the SoftDevice running).
static VBUS: AtomicPtr<SoftwareVbusDetect> = AtomicPtr::new(core::ptr::null_mut());

/// VBUS events from here on go to `vbus`; it learns at once whether USB is plugged in.
pub fn usb_power_events(vbus: &'static SoftwareVbusDetect) {
    VBUS.store(core::ptr::from_ref(vbus).cast_mut(), Ordering::Relaxed);
    // SAFETY: each takes a flag
    unsafe {
        svc::<SD_POWER_USBDETECTED_ENABLE>(1, 0, 0, 0);
        svc::<SD_POWER_USBREMOVED_ENABLE>(1, 0, 0, 0);
        svc::<SD_POWER_USBPWRRDY_ENABLE>(1, 0, 0, 0);
    }
    // USBREGSTATUS: VBUSDETECT (bit 0), OUTPUTRDY (bit 1)
    let mut status = 0u32;
    // SAFETY: a word to write the status to
    if unsafe { svc::<SD_POWER_USBREGSTATUS_GET>(&raw mut status as u32, 0, 0, 0) } == NRF_SUCCESS {
        vbus.detected(status & 1 != 0);
        if status & 2 != 0 {
            vbus.ready();
        }
    }
}

/// The next SoC event, if any.
fn soc_event() -> Option<u32> {
    let mut evt = 0u32;
    // SAFETY: a word to write the event to
    (unsafe { svc::<SD_EVT_GET>(&raw mut evt as u32, 0, 0, 0) } == NRF_SUCCESS).then_some(evt)
}

/// Pass a SoC event on to whoever takes it: VBUS ones to the USB driver.
fn dispatch(evt: u32) {
    // SAFETY: set once from a `'static` reference
    let Some(vbus) = (unsafe { VBUS.load(Ordering::Relaxed).as_ref() }) else {
        return;
    };
    match evt {
        NRF_EVT_POWER_USB_DETECTED => vbus.detected(true),
        NRF_EVT_POWER_USB_REMOVED => vbus.detected(false),
        NRF_EVT_POWER_USB_POWER_READY => vbus.ready(),
        _ => {}
    }
}

/// Take the SoC events that came (after [`EVENTS`]): flash results nobody waits for are dropped.
pub fn poll_soc_events() {
    while let Some(evt) = soc_event() {
        dispatch(evt);
    }
}

/// Wait for the end of the flash operation just started; other SoC events are passed on.
fn flash_done() -> bool {
    let since = Instant::now();
    while since.elapsed() < Duration::from_secs(2) {
        match soc_event() {
            Some(NRF_EVT_FLASH_OPERATION_SUCCESS) => return true,
            Some(NRF_EVT_FLASH_OPERATION_ERROR) => return false,
            Some(evt) => dispatch(evt),
            None => {}
        }
    }
    false
}

/// The SoftDevice fits flash work between its radio events, and may find no room: tries again.
const FLASH_TRIES: usize = 4;

/// Erase the page at `addr` while the SoftDevice runs.
pub fn flash_erase(addr: u32) -> bool {
    // SAFETY: a page number in the app's flash
    (0..FLASH_TRIES).any(|_| unsafe { svc::<SD_FLASH_PAGE_ERASE>(addr / 4096, 0, 0, 0) } == NRF_SUCCESS && flash_done())
}

/// Write `words` at `addr` (word aligned, at most a page) while the SoftDevice runs.
pub fn flash_write(addr: u32, words: &[u32]) -> bool {
    // SAFETY: `words` stays in place until the operation's event has come
    (0..FLASH_TRIES).any(|_| {
        (unsafe { svc::<SD_FLASH_WRITE>(addr, words.as_ptr() as u32, words.len() as u32, 0) }) == NRF_SUCCESS && flash_done()
    })
}

/// The chip's temperature, while the SoftDevice runs (it has the TEMP peripheral).
pub fn temp_c() -> Option<f32> {
    let mut t = 0i32;
    // SAFETY: a word to write the reading to (0.25 °C units)
    (unsafe { svc::<SD_TEMP_GET>(&raw mut t as u32, 0, 0, 0) } == NRF_SUCCESS).then(|| t as f32 / 4.0)
}

/// The chip's System OFF while the SoftDevice runs (it has the POWER registers); `false` if it
/// did not take the call.
pub fn system_off() -> bool {
    // SAFETY: no arguments; it does not return when it works
    unsafe { svc::<SD_POWER_SYSTEM_OFF>(0, 0, 0, 0) == NRF_SUCCESS }
}

/// Bytes from the chip's random number generator, while the SoftDevice runs (it has the RNG and
/// keeps a pool of its output; this waits while the pool fills).
pub fn random(out: &mut [u8]) -> bool {
    for chunk in out.chunks_mut(16) {
        let since = Instant::now();
        loop {
            // SAFETY: a buffer of `chunk.len()` bytes
            match unsafe { svc::<SD_RAND_APPLICATION_VECTOR_GET>(chunk.as_mut_ptr() as u32, chunk.len() as u32, 0, 0) } {
                NRF_SUCCESS => break,
                NRF_ERROR_SOC_RAND_NOT_ENOUGH_VALUES if since.elapsed() < Duration::from_millis(500) => {}
                _ => return false,
            }
        }
    }
    true
}

// ---------------------------------------------------------------- critical sections

struct BasepriCriticalSection;
critical_section::set_impl!(BasepriCriticalSection);

// SAFETY: single core; every interrupt of the app runs at a priority BASEPRI masks (2 and lower:
// `repeater.rs` sets them, `embassy_nrf::config` the time driver's and GPIOTE's)
unsafe impl critical_section::Impl for BasepriCriticalSection {
    unsafe fn acquire() -> u8 {
        let was = cortex_m::register::basepri::read();
        cortex_m::register::basepri_max::write(APP_MASK); // only raises the mask
        was
    }

    unsafe fn release(was: u8) {
        // SAFETY: back to the mask `acquire` found
        unsafe { cortex_m::register::basepri::write(was) };
    }
}
