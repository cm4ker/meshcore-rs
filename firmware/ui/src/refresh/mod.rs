//! When to redraw, per screen technology: an e-paper keeps its image without power but is slow
//! and wears with every refresh; an OLED redraws in a moment but glows (and ages) while lit.

pub mod epaper;
pub mod oled;
