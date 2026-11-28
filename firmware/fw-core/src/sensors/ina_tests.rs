use embassy_futures::block_on;
use embedded_hal_async::i2c::{ErrorKind, ErrorType, I2c, Operation};

use super::{ina219::Ina219, ina226::Ina226};

#[derive(Clone, Copy, Debug)]
enum Exchange {
    Write(u8, u16),
    Read(u8, Result<u16, ErrorKind>),
}

use Exchange::{Read, Write};

struct ScriptBus<'a> {
    address: u8,
    remaining: &'a [Exchange],
}

impl<'a> ScriptBus<'a> {
    fn new(address: u8, remaining: &'a [Exchange]) -> Self {
        Self { address, remaining }
    }

    fn assert_finished(&self) {
        assert!(self.remaining.is_empty(), "unconsumed I2C exchanges: {:?}", self.remaining);
    }
}

impl ErrorType for ScriptBus<'_> {
    type Error = ErrorKind;
}

impl I2c for ScriptBus<'_> {
    async fn transaction(&mut self, address: u8, operations: &mut [Operation<'_>]) -> Result<(), Self::Error> {
        assert_eq!(address, self.address);
        let (exchange, remaining) = self.remaining.split_first().expect("unexpected I2C exchange");
        self.remaining = remaining;
        match (*exchange, operations) {
            (Write(reg, value), [Operation::Write(bytes)]) => {
                let [hi, lo] = value.to_be_bytes();
                assert_eq!(*bytes, &[reg, hi, lo]);
                Ok(())
            }
            (Read(reg, value), [Operation::Write(request), Operation::Read(response)]) => {
                assert_eq!(*request, &[reg]);
                response.copy_from_slice(&value?.to_be_bytes());
                Ok(())
            }
            _ => panic!("I2C exchange does not match {exchange:?}"),
        }
    }
}

#[test]
fn ina219_reads_the_completed_triggered_sample() {
    let script = [
        Write(0x00, 0x3998), // Probe in power-down.
        Write(0x05, 4096),
        Write(0x05, 4096),
        Write(0x00, 0x399B), // Trigger shunt and bus together.
        Read(0x02, Ok(0x4E20)), // Stale bus voltage, CNVR clear.
        Read(0x02, Ok(0x5DC2)), // 12 V, CNVR set.
        Read(0x04, Ok((-1250i16) as u16)),
        Read(0x03, Ok(600)),
        Write(0x00, 0x3998),
    ];
    let mut bus = ScriptBus::new(0x40, &script);
    let sensor = block_on(Ina219::probe(&mut bus)).unwrap();
    let reading = block_on(sensor.read(&mut bus)).unwrap();
    assert!((reading.bus_v - 12.0).abs() < 0.001);
    assert_eq!(reading.current_ma, -125.0);
    assert_eq!(reading.power_mw, 1200.0);
    bus.assert_finished();
}

#[test]
fn ina226_reads_the_completed_triggered_sample() {
    let script = [
        Read(0xFF, Ok(0x2260)),
        Write(0x00, 0x4520), // Probe in power-down.
        Write(0x05, 2097),
        Write(0x05, 2097),
        Write(0x00, 0x4523), // Trigger averaged shunt and bus measurements.
        Read(0x06, Ok(0)),
        Read(0x06, Ok(1 << 3)),
        Read(0x02, Ok(9600)),
        Read(0x04, Ok((-4096i16) as u16)),
        Read(0x03, Ok(2048)),
        Write(0x00, 0x4520),
    ];
    let mut bus = ScriptBus::new(0x44, &script);
    let sensor = block_on(Ina226::probe(&mut bus)).unwrap();
    let reading = block_on(sensor.read(&mut bus)).unwrap();
    assert!((reading.bus_v - 12.0).abs() < 0.001);
    assert!((reading.current_ma + 100.0).abs() < 0.001);
    assert!((reading.power_mw - 1250.0).abs() < 0.001);
    bus.assert_finished();
}

#[test]
fn ina219_resumes_after_conversion_timeout() {
    let script = [
        Write(0x05, 4096),
        Write(0x00, 0x399B),
        Read(0x02, Ok(0x5DC0)),
        Read(0x02, Ok(0x5DC0)),
        Read(0x02, Ok(0x5DC0)),
        Write(0x00, 0x3998),
        Write(0x05, 4096),
        Write(0x00, 0x399B),
        Read(0x02, Ok(0x5DC2)),
        Read(0x04, Ok(1250)),
        Read(0x03, Ok(600)),
        Write(0x00, 0x3998),
    ];
    let mut bus = ScriptBus::new(0x40, &script);
    assert!(block_on(Ina219.read(&mut bus)).is_none());
    assert!(block_on(Ina219.read(&mut bus)).is_some());
    bus.assert_finished();
}

#[test]
fn ina226_resumes_after_a_failed_result_read() {
    let script = [
        Write(0x05, 2097),
        Write(0x00, 0x4523),
        Read(0x06, Ok(1 << 3)),
        Read(0x02, Ok(9600)),
        Read(0x04, Err(ErrorKind::Other)),
        Write(0x00, 0x4520),
        Write(0x05, 2097),
        Write(0x00, 0x4523),
        Read(0x06, Ok(1 << 3)),
        Read(0x02, Ok(9600)),
        Read(0x04, Ok(4096)),
        Read(0x03, Ok(2048)),
        Write(0x00, 0x4520),
    ];
    let mut bus = ScriptBus::new(0x44, &script);
    assert!(block_on(Ina226.read(&mut bus)).is_none());
    assert!(block_on(Ina226.read(&mut bus)).is_some());
    bus.assert_finished();
}
