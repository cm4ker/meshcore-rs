//! The board around the radio: its parts on their pins ([`board_parts!`]), the battery, the clock
//! kept across resets, power off. One crate for two boards with the same chip, bootloader and
//! flash layout: the Pro Micro by default, the Heltec T114 with the `t114` feature.
//!
//! Pro Micro, as upstream's `variants/promicro` (its Arduino pin numbers resolved through
//! `variant.cpp`): SX1262 on SPI (SCK P1.11, MOSI P1.15, MISO P0.02, NSS P1.13), RESET P0.09,
//! DIO1 P0.10, BUSY P0.29, its supply on P0.13, RXEN P0.17 (DIO2 switches the antenna to TX),
//! a 1.8 V TCXO on DIO3; the button P1.00 (low while pressed), the LED P0.15 (active high), the
//! battery on P0.31 (AIN7) behind a divider; I2C for sensors on SDA P1.04, SCL P0.11.
//!
//! Heltec T114, as upstream's `variants/heltec_t114` and Meshtastic's `heltec_mesh_node_t114`:
//! SX1262 on SPI (SCK P0.19, MOSI P0.22, MISO P0.23, NSS P0.24), RESET P0.25, DIO1 P0.20, BUSY
//! P0.17, DIO2 switching the antenna, a 1.8 V TCXO on DIO3; the button P1.10 (low while pressed),
//! the LED P1.03 (active low), the battery on P0.04 (AIN2) behind a divider that P0.06 switches
//! on; I2C on SDA P0.26, SCL P0.27. The ST7789 screen (power P0.03, backlight P0.15, both active
//! low) and the GPS with the I2C connector's supply (P0.21) stay off.

use core::mem::MaybeUninit;

use embassy_nrf::gpio::{Input, Output};
use embassy_nrf::pac;
use embassy_nrf::saadc::Saadc;
use embassy_nrf::spim::Spim;
use embassy_nrf::twim::Twim;
use embassy_time::{Duration, Instant, Timer};
use meshcore_fw_core::radio::WaitIrq;
use meshcore_fw_core::sx1262::Sx1262;

/// Upstream `ADC_MULTIPLIER`: millivolts per LSB of a 12-bit sample over 0..3.6 V (internal
/// 0.6 V reference, gain 1/6), divider included; `set adc.multiplier` calibrates it per board.
#[cfg(not(feature = "t114"))]
pub const BATT_MV_PER_LSB: f32 = 1.815;
/// Upstream's divider of 4.9 over our 0..3.6 V range (upstream reads 0..3.0 V).
#[cfg(feature = "t114")]
pub const BATT_MV_PER_LSB: f32 = 3600.0 / 4096.0 * 4.9;
const BATT_SAMPLES: i32 = 8;

/// The board's name in the boot log, its maker in `ver`/telemetry, and its number in releases.
#[cfg(not(feature = "t114"))]
pub const NAME: &str = "ProMicro";
#[cfg(not(feature = "t114"))]
pub const MANUFACTURER: &str = "ProMicro DIY";
#[cfg(not(feature = "t114"))]
pub const RELEASE_BOARD: u8 = meshcore::release::board::PROMICRO;
#[cfg(feature = "t114")]
pub const NAME: &str = "Heltec T114";
#[cfg(feature = "t114")]
pub const MANUFACTURER: &str = "Heltec T114";
#[cfg(feature = "t114")]
pub const RELEASE_BOARD: u8 = meshcore::release::board::HELTEC_T114;

/// The USB IDs and product name of upstream's firmware for the board: the host keeps its COM port.
#[cfg(not(feature = "t114"))]
pub const USB_ID: (u16, u16, &str) = (0x239A, 0x00B3, "ProMicro NRF52840");
#[cfg(feature = "t114")]
pub const USB_ID: (u16, u16, &str) = (0x239A, 0x8029, "HT-n5262");

/// The button as a pin number in P1.
#[cfg(not(feature = "t114"))]
const BUTTON_PIN: usize = 0;
#[cfg(feature = "t114")]
const BUTTON_PIN: usize = 10;

pub type Radio = Sx1262<Spim<'static>, Output<'static>, Input<'static>, WaitIrq<Input<'static>>>;

/// The board's parts on their pins, from [`board_parts!`].
pub struct Parts {
    /// Not initialised yet; its supply is on.
    pub radio: Radio,
    pub led: Led,
    /// The receive side of the antenna switch, on a board that has one to turn off for TX.
    pub rxen: Option<Output<'static>>,
    pub button: Input<'static>,
    pub battery: Battery,
    pub i2c: Twim<'static>,
    /// Outputs that only have to stay as they are: the radio's supply, the T114's screen off.
    pub hold: heapless::Vec<Output<'static>, 4>,
}

/// The LED, lit while transmitting, at whichever level lights it.
pub struct Led {
    pin: Output<'static>,
    active_low: bool,
}

