//! The board's I2C bus, shared by everything on it: the sensors of `meshcore_fw_core::sensors`,
//! the screen, and the text-described sensors (`sensor add`). A Pro Micro puts the OLED and the
//! sensor connector on the same two pins (SDA P1.04, SCL P0.11), so the one TWIM is lent to each
//! transfer's owner in turn rather than owned by any of them.
//!
//! The bus is an `embedded-hal-async` I2C, so a driver written against that (the SSD1306 below,
//! the sensors) does not know it is waiting for another task to finish its transfer.

use core::ops::DerefMut;

use embassy_nrf::twim::{Error, Twim};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embedded_hal_async::i2c::{ErrorType, I2c, Operation};
use static_cell::StaticCell;

/// The board's one I2C bus, cheap to copy: each copy points at the same mutex.
#[derive(Clone, Copy)]
pub struct Bus(&'static Mutex<CriticalSectionRawMutex, Twim<'static>>);

impl Bus {
    /// Wraps the board's `Twim`. Called once per board, by its `board_parts!`; a second call would
    /// panic in `StaticCell`, and there is no second bus to wrap.
    pub fn new(twim: Twim<'static>) -> Self {
        static BUS: StaticCell<Mutex<CriticalSectionRawMutex, Twim<'static>>> = StaticCell::new();
        Self(BUS.init(Mutex::new(twim)))
    }
}

impl ErrorType for Bus {
    type Error = Error;
}

impl I2c for Bus {
    async fn transaction(&mut self, address: u8, operations: &mut [Operation<'_>]) -> Result<(), Error> {
        let mut bus = self.0.lock().await;
        bus.deref_mut().transaction(address, operations).await
    }
}
