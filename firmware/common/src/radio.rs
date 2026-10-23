//! The LoRa radio interface (`meshcore_fw_core::radio`), and the ESP32 interrupt line for it.

use esp_hal::gpio::{Event, Input};
pub use meshcore_fw_core::radio::*;

/// A radio's interrupt line on an ESP32: listening for its high level also wakes the chip from
/// light sleep (`powersaving on`).
pub struct EspIrq<'d>(pub Input<'d>);

impl IrqPin for EspIrq<'_> {
    async fn wait_high(&mut self) {
        self.0.wait_for_high().await
    }

    fn is_high(&mut self) -> bool {
        self.0.is_high()
    }

    fn arm_wake(&mut self) -> bool {
        if self.0.is_high() {
            return false;
        }
        // a listening pin wakes the chip; the next wait_high takes the interrupt over
        self.0.listen(Event::HighLevel);
        true
    }
}
