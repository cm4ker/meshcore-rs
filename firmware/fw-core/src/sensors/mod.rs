//! Sensors on a board's I2C that telemetry reports, for any board that has the bus: probed once at
//! boot, present or not (upstream scans for them the same way), then read for each refresh. Each
//! one found takes the next channel from 2 up, in upstream's `SENSOR_TABLE` order (AHTX0, BME280,
//! BMP280, SHTC3, SHT4X, INA219), so a board reports the same channels as under upstream; the
//! SHT3x, which upstream lacks, comes last.
//!
//! After the probe the whole bus is scanned once, and [`Sensors::scan`] says what answers and
//! which driver took it (`sensor scan`): a sensor that does not show in telemetry can be told from
//! one that is not there at all.
//!
//! The drivers borrow the bus per call, so the sensors share one I2C with the text-described
//! ones (`sensor add`, [`descriptor`]). The maths of each is sans-IO in `meshcore::sensor`.

pub mod ahtx0;
pub mod bme280;
pub mod bmp280;
pub mod descriptor;
pub mod ina219;
pub mod sht3x;
pub mod sht4x;
pub mod shtc3;

use core::fmt::Write as _;
use alloc::string::String;
use alloc::vec::Vec;

use embassy_time::{Duration, with_timeout};
use embedded_hal_async::i2c::{Error as _, ErrorKind, I2c, NoAcknowledgeSource};
use log::info;

use self::ahtx0::Ahtx0;
use self::bme280::Bme280;
use self::bmp280::Bmp280;
pub use self::descriptor::read_descriptor;
use self::ina219::Ina219;
use self::sht3x::Sht3x;
#[cfg(feature = "extra-sensors")]
use self::sht4x::Sht4x;
#[cfg(feature = "extra-sensors")]
use self::shtc3::Shtc3;

pub struct Sensors<I> {
    /// The bus, shared with the text-described sensors (`sensor add`).
    pub i2c: I,
    aht: Option<Ahtx0>,
    bme: Option<Bme280>,
    bmp: Option<Bmp280>,
    #[cfg(feature = "extra-sensors")]
    shtc3: Option<Shtc3>,
    #[cfg(feature = "extra-sensors")]
    sht4x: Option<Sht4x>,
    ina: Option<Ina219>,
    sht3x: Option<Sht3x>,
    /// What the boot scan found, for `sensor scan`.
    pub scan: String,
}

impl<I: I2c> Sensors<I> {
    /// The sensors that answer, and the scan of the bus (logged).
    pub async fn probe(mut i2c: I) -> Self {
        let aht = Ahtx0::probe(&mut i2c).await;
        let bme = Bme280::probe(&mut i2c).await;
        // the probe goes by chip id (a BME280 answers 0x60, a BMP280 0x58 at the same addresses):
        // it passes over a BME280 and finds a BMP280 at the other address, as upstream's scan does
        let bmp = Bmp280::probe(&mut i2c).await;
        #[cfg(feature = "extra-sensors")]
        let shtc3 = Shtc3::probe(&mut i2c).await;
        #[cfg(feature = "extra-sensors")]
        let sht4x = Sht4x::probe(&mut i2c).await;
        let ina = Ina219::probe(&mut i2c).await;
        let sht3x = Sht3x::probe(&mut i2c).await;

        let mut s = Self {
            i2c,
            aht,
            bme,
            bmp,
            #[cfg(feature = "extra-sensors")]
            shtc3,
            #[cfg(feature = "extra-sensors")]
            sht4x,
            ina,
            sht3x,
            scan: String::new(),
        };
        s.scan = s.scan_bus().await;
        info!("I2C: {}", s.scan);
        s
    }

