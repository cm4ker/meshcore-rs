//! The T114's repeater pages on its TFT: the shared ones (`promicro::screen`, rendered in the TFT
//! class by [`crate::tft`]), with the GPS this board has and the Pro Micro does not.

use alloc::string::String;

use meshcore::mesh::Mesh;
use meshcore::repeater::Repeater;
use meshcore_ui::Action;

pub use promicro::screen::{PAGES, RepeaterUi, Screen, repeater_view};

/// The T114 around the repeater (`promicro::repeater_app`): the screen, turned with the button,
/// and the GPS.
pub struct Extras {
    pub ui: RepeaterUi,
    /// The binary's `BUILD_DATE`, for the version on the pages.
    pub build_date: &'static str,
}

impl promicro::repeater_app::Extras for Extras {
    fn has_gps(&self) -> bool {
        true
    }

    fn has_screen(&self) -> bool {
        true
    }

    fn set_gps(&mut self, on: bool) {
        crate::gps::GPS_ON.signal(on);
    }

    fn gps(&self) -> Option<meshcore::gps::GpsReading> {
        crate::gps::reading()
    }

    fn poll(&mut self, mesh: &mut Mesh<Repeater>, down: bool, window: Option<(u32, String, u64)>, now: u64) -> bool {
        matches!(self.ui.poll(mesh, down, window, now, self.build_date), Some(Action::ToggleUpdate))
    }

    fn wake(&mut self, now: u64) {
        self.ui.wake(now);
    }
}
