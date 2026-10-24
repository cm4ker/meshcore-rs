//! The on-device interface of the MeshCore firmware, shared by every board with a screen.
//!
//! - [`model`]: what a screen shows, as a [`View`] without pixels: the frame every page sits in
//!   (top bar, page dots, notices, the pairing PIN) and each role's pages (the companion's
//!   messages and nodes nearby, the repeater's passport, neighbours and traffic);
//! - [`input`] and [`nav`]: buttons turned into intents (next, back, act), and which page is up;
//! - [`layout`]: one layout per screen class (square e-paper, 128 x 64 OLED), not per board,
//!   built from the shared blocks in [`widgets`] with the class's fonts ([`theme`]);
//! - [`refresh`]: when to redraw, per screen technology (e-paper, OLED).
//!
//! The panel driver, its rotation and the buttons stay with the board, which picks a class.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod input;
pub mod layout;
pub mod model;
pub mod nav;
pub mod refresh;
#[cfg(any(test, feature = "samples"))]
pub mod samples;
pub mod text;
pub mod theme;
pub mod widgets;

pub use input::{Button, Intent};
pub use model::*;
pub use nav::Ui;
pub use text::{battery_percent, coarse_minutes};
