//! The Wireless Paper's 2.13" e-paper (250 x 122 of a 128 x 250 controller memory): the
//! interface's pages in the wide e-paper layout ([`meshcore_ui::layout::Class::EpaperWide`]),
//! drawn in their own task so a refresh never holds up the radio, when the e-paper refresh policy
//! ([`meshcore_ui::refresh::epaper`]) says so.
//!
//! The board came with two panels (upstream's `E213Display`, after todd-herbert's
//! heltec-eink-modules): V1.1's LCMEN2R13EFC1 (a Fitipower controller, BUSY low while busy, its
//! partial refresh from the waveforms below) and V1.1.1's E0213A367 (Solomon SSD1682, BUSY high
//! while busy, both refreshes from the panel's own waveforms with a pulse added, see
//! [`Driver::load_waveform`]). Held in reset, the controller shows which by its BUSY level.

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer, with_deadline, with_timeout};
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embassy_executor::Spawner;
use esp_hal::Blocking;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::peripherals::{GPIO2, GPIO3, GPIO4, GPIO5, GPIO6, GPIO7, SPI3};
use esp_hal::spi::Mode as SpiMode;
use esp_hal::spi::master::{Address, Command, Config as SpiConfig, DataMode, Spi};
use esp_hal::time::Rate;
use meshcore_ui::layout::Class;
use meshcore_ui::refresh::epaper::Epaper as Policy;
use meshcore_ui::{Body, View};

/// The drawing, landscape.
pub const WIDTH: usize = 250;
pub const HEIGHT: usize = 122;
/// The controller's memory: rows of 16 bytes (128 pixels, of which the first 122 show), 250 rows.
const ROW_BYTES: usize = 16;
const ROWS: usize = 250;

pub static SCREEN: Signal<CriticalSectionRawMutex, View> = Signal::new();
/// The "off" page is on the screen.
pub static OFF_DRAWN: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// The partial refresh of V1.1's panel (heltec-eink-modules' LCMEN2R13EFC1, after GxEPD2's
/// GDEW0154M09): VCOM, then white to white, black to white, white to black, black to black.
const LUT_FC1: [[u8; 56]; 5] = [
    lut([0x01, 0x06, 0x03, 0x02, 0x01, 0x01, 0x01], [0x01, 0x06, 0x02, 0x01, 0x01, 0x01, 0x01]),
    lut([0x01, 0x06, 0x03, 0x02, 0x81, 0x01, 0x01], [0x01, 0x06, 0x02, 0x01, 0x01, 0x01, 0x01]),
    lut([0x01, 0x86, 0x83, 0x82, 0x81, 0x01, 0x01], [0x01, 0x86, 0x82, 0x01, 0x01, 0x01, 0x01]),
    lut([0x01, 0x46, 0x43, 0x02, 0x01, 0x01, 0x01], [0x01, 0x46, 0x42, 0x01, 0x01, 0x01, 0x01]),
    lut([0x01, 0x06, 0x03, 0x42, 0x41, 0x01, 0x01], [0x01, 0x06, 0x02, 0x01, 0x01, 0x01, 0x01]),
];

/// Two groups of seven, the other six empty.
const fn lut(a: [u8; 7], b: [u8; 7]) -> [u8; 56] {
    let mut t = [0u8; 56];
    let mut i = 0;
    while i < 7 {
        t[i] = a[i];
        t[7 + i] = b[i];
        i += 1;
    }
    t
}

/// The SSD1682's waveform register (0x32 to write, 0x33 to read), laid out as the SSD1680's: for
/// each of 12 groups the voltage of its four phases (two bits each) per transition (black to black,
/// black to white, white to black, white to white, then VCOM), from byte 60 the groups' timing
/// (frames of phases A and B, a repeat, of C and D, a repeat, repeats of the group), then frame
/// rates and gate scan.
const WAVEFORM: usize = 153;
const TIMING: usize = 60;
/// A group in which each pixel is driven towards its own colour: VSH1 (0b01) draws black, VSL
/// (0b10) white.
const HOLD: [u8; 5] = [0x40, 0x80, 0x40, 0x80, 0x00];

/// Appends a [`HOLD`] group to the panel's own waveform: a frame per 15 that the waveform drives.
/// Its partial waveform drives only the pixels that change and leaves the rest at VCOM, which
/// darkens the white a little each time (filmed: from 154 to 141 of the camera's levels in ten
/// refreshes, the black untouched); with the pulse the white and the black hold through any number
/// of refreshes, and its full waveform ends the darker and the whiter for it. None if the waveform
/// read back has no groups (the read failed) or no room.
fn with_hold(mut w: [u8; WAVEFORM]) -> Option<[u8; WAVEFORM]> {
    let mut frames = 0u32;
    for g in 0..12 {
        let t = &w[TIMING + g * 7..TIMING + g * 7 + 7];
        if t.iter().any(|&b| b != 0) {
            frames += (t[0] as u32 + t[1] as u32 + t[3] as u32 + t[4] as u32) * (t[6] as u32 + 1);
            continue;
        }
        if frames == 0 {
            return None;
        }
        for (i, v) in HOLD.iter().enumerate() {
            w[i * 12 + g] = *v;
        }
        w[TIMING + g * 7] = frames.div_ceil(15) as u8;
        return Some(w);
    }
    None
}

