//! The Heltec V4's SSD1306 OLED (128x64, I2C), showing the interface's pages in the 128 x 64
//! layout ([`meshcore_ui::layout::Class::Mono128x64`]).
//!
//! The screen goes dark a minute after the last press of the PRG button (OLED burn-in on an
//! always-on node; [`meshcore_ui::Ui::with_screen_timeout`]), and is lit again with it.
//!
//! Transfers are handled like the C++ firmware does (Adafruit's driver ignores I2C errors): a
//! failed transfer does not abandon the frame and is not retried (a NACKed transfer has usually
//! been received, and a repeat would shift the panel's write pointer); the next frame sets the
//! area and redraws it all. Only when a frame loses several transfers is the controller reset
//! through its RST pin (which also releases a held bus) and initialised again.

use alloc::rc::Rc;
use core::cell::Cell;

use esp_hal::Blocking;
use esp_hal::delay::Delay;
use esp_hal::gpio::Output;
use esp_hal::i2c::master::I2c;
use meshcore_ui::View;
use meshcore_ui::layout::Class;
use ssd1306::mode::BufferedGraphicsMode;
use ssd1306::prelude::*;
use ssd1306::{I2CDisplayInterface, Ssd1306};

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

    /// Draw `v`; `false` if the frame could not be sent (too many lost transfers).
    pub fn show(&mut self, v: &View) -> bool {
        if !self.on {
            return true;
        }
        if self.needs_init && !self.reinit() {
            return false;
        }
        self.display.clear_buffer();
        Class::Mono128x64.render(v, &mut self.display);
        self.set_strict(false);
        let ok = self.display.flush().is_ok();
        self.needs_init = !ok;
        ok
    }
}
