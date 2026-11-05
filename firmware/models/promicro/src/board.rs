//! The Pro Micro around the radio: the battery, the clock kept across resets, power off.
//!
//! Pins as upstream's `variants/promicro` (its Arduino pin numbers resolved through
//! `variant.cpp`): SX1262 on SPI (SCK P1.11, MOSI P1.15, MISO P0.02, NSS P1.13), RESET P0.09,
//! DIO1 P0.10, BUSY P0.29, its supply on P0.13, RXEN P0.17 (DIO2 switches the antenna to TX),
//! a 1.8 V TCXO on DIO3; the button P1.00 (low while pressed), the LED P0.15 (active high), the
//! battery on P0.31 (AIN7) behind a divider; I2C for sensors on SDA P1.04, SCL P0.11.

use core::mem::MaybeUninit;

use embassy_nrf::pac;
use embassy_nrf::saadc::Saadc;
use embassy_time::{Duration, Instant};

/// Upstream `ADC_MULTIPLIER`: millivolts per LSB of a 12-bit sample over 0..3.6 V (internal
/// 0.6 V reference, gain 1/6), divider included; `set adc.multiplier` calibrates it per board.
pub const BATT_MV_PER_LSB: f32 = 1.815;
const BATT_SAMPLES: i32 = 8;

/// The button (P1.00) as a pin number in P1.
const BUTTON_PIN: usize = 0;

pub struct Battery {
    pub adc: Saadc<'static, 1>,
}

impl Battery {
    pub async fn read_mv(&mut self, mv_per_lsb: f32) -> u16 {
        let mut sum = 0i32;
        for _ in 0..BATT_SAMPLES {
            let mut sample = [0i16; 1];
            self.adc.sample(&mut sample).await;
            sum += sample[0].max(0) as i32;
        }
        ((sum / BATT_SAMPLES) as f32 * mv_per_lsb) as u16
    }
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
    pac::P0.outclr().write(|w| {
        w.set_pin(15, true); // LED
        w.set_pin(13, true); // radio supply
        w.set_pin(17, true); // RXEN
    });
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
