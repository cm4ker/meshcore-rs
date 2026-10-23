//! MeshCore companion radio firmware for the LilyGO T-Echo, in Rust.
//!
//! Wiring (upstream `variants/lilygo_techo`): SX1262 on SPI (SCK P0.19, MOSI P0.22, MISO P0.23,
//! NSS P0.24), RESET P0.25, BUSY P0.17, DIO1 P0.20, DIO2 switching the antenna, a 1.8 V TCXO on
//! DIO3 (its supply on P0.21), radio supply on P1.05; 2 MB QSPI flash (SCK P1.14, CS P1.15, IO
//! P1.12 P1.13 P0.07 P0.05); peripherals' 3.3 V on P0.12; button P1.10; touch pad P0.11 and the
//! screen's light P1.11; GPS (L76K) on UARTE1 P1.09/P1.08, enable P1.02, reset P1.05; LEDs
//! P0.13-15 (active low); battery on AIN2 (P0.04) behind a 1:2 divider.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::rc::Rc;
use core::cell::RefCell;
use core::mem::MaybeUninit;
use core::panic::PanicInfo;

use embassy_executor::Spawner;
use embassy_nrf::gpio::{Input, Level, Output, OutputDrive, Pull};
use embassy_nrf::qspi::{self, Qspi};
use embassy_nrf::mode::Blocking;
use embassy_nrf::rng::Rng;
use embassy_nrf::saadc::{self, ChannelConfig, Saadc};
use embassy_nrf::spim::{self, Spim};
use embassy_nrf::usb::Driver;
use embassy_nrf::usb::vbus_detect::HardwareVbusDetect;
use embassy_nrf::{bind_interrupts, config, peripherals, usb};
use embassy_time::Timer;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embassy_usb::Builder;
use embedded_alloc::LlffHeap as Heap;
use meshcore::radio::RadioParams;
use meshcore_fw_core::companion::{self, Config};
use meshcore_fw_core::radio::WaitIrq;
use meshcore_fw_core::storage::Store;
use meshcore_fw_core::sx1262::Sx1262;
use nrf_sdc::mpsl::{self, MultiprotocolServiceLayer};
use nrf_sdc::{self as sdc};
use static_cell::StaticCell;
use log::{info, warn};
use t_echo::board::{self as board, TEcho};
use t_echo::diag;
use t_echo::epaper::{self, Epaper};
use t_echo::gps::{self, Gps};
use t_echo::usb as frames_usb;
use t_echo::platform::Nrf;

const BUILD_EPOCH: &str = env!("BUILD_EPOCH");
const BUILD_DATE: &str = env!("BUILD_DATE");

/// The 2 MB QSPI flash: the node's blobs in the first 64 KiB, the companion's data in the next
/// megabyte.
const FLASH_SIZE: u32 = 2 * 1024 * 1024;
const DATA_REGION: (u32, u32) = (0, 0x1_0000);
const COMPANION_REGION: (u32, u32) = (0x1_0000, 0x10_0000);

/// The BLE controller's buffers: one connection, 3 x 251-byte L2CAP packets each way.
const L2CAP_MTU: u16 = 251;
const L2CAP_TXQ: u8 = 3;
const L2CAP_RXQ: u8 = 3;
const SDC_MEM_SIZE: usize = 8192;

fn build_sdc(
    p: sdc::Peripherals<'static>, rng: &'static mut Rng<'static, Blocking>, mpsl: &'static MultiprotocolServiceLayer<'static>,
    mem: &'static mut sdc::Mem<SDC_MEM_SIZE>,
) -> Result<sdc::SoftdeviceController<'static>, sdc::Error> {
    sdc::Builder::new()?
        .support_adv()
        .support_peripheral()
        .peripheral_count(1)?
        .buffer_cfg(L2CAP_MTU, L2CAP_MTU, L2CAP_TXQ, L2CAP_RXQ)?
        .build(p, rng, mpsl, mem)
}

#[embassy_executor::task]
async fn mpsl_task(mpsl: &'static MultiprotocolServiceLayer<'static>) -> ! {
    mpsl.run().await
}

#[global_allocator]
static HEAP: Heap = Heap::empty();

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    CLOCK_POWER => usb::vbus_detect::InterruptHandler, mpsl::ClockInterruptHandler;
    EGU0_SWI0 => mpsl::LowPrioInterruptHandler;
    RADIO => mpsl::HighPrioInterruptHandler;
    TIMER0 => mpsl::HighPrioInterruptHandler;
    RTC0 => mpsl::HighPrioInterruptHandler;
    QSPI => qspi::InterruptHandler<peripherals::QSPI>;
    SPI2 => spim::InterruptHandler<peripherals::SPI2>;
    SPIM3 => spim::InterruptHandler<peripherals::SPI3>;
    SAADC => saadc::InterruptHandler;
});

/// The LEDs' pins (active low), driven straight through the GPIO registers so a start that
/// stops before the HAL is up still shows how far it got.
const BLUE: u32 = 14;
const GREEN: u32 = 15;