    /// The sensors taken, in channel order, with their addresses.
    fn taken(&self) -> Vec<(&'static str, u8)> {
        let mut t = Vec::new();
        if self.aht.is_some() {
            t.push(("AHTX0", meshcore::sensor::ahtx0::ADDR));
        }
        if let Some(b) = &self.bme {
            t.push(("BME280", b.addr()));
        }
        if let Some(b) = &self.bmp {
            t.push(("BMP280", b.addr()));
        }
        #[cfg(feature = "extra-sensors")]
        if self.shtc3.is_some() {
            t.push(("SHTC3", meshcore::sensor::shtc3::ADDR));
        }
        #[cfg(feature = "extra-sensors")]
        if let Some(s) = &self.sht4x {
            t.push(("SHT4X", s.addr()));
        }
        if self.ina.is_some() {
            t.push(("INA219", ina219::ADDRESS));
        }
        if let Some(s) = &self.sht3x {
            t.push(("SHT3X", s.addr()));
        }
        t
    }

    /// Every address on the bus asked once, with a one-byte write (a read would miss the
    /// Sensirion chips, which refuse it with no measurement pending): the sensors taken, by
    /// channel, then whatever else answers. A stuck bus ends the scan.
    async fn scan_bus(&mut self) -> String {
        let taken = self.taken();
        let (mut other, mut stuck) = (Vec::new(), false);
        for addr in 0x08..=0x77u8 {
            match with_timeout(Duration::from_millis(50), self.i2c.write(addr, &[0])).await {
                Ok(Err(e)) if e.kind() == ErrorKind::NoAcknowledge(NoAcknowledgeSource::Address) => {}
                Ok(_) if taken.iter().any(|t| t.1 == addr) => {}
                Ok(_) => other.push(addr),
                Err(_) => {
                    stuck = true;
                    break;
                }
            }
        }
        let mut out = String::new();
        for (i, (name, a)) in taken.iter().enumerate() {
            let _ = write!(out, "{}ch{} {name} 0x{a:02X}", if i > 0 { ", " } else { "" }, i + 2);
        }
        for (i, a) in other.iter().enumerate() {
            let head = match (i, out.is_empty()) {
                (0, true) => "not known: ",
                (0, false) => ", not known: ",
                _ => " ",
            };
            let _ = write!(out, "{head}0x{a:02X}");
        }
        if out.is_empty() {
            out.push_str("nothing answers on the I2C");
        }
        if stuck {
            out.push_str("; the bus is stuck");
        }
        out
    }

    /// The readings as Cayenne LPP, appended to `lpp`; the next free channel. A sensor that does
    /// not answer this time keeps its channel, so the others' do not move.
    pub async fn read_lpp(&mut self, lpp: &mut Vec<u8>) -> u8 {
        let i2c = &mut self.i2c;
        let mut channel = meshcore::lpp::CHANNEL_SELF + 1;
        if let Some(a) = self.aht.as_ref() {
            if let Some(r) = a.read(i2c).await {
                meshcore::sensor::ahtx0::push_lpp(&r, channel, lpp);
            }
            channel += 1;
        }
        if let Some(b) = self.bme.as_ref() {
            if let Some(r) = b.read(i2c).await {
                meshcore::sensor::bme280::push_lpp(&r, channel, lpp);
            }
            channel += 1;
        }
        if let Some(b) = self.bmp.as_ref() {
            if let Some(r) = b.read(i2c).await {
                meshcore::sensor::bmp280::push_lpp(&r, channel, lpp);
            }
            channel += 1;
        }
        #[cfg(feature = "extra-sensors")]
        if let Some(s) = self.shtc3.as_ref() {
            if let Some(r) = s.read(i2c).await {
                meshcore::sensor::shtc3::push_lpp(&r, channel, lpp);
            }
            channel += 1;
        }
        #[cfg(feature = "extra-sensors")]
        if let Some(s) = self.sht4x.as_ref() {
            if let Some(r) = s.read(i2c).await {
                meshcore::sensor::sht4x::push_lpp(&r, channel, lpp);
            }
            channel += 1;
        }
        if let Some(s) = self.ina.as_ref() {
            if let Some(r) = s.read(i2c).await {
                r.lpp(channel, lpp);
            }
            channel += 1;
        }
        if let Some(s) = self.sht3x.as_ref() {
            if let Some(r) = s.read(i2c).await {
                meshcore::sensor::sht3x::push_lpp(&r, channel, lpp);
            }
            channel += 1;
        }
        channel
    }
}
