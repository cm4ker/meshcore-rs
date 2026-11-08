//! An optional external GPS on the Heltec V4's dedicated header (upstream `variants/heltec_v4`:
//! `ENV_INCLUDE_GPS=1`). A bare NMEA receiver on UART1 — its TX on GPIO39 (our RX), its RX on
//! GPIO38 (our TX) — powered through EN 34 and held in reset through RESET 42, both active LOW.
//! The variant's `PIN_GPS_RX=38`/`PIN_GPS_TX=39` name the receiver's pins: upstream passes them to
//! `Serial1.setPins(rx, tx)` as `(PIN_GPS_TX, PIN_GPS_RX)`. Bytes feed the shared parser
//! (`meshcore_fw_core::gps`), whose readings the companion loop and the repeater `gps` CLI read.
//!
//! The Vext rail (GPIO36) that powers the receiver is already held on by the bins (LOW = on), so
//! this task must not touch it. Baud is 9600 (the variant sets no `GPS_BAUD_RATE`; bare NMEA
//! modules and the T-Echo's L76K default to it).

use embassy_futures::select::select;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::Timer;
use esp_hal::Async;
use esp_hal::gpio::Output;
use esp_hal::uart::{UartRx, UartTx};
use meshcore_fw_core::gps::{self, Nmea};

/// The board's switch: on or off (`gps on`/`gps off`, or `prefs.gps_enabled` at boot).
pub static GPS_ON: Signal<CriticalSectionRawMutex, bool> = Signal::new();

pub struct Gps {
    pub rx: UartRx<'static, Async>,
    pub tx: UartTx<'static, Async>,
    /// GPS_EN (GPIO34), active LOW: Low = powered.
    pub enable: Output<'static>,
    /// RESET (GPIO42), active LOW: Low = held in reset.
    pub reset: Output<'static>,
}

#[embassy_executor::task]
pub async fn task(mut g: Gps) {
    let mut nmea = Nmea::new();
    let mut buf = [0u8; 64];
    loop {
        while !GPS_ON.wait().await {}
        // power up: enable (active low) on, pulse reset low->high
        g.enable.set_low();
        g.reset.set_low();
        Timer::after_millis(10).await;
        g.reset.set_high();
        Timer::after_millis(250).await;
        // GPS, BeiDou and GLONASS, as upstream's `configureL76K` (harmless to modules that ignore it)
        let _ = g.tx.write_async(b"$PCAS04,7*1E\r\n").await;
        let read = async {
            loop {
                match g.rx.read_async(&mut buf).await {
                    Ok(n) => {
                        for &b in &buf[..n] {
                            if let Some(fix) = nmea.push(b) {
                                gps::publish(fix);
                            }
                        }
                    }
                    Err(_) => Timer::after_millis(100).await, // overrun: carry on
                }
            }
        };
        select(read, async { while GPS_ON.wait().await {} }).await;
        g.reset.set_low();
        g.enable.set_high(); // off
        gps::clear();
    }
}

/// The latest reading as the core's board type, for the repeater `gps` CLI (`BoardInfo::gps`).
pub fn latest() -> Option<meshcore::gps::GpsReading> {
    gps::latest().map(|f| meshcore::gps::GpsReading {
        valid: f.valid,
        lat: f.lat,
        lon: f.lon,
        alt_mm: f.alt_mm,
        satellites: f.satellites,
        in_view: f.in_view,
        heard: f.heard,
        best_snr: f.best_snr,
        time: f.time,
    })
}
