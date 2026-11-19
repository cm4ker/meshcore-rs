//! The Heltec T096's ST7735 TFT in landscape, with the 128 × 64 companion UI centered on
//! the 160 × 80 panel. Its supply is shared with GNSS.

use embassy_nrf::gpio::Output;
use embassy_nrf::spim::Spim;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::Timer;
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use meshcore_ui::View;
use meshcore_ui::layout::Class;

use crate::board::vext;

const WIDTH: usize = 160;
const HEIGHT: usize = 80;
const UI_WIDTH: usize = 128;
const UI_HEIGHT: usize = 64;
const UI_X: usize = (WIDTH - UI_WIDTH) / 2;
const UI_Y: usize = (HEIGHT - UI_HEIGHT) / 2;
// Panel origin in controller memory in MeshCore's DISPLAY_ROTATION=1.
const PANEL_X: u8 = 0;
const PANEL_Y: u8 = 24;

/// The page to show, or `None` to turn off the display.
pub static SCREEN: Signal<CriticalSectionRawMutex, Option<View>> = Signal::new();

#[derive(PartialEq)]
struct Frame([u8; WIDTH * HEIGHT / 8]);

impl OriginDimensions for Frame {
    fn size(&self) -> Size {
        Size::new(UI_WIDTH as u32, UI_HEIGHT as u32)
    }
}

impl DrawTarget for Frame {
    type Color = BinaryColor;
    type Error = core::convert::Infallible;

    fn draw_iter<I: IntoIterator<Item = Pixel<BinaryColor>>>(&mut self, pixels: I) -> Result<(), Self::Error> {
        for Pixel(point, color) in pixels {
            if (0..UI_WIDTH as i32).contains(&point.x) && (0..UI_HEIGHT as i32).contains(&point.y) {
                let x = point.x as usize + UI_X;
                let y = point.y as usize + UI_Y;
                let i = y * (WIDTH / 8) + x / 8;
                let mask = 0x80 >> (x % 8);
                if color.is_on() {
                    self.0[i] |= mask;
                } else {
                    self.0[i] &= !mask;
                }
            }
        }
        Ok(())
    }
}

pub struct Tft {
    pub spi: Spim<'static>,
    pub cs: Output<'static>,
    pub dc: Output<'static>,
    pub reset: Output<'static>,
    /// Low turns on the backlight.
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

    async fn init(&mut self) {
        // The TFT and GNSS share VEXT. The LCD may already be powered through GNSS.
        vext::want(vext::SCREEN, true);
        Timer::after_millis(100).await;
        self.cs.set_high();
        self.reset.set_high();
        Timer::after_millis(2).await;
        self.reset.set_low();
        Timer::after_millis(10).await;
        self.reset.set_high();
        Timer::after_millis(2).await;

        self.command(0x11, &[]).await; // sleep out
        Timer::after_millis(120).await;
        // ST7735R timing, power and gamma from upstream's ST7735Display.
        const SETUP: &[(u8, &[u8])] = &[
            (0xB1, &[0x01, 0x2C, 0x2D]),
            (0xB2, &[0x01, 0x2C, 0x2D]),
            (0xB3, &[0x01, 0x2C, 0x2D, 0x01, 0x2C, 0x2D]),
            (0xB4, &[0x07]),
            (0xC0, &[0xA2, 0x02, 0x84]),
            (0xC1, &[0xC5]),
            (0xC2, &[0x0A, 0x00]),
            (0xC3, &[0x8A, 0x2A]),
            (0xC4, &[0x8A, 0xEE]),
            (0xC5, &[0x0E]),
            (
                0xE0,
                &[
                    0x02, 0x1C, 0x07, 0x12, 0x37, 0x32, 0x29, 0x2D, 0x29, 0x25, 0x2B, 0x39, 0x00, 0x01, 0x03, 0x10,
                ],
            ),
            (
                0xE1,
                &[
                    0x03, 0x1D, 0x07, 0x06, 0x2E, 0x2C, 0x29, 0x2D, 0x2E, 0x2E, 0x37, 0x3F, 0x00, 0x00, 0x02, 0x10,
                ],
            ),
        ];
        for &(cmd, data) in SETUP {
            self.command(cmd, data).await;
        }
        self.command(0x20, &[]).await; // inversion off
        self.command(0x3A, &[0x05]).await; // 16-bit pixel format
        self.command(0x36, &[0xA8]).await; // MY | MV | BGR, landscape
        self.command(0x13, &[]).await; // normal mode
    }

    async fn show(&mut self, frame: &Frame) {
        self.command(0x2A, &[0, PANEL_X, 0, PANEL_X + WIDTH as u8 - 1]).await;
        self.command(0x2B, &[0, PANEL_Y, 0, PANEL_Y + HEIGHT as u8 - 1]).await;
        self.cs.set_low();
        self.dc.set_low();
        let _ = self.spi.write(&[0x2C]).await;
        self.dc.set_high();
        let mut line = [0u8; WIDTH * 2];
        for row in frame.0.chunks_exact(WIDTH / 8) {
            for (x, pixel) in line.chunks_exact_mut(2).enumerate() {
                // Black and white RGB565 pixels both have identical high and low bytes.
                pixel.fill(if row[x / 8] & (0x80 >> (x % 8)) != 0 { 0xFF } else { 0 });
            }
            let _ = self.spi.write(&line).await;
        }
        self.cs.set_high();
    }

    async fn off(&mut self) {
        self.light.set_high();
        self.command(0x28, &[]).await; // display off
        self.command(0x10, &[]).await; // sleep in, including when GNSS retains VEXT
        self.cs.set_low();
        self.reset.set_low();
        vext::want(vext::SCREEN, false);
    }
}

#[embassy_executor::task]
pub async fn task(mut tft: Tft) {
    let mut shown: Option<(View, Frame)> = None;
    loop {
        let Some(view) = SCREEN.wait().await else {
            if shown.take().is_some() {
                tft.off().await;
            }
            continue;
        };
        if shown.as_ref().is_some_and(|(previous, _)| *previous == view) {
            continue;
        }

        let mut frame = Frame([0; WIDTH * HEIGHT / 8]);
        Class::Mono128x64.render(&view, &mut frame);
        if shown.is_none() {
            tft.init().await;
        }
        if shown.as_ref().is_none_or(|(_, pixels)| *pixels != frame) {
            tft.show(&frame).await;
        }
        if shown.is_none() {
            tft.command(0x29, &[]).await; // display on after the first frame
            tft.light.set_low();
        }
        shown = Some((view, frame));
    }
}
