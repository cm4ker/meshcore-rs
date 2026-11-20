//! MeshCore companion radio firmware for an nRF52840 Pro Micro with an SX1262 module, in Rust: the
//! chip set up, the parts on their pins (`board_parts!`), then the companion
//! (`promicro::companion_app`), its board the Pro Micro's (`promicro::companion::ProMicro`).

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
use promicro::{Spec, board, console, sd};

const HEAP_SIZE: usize = 160 * 1024;

#[global_allocator]
static HEAP: Heap = Heap::empty();

static SPEC: Spec = Spec {
    name: board::NAME,
    manufacturer: board::MANUFACTURER,
    release_board: board::RELEASE_BOARD,
    usb: board::USB_ID,
    batt_mv_per_lsb: board::BATT_MV_PER_LSB,
    system_off: board::system_off,
    updates: Some(&promicro::ota::IN_PLACE_AFTER_SWAP),
    companion_regions: promicro::COMPANION_REGIONS,
    companion_tx_dbm: 22,
    companion_before: Some(promicro::COMPANION_BEFORE),
    build_epoch: env!("BUILD_EPOCH"),
    build_date: env!("BUILD_DATE"),
};

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    // the SoftDevice's while it runs; VBUS events that come once it has stopped, right before a
    // reset, are taken here
    CLOCK_POWER => usb::vbus_detect::InterruptHandler;
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
    // the board has no 32 kHz crystal (upstream `USE_LFRC`); USB needs the HF crystal
    let mut ncfg = config::Config::default();
    ncfg.hfclk_source = config::HfclkSource::ExternalXtal;
    ncfg.lfclk_source = config::LfclkSource::InternalRC;
    // the LDO, not the DC/DC converter upstream turns on (`NRF52BoardDCDC`): clones without its
    // inductor (a SuperMini seen 2026-11-06) lock up the moment it starts
    // priorities the SoftDevice leaves to the app (2, 3, 5..7); the critical sections mask them
    // all (`promicro::sd`)
    ncfg.time_interrupt_priority = Priority::P2;
    ncfg.gpiote_interrupt_priority = Priority::P2;
    let p = embassy_nrf::init(ncfg);
    for irq in [interrupt::USBD, interrupt::CLOCK_POWER, interrupt::SPI2, interrupt::SAADC, interrupt::TWISPI0, interrupt::TEMP] {
        irq.set_priority(Priority::P3);
    }
    let parts = promicro::board_parts!(p, Irqs);
    // the panel answers on the shared I2C (or not: the board then runs headless), and the speaker
    // is on the board's tone pin
    let screen = promicro::oled::Oled::probe(parts.i2c).await;
    let has_screen = screen.is_some();
    spawner.spawn(promicro::oled::task(screen).unwrap());
    spawner.spawn(promicro::buzzer::task(promicro::buzzer::new(p.PWM0, p.P1_06)).unwrap());
    let chip = Chip { usbd: p.USBD, irqs: Irqs, wdt: p.WDT, nvmc: p.NVMC, rng: p.RNG, temp: Temp::new(p.TEMP, Irqs) };
    companion_app::run(spawner, chip, parts, &SPEC, |b| ProMicro {
        led: b.led,
        rxen: b.rxen,
        button: b.button,
        battery: b.battery,
        temp: b.temp,
        sensors: b.sensors,
        screen: has_screen,
        buzzer: true,
        _hold: b.hold,
    })
    .await
}
