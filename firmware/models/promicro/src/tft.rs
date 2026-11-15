//! The Heltec T114's 1.14" colour TFT (ST7789, 135 x 240, turned to landscape): the interface's
//! pages in the TFT layout ([`Class::Tft240x135`]), white on black, drawn in their own task.
//! Set up as upstream's `ST7789Spi`: 16-bit colour, rows and columns swapped and mirrored, colours
//! inverted, the panel's 240 x 135 at (40, 52) of the controller's 320 x 240. The screen's supply
//! and its light go off with it ([`SCREEN`] `None`); it starts afresh when it comes back.

use embassy_nrf::gpio::Output;
use embassy_nrf::spim::Spim;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::Timer;
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use meshcore_ui::View;
use meshcore_ui::layout::Class;

pub const WIDTH: usize = 240;
pub const HEIGHT: usize = 135;
const X_OFFSET: u16 = 40;
const Y_OFFSET: u16 = 52;

/// RGB565, as the controller takes it (high byte first).
const INK: [u8; 2] = 0xFFFFu16.to_be_bytes();
const PAPER: [u8; 2] = 0x0000u16.to_be_bytes();

/// The page to show, or `None`: the screen goes dark.
pub static SCREEN: Signal<CriticalSectionRawMutex, Option<View>> = Signal::new();

/// A 1-bit frame: rows of 30 bytes, bit 7 leftmost, 1 = ink.
#[derive(Clone, PartialEq)]
pub struct Frame([u8; WIDTH * HEIGHT / 8]);

impl Frame {
    pub fn blank() -> Self {
        Self([0; WIDTH * HEIGHT / 8])
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
                let i = p.y as usize * WIDTH / 8 + p.x as usize / 8;
                let bit = 0x80 >> (p.x % 8);
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

pub struct Tft {
    /// SPIM3, the only one that runs at 32 MHz.
    pub spi: Spim<'static>,
    pub cs: Output<'static>,
    pub dc: Output<'static>,
    pub reset: Output<'static>,
    /// The screen's supply, low = on.
    pub power: Output<'static>,
    /// The light, low = on.
    pub light: Output<'static>,
}

impl Tft {
    async fn command(&mut self, cmd: u8, data: &[u8]) {
        self.cs.set_low();
        self.dc.set_low();
        let _ = self.spi.write(&[cmd]).await;
        if !data.is_empty() {
            self.dc.set_high();
            let _ = self.spi.write(data).await;
        }
        self.cs.set_high();
    }

    /// Power, reset and set the controller up (upstream's `ST7789Spi::sendInitCommands`).
    async fn init(&mut self) {
        self.power.set_low();
        self.cs.set_high();
        self.reset.set_high();
        Timer::after_millis(5).await;
        self.reset.set_low();
        Timer::after_millis(10).await;
        self.reset.set_high();
        Timer::after_millis(120).await;
        self.command(0x01, &[]).await; // software reset
        Timer::after_millis(150).await;
        self.command(0x11, &[]).await; // out of sleep
        Timer::after_millis(120).await;
        self.command(0x3A, &[0x55]).await; // 16-bit colour
        self.command(0x36, &[0x60]).await; // MV | MX: landscape, as upstream's `landscapeScreen`
        self.command(0x21, &[]).await; // inverted, as the panel wants
        self.command(0x13, &[]).await; // normal mode
        self.command(0x29, &[]).await; // display on
    }

    async fn show(&mut self, frame: &Frame) {
        let (x0, y0) = (X_OFFSET, Y_OFFSET);
        let (x1, y1) = (x0 + WIDTH as u16 - 1, y0 + HEIGHT as u16 - 1);
        self.command(0x2A, &[(x0 >> 8) as u8, x0 as u8, (x1 >> 8) as u8, x1 as u8]).await;
        self.command(0x2B, &[(y0 >> 8) as u8, y0 as u8, (y1 >> 8) as u8, y1 as u8]).await;
        self.cs.set_low();
        self.dc.set_low();
        let _ = self.spi.write(&[0x2C]).await; // memory write
        self.dc.set_high();
        let mut line = [0u8; WIDTH * 2];
        for row in frame.0.chunks(WIDTH / 8) {
            for (x, px) in line.chunks_mut(2).enumerate() {
                px.copy_from_slice(if row[x / 8] & (0x80 >> (x % 8)) != 0 { &INK } else { &PAPER });
            }
            let _ = self.spi.write(&line).await;
        }
        self.cs.set_high();
    }

    /// Dark and unpowered; its pins low, so nothing feeds the controller.
    fn off(&mut self) {
        self.light.set_high();
        self.power.set_high();
        self.cs.set_low();
        self.dc.set_low();
        self.reset.set_low();
    }
}

#[embassy_executor::task]
pub async fn task(mut tft: Tft) {
    tft.off();
    let mut shown: Option<(View, Frame)> = None;
    let mut on = false;
    loop {
        let Some(view) = SCREEN.wait().await else {
            if on {
                tft.off();
                on = false;
                shown = None;
            }
            continue;
        };
        if shown.as_ref().is_some_and(|(v, _)| *v == view) {
            continue;
        }
        let mut frame = Frame::blank();
        Class::Tft240x135.render(&view, &mut frame);
        if !on {
            tft.init().await;
        }
        if shown.as_ref().is_none_or(|(_, f)| *f != frame) {
            tft.show(&frame).await;
        }
        if !on {
            // lit once it shows the page, not the controller's leftovers
            tft.light.set_low();
            on = true;
        }
        shown = Some((view, frame));
    }
}