impl Led {
    pub fn new(pin: Output<'static>, active_low: bool) -> Self {
        let mut led = Self { pin, active_low };
        led.off();
        led
    }

    pub fn on(&mut self) {
        self.pin.set_level((!self.active_low).into());
    }

    pub fn off(&mut self) {
        self.pin.set_level(self.active_low.into());
    }

    pub fn toggle(&mut self) {
        self.pin.toggle();
    }
}

pub struct Battery {
    pub adc: Saadc<'static, 1>,
    /// The divider's switch, on a board that keeps it off between readings.
    pub enable: Option<Output<'static>>,
}

impl Battery {
    pub async fn read_mv(&mut self, mv_per_lsb: f32) -> u16 {
        if let Some(e) = &mut self.enable {
            e.set_high();
            Timer::after_millis(10).await; // upstream's settling time
        }
        let mut sum = 0i32;
        for _ in 0..BATT_SAMPLES {
            let mut sample = [0i16; 1];
            self.adc.sample(&mut sample).await;
            sum += sample[0].max(0) as i32;
        }
        if let Some(e) = &mut self.enable {
            e.set_low();
        }
        ((sum / BATT_SAMPLES) as f32 * mv_per_lsb) as u16
    }
}

/// The board's [`Parts`], moved out of `embassy_nrf::init`'s peripherals: the radio on SPI2 (at
/// upstream's 2 MHz, RadioLib's speed: some modules and boards do not take 8), the battery on the
/// SAADC, the I2C on TWISPI0, with the interrupts bound to `$irqs`.
#[cfg(not(feature = "t114"))]
#[macro_export]
macro_rules! board_parts {
    ($p:ident, $irqs:expr) => {{
        use embassy_nrf::gpio::{Input, Level, Output, OutputDrive, Pull};
        use embassy_nrf::{saadc, spim, twim};
        let mut hold = heapless::Vec::new();
        let _ = hold.push(Output::new($p.P0_13, Level::High, OutputDrive::Standard)); // radio supply
        let mut scfg = spim::Config::default();
        scfg.frequency = spim::Frequency::M2;
        let spi = spim::Spim::new($p.SPI2, $irqs, $p.P1_11, $p.P0_02, $p.P1_15, scfg);
        let radio = meshcore_fw_core::sx1262::Sx1262::new(
            spi,
            Output::new($p.P1_13, Level::High, OutputDrive::Standard),
            Output::new($p.P0_09, Level::High, OutputDrive::Standard),
            Input::new($p.P0_29, Pull::None),
            meshcore_fw_core::radio::WaitIrq(Input::new($p.P0_10, Pull::Down)),
        )
        .without_rx_patch();
        let mut icfg = twim::Config::default();
        icfg.sda_pullup = true;
        icfg.scl_pullup = true;
        static TWIM_BUF: static_cell::StaticCell<[u8; 8]> = static_cell::StaticCell::new();
        $crate::board::Parts {
            radio,
            led: $crate::board::Led::new(Output::new($p.P0_15, Level::Low, OutputDrive::Standard), false),
            rxen: Some(Output::new($p.P0_17, Level::High, OutputDrive::Standard)),
            button: Input::new($p.P1_00, Pull::Up),
            battery: $crate::board::Battery {
                adc: saadc::Saadc::new($p.SAADC, $irqs, saadc::Config::default(), [saadc::ChannelConfig::single_ended($p.P0_31)]),
                enable: None,
            },
            i2c: twim::Twim::new($p.TWISPI0, $irqs, $p.P1_04, $p.P0_11, icfg, TWIM_BUF.init([0; 8])),
            hold,
        }
    }};
}

