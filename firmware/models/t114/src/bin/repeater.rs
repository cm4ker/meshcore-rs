//! MeshCore repeater firmware for the Heltec T114, in Rust: the chip set up, the parts on their
//! pins (`board_parts!`), then the Pro Micro's repeater (`promicro::repeater_app`) with the
//! T114's screen and GPS around it (`t114::screen::Extras`).

#![no_std]
#![no_main]

use core::mem::MaybeUninit;
use core::panic::PanicInfo;

use embassy_executor::Spawner;
use embassy_nrf::interrupt::{self, InterruptExt, Priority};
use embassy_nrf::temp::{self, Temp};
use embassy_nrf::usb::Driver;
use embassy_nrf::usb::vbus_detect::HardwareVbusDetect;
use embassy_nrf::{bind_interrupts, config, peripherals, saadc, spim, twim, usb};
use embassy_time::Instant;
use embedded_alloc::LlffHeap as Heap;
use promicro::repeater_app::{self, Chip};
use promicro::{Spec, console, sd};
use t114::screen::{Extras, RepeaterUi};
use t114::{board, gps, tft};

const HEAP_SIZE: usize = 144 * 1024;
const BUILD_DATE: &str = env!("BUILD_DATE");

#[global_allocator]
static HEAP: Heap = Heap::empty();

static SPEC: Spec = board::spec(env!("BUILD_EPOCH"), BUILD_DATE);

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    CLOCK_POWER => usb::vbus_detect::InterruptHandler;
    TWISPI1 => spim::InterruptHandler<peripherals::TWISPI1>;
    SPI2 => spim::InterruptHandler<peripherals::SPI2>;
    SAADC => saadc::InterruptHandler;
    TWISPI0 => twim::InterruptHandler<peripherals::TWISPI0>;
    TEMP => temp::InterruptHandler;
    EGU2_SWI2 => sd::SoftDeviceEvents;
});

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    console::record_crash(&console::panic_text(info));
    cortex_m::peripheral::SCB::sys_reset()
}

#[cortex_m_rt::exception]
unsafe fn HardFault(frame: &cortex_m_rt::ExceptionFrame) -> ! {
    let mut s: heapless::String<120> = heapless::String::new();
    let _ = core::fmt::write(&mut s, format_args!("hard fault at pc {:#010x} lr {:#010x}", frame.pc(), frame.lr()));
    console::record_crash(&s);
    cortex_m::peripheral::SCB::sys_reset()
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    {
        static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];
        // SAFETY: called once, before anything allocates
        unsafe { HEAP.init(&raw mut HEAP_MEM as usize, HEAP_SIZE) }
    }
    // the internal RC for the low-frequency clock, as on the Pro Micro (the SoftDevice is set up
    // for it); USB needs the HF crystal
    let mut ncfg = config::Config::default();
    ncfg.hfclk_source = config::HfclkSource::ExternalXtal;
    ncfg.lfclk_source = config::LfclkSource::InternalRC;
    // priorities the SoftDevice leaves to the app (2, 3, 5..7), so it can run beside them in the
    // BLE window; the critical sections mask them all (`promicro::sd`)
    ncfg.time_interrupt_priority = Priority::P2;
    ncfg.gpiote_interrupt_priority = Priority::P2;
    let p = embassy_nrf::init(ncfg);
    for irq in [
        interrupt::USBD,
        interrupt::CLOCK_POWER,
        interrupt::TWISPI1,
        interrupt::SPI2,
        interrupt::UARTE0,
        interrupt::SAADC,
        interrupt::TWISPI0,
        interrupt::TEMP,
    ] {
        irq.set_priority(Priority::P3);
    }
    let (parts, screen, gps) = t114::board_parts!(p, Irqs);
    spawner.spawn(tft::task(screen).unwrap());
    spawner.spawn(gps::task(gps).unwrap());
    let chip = Chip {
        usb: Driver::new(p.USBD, Irqs, HardwareVbusDetect::new(Irqs)),
        wdt: p.WDT,
        nvmc: p.NVMC,
        rng: p.RNG,
        temp: Temp::new(p.TEMP, Irqs),
    };
    let ui = RepeaterUi::new(parts.button.is_low(), Instant::now().as_millis());
    repeater_app::run(spawner, chip, parts, &SPEC, Extras { ui, build_date: BUILD_DATE }).await
}
