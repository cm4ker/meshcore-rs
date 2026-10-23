//! The T-Echo's 1.54" e-paper (GDEH0154D67, SSD1681 controller, 200 x 200): the companion's
//! status screen, drawn in its own task so a refresh (up to 2 s) never holds up the radio.
//!
//! The screen is redrawn only when what it shows changed, at most every [`MIN_REFRESH_MS`] (an
//! e-paper keeps its image without power, and refreshes are slow and wear it), and only the
//! messages waiting or the link note forces an earlier one. Most refreshes are partial; every
//! [`FULL_EVERY`]th one is full, to clear the ghosting partial refreshes leave.

use alloc::format;
use alloc::string::String;

use embassy_nrf::gpio::{Input, Output};
use embassy_nrf::spim::Spim;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Instant, Timer};
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::mono_font::iso_8859_5::{FONT_6X10, FONT_9X15_BOLD};
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Line, PrimitiveStyle};
use embedded_graphics::text::{Baseline, Text};
use meshcore_fw_core::companion::Status;

const WIDTH: usize = 200;
const HEIGHT: usize = 200;
const MIN_REFRESH_MS: u64 = 30_000;
const FULL_EVERY: u32 = 10;

/// What the screen shows: a copy of the companion's status, without the fields that change all
/// the time (uptime, noise floor), which would otherwise force a refresh every time.
#[derive(Clone, PartialEq, Default)]
pub struct Snapshot {
    pub name: String,
    pub radio: String,
    pub id: String,
    pub batt_mv: u16,
    pub contacts: usize,
    pub rx: u32,
    pub tx: u32,
    pub note: Option<String>,
}

impl Snapshot {
    pub fn of(s: &Status<'_>) -> Self {
        Self {
            name: s.name.into(),
            radio: s.radio.into(),
            id: s.id_prefix.into(),
            // to 0.05 V: the reading wanders by a few millivolts
            batt_mv: s.batt_mv / 50 * 50,
            contacts: s.contacts,
            rx: s.rx,
            tx: s.tx,
            note: s.note.map(String::from),
        }
    }
}

pub static SCREEN: Signal<CriticalSectionRawMutex, Snapshot> = Signal::new();

/// A 1-bit frame in the controller's layout: rows of 25 bytes, bit 7 leftmost, 1 = white.
struct Frame([u8; WIDTH * HEIGHT / 8]);

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
                let bit = 0x80 >> (p.x as usize % 8);
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
        let _ = embassy_time::with_timeout(embassy_time::Duration::from_secs(5), self.busy.wait_for_low()).await;
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
}

fn render(s: &Snapshot) -> Frame {
    let mut f = Frame([0xFF; WIDTH * HEIGHT / 8]);
    let big = MonoTextStyle::new(&FONT_9X15_BOLD, BinaryColor::On);
    let small = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let text = |f: &mut Frame, s: &str, y: i32, style| {
        let _ = Text::with_baseline(s, Point::new(4, y), style, Baseline::Top).draw(f);
    };
    // names may hold characters the font lacks (emoji): they come out as '?'
    let name: String = s.name.chars().take(21).collect();
    text(&mut f, &name, 4, big);
    let _ = Line::new(Point::new(0, 24), Point::new(WIDTH as i32 - 1, 24))
        .into_styled(PrimitiveStyle::with_stroke(BinaryColor::On, 1))
        .draw(&mut f);
    text(&mut f, &s.radio, 32, small);
    text(&mut f, &s.id, 46, small);
    let batt = format!("Battery {}.{:02} V", s.batt_mv / 1000, s.batt_mv % 1000 / 10);
    text(&mut f, &batt, 64, small);
    text(&mut f, &format!("Contacts {}", s.contacts), 78, small);
    text(&mut f, &format!("RX {}  TX {}", s.rx, s.tx), 92, small);
    if let Some(note) = &s.note {
        let note: String = note.chars().take(21).collect();
        text(&mut f, &note, 120, big);
    }
    f
}

#[embassy_executor::task]
pub async fn task(mut epd: Epaper) {
    epd.init().await;
    let mut shown: Option<Snapshot> = None;
    let mut last = Instant::from_millis(0);
    let mut count = 0u32;
    loop {
        let mut snap = SCREEN.wait().await;
        if shown.as_ref() == Some(&snap) {
            continue;
        }
        // a note change (messages, app link) is worth an early refresh; counters can wait
        let urgent = shown.as_ref().is_none_or(|s| s.note != snap.note || s.name != snap.name);
        if !urgent {
            let wait = (last + embassy_time::Duration::from_millis(MIN_REFRESH_MS)).saturating_duration_since(Instant::now());
            Timer::after(wait).await;
            if let Some(newer) = SCREEN.try_take() {
                snap = newer; // show the latest state
            }
        }
        let full = count.is_multiple_of(FULL_EVERY);
        epd.show(&render(&snap), full).await;
        count += 1;
        last = Instant::now();
        shown = Some(snap);
    }
}