fn stage_led(pin: u32, on: bool) {
    const P0: usize = 0x5000_0000;
    // SAFETY: plain writes to P0's pin configuration and set/clear registers for an LED pin
    unsafe {
        core::ptr::write_volatile((P0 + 0x700 + 4 * pin as usize) as *mut u32, 0b11); // output, input off
        core::ptr::write_volatile((P0 + if on { 0x50C } else { 0x508 }) as *mut u32, 1 << pin);
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    diag::record_crash(&diag::panic_text(info));
    cortex_m::peripheral::SCB::sys_reset()
}

#[cortex_m_rt::exception]
unsafe fn HardFault(frame: &cortex_m_rt::ExceptionFrame) -> ! {
    let mut s: heapless::String<120> = heapless::String::new();
    let _ = core::fmt::write(&mut s, format_args!("hard fault at pc {:#010x} lr {:#010x}", frame.pc(), frame.lr()));
    diag::record_crash(&s);
    cortex_m::peripheral::SCB::sys_reset()
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    stage_led(BLUE, true); // main reached
    {
        static mut HEAP_MEM: [MaybeUninit<u8>; 160 * 1024] = [MaybeUninit::uninit(); 160 * 1024];
        // SAFETY: called once, before anything allocates
        unsafe { HEAP.init(&raw mut HEAP_MEM as usize, 160 * 1024) }
    }
    // the clocks are MPSL's (the BLE controller's service layer): it runs the 32 kHz crystal, and
    // keeps the HF crystal on while asked to
    let p = embassy_nrf::init(config::Config::default());
    let mpsl_p = mpsl::Peripherals::new(p.RTC0, p.TIMER0, p.TEMP, p.PPI_CH19, p.PPI_CH30, p.PPI_CH31);
    let lfclk = mpsl::raw::mpsl_clock_lfclk_cfg_t {
        source: mpsl::raw::MPSL_CLOCK_LF_SRC_XTAL as u8,
        rc_ctiv: 0,
        rc_temp_ctiv: 0,
        accuracy_ppm: 20,
        skip_wait_lfclk_started: false,
    };
    static MPSL: StaticCell<MultiprotocolServiceLayer> = StaticCell::new();
    let mpsl = MPSL.init(MultiprotocolServiceLayer::new(mpsl_p, Irqs, lfclk).unwrap());
    spawner.spawn(mpsl_task(mpsl).unwrap());
    // USB needs the HF crystal all the time: the guard lives as long as the firmware
    match mpsl.request_hfclk().await {
        Ok(hfclk) => {
            let _ = mpsl::Hfclk::wait().await;
            core::mem::forget(hfclk);
        }
        Err(_) => stage_led(GREEN, true),
    }

    // ---- USB first: the boot log goes there, so a start that goes wrong shows why
    let driver = Driver::new(p.USBD, Irqs, HardwareVbusDetect::new(Irqs));
    let mut usb_cfg = embassy_usb::Config::new(0x239A, 0x8029);
    usb_cfg.manufacturer = Some("LilyGO");
    usb_cfg.product = Some("T-Echo MeshCore-rs");
    usb_cfg.max_power = 100;
    static CONFIG_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static CONTROL_BUF: StaticCell<[u8; 64]> = StaticCell::new();
    static STATE: StaticCell<State> = StaticCell::new();
    let mut builder = Builder::new(
        driver,
        usb_cfg,
        CONFIG_DESC.init([0; 256]),
        BOS_DESC.init([0; 256]),
        &mut [],
        CONTROL_BUF.init([0; 64]),
    );
    let class = CdcAcmClass::new(&mut builder, STATE.init(State::new()), 64);
    let (tx, rx, control) = class.split_with_control();
    spawner.spawn(frames_usb::device_task(builder.build()).unwrap());
    spawner.spawn(frames_usb::rx_task(rx, control).unwrap());
    spawner.spawn(frames_usb::tx_task(tx).unwrap());
    stage_led(GREEN, true); // USB running
    diag::init_logger();
    let mut busy_led = Output::new(p.P0_14, Level::Low, OutputDrive::Standard); // blue: starting
    Timer::after_millis(1500).await; // time for the host to open the port
    info!("MeshCore companion (Rust) {} build {} for T-Echo", meshcore::FIRMWARE_VERSION, BUILD_DATE);
    if let Some(crash) = diag::take_crash() {
        warn!("last reset: {crash}");
    }

    // ---- power: peripherals, TCXO; the GPS off and in reset until the companion wants it
    let power = Output::new(p.P0_12, Level::High, OutputDrive::Standard);
    let _tcxo = Output::new(p.P0_21, Level::High, OutputDrive::Standard);
    let gps_enable = Output::new(p.P1_02, Level::Low, OutputDrive::Standard);
    let gps_reset = Output::new(p.P1_05, Level::Low, OutputDrive::Standard);
    let _red = Output::new(p.P0_13, Level::High, OutputDrive::Standard);
    Timer::after_millis(10).await; // the radio powers up

    // ---- flash
    let mut qcfg = qspi::Config::default();
    qcfg.capacity = FLASH_SIZE;
    qcfg.read_opcode = qspi::ReadOpcode::Fastread;
    qcfg.write_opcode = qspi::WriteOpcode::Pp;
    qcfg.frequency = qspi::Frequency::M16;
    let flash = Qspi::new(p.QSPI, Irqs, p.P1_14, p.P1_15, p.P1_12, p.P1_13, p.P0_07, p.P0_05, qcfg);
    let store = Some(Rc::new(RefCell::new(Store::with_regions(flash, DATA_REGION, Some(COMPANION_REGION)))));
    info!("flash ready");

    // ---- radio
    let mut scfg = spim::Config::default();
    scfg.frequency = spim::Frequency::M8;
    let spi = Spim::new(p.SPI2, Irqs, p.P0_19, p.P0_23, p.P0_22, scfg);
    let radio = Sx1262::new(
        spi,
        Output::new(p.P0_24, Level::High, OutputDrive::Standard),
        Output::new(p.P0_25, Level::High, OutputDrive::Standard),
        Input::new(p.P0_17, Pull::None),
        WaitIrq(Input::new(p.P0_20, Pull::Down)),
    );

    // ---- entropy, BLE controller (it keeps the hardware RNG)
    static RNG: StaticCell<Rng<'static, Blocking>> = StaticCell::new();
    let rng = RNG.init(Rng::new_blocking(p.RNG));
    let mut entropy = [0u8; 32];
    rng.blocking_fill_bytes(&mut entropy);
    let sdc_p = sdc::Peripherals::new(
        p.PPI_CH17, p.PPI_CH18, p.PPI_CH20, p.PPI_CH21, p.PPI_CH22, p.PPI_CH23, p.PPI_CH24, p.PPI_CH25, p.PPI_CH26,
        p.PPI_CH27, p.PPI_CH28, p.PPI_CH29,
    );
    static SDC_MEM: StaticCell<sdc::Mem<SDC_MEM_SIZE>> = StaticCell::new();
    let sdc = match build_sdc(sdc_p, rng, mpsl, SDC_MEM.init(sdc::Mem::new())) {
        Ok(sdc) => Some(sdc),
        Err(e) => {
            warn!("BLE controller: {e:?}");
            None
        }
    };
    // 0..3 V full scale (internal 0.6 V reference, gain 1/5), as upstream's `AR_INTERNAL_3_0`
    let mut vbat = ChannelConfig::single_ended(p.P0_04);
    vbat.gain = saadc::Gain::Gain1_5;
    let battery = Saadc::new(p.SAADC, Irqs, saadc::Config::default(), [vbat]);
    let board = TEcho {
        tx_led: Output::new(p.P0_15, Level::High, OutputDrive::Standard),
        button: Input::new(p.P1_10, Pull::Up),
        battery,
        power,
    };
    let gps = Gps {
        uarte: p.UARTE1,
        timer: p.TIMER1,
        ppi_ch1: p.PPI_CH0,
        ppi_ch2: p.PPI_CH1,
        ppi_group: p.PPI_GROUP0,
        rx: p.P1_09,
        tx: p.P1_08,
        enable: gps_enable,
        reset: gps_reset,
    };
    spawner.spawn(gps::task(gps).unwrap());
    let touch = Input::new(p.P0_11, Pull::Up);
    let light = Output::new(p.P1_11, Level::Low, OutputDrive::Standard);
    spawner.spawn(board::backlight_task(touch, light).unwrap());

    // ---- the e-paper, in its own task
    let mut ecfg = spim::Config::default();
    ecfg.frequency = spim::Frequency::M4;
    let epd = Epaper {
        spi: Spim::new_txonly(p.SPI3, Irqs, p.P0_31, p.P0_29, ecfg),
        cs: Output::new(p.P0_30, Level::High, OutputDrive::Standard),
        dc: Output::new(p.P0_28, Level::High, OutputDrive::Standard),
        reset: Output::new(p.P0_02, Level::High, OutputDrive::Standard),
        busy: Input::new(p.P0_03, Pull::None),
    };
    spawner.spawn(epaper::task(epd).unwrap());

    busy_led.set_high();
    stage_led(GREEN, false);
    let seed = u64::from_le_bytes(entropy[..8].try_into().unwrap());
    let platform = Nrf { sdc, seed };
    let cfg = Config {
        // the Omsk mesh (upstream builds: `LORA_FREQ` etc. of the variant)
        radio: RadioParams { freq_mhz: 868.731, bw_khz: 62.5, sf: 7, cr: 7 },
        tx_power_dbm: 22,
        build_epoch: BUILD_EPOCH.parse().unwrap_or(0),
        build_date: BUILD_DATE,
    };
    companion::run(spawner, platform, radio, store, entropy, board, cfg).await
}
