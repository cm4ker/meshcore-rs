//! The Pro Micro's OLED: an SSD1306, 128 x 64, on the board's I2C (upstream `SSD1306Display` with
//! `PIN_BOARD_SDA` 8 / `PIN_BOARD_SCL` 7, which are P1.04 / P0.11: the same pins the sensors use).
//! It shows the interface's pages in the 128 x 64 layout ([`Class::Mono128x64`]), drawn in its own
//! task: [`SCREEN`] carries the page to show, or `None` when the screen should go dark.
//!
//! The panel is optional, as on the T3-S3: [`Oled::probe`] answers `None` when nothing acknowledges
//! its address, and the board then runs headless. A frame is eight pages of 128 bytes, each sent as
//! a set-page command followed by its bytes.
//!
//! The driver is its own rather than the `ssd1306` crate's: the Pro Micro's image must fit the
//! 340 KiB its updates swap in, a 1 KiB frame buffer and the few commands below cost under a
//! kilobyte of code, while the crate brings a generic driver, its modes and its rotations.

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, with_timeout};
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use meshcore_ui::View;
use meshcore_ui::layout::Class;

use crate::i2c::Bus;

/// A transfer on a stuck bus must not hold up the boot or the loop that feeds the screen.
const BUS_TIMEOUT: Duration = Duration::from_millis(100);

/// The SSD1306's address on the usual breakouts (upstream's Adafruit driver takes the same).
const ADDRESS: u8 = 0x3C;
pub const WIDTH: usize = 128;
pub const HEIGHT: usize = 64;
/// 8-pixel rows, as the panel stores them.
const PAGES: usize = HEIGHT / 8;

/// The page to show, or `None`: the screen goes dark.
pub static SCREEN: Signal<CriticalSectionRawMutex, Option<View>> = Signal::new();

/// A 1-bit frame: rows of 16 bytes, bit 7 leftmost, 1 = ink.
/// A 1-bit frame in the panel's own layout: one byte per column, 8 vertical pixels, bit 0 the
/// top one (the SSD1306's GDDRAM), so a frame goes to the panel as it is.
#[derive(Clone, PartialEq, Eq)]
pub struct Frame([u8; WIDTH * PAGES]);

impl Frame {
    pub fn blank() -> Self {
        Self([0; WIDTH * PAGES])
    }
}

impl OriginDimensions for Frame {
    fn size(&self) -> Size {
        Size::new(WIDTH as u32, HEIGHT as u32)
    }
}

impl DrawTarget for Frame {
    type Color = BinaryColor;
    type Error = core::convert::Infallible;

    fn draw_iter<I: IntoIterator<Item = Pixel<BinaryColor>>>(&mut self, pixels: I) -> Result<(), Self::Error> {
        for Pixel(p, c) in pixels {
            if (0..WIDTH as i32).contains(&p.x) && (0..HEIGHT as i32).contains(&p.y) {
                // the byte of the column `p.x` in the page of `p.y`; its bit 0 is the page's top
                let i = (p.y as usize / 8) * WIDTH + p.x as usize;
                let bit = 1u8 << (p.y % 8);
                if c.is_on() {
                    self.0[i] |= bit;
                } else {
                    self.0[i] &= !bit;
                }
            }
        }
        Ok(())
    }
}

/// The panel: the shared bus, and whether it is lit.
pub struct Oled {
    bus: Bus,
    on: bool,
}

impl Oled {
    /// The panel set up, or `None` if nothing answers at its address (the board runs headless).
    pub async fn probe(bus: Bus) -> Option<Self> {
        let mut oled = Self { bus, on: false };
        oled.init().await.then_some(oled)
    }

    /// Adafruit's SSD1306 initialisation for the 128 x 64 panel with its charge pump, which is what
    /// upstream's driver sends: the same panel, the same picture.
    async fn init(&mut self) -> bool {
        const INIT: [u8; 25] = [
            0xAE, // display off
            0xD5, 0x80, // clock divide
            0xA8, 0x3F, // multiplex 64
            0xD3, 0x00, // display offset
            0x40, // start line 0
            0x8D, 0x14, // charge pump on
            0x20, 0x00, // horizontal addressing
            0xA1, // segment remap
            0xC8, // COM scan down
            0xDA, 0x12, // COM pins
            0x81, 0xCF, // contrast
            0xD9, 0xF1, // pre-charge
            0xDB, 0x40, // VCOMH
            0xA4, // resume from RAM
            0xA6, // not inverted
            0xAF, // display on
        ];
        let mut frame = [0u8; INIT.len() + 1];
        frame[0] = 0x00; // commands follow
        frame[1..].copy_from_slice(&INIT);
        let ok = self.bus_write(&frame).await;
        self.on = ok;
        ok
    }

    /// One frame: each of the eight pages set and written. A failed transfer ends the frame (the
    /// next one sets the pages again); it is not retried, as in the C++ firmware.
    ///
    /// The panel is in horizontal addressing mode (`0x20 0x00`, as Adafruit's driver leaves it), so
    /// a page is selected by its address range (`0x22`) and the columns by theirs (`0x21`): the
    /// older `0xB0` page command belongs to page addressing mode and is ignored here.
    pub async fn show(&mut self, frame: &Frame) {
        if !self.on && !self.init().await {
            return;
        }
        let mut row = [0u8; WIDTH + 1];
        row[0] = 0x40; // data follow
        for page in 0..PAGES {
            // page range: this page alone; column range: all 128
            if !self.bus_write(&[0x00, 0x22, page as u8, page as u8, 0x21, 0x00, (WIDTH - 1) as u8]).await {
                return;
            }
            row[1..].copy_from_slice(&frame.0[page * WIDTH..(page + 1) * WIDTH]);
            if !self.bus_write(&row).await {
                return;
            }
        }
    }

    /// Dark; the panel keeps its RAM, so the next frame draws it all again.
    pub async fn off(&mut self) {
        if self.on {
            self.bus_write(&[0x00, 0xAE]).await;
            self.on = false;
        }
    }

    async fn bus_write(&mut self, bytes: &[u8]) -> bool {
        use embedded_hal_async::i2c::I2c as _;
        // a stuck bus must not hold the boot or the screen's task up (as the sensors' reads)
        matches!(with_timeout(BUS_TIMEOUT, self.bus.write(ADDRESS, bytes)).await, Ok(Ok(())))
    }
}

/// Draws the pages to the panel; without a panel it takes the pages and drops them, so the rest of
/// the firmware needs no second path.
#[embassy_executor::task]
pub async fn task(oled: Option<Oled>) {
    let Some(mut oled) = oled else {
        loop {
            SCREEN.wait().await;
        }
    };
    let mut shown: Option<(View, Frame)> = None;
    loop {
        let Some(view) = SCREEN.wait().await else {
            oled.off().await;
            shown = None;
            continue;
        };
        if shown.as_ref().is_some_and(|(v, _)| *v == view) {
            continue;
        }
        let mut frame = Frame::blank();
        Class::Mono128x64.render(&view, &mut frame);
        if shown.as_ref().is_none_or(|(_, f)| *f != frame) {
            oled.show(&frame).await;
        }
        shown = Some((view, frame));
    }
}
