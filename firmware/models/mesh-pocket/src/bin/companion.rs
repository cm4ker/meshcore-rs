#![no_std]
#![no_main]

use core::mem::MaybeUninit;
use core::panic::PanicInfo;

use embassy_executor::Spawner;
use embassy_nrf::interrupt::{self, InterruptExt, Priority};
use embassy_nrf::temp::{self, Temp};
use embassy_nrf::{bind_interrupts, config, peripherals, saadc, spim, twim, usb};
use promicro::console::Heap;
use mesh_pocket::companion::{MeshPocket, power_off_task};
use mesh_pocket::{board, epaper};
use promicro::companion::ProMicro;
use promicro::companion_app::{self, Chip};
use promicro::{Spec, console, sd};

const HEAP_SIZE: usize = 160 * 1024;

#[global_allocator]
static HEAP: Heap = Heap::empty();

static SPEC: Spec = board::spec(env!("BUILD_EPOCH"), env!("BUILD_DATE"));

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
    console::record_hard_fault(frame)
}

#[cortex_m_rt::entry]
fn main() -> ! {
    promicro::board::run_executor(|spawner| spawner.spawn(app(spawner).unwrap()))
}

#[embassy_executor::task]
async fn app(spawner: Spawner) {
    static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];
    // SAFETY: this is called once, before allocating.
    unsafe { HEAP.init(&raw mut HEAP_MEM as usize, HEAP_SIZE) }

    // the 32 MHz crystal stays off until USB VBUS arrives (`sd::HfclkVbusDetect`)
    let mut config = config::Config::default();
    config.lfclk_source = config::LfclkSource::ExternalXtal;
    config.dcdc.reg1 = true;
    config.debug = config::Debug::NotConfigured;
    // IRQ priorities left to the application by S140.
    config.time_interrupt_priority = Priority::P2;
    config.gpiote_interrupt_priority = Priority::P2;
    let p = embassy_nrf::init(config);
    for irq in [
        interrupt::USBD,
        interrupt::CLOCK_POWER,
        interrupt::TWISPI1,
        interrupt::SPI2,
        interrupt::SAADC,
        interrupt::TWISPI0,
        interrupt::TEMP,
    ] {
        irq.set_priority(Priority::P3);
    }

    let (parts, screen) = mesh_pocket::board_parts!(p, Irqs);
    spawner.spawn(epaper::task(screen).unwrap());
    spawner.spawn(power_off_task().unwrap());
    let chip = Chip {
        usbd: p.USBD,
        irqs: Irqs,
        wdt: p.WDT,
        nvmc: p.NVMC,
        rng: p.RNG,
        temp: Temp::new(p.TEMP, Irqs),
    };
    companion_app::run(spawner, chip, parts, &SPEC, |board| MeshPocket {
        base: ProMicro {
            led: board.led,
            rxen: board.rxen,
            button: board.button,
            battery: board.battery,
            temp: board.temp,
            sensors: board.sensors,
            screen: false,
            buzzer: false,
            _hold: board.hold,
        },
    })
    .await
}