/// A 1-bit frame in the controller's layout (bit 7 leftmost, 1 = white), drawn turned as
/// upstream's (rotation 3).
pub struct Frame([u8; ROW_BYTES * ROWS]);

impl Frame {
    pub fn blank() -> Self {
        Self([0xFF; ROW_BYTES * ROWS])
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
                let (x, y) = (p.y as usize, ROWS - 1 - p.x as usize);
                let i = y * ROW_BYTES + x / 8;
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Panel {
    /// V1.1: LCMEN2R13EFC1, Fitipower.
    Fc1,
    /// V1.1.1: E0213A367, Solomon SSD1682.
    Ssd1682,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Full,
    Fast,
}

pub struct Epaper<'d> {
    spi: Spi<'d, Blocking>,
    cs: Output<'d>,
    dc: Output<'d>,
    reset: Output<'d>,
    busy: Input<'d>,
}

/// The screen on its pins (SCK 3, data 2, CS 4, DC 5, RST 6, BUSY 7), drawn by its own task from
/// what [`SCREEN`] is given.
#[allow(clippy::too_many_arguments)]
pub fn start(
    spawner: Spawner, spi: SPI3<'static>, sck: GPIO3<'static>, data: GPIO2<'static>, cs: GPIO4<'static>,
    dc: GPIO5<'static>, reset: GPIO6<'static>, busy: GPIO7<'static>,
) {
    let spi = Spi::new(spi, SpiConfig::default().with_frequency(Rate::from_mhz(4)).with_mode(SpiMode::_0))
        .unwrap()
        .with_sck(sck)
        .with_sio0(data);
    let io = Epaper {
        spi,
        cs: Output::new(cs, Level::High, OutputConfig::default()),
        dc: Output::new(dc, Level::High, OutputConfig::default()),
        reset: Output::new(reset, Level::High, OutputConfig::default()),
        busy: Input::new(busy, InputConfig::default().with_pull(Pull::None)),
    };
    spawner.spawn(task(io).unwrap());
}

struct Driver<'d> {
    io: Epaper<'d>,
    panel: Panel,
    /// The refresh the controller is set up for (none since the last reset).
    mode: Option<Mode>,
}

impl Epaper<'_> {
    /// Which panel: held in reset, a Fitipower controller reads busy as low, a Solomon one as high.
    async fn detect(&mut self) -> Panel {
        self.reset.set_low();
        Timer::after_millis(10).await;
        let panel = if self.busy.is_low() { Panel::Fc1 } else { Panel::Ssd1682 };
        self.reset.set_high();
        panel
    }
}

impl<'d> Driver<'d> {
    async fn wait_idle(&mut self) {
        // a full refresh takes ~2 s
        let busy = &mut self.io.busy;
        let _ = match self.panel {
            Panel::Fc1 => with_timeout(Duration::from_secs(5), busy.wait_for_high()).await,
            Panel::Ssd1682 => with_timeout(Duration::from_secs(5), busy.wait_for_low()).await,
        };
    }

    /// Over the one data line, which the controller also answers on (its 3-wire mode).
    fn command(&mut self, cmd: u8, data: &[u8]) {
        self.io.cs.set_low();
        self.io.dc.set_low();
        let _ = self.io.spi.half_duplex_write(DataMode::Single, Command::None, Address::None, 0, &[cmd]);
        if !data.is_empty() {
            self.io.dc.set_high();
            let _ = self.io.spi.half_duplex_write(DataMode::Single, Command::None, Address::None, 0, data);
        }
        self.io.cs.set_high();
    }

    /// Reads what `cmd` answers into `out`.
    fn read(&mut self, cmd: u8, out: &mut [u8]) {
        self.io.cs.set_low();
        self.io.dc.set_low();
        let _ = self.io.spi.half_duplex_write(DataMode::Single, Command::None, Address::None, 0, &[cmd]);
        self.io.dc.set_high();
        let _ = self.io.spi.half_duplex_read(DataMode::Single, Command::None, Address::None, 0, out);
        self.io.cs.set_high();
    }

    /// SSD1682: loads the panel's own waveform for its temperature (the colder, the longer: 15
    /// frames for a partial refresh at 25 °C, 36 at 10 °C and below), with [`HOLD`] appended.
    /// Whether it is in the register: if not, the refresh loads the panel's own as it is.
    async fn load_waveform(&mut self, mode: Mode) -> bool {
        // clock on, the temperature and the waveform of display mode 1 (full) or 2, clock off
        self.command(0x22, &[if mode == Mode::Full { 0xB1 } else { 0xB9 }]);
        self.command(0x20, &[]);
        self.wait_idle().await;
        let mut w = [0u8; WAVEFORM];
        self.read(0x33, &mut w);
        match with_hold(w) {
            Some(w) => {
                self.command(0x32, &w);
                true
            }
            None => false,
        }
    }

