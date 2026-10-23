//! The LoRa transceiver interface the firmware drives (SX1262, LR2021).

use embedded_hal::digital::InputPin;
use embedded_hal_async::digital::Wait;
use meshcore::radio::RadioParams;

/// The radio's interrupt line.
// Only used generically (never as `dyn`), so the futures' `Send`-ness does not matter.
#[allow(async_fn_in_trait)]
pub trait IrqPin {
    /// Wait until the line is high.
    async fn wait_high(&mut self);

    fn is_high(&mut self) -> bool;

    /// Let the line wake the chip from light sleep. False if it is already high (an interrupt is
    /// pending: do not sleep). A platform that does not sleep that way has nothing to arm.
    fn arm_wake(&mut self) -> bool {
        !self.is_high()
    }
}

/// Any embedded-hal input that can wait for a level, as an [`IrqPin`].
pub struct WaitIrq<P>(pub P);

impl<P: InputPin + Wait> IrqPin for WaitIrq<P> {
    async fn wait_high(&mut self) {
        let _ = self.0.wait_for_high().await;
    }

    fn is_high(&mut self) -> bool {
        self.0.is_high().unwrap_or(false)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// BUSY stayed high.
    Busy,
    /// No (or the wrong) chip answered.
    NotFound,
    TxTimeout,
    /// The chip rejected a command or failed to calibrate.
    Command,
}

pub struct RxFrame {
    pub data: [u8; 256],
    pub len: usize,
    pub rssi: f32,
    pub snr: f32,
}

// Moved once per received frame; boxing would put the receive path on the heap.
#[allow(clippy::large_enum_variant)]
pub enum RxEvent {
    Frame(RxFrame),
    CrcError,
    Nothing,
}

/// What MeshCore needs from a LoRa radio: explicit header, CRC on, private sync word 0x12.
// Only used generically (never as `dyn`), so the futures' `Send`-ness does not matter.
#[allow(async_fn_in_trait)]
pub trait LoraRadio {
    /// Reset and configure the modem, then start receiving.
    async fn init(&mut self, params: RadioParams, tx_power: i8, rx_boost: bool) -> Result<(), Error>;

    /// Frequency / bandwidth / SF / CR (and the derived preamble and receive timeouts).
    fn apply_params(&mut self, params: RadioParams) -> Result<(), Error>;

    /// Change only the coding rate for the next frame (dynamic CR); `None` restores the configured one.
    fn set_frame_cr(&mut self, cr: Option<u8>) -> Result<(), Error>;

    fn set_tx_power(&mut self, dbm: i8) -> Result<(), Error>;

    fn set_rx_boosted_gain(&mut self, on: bool) -> Result<(), Error>;

    /// Continuous receive; the IRQ line rises on RX-done.
    fn start_rx(&mut self) -> Result<(), Error>;

    /// Wait for the IRQ line (a frame arrived).
    async fn wait_irq(&mut self);

    /// Let the IRQ line wake the chip from light sleep. False if an IRQ is already pending: then
    /// do not sleep.
    fn arm_wake(&mut self) -> bool;

    /// Read whatever completed (after [`Self::wait_irq`]); the radio stays in RX.
    fn read_rx(&mut self) -> Result<RxEvent, Error>;

    /// Instantaneous RSSI in dBm.
    fn rssi_inst(&mut self) -> Result<i16, Error>;

    /// Is a packet arriving right now (preamble or header seen, not stale)?
    fn is_receiving(&mut self) -> Result<bool, Error>;

    async fn random_bytes(&mut self, out: &mut [u8]) -> Result<(), Error>;

    /// Transmit one frame and wait for TX-done; call [`Self::start_rx`] afterwards.
    async fn transmit(&mut self, frame: &[u8], timeout_ms: u32) -> Result<(), Error>;

    /// Channel activity detection; leaves the radio back in RX.
    async fn channel_activity(&mut self) -> Result<bool, Error>;

    /// Receiver reset against a stuck AGC; leaves the radio in RX.
    async fn reset_agc(&mut self) -> Result<(), Error>;

    fn sleep(&mut self) -> Result<(), Error>;

    fn params(&self) -> RadioParams;
}