#[cfg(feature = "t114")]
#[macro_export]
macro_rules! board_parts {
    ($p:ident, $irqs:expr) => {{
        use embassy_nrf::gpio::{Input, Level, Output, OutputDrive, Pull};
        use embassy_nrf::{saadc, spim, twim};
        let mut hold = heapless::Vec::new();
        // the screen's supply and backlight off, as upstream's ST7789Display::turnOff
        let _ = hold.push(Output::new($p.P0_03, Level::High, OutputDrive::Standard));
        let _ = hold.push(Output::new($p.P0_15, Level::High, OutputDrive::Standard));
        // the GPS and the I2C connector unpowered
        let _ = hold.push(Output::new($p.P0_21, Level::Low, OutputDrive::Standard));
        let mut scfg = spim::Config::default();
        scfg.frequency = spim::Frequency::M2;
        let spi = spim::Spim::new($p.SPI2, $irqs, $p.P0_19, $p.P0_23, $p.P0_22, scfg);
        let radio = meshcore_fw_core::sx1262::Sx1262::new(
            spi,
            Output::new($p.P0_24, Level::High, OutputDrive::Standard),
            Output::new($p.P0_25, Level::High, OutputDrive::Standard),
            Input::new($p.P0_17, Pull::None),
            meshcore_fw_core::radio::WaitIrq(Input::new($p.P0_20, Pull::Down)),
        )
        .without_rx_patch();
        let mut icfg = twim::Config::default();
        icfg.sda_pullup = true;
        icfg.scl_pullup = true;
        static TWIM_BUF: static_cell::StaticCell<[u8; 8]> = static_cell::StaticCell::new();
        $crate::board::Parts {
            radio,
            led: $crate::board::Led::new(Output::new($p.P1_03, Level::High, OutputDrive::Standard), true),
            rxen: None,
            button: Input::new($p.P1_10, Pull::Up),
            battery: $crate::board::Battery {
                adc: saadc::Saadc::new($p.SAADC, $irqs, saadc::Config::default(), [saadc::ChannelConfig::single_ended($p.P0_04)]),
                enable: Some(Output::new($p.P0_06, Level::Low, OutputDrive::Standard)),
            },
            i2c: twim::Twim::new($p.TWISPI0, $irqs, $p.P0_26, $p.P0_27, icfg, TWIM_BUF.init([0; 8])),
            hold,
        }
    }};
}

const CLOCK_MAGIC: u32 = 0x434C_4B21; // "CLK!"

/// The wall clock in RAM a reset does not clear (the nRF52840 keeps its RAM through a soft reset
/// or the watchdog), so a reboot does not send the clock back to the last time saved to flash.
#[unsafe(link_section = ".uninit.clock")]
static mut CLOCK: MaybeUninit<[u32; 2]> = MaybeUninit::uninit();

pub fn clock_load() -> Option<u32> {
    // SAFETY: main task only; any bit pattern is a valid pair of words
    let [check, epoch] = unsafe { (&raw const CLOCK).cast::<[u32; 2]>().read_volatile() };
    (check == CLOCK_MAGIC ^ epoch).then_some(epoch)
}

pub fn clock_store(epoch: u32) {
    // SAFETY: main task only
    unsafe { (&raw mut CLOCK).cast::<[u32; 2]>().write_volatile([CLOCK_MAGIC ^ epoch, epoch]) };
}

/// The UF2 bootloader's version, as upstream's `NRF52Board::getBootloaderVersion` finds it: the
/// word after "UF2 Bootloader " in its info text (0xFB000..0xFE000); empty if there is none.
pub fn bootloader_version() -> alloc::string::String {
    const MARKER: &[u8] = b"UF2 Bootloader ";
    // SAFETY: the bootloader's flash, memory-mapped and readable
    let flash = unsafe { core::slice::from_raw_parts(0xFB000 as *const u8, 0x3000) };
    let Some(at) = flash.windows(MARKER.len()).position(|w| w == MARKER) else {
        return alloc::string::String::new();
    };
    let ver = &flash[at + MARKER.len()..];
    let len = ver.iter().take(31).position(|&c| matches!(c, 0 | b' ' | b'\n' | b'\r')).unwrap_or(ver.len().min(31));
    alloc::string::String::from_utf8_lossy(&ver[..len]).into_owned()
}

/// The serial number the Adafruit firmware gives the USB port (the chip's device ID), so the
/// host keeps the board's COM port.
pub fn usb_serial() -> heapless::String<16> {
    // `%08X%08X` of DEVICEID[1], DEVICEID[0]
    let mut s = heapless::String::new();
    let (hi, lo) = (pac::FICR.deviceid(1).read(), pac::FICR.deviceid(0).read());
    let _ = core::fmt::write(&mut s, format_args!("{hi:08X}{lo:08X}"));
    s
}

/// The chip's System OFF, which only the button ends (with a reset, so the firmware starts over).
/// The radio is asleep and its supply off by then.
pub fn system_off() -> ! {
    // a pin already at its sense level would wake the chip at once: wait for the button to be let go
    let since = Instant::now();
    while !pac::P1.in_().read().pin(BUTTON_PIN) && since.elapsed() < Duration::from_secs(10) {}
    #[cfg(not(feature = "t114"))]
    pac::P0.outclr().write(|w| {
        w.set_pin(15, true); // LED
        w.set_pin(13, true); // radio supply
        w.set_pin(17, true); // RXEN
    });
    // the LED off; the screen and the GPS are off already
    #[cfg(feature = "t114")]
    pac::P1.outset().write(|w| w.set_pin(3, true));
    pac::P1.pin_cnf(BUTTON_PIN).modify(|w| w.set_sense(pac::gpio::vals::Sense::Low));
    // the POWER registers are the SoftDevice's while it runs (the companion's BLE)
    if crate::sd::enabled() {
        crate::sd::system_off();
    } else {
        embassy_nrf::power::set_system_off();
    }
    loop {
        cortex_m::asm::wfe();
    }
}