    /// A hardware reset and the controller set up for `mode` (heltec-eink-modules' `reset`,
    /// `configFull`, `configPartial`).
    async fn setup(&mut self, mode: Mode) {
        self.io.reset.set_low();
        Timer::after_millis(10).await;
        self.io.reset.set_high();
        Timer::after_millis(10).await;
        self.wait_idle().await;
        match self.panel {
            Panel::Fc1 => match mode {
                Mode::Full => {
                    self.command(0x00, &[0xDF]); // panel setting: the panel's own waveform
                    self.command(0x50, &[0xB7]); // VCOM and data interval
                }
                Mode::Fast => {
                    self.command(0x00, &[0xFF]); // panel setting: the waveform from the registers
                    self.command(0x50, &[0xD7]);
                    for (i, t) in LUT_FC1.iter().enumerate() {
                        self.command(0x20 + i as u8, t);
                    }
                }
            },
            Panel::Ssd1682 => {
                self.command(0x12, &[]); // software reset
                self.wait_idle().await;
                // as Heltec's own driver: 250 gate lines, the controller's own temperature sensor
                // choosing the waveform (the reset default is an external one)
                self.command(0x01, &[(ROWS - 1) as u8, 0x00]);
                self.command(0x18, &[0x80]);
                // which of the panel's waveforms serve which refresh
                self.command(0x37, &[0x00, 0x80, 0x03, 0x0E]);
                self.command(0x3C, &[if mode == Mode::Full { 0x01 } else { 0x80 }]); // border
                self.wait_idle().await;
            }
        }
        self.mode = Some(mode);
    }

    /// Writes `frame` into the controller's memory for the new image (`new`) or the old one.
    fn write(&mut self, frame: &Frame, new: bool) {
        match self.panel {
            Panel::Fc1 => self.command(if new { 0x13 } else { 0x10 }, &frame.0),
            Panel::Ssd1682 => {
                self.command(0x11, &[0x03]); // x then y, both increasing
                self.command(0x44, &[0x00, (ROW_BYTES - 1) as u8]);
                self.command(0x45, &[0x00, (ROWS - 1) as u8]);
                self.command(0x4E, &[0x00]);
                self.command(0x4F, &[0x00]);
                self.command(if new { 0x24 } else { 0x26 }, &frame.0);
            }
        }
    }

    /// `own`: the SSD1682's waveform register holds [`load_waveform`](Self::load_waveform)'s.
    async fn refresh(&mut self, mode: Mode, own: bool) {
        match self.panel {
            Panel::Fc1 => {
                self.command(0x04, &[]); // power on
                self.wait_idle().await;
                self.command(0x12, &[]); // refresh
                self.wait_idle().await;
                self.command(0x02, &[]); // power off
            }
            Panel::Ssd1682 => {
                // clock and voltages on, [the temperature and the waveform loaded], display mode 1
                // (full) or 2, voltages and clock off
                let seq = match (mode, own) {
                    (Mode::Full, true) => 0xC7,
                    (Mode::Fast, true) => 0xCF,
                    (Mode::Full, false) => 0xF7,
                    (Mode::Fast, false) => 0xFF,
                };
                self.command(0x22, &[seq]);
                self.command(0x20, &[]);
                self.wait_idle().await;
            }
        }
    }

    /// Show `frame`: a full refresh, or a fast one against the image shown before.
    async fn show(&mut self, frame: &Frame, full: bool) {
        let mode = if full { Mode::Full } else { Mode::Fast };
        if self.mode != Some(mode) {
            self.setup(mode).await;
        }
        let own = self.panel == Panel::Ssd1682 && self.load_waveform(mode).await;
        self.write(frame, true);
        if full {
            self.write(frame, false);
        }
        self.refresh(mode, own).await;
        if !full {
            // the base of the next fast refresh
            self.write(frame, false);
            if self.panel == Panel::Ssd1682 {
                self.command(0x7F, &[]); // end of the image, no refresh
                self.wait_idle().await;
            }
        }
    }
}

fn render(v: &View) -> Frame {
    let mut f = Frame::blank();
    Class::EpaperWide.render(v, &mut f);
    f
}

fn now_ms() -> u64 {
    Instant::now().as_millis()
}

#[embassy_executor::task]
async fn task(mut io: Epaper<'static>) {
    let panel = io.detect().await;
    log::info!("e-paper: {panel:?}");
    let mut epd = Driver { io, panel, mode: None };
    let mut policy = Policy::default();
    loop {
        let next = match policy.clean_at() {
            Some(at) => with_deadline(Instant::from_millis(at), SCREEN.wait()).await.ok(),
            None => Some(SCREEN.wait().await),
        };
        let Some(mut view) = next else {
            // nothing changed for a while: restore the contrast the fast refreshes wore off
            if let Some(v) = policy.shown() {
                epd.show(&render(v), true).await;
            }
            policy.cleaned(now_ms());
            continue;
        };
        // show the latest state once its time has come (at once if it cannot wait)
        loop {
            match policy.due(&view) {
                Some(at) if at > now_ms() => match with_deadline(Instant::from_millis(at), SCREEN.wait()).await {
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
            OFF_DRAWN.signal(());
        }
    }
}
