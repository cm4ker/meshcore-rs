//! MeshCore companion radio firmware for the Heltec T096, in Rust: the chip set up, the parts on
//! their pins (`board_parts!`), then the Pro Micro's companion (`promicro::companion_app`), its
//! board the T096's, with the screen and the GPS (`t096::companion::T096`).

#![no_std]
#![no_main]

use core::mem::MaybeUninit;
use core::panic::PanicInfo;

use embassy_executor::Spawner;
use embassy_nrf::interrupt::{self, InterruptExt, Priority};
use embassy_nrf::temp::{self, Temp};
use embassy_nrf::{bind_interrupts, config, peripherals, saadc, spim, twim, usb};
use embedded_alloc::LlffHeap as Heap;
use promicro::companion::ProMicro;
use promicro::companion_app::{self, Chip};
use promicro::{Spec, console, sd};
use t096::companion::T096;
use t096::{board, gps, tft};

const HEAP_SIZE: usize = 160 * 1024;

#[global_allocator]
static HEAP: Heap = Heap::empty();

static SPEC: Spec = board::spec(env!("BUILD_EPOCH"), env!("BUILD_DATE"));

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    // the SoftDevice's while it runs; VBUS events that come once it has stopped, right before a
    // reset, are taken here
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
    let _ = core::fmt::write(
        &mut s,
        format_args!("hard fault at pc {:#010x} lr {:#010x}", frame.pc(), frame.lr()),
    );
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
    // priorities the SoftDevice leaves to the app (2, 3, 5..7); the critical sections mask them
    // all (`promicro::sd`)
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
    let (parts, screen, gps, fem_ctx) = t096::board_parts!(p, Irqs);
    spawner.spawn(tft::task(screen).unwrap());
    spawner.spawn(gps::task(gps).unwrap());
    let chip = Chip {
        usbd: p.USBD,
        irqs: Irqs,
        wdt: p.WDT,
        nvmc: p.NVMC,
        rng: p.RNG,
        temp: Temp::new(p.TEMP, Irqs),
    };
    companion_app::run(spawner, chip, parts, &SPEC, |b| {
        T096::new(
            fem_ctx,
            ProMicro {
                led: b.led,
                rxen: b.rxen,
                button: b.button,
                battery: b.battery,
                temp: b.temp,
                sensors: b.sensors,
                screen: false,
                buzzer: false,
                _hold: b.hold,
            },
        )
    })
    .await
}
