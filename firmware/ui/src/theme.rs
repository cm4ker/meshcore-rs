//! Fonts of each screen class, with the line pitch each is laid out at.
//!
//! The text is in U8g2's crox fonts (proportional, Helvetica-like), which keep Cyrillic where
//! code page 1251 has it, in place of Latin-1 ([`crate::text::cp1251`]); big numbers are in
//! digit-only fonts.

use u8g2_fonts::{FontRenderer, fonts};

pub struct Font {
    pub r: FontRenderer,
    /// From one line's top to the next.
    pub pitch: i32,
}

impl Font {
    const fn new(r: FontRenderer, pitch: i32) -> Self {
        Self { r, pitch }
    }
}

/// Square e-paper, 200 x 200 (T-Echo): 15 to 25 px type.
pub mod square {
    use super::*;

    pub static SMALL: Font = Font::new(FontRenderer::new::<fonts::u8g2_font_crox3h_tf>(), 15);
    pub static TEXT: Font = Font::new(FontRenderer::new::<fonts::u8g2_font_crox4h_tf>(), 20);
    pub static TEXT_BOLD: Font = Font::new(FontRenderer::new::<fonts::u8g2_font_crox4hb_tf>(), 20);
    pub static BIG: Font = Font::new(FontRenderer::new::<fonts::u8g2_font_crox5h_tf>(), 25);
    pub static BIG_BOLD: Font = Font::new(FontRenderer::new::<fonts::u8g2_font_crox5hb_tf>(), 25);
    /// Digits only (the pairing PIN, the unread count); in a box it takes 14 px more than its
    /// nominal 25.
    pub static HUGE_DIGITS: Font = Font::new(FontRenderer::new::<fonts::u8g2_font_inr33_t_cyrillic>(), 25 + 14);
    /// Digits only, for the frequency on the repeater's radio page.
    pub static FREQ_DIGITS: Font = Font::new(FontRenderer::new::<fonts::u8g2_font_logisoso32_tn>(), 36);
}

/// Monochrome 128 x 64 (Heltec OLED): 10 to 12 px type, digits up to 24 px.
pub mod mono {
    use super::*;

    pub static SMALL: Font = Font::new(FontRenderer::new::<fonts::u8g2_font_crox1h_tf>(), 11);
    pub static SMALL_BOLD: Font = Font::new(FontRenderer::new::<fonts::u8g2_font_crox1hb_tf>(), 11);
    pub static TEXT: Font = Font::new(FontRenderer::new::<fonts::u8g2_font_crox2h_tf>(), 13);
    pub static TEXT_BOLD: Font = Font::new(FontRenderer::new::<fonts::u8g2_font_crox2hb_tf>(), 13);
    pub static BIG_BOLD: Font = Font::new(FontRenderer::new::<fonts::u8g2_font_crox3hb_tf>(), 16);
    /// Digits only: the unread count, the pairing PIN.
    pub static DIGITS: Font = Font::new(FontRenderer::new::<fonts::u8g2_font_logisoso24_tn>(), 28);
    /// Digits only: the frequency, the update PIN.
    pub static MID_DIGITS: Font = Font::new(FontRenderer::new::<fonts::u8g2_font_logisoso16_tn>(), 20);
}
