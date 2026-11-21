//! MeshPocket's GDEM0213B74 panel (SSD1680), with the same SPI initialization and
//! refresh sequences as the GxEPD2 driver used by upstream MeshCore.

use embassy_nrf::gpio::{Input, Output};
use embassy_nrf::spim::{self, Spim};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer, with_deadline, with_timeout};
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use meshcore_ui::layout::Class;
use meshcore_ui::refresh::epaper::Epaper as Policy;
use meshcore_ui::{Body, View};

const WIDTH: usize = 250;
const HEIGHT: usize = 122;
const ROW_BYTES: usize = 16; // 128 controller pixels, 122 visible
const LAST_ROW: u8 = (WIDTH - 1) as u8;

pub static SCREEN: Signal<CriticalSectionRawMutex, Option<View>> = Signal::new();
/// The display task has finished attempting to draw the shutdown page and sleep the panel.
pub static OFF_DRAWN: Signal<CriticalSectionRawMutex, ()> = Signal::new();

struct Frame([u8; ROW_BYTES * WIDTH]);

impl Frame {
    fn render(view: &View) -> Self {
        let mut frame = Self([0xFF; ROW_BYTES * WIDTH]);
        Class::EpaperWide.render(view, &mut frame);
        frame
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

    fn draw_iter<I: IntoIterator<Item = Pixel<BinaryColor>>>(
        &mut self,
        pixels: I,
    ) -> Result<(), Self::Error> {
        for Pixel(p, color) in pixels {
            if (0..WIDTH as i32).contains(&p.x) && (0..HEIGHT as i32).contains(&p.y) {
                // Upstream's landscape rotation (3). A cleared bit is black.
                let x = p.y as usize;
                let y = WIDTH - 1 - p.x as usize;
                let byte = &mut self.0[y * ROW_BYTES + x / 8];
                let bit = 0x80 >> (x % 8);
                if color.is_on() {
                    *byte &= !bit;
                } else {
                    *byte |= bit;
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

enum Error {
    Spi,
    Busy,
}

impl From<spim::Error> for Error {
    fn from(_: spim::Error) -> Self {
        Self::Spi
    }
}

impl Epaper {
    async fn wait_idle(&mut self) -> Result<(), Error> {
        // Let BUSY assert after the command. A full refresh takes about 3.6 s.
        Timer::after_millis(1).await;
        with_timeout(Duration::from_secs(5), self.busy.wait_for_low())
            .await
            .map_err(|_| Error::Busy)
    }

    async fn command(&mut self, cmd: u8, data: &[u8]) -> Result<(), Error> {
        self.dc.set_low();
        self.cs.set_low();
        let sent = self.spi.write(&[cmd]).await;
        self.cs.set_high();
        sent?;
        if !data.is_empty() {
            self.dc.set_high();
            self.cs.set_low();
            let sent = self.spi.write(data).await;
            self.cs.set_high();
            sent?;
        }
        Ok(())
    }

    async fn init(&mut self) -> Result<(), Error> {
        self.reset.set_high();
        Timer::after_millis(10).await;
        self.reset.set_low();
        Timer::after_millis(2).await;
        self.reset.set_high();
        Timer::after_millis(10).await;
        self.wait_idle().await?;
        self.command(0x12, &[]).await?; // software reset
        Timer::after_millis(10).await;
        self.wait_idle().await?;
        self.command(0x01, &[LAST_ROW, 0, 0]).await?; // gate lines
        self.command(0x3C, &[0x05]).await?; // border waveform
        self.command(0x21, &[0x00, 0x80]).await?; // display update control
        self.command(0x18, &[0x80]).await?; // internal temperature sensor
        self.command(0x11, &[0x03]).await?; // x then y, both increasing
        self.command(0x44, &[0x00, (ROW_BYTES - 1) as u8]).await?;
        self.command(0x45, &[0x00, 0x00, LAST_ROW, 0x00]).await
    }

    async fn write_ram(&mut self, plane: u8, frame: &Frame) -> Result<(), Error> {
        self.command(0x4E, &[0x00]).await?;
        self.command(0x4F, &[0x00, 0x00]).await?;
        self.command(plane, &frame.0).await
    }

    async fn update(&mut self, control: u8) -> Result<(), Error> {
        self.command(0x22, &[control]).await?;
        self.command(0x20, &[]).await?;
        self.wait_idle().await
    }

    async fn draw(&mut self, view: &View, full: bool, init: bool) -> Result<(), Error> {
        if init {
            self.init().await?;
        }
        let frame = Frame::render(view);
        if full {
            self.write_ram(0x26, &frame).await?;
        }
        self.write_ram(0x24, &frame).await?;
        self.update(if full { 0xF7 } else { 0xFC }).await?;
        // Make both RAM planes match the displayed image for the next differential update.
        self.write_ram(0x26, &frame).await?;
        self.write_ram(0x24, &frame).await?;
        if !full {
            self.update(0x83).await?; // turn off panel driving voltages
        }
        Ok(())
    }
}

#[embassy_executor::task]
pub async fn task(mut panel: Epaper) {
    let mut policy = Policy::default();
    let mut failure_logged = false;
    'updates: loop {
        let (mut view, clean) = match policy.clean_at() {
            Some(at) => match with_deadline(Instant::from_millis(at), SCREEN.wait()).await {
                Ok(Some(view)) => (view, false),
                Ok(None) => continue,
                Err(_) => {
                    let Some(shown) = policy.shown() else {
                        continue;
                    };
                    (shown.clone(), true)
                }
            },
            None => match SCREEN.wait().await {
                Some(view) => (view, false),
                None => continue,
            },
        };
        if !clean {
            loop {
                let Some(at) = policy.due(&view) else {
                    continue 'updates;
                };
                if at <= Instant::now().as_millis() {
                    break;
                }
                match with_deadline(Instant::from_millis(at), SCREEN.wait()).await {
                    Ok(Some(newer)) => view = newer,
                    Ok(None) => continue 'updates,
                    Err(_) => break,
                }
            }
        }
        let off = matches!(view.body, Body::Off { .. });
        let full = clean || policy.full(&view);
        let result = panel.draw(&view, full, policy.shown().is_none()).await;
        match &result {
            Ok(()) => {
                failure_logged = false;
                let now = Instant::now().as_millis();
                if clean {
                    policy.cleaned(now);
                } else {
                    policy.drawn(view, full, now);
                }
            }
            Err(error) => {
                if !failure_logged {
                    match error {
                        Error::Spi => log::warn!("MeshPocket e-paper SPI transfer failed"),
                        Error::Busy => log::warn!("MeshPocket e-paper BUSY timed out"),
                    }
                    failure_logged = true;
                }
                policy = Policy::default();
            }
        }
        if off {
            if result.is_ok() {
                let _ = panel.command(0x10, &[0x01]).await; // controller deep sleep, image retained
            }
            OFF_DRAWN.signal(());
            return;
        }
    }
}
