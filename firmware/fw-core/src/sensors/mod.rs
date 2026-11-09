//! Sensors on a board's I2C that telemetry reports, for any board that has the bus: probed once at
//! boot, present or not (upstream scans for them the same way), then read for each refresh. Each
//! one found takes the next channel from 2 up, in upstream's `SENSOR_TABLE` order (AHTX0, BME280,
//! BMP280, SHTC3, SHT4X, INA219), so a board reports the same channels as under upstream.
//!
//! The drivers borrow the bus per call, so the sensors share one I2C with the text-described
//! ones (`sensor add`, [`descriptor`]). The maths of each is sans-IO in `meshcore::sensor`.

pub mod ahtx0;
pub mod bme280;
pub mod bmp280;
pub mod descriptor;
pub mod ina219;
pub mod sht4x;
pub mod shtc3;

use alloc::vec::Vec;

use embedded_hal_async::i2c::I2c;
use log::{info, warn};

#[cfg(feature = "extra-sensors")]
use self::ahtx0::Ahtx0;
use self::bme280::Bme280;
use self::bmp280::Bmp280;
pub use self::descriptor::read_descriptor;
use self::ina219::Ina219;
#[cfg(feature = "extra-sensors")]
use self::sht4x::Sht4x;
#[cfg(feature = "extra-sensors")]
use self::shtc3::Shtc3;

pub struct Sensors<I> {
    /// The bus, shared with the text-described sensors (`sensor add`).
    pub i2c: I,
    #[cfg(feature = "extra-sensors")]
    aht: Option<Ahtx0>,
    bme: Option<Bme280>,
    bmp: Option<Bmp280>,
    #[cfg(feature = "extra-sensors")]
    shtc3: Option<Shtc3>,
    #[cfg(feature = "extra-sensors")]
    sht4x: Option<Sht4x>,
    ina: Option<Ina219>,
}

impl<I: I2c> Sensors<I> {
    /// The sensors that answer, each with its first reading logged.
    pub async fn probe(mut i2c: I) -> Self {
        #[cfg(feature = "extra-sensors")]
        let aht = Ahtx0::probe(&mut i2c).await;
        #[cfg(feature = "extra-sensors")]
        match &aht {
            Some(a) => match a.read(&mut i2c).await {
                Some(r) => info!("AHTX0: {:.2} C, {:.1} %RH", r.temp_c, r.humidity_pct),
                None => warn!("AHTX0 found but no reading"),
            },
            None => info!("no AHTX0 on the I2C"),
        }
        let bme = Bme280::probe(&mut i2c).await;
        match &bme {
            Some(b) => match b.read(&mut i2c).await {
                Some(r) => info!("BME280: {:.2} C, {:.1} %RH, {:.1} hPa", r.temp_c, r.humidity_pct, r.pressure_hpa),
                None => warn!("BME280 found but no reading"),
            },
            None => info!("no BME280 on the I2C"),
        }
        // the probe goes by chip id (a BME280 answers 0x60, a BMP280 0x58 at the same addresses):
        // it passes over a BME280 and finds a BMP280 at the other address, as upstream's scan does
        let bmp = Bmp280::probe(&mut i2c).await;
        match &bmp {
            Some(b) => match b.read(&mut i2c).await {
                Some(r) => info!("BMP280: {:.2} C, {:.1} hPa", r.temp_c, r.pressure_hpa),
                None => warn!("BMP280 found but no reading"),
            },
            None => info!("no BMP280 on the I2C"),
        }
        #[cfg(feature = "extra-sensors")]
        let shtc3 = Shtc3::probe(&mut i2c).await;
        #[cfg(feature = "extra-sensors")]
        match &shtc3 {
            Some(s) => match s.read(&mut i2c).await {
                Some(r) => info!("SHTC3: {:.2} C, {:.1} %RH", r.temp_c, r.humidity_pct),
                None => warn!("SHTC3 found but no reading"),
            },
            None => info!("no SHTC3 on the I2C"),
        }
        #[cfg(feature = "extra-sensors")]
        let sht4x = Sht4x::probe(&mut i2c).await;
        #[cfg(feature = "extra-sensors")]
        match &sht4x {
            Some(s) => match s.read(&mut i2c).await {
                Some(r) => info!("SHT4X: {:.2} C, {:.1} %RH", r.temp_c, r.humidity_pct),
                None => warn!("SHT4X found but no reading"),
            },
            None => info!("no SHT4X on the I2C"),
        }
        let ina = Ina219::probe(&mut i2c).await;
        match &ina {
            Some(s) => match s.read(&mut i2c).await {
                Some(r) => info!("INA219 at 0x{:02X}: {:.3} V, {:.1} mA, {:.0} mW", ina219::ADDRESS, r.bus_v, r.current_ma, r.power_mw),
                None => warn!("INA219 at 0x{:02X}: no reading", ina219::ADDRESS),
            },
            None => info!("no INA219 on the I2C"),
        }
        Self {
            i2c,
            #[cfg(feature = "extra-sensors")]
            aht,
            bme,
            bmp,
            #[cfg(feature = "extra-sensors")]
            shtc3,
            #[cfg(feature = "extra-sensors")]
            sht4x,
            ina,
        }
    }

    /// The readings as Cayenne LPP, appended to `lpp`; the next free channel. A sensor that does
    /// not answer this time keeps its channel, so the others' do not move.
    pub async fn read_lpp(&mut self, lpp: &mut Vec<u8>) -> u8 {
        let i2c = &mut self.i2c;
        let mut channel = meshcore::lpp::CHANNEL_SELF + 1;
        #[cfg(feature = "extra-sensors")]
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
        channel
    }
}
