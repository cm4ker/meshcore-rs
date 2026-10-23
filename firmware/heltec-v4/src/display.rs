//! Status screen on the Heltec V4's SSD1306 OLED (128x64, I2C).
//!
//! The screen turns itself off after a while (OLED burn-in on an always-on repeater) and is woken
//! with the PRG button.
//!
//! Transfers are handled like the C++ firmware does (Adafruit's driver ignores I2C errors): a
//! failed transfer does not abandon the frame and is not retried (a NACKed transfer has usually
//! been received, and a repeat would shift the panel's write pointer); the next frame sets the
//! area and redraws it all. Only when a frame loses several transfers is the controller reset
//! through its RST pin (which also releases a held bus) and initialised again.

use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use core::cell::Cell;

use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::mono_font::ascii::{FONT_6X10, FONT_7X13_BOLD};
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Line, PrimitiveStyle};
use embedded_graphics::text::{Baseline, Text};
use esp_hal::Blocking;
use esp_hal::delay::Delay;
use esp_hal::gpio::Output;
use esp_hal::i2c::master::I2c;
use ssd1306::mode::BufferedGraphicsMode;
use ssd1306::prelude::*;
use ssd1306::{I2CDisplayInterface, Ssd1306};

/// What the status screen shows.
pub struct Status<'a> {
    pub name: &'a str,
    pub radio: &'a str,
    pub rx: u32,
    pub tx: u32,
    pub noise_floor: i16,
    pub last_rssi: f32,
    pub last_snr: f32,
    pub batt_mv: u16,
    pub uptime_s: u64,
    /// Neighbours (repeater) or contacts (companion), shown as `<label> <count>`.
    pub counter: (&'a str, usize),
    pub id_prefix: &'a str,
    pub note: Option<&'a str>,
}

/// Transfer outcome counters shared with [`TolerantI2c`].
#[derive(Clone, Copy, Default)]
struct BusStats {
    /// Report failures (initialisation must know whether a display is there).
    strict: bool,
    /// Failed transfers in the current frame, and since boot.
    failed: u32,
    lost: u32,
    /// Give up on a frame after this many failed transfers.
    max_failed: u32,
}

/// Outside strict mode, swallows transfer errors so a frame goes on; after `max_failed`
/// failures it gives up on the frame without touching the bus.
struct TolerantI2c<'d> {
    inner: I2c<'d, Blocking>,
    stats: Rc<Cell<BusStats>>,
}


impl embedded_hal::i2c::ErrorType for TolerantI2c<'_> {
    type Error = esp_hal::i2c::master::Error;
}

impl embedded_hal::i2c::I2c for TolerantI2c<'_> {
    fn transaction(&mut self, address: u8, operations: &mut [embedded_hal::i2c::Operation<'_>]) -> Result<(), Self::Error> {
        let mut st = self.stats.get();
        if !st.strict && st.failed >= st.max_failed {
            return Err(esp_hal::i2c::master::Error::Timeout);
        }
        let result = embedded_hal::i2c::I2c::transaction(&mut self.inner, address, operations);
        if result.is_err() {
            st.failed += 1;
            st.lost += 1;
        }
        self.stats.set(st);
        if st.strict || st.failed >= st.max_failed { result } else { Ok(()) }
    }
}

/// As in the C++ firmware (Adafruit SSD1306 with an internal charge pump): pre-charge 0xF1, contrast 0xCF.
const BRIGHTNESS: Brightness = Brightness::custom(15, 0xCF);

type Display<'d> = Ssd1306<I2CInterface<TolerantI2c<'d>>, DisplaySize128x64, BufferedGraphicsMode<DisplaySize128x64>>;

pub struct Oled<'d> {
    display: Display<'d>,
    stats: Rc<Cell<BusStats>>,
    rst: Output<'d>,
    on: bool,
    /// The last transfer failed: reset and initialise before drawing again.
    needs_init: bool,
}

fn uptime(s: u64) -> String {
    match s {
        s if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        s if s < 86400 => format!("{}h{:02}m", s / 3600, (s / 60) % 60),
        s => format!("{}d{:02}h", s / 86400, (s / 3600) % 24),
    }
}

impl<'d> Oled<'d> {
    /// `None` if no display answers on the bus.
    pub fn new(i2c: I2c<'d, Blocking>, rst: Output<'d>) -> Option<Self> {
        let stats = Rc::new(Cell::new(BusStats { max_failed: 8, ..Default::default() }));
        let bus = TolerantI2c { inner: i2c, stats: stats.clone() };
        let display =
            Ssd1306::new(I2CDisplayInterface::new(bus), DisplaySize128x64, DisplayRotation::Rotate0).into_buffered_graphics_mode();
        let mut oled = Self { display, stats, rst, on: true, needs_init: true };
        oled.reinit().then_some(oled)
    }

    fn set_strict(&self, strict: bool) {
        self.stats.set(BusStats { strict, failed: 0, ..self.stats.get() });
    }

    /// Failed transfers since boot (diagnostics).
    pub fn bus_errors(&self) -> u32 {
        self.stats.get().lost
    }

    /// Hardware reset (releases the bus if the controller was holding it) and initialisation.
    fn reinit(&mut self) -> bool {
        let delay = Delay::new();
        self.rst.set_low();
        delay.delay_millis(1);
        self.rst.set_high();
        delay.delay_millis(2);
        self.set_strict(true);
        self.needs_init = self.display.init().is_err() || self.display.set_brightness(BRIGHTNESS).is_err();
        self.set_strict(false);
        if !self.needs_init && !self.on {
            let _ = self.display.set_display_on(false);
        }
        !self.needs_init
    }

    pub fn is_on(&self) -> bool {
        self.on
    }

    pub fn set_on(&mut self, on: bool) {
        if on != self.on {
            self.on = on;
            let _ = self.display.set_display_on(on);
        }
    }

    /// Render the status screen; `false` if the frame could not be sent (too many lost transfers).
    pub fn show(&mut self, s: &Status<'_>) -> bool {
        if !self.on {
            return true;
        }
        if self.needs_init && !self.reinit() {
            return false;
        }
        let small = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
        let big = MonoTextStyle::new(&FONT_7X13_BOLD, BinaryColor::On);
        let d = &mut self.display;
        d.clear_buffer();
        let line = |d: &mut Display<'d>, y: i32, text: &str| {
            let _ = Text::with_baseline(text, Point::new(0, y), small, Baseline::Top).draw(d);
        };
        let _ = Text::with_baseline(s.name, Point::zero(), big, Baseline::Top).draw(d);
        let _ = Line::new(Point::new(0, 13), Point::new(127, 13))
            .into_styled(PrimitiveStyle::with_stroke(BinaryColor::On, 1))
            .draw(d);
        line(d, 15, s.radio);
        line(d, 25, &format!("RX {}  TX {}  {} {}", s.rx, s.tx, s.counter.0, s.counter.1));
        line(d, 35, &format!("{:.0}dBm {:.1}dB nf{}", s.last_rssi, s.last_snr, s.noise_floor));
        line(d, 45, &format!("{:.2}V  up {}", s.batt_mv as f32 / 1000.0, uptime(s.uptime_s)));
        line(d, 55, s.note.unwrap_or(s.id_prefix));
        self.set_strict(false);
        let ok = self.display.flush().is_ok();
        self.needs_init = !ok;
        ok
    }
}
