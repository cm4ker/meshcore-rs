//! Draws every sample page ([`meshcore_ui::samples`]) of every screen class into one PNG sheet per
//! class, in the screen's own colours, and lists anything drawn off the screen or over
//! something else (exit code 1 if there is).
//!
//!     cargo run -p ui-preview [-- <output dir>]      (default: target/ui-preview)

use std::path::PathBuf;

use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::mono_font::ascii::FONT_6X10;
use embedded_graphics::pixelcolor::{BinaryColor, Rgb888};
use embedded_graphics::prelude::*;
use embedded_graphics::text::{Baseline, Text};
use meshcore_ui::layout::{Class, Probe, check};
use meshcore_ui::samples;

const COLUMNS: usize = 4;
const GAP: u32 = 12;
const LABEL: u32 = 14;

struct Sheet {
    w: u32,
    h: u32,
    px: Vec<Rgb888>,
}

impl OriginDimensions for Sheet {
    fn size(&self) -> Size {
        Size::new(self.w, self.h)
    }
}

impl DrawTarget for Sheet {
    type Color = Rgb888;
    type Error = core::convert::Infallible;

    fn draw_iter<I: IntoIterator<Item = Pixel<Rgb888>>>(&mut self, pixels: I) -> Result<(), Self::Error> {
        for Pixel(p, c) in pixels {
            if (0..self.w as i32).contains(&p.x) && (0..self.h as i32).contains(&p.y) {
                self.px[(p.y as u32 * self.w + p.x as u32) as usize] = c;
            }
        }
        Ok(())
    }
}

/// The screen's look: ink and background colours, and how much to enlarge it.
fn look(class: Class) -> (Rgb888, Rgb888, u32) {
    match class {
        Class::EpaperSquare => (Rgb888::new(24, 26, 24), Rgb888::new(232, 233, 224), 1),
        Class::Mono128x64 => (Rgb888::new(170, 220, 255), Rgb888::new(0, 0, 0), 2),
    }
}

fn main() {
    let out = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("target/ui-preview"));
    std::fs::create_dir_all(&out).expect("output directory");
    let views = samples::all();
    let mut problems = Vec::new();
    for class in [Class::EpaperSquare, Class::Mono128x64] {
        let (ink, paper, scale) = look(class);
        let size = class.size();
        let (tw, th) = (size.width * scale, size.height * scale);
        let rows = views.len().div_ceil(COLUMNS) as u32;
        let (w, h) = (GAP + COLUMNS as u32 * (tw + GAP), GAP + rows * (th + LABEL + GAP));
        let mut sheet = Sheet { w, h, px: vec![Rgb888::new(128, 128, 128); (w * h) as usize] };
        for (i, (name, v)) in views.iter().enumerate() {
            let (x0, y0) = (GAP + (i % COLUMNS) as u32 * (tw + GAP), GAP + (i / COLUMNS) as u32 * (th + LABEL + GAP));
            for y in 0..th {
                for x in 0..tw {
                    sheet.px[((y0 + y) * w + x0 + x) as usize] = paper;
                }
            }
            let mut probe = Probe::new(size);
            class.render(v, &mut probe);
            for p in probe.ink() {
                for dy in 0..scale {
                    for dx in 0..scale {
                        let (x, y) = (x0 + p.x as u32 * scale + dx, y0 + p.y as u32 * scale + dy);
                        sheet.px[(y * w + x) as usize] = ink;
                    }
                }
            }
            let found = check(class, v);
            let color = if found.is_empty() { Rgb888::BLACK } else { Rgb888::new(200, 0, 0) };
            let label = MonoTextStyle::new(&FONT_6X10, color);
            let _ = Text::with_baseline(name, Point::new(x0 as i32, (y0 + th + 2) as i32), label, Baseline::Top).draw(&mut sheet);
            problems.extend(found.into_iter().map(|p| format!("{class:?} {name}: {p}")));
        }
        let path = out.join(format!("{class:?}.png"));
        let file = std::io::BufWriter::new(std::fs::File::create(&path).expect("sheet file"));
        let mut enc = png::Encoder::new(file, w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let data: Vec<u8> = sheet.px.iter().flat_map(|c| [c.r(), c.g(), c.b()]).collect();
        enc.write_header().and_then(|mut wr| wr.write_image_data(&data)).expect("PNG");
        println!("{}", path.display());
    }
    for p in &problems {
        println!("problem: {p}");
    }
    let _ = BinaryColor::On;
    std::process::exit(if problems.is_empty() { 0 } else { 1 });
}
