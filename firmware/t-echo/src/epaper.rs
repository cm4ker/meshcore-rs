//! The T-Echo's 1.54" e-paper (GDEH0154D67, SSD1681 controller, 200 x 200): the interface's
//! pages in the square e-paper layout ([`meshcore_ui::layout::Class::EpaperSquare`]), drawn in
//! their own task so a refresh (up to 2 s) never holds up the radio, when the e-paper refresh
//! policy ([`meshcore_ui::refresh::epaper`]) says so.

use embassy_nrf::gpio::{Input, Output};
use embassy_nrf::spim::Spim;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use meshcore_ui::layout::Class;
use meshcore_ui::refresh::epaper::Epaper as Policy;
use meshcore_ui::{Body, View};

pub const WIDTH: usize = 200;
pub const HEIGHT: usize = 200;

pub static SCREEN: Signal<CriticalSectionRawMutex, View> = Signal::new();

/// A 1-bit frame in the controller's layout: rows of 25 bytes, bit 7 leftmost, 1 = white. Drawn
/// turned as upstream's (GxEPD2 rotation 3).
pub struct Frame([u8; WIDTH * HEIGHT / 8]);

impl Frame {
    pub fn blank() -> Self {
        Self([0xFF; WIDTH * HEIGHT / 8])
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
                let (x, y) = (p.y as usize, HEIGHT - 1 - p.x as usize);
                let i = y * WIDTH / 8 + x / 8;
                let bit = 0x80 >> (x % 8);
                // BinaryColor::On is ink
                if c.is_on() {
                    self.0[i] &= !bit;
                } else {
                    self.0[i] |= bit;
                }
            }
        }
        Ok(())
    }
}

pub struct Epaper {
    pub spi: Spim<'static>,
    pub cs: Output<'static>,
    pub dc: Output<'static>,
    pub reset: Output<'static>,
    pub busy: Input<'static>,
}

impl Epaper {
    async fn wait_idle(&mut self) {
        // BUSY is high while the controller works; a refresh takes up to ~2 s
        let _ = embassy_time::with_timeout(Duration::from_secs(5), self.busy.wait_for_low()).await;
    }

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

    /// Reset and set the panel up (GxEPD2's `GxEPD2_154_D67::_InitDisplay`).
    async fn init(&mut self) {
        self.reset.set_low();
        Timer::after_millis(10).await;
        self.reset.set_high();
        Timer::after_millis(10).await;
        self.wait_idle().await;
        self.command(0x12, &[]).await; // software reset
        self.wait_idle().await;
        self.command(0x01, &[(HEIGHT - 1) as u8, ((HEIGHT - 1) >> 8) as u8, 0x00]).await; // gate lines
        self.command(0x11, &[0x03]).await; // data entry: x then y, both increasing
        self.command(0x44, &[0x00, (WIDTH / 8 - 1) as u8]).await; // RAM x window (bytes)
        self.command(0x45, &[0x00, 0x00, (HEIGHT - 1) as u8, ((HEIGHT - 1) >> 8) as u8]).await; // RAM y window
        self.command(0x3C, &[0x05]).await; // border
        self.command(0x18, &[0x80]).await; // internal temperature sensor
    }

    async fn write_ram(&mut self, ram: u8, frame: &Frame) {
        self.command(0x4E, &[0x00]).await;
        self.command(0x4F, &[0x00, 0x00]).await;
        self.command(ram, &frame.0).await;
    }

    /// Show `frame`: a full refresh, or a partial one against the image shown before.
    async fn show(&mut self, frame: &Frame, full: bool) {
        self.write_ram(0x24, frame).await; // new image
        if full {
            self.write_ram(0x26, frame).await; // the base of the next partial refresh
            self.command(0x22, &[0xF7]).await;
        } else {
            self.command(0x3C, &[0x80]).await; // border unchanged during partial refreshes
            self.command(0x22, &[0xFC]).await;
        }
        self.command(0x20, &[]).await;
        self.wait_idle().await;
        if !full {
            self.write_ram(0x26, frame).await;
        }
    }

    /// Deep sleep: the image stays, the controller draws next to nothing until a reset.
    async fn sleep(&mut self) {
        self.command(0x10, &[0x01]).await;
    }
}

fn render(v: &View) -> Frame {
    let mut f = Frame::blank();
    Class::EpaperSquare.render(v, &mut f);
    f
}

fn now_ms() -> u64 {
    Instant::now().as_millis()
}

#[embassy_executor::task]
pub async fn task(mut epd: Epaper) {
    epd.init().await;
    let mut policy = Policy::default();
    loop {
        let next = match policy.clean_at() {
            Some(at) => embassy_time::with_deadline(Instant::from_millis(at), SCREEN.wait()).await.ok(),
            None => Some(SCREEN.wait().await),
        };
        let Some(mut view) = next else {
            // nothing changed for a while: restore the contrast the partial refreshes wore off
            if let Some(v) = policy.shown() {
                epd.show(&render(v), true).await;
            }
            policy.cleaned(now_ms());
            continue;
        };
        // show the latest state once its time has come (at once if it cannot wait)
        loop {
            match policy.due(&view) {
                Some(at) if at > now_ms() => match embassy_time::with_deadline(Instant::from_millis(at), SCREEN.wait()).await {
                    Ok(newer) => view = newer,
                    Err(_) => break,
                },
                _ => break,
            }
        }
        if policy.due(&view).is_none() {
            continue;
        }
        let off = matches!(view.body, Body::Off { .. });
        let full = policy.full(&view);
        epd.show(&render(&view), full).await;
        policy.drawn(view, full, now_ms());
        if off {
            epd.sleep().await;
            crate::board::system_off();
        }
    }
}
