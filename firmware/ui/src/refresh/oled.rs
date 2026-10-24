//! OLED: a page turn goes on the screen at once, other changes at most every [`MIN_REDRAW_MS`],
//! and only when the page differs from what is shown (each frame is a kilobyte over I2C). The
//! screen goes dark a minute after the last press ([`crate::Ui::with_screen_timeout`]), so it
//! never shows one image long enough to burn in, and needs no pixel shifting.

use crate::model::View;

pub const MIN_REDRAW_MS: u64 = 1000;

#[derive(Default)]
pub struct Oled {
    shown: Option<View>,
    last: u64,
}

impl Oled {
    /// Whether `v` should be drawn now: a page turn, a notice or a PIN at once, anything else
    /// once [`MIN_REDRAW_MS`] has passed.
    pub fn due(&self, v: &View, now: u64) -> bool {
        match &self.shown {
            None => true,
            Some(s) if s == v => false,
            Some(s) => s.page_idx != v.page_idx || s.alert != v.alert || s.pin != v.pin || now >= self.last + MIN_REDRAW_MS,
        }
    }

    pub fn drawn(&mut self, v: View, now: u64) {
        self.shown = Some(v);
        self.last = now;
    }

    /// The screen went dark or lost its image (a reset): draw the next page whatever it is.
    pub fn forget(&mut self) {
        self.shown = None;
    }
}
