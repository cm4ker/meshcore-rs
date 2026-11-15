//! The T114's GPS socket: a Quectel L76K at 9600 baud on UARTE0 (it sends on P1.05, listens on
//! P1.07), powered by P0.21 (the I2C connector's supply too), reset through P1.06 (active low),
//! kept awake by P1.02 (Meshtastic's `PIN_GPS_STANDBY`). Read by plain DMA in short pieces: the
//! buffered UART's PPI is the SoftDevice's business while it runs, and a sentence cut between two
//! pieces only fails its checksum.

use embassy_futures::select::select;
use embassy_nrf::gpio::Output;
use embassy_nrf::peripherals::{P1_05, P1_07, UARTE0};
use embassy_nrf::uarte::{self, Baudrate, Uarte};
use embassy_nrf::{Peri, bind_interrupts};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::Timer;
use meshcore_fw_core::gps::{self, Nmea};

bind_interrupts!(struct Irqs {
    UARTE0 => uarte::InterruptHandler<UARTE0>;
});

/// On or off, from the companion's switch or the repeater's `gps on|off`.
pub static GPS_ON: Signal<CriticalSectionRawMutex, bool> = Signal::new();

pub struct Gps {
    pub uarte: Peri<'static, UARTE0>,
    pub rx: Peri<'static, P1_05>,
    pub tx: Peri<'static, P1_07>,
    /// High = powered.
    pub power: Output<'static>,
    /// Low = held in reset.
    pub reset: Output<'static>,
    /// High = awake.
    pub wake: Output<'static>,
}

/// The latest reading as the core's board type, for the repeater's `gps` CLI (`BoardInfo::gps`).
pub fn reading() -> Option<meshcore::gps::GpsReading> {
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

#[embassy_executor::task]
pub async fn task(mut g: Gps) {
    loop {
        while !GPS_ON.wait().await {}
        g.power.set_high();
        g.wake.set_high();
        g.reset.set_low();
        Timer::after_millis(10).await;
        g.reset.set_high();
        {
            let mut cfg = uarte::Config::default();
            cfg.baudrate = Baudrate::Baud9600;
            let mut uart = Uarte::new(g.uarte.reborrow(), g.rx.reborrow(), g.tx.reborrow(), Irqs, cfg);
            // GPS, BeiDou and GLONASS: set again at every start (upstream `configureL76K`)
            Timer::after_millis(250).await;
            let _ = uart.write(b"$PCAS04,7*1E\r\n").await;
            let read = async {
                let mut nmea = Nmea::new();
                let mut buf = [0u8; 32];
                loop {
                    match uart.read(&mut buf).await {
                        Ok(()) => {
                            for &b in &buf {
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
        }
        g.reset.set_low();
        g.wake.set_low();
        g.power.set_low();
        gps::clear();
    }
}
