use alloc::vec::Vec;
use core::cell::Cell;
use core::convert::Infallible;

use embassy_futures::block_on;
use embedded_hal::{digital, spi};

use super::*;

#[derive(Default)]
struct TestSpi {
    command: u8,
    bad_sync_reads: usize,
    writes: Vec<Vec<u8>>,
}

impl TestSpi {
    fn sent(&self, command: &[u8], args: &[u8]) -> bool {
        self.writes.windows(2).any(|w| w[0] == command && w[1] == args)
    }
}

impl spi::ErrorType for TestSpi {
    type Error = Infallible;
}

impl SpiBus for TestSpi {
    fn read(&mut self, words: &mut [u8]) -> Result<(), Self::Error> {
        words.fill(0);
        Ok(())
    }

    fn write(&mut self, words: &[u8]) -> Result<(), Self::Error> {
        if let [opcode] = words {
            self.command = *opcode;
        }
        self.writes.push(words.to_vec());
        Ok(())
    }

    fn transfer(&mut self, read: &mut [u8], _: &[u8]) -> Result<(), Self::Error> {
        self.read(read)
    }

    fn transfer_in_place(&mut self, words: &mut [u8]) -> Result<(), Self::Error> {
        if self.command == CMD_READ_REGISTER && words.len() == 2 {
            // A missing chip reads as zero. The only two-byte register read in init
            // is the sync word used to check whether the chip responded.
            if self.bad_sync_reads == 0 {
                words.copy_from_slice(&SYNC_WORD_PRIVATE);
            } else {
                self.bad_sync_reads -= 1;
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Default)]
struct ModemPins {
    busy: Cell<bool>,
    resets: Cell<usize>,
}

// Only RESET and BUSY use the shared state. NSS and DIO1 are inert in these tests.
struct TestPin<'a>(Option<&'a ModemPins>);

impl digital::ErrorType for TestPin<'_> {
    type Error = Infallible;
}

impl InputPin for TestPin<'_> {
    fn is_high(&mut self) -> Result<bool, Self::Error> {
        Ok(self.0.is_some_and(|pins| pins.busy.get()))
    }

    fn is_low(&mut self) -> Result<bool, Self::Error> {
        Ok(!InputPin::is_high(self)?)
    }
}

impl OutputPin for TestPin<'_> {
    fn set_low(&mut self) -> Result<(), Self::Error> {
        if let Some(pins) = self.0 {
            pins.resets.set(pins.resets.get() + 1);
            pins.busy.set(false);
        }
        Ok(())
    }

    fn set_high(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl IrqPin for TestPin<'_> {
    async fn wait_high(&mut self) {
        unreachable!("init and RX recovery must not wait for DIO1");
    }

    fn is_high(&mut self) -> bool {
        false
    }
}

fn radio(pins: &ModemPins) -> Sx1262<TestSpi, TestPin<'_>, TestPin<'_>, TestPin<'_>> {
    Sx1262::new(TestSpi::default(), TestPin(None), TestPin(Some(pins)), TestPin(Some(pins)), TestPin(None))
}

#[test]
fn init_retries_transient_failures() {
    let pins = ModemPins::default();
    let mut radio = radio(&pins);
    radio.spi.bad_sync_reads = 2;

    assert_eq!(block_on(radio.init_with_retry(RadioParams::default(), 22, true)), Ok(()));
    assert_eq!(pins.resets.get(), 3);
    assert!(radio.spi.sent(&[CMD_SET_RX], &[0xFF; 3]));
}

#[test]
fn init_returns_error_after_three_attempts() {
    let pins = ModemPins::default();
    let mut radio = radio(&pins);
    radio.spi.bad_sync_reads = 3;

    assert_eq!(block_on(radio.init_with_retry(RadioParams::default(), 22, true)), Err(Error::NotFound));
    assert_eq!(pins.resets.get(), 3);
    assert!(!radio.spi.sent(&[CMD_SET_RX], &[0xFF; 3]));
}

#[test]
fn post_tx_restores_cr_and_rx_without_reset() {
    let pins = ModemPins::default();
    let mut radio = radio(&pins);
    let params = RadioParams { cr: 7, ..RadioParams::default() };
    block_on(radio.init(params, 17, false)).unwrap();
    radio.set_frame_cr(Some(8)).unwrap();
    radio.spi.writes.clear();

    assert_eq!(block_on(radio.resume_rx_after_tx(true)), Ok(()));
    assert_eq!(pins.resets.get(), 1);
    assert!(radio.spi.sent(&[CMD_SET_MODULATION_PARAMS], &[8, 3, 3, 0]));
    assert!(radio.spi.sent(&[CMD_SET_RX], &[0xFF; 3]));
}

#[test]
fn post_tx_retries_reinitialisation_with_active_settings() {
    for restore_cr in [false, true] {
        let pins = ModemPins::default();
        let mut radio = radio(&pins);
        let params = RadioParams { freq_mhz: 868.25, bw_khz: 125.0, sf: 9, cr: 7 };
        block_on(radio.init(RadioParams::default(), 22, true)).unwrap();
        radio.apply_params(params).unwrap();
        radio.set_tx_power(17).unwrap();
        radio.set_rx_boosted_gain(false).unwrap();
        if restore_cr {
            radio.set_frame_cr(Some(8)).unwrap();
        }
        radio.spi.writes.clear();
        pins.busy.set(true); // RX or CR restore fails until the modem is reset.
        radio.spi.bad_sync_reads = 1; // The first full reinit fails too.

        assert_eq!(block_on(radio.resume_rx_after_tx(restore_cr)), Ok(()));
        assert_eq!(pins.resets.get(), 3);
        assert!(radio.spi.sent(&[CMD_SET_RF_FREQUENCY], &[0x36, 0x44, 0, 0]));
        assert!(radio.spi.sent(&[CMD_SET_MODULATION_PARAMS], &[9, 4, 3, 0]));
        assert!(radio.spi.sent(&[CMD_SET_TX_PARAMS], &[17, PA_RAMP_200U]));
        let [hi, lo] = REG_RX_GAIN.to_be_bytes();
        assert!(radio.spi.sent(&[CMD_WRITE_REGISTER, hi, lo], &[RX_GAIN_POWER_SAVING]));
        assert!(radio.spi.sent(&[CMD_SET_RX], &[0xFF; 3]));
    }
}

#[test]
fn post_tx_returns_error_when_rx_cannot_be_recovered() {
    let pins = ModemPins::default();
    let mut radio = radio(&pins);
    block_on(radio.init(RadioParams::default(), 22, true)).unwrap();
    radio.spi.writes.clear();
    pins.busy.set(true);
    radio.spi.bad_sync_reads = 3;

    assert_eq!(block_on(radio.resume_rx_after_tx(false)), Err(Error::NotFound));
    assert_eq!(pins.resets.get(), 4);
    assert!(!radio.spi.sent(&[CMD_SET_RX], &[0xFF; 3]));
}
