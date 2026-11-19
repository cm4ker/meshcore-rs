//! The UC6580 GNSS receiver on the T096, connected to UARTE0 at 115200 baud.

use embassy_futures::select::select;
use embassy_nrf::gpio::Output;
use embassy_nrf::peripherals::{P0_23, P0_25, UARTE0};
use embassy_nrf::uarte::{self, Baudrate, Uarte};
use embassy_nrf::{Peri, bind_interrupts};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::Timer;
use meshcore_fw_core::gps::{self, Nmea};

use crate::board::vext;

bind_interrupts!(struct Irqs {
    UARTE0 => uarte::InterruptHandler<UARTE0>;
});

pub static GPS_ON: Signal<CriticalSectionRawMutex, bool> = Signal::new();

pub struct Gps {
    pub uarte: Peri<'static, UARTE0>,
    pub rx: Peri<'static, P0_23>,
    pub tx: Peri<'static, P0_25>,
    /// Low holds the receiver in reset.
    pub reset: Output<'static>,
    /// Low enables the receiver's supply from VEXT.
    pub enable: Output<'static>,
}

#[embassy_executor::task]
pub async fn task(mut gps_hw: Gps) {
    loop {
        while !GPS_ON.wait().await {}

        vext::want(vext::GPS, true);
        gps_hw.reset.set_low();
        gps_hw.enable.set_low();
        Timer::after_millis(10).await;
        {
            let mut cfg = uarte::Config::default();
            cfg.baudrate = Baudrate::Baud115200;
            let mut uart = Uarte::new(gps_hw.uarte.reborrow(), gps_hw.rx.reborrow(), gps_hw.tx.reborrow(), Irqs, cfg);
            gps_hw.reset.set_high();
            let read = async {
                let mut nmea = Nmea::new();
                let mut buf = [0u8; 128];
                loop {
                    match uart.read(&mut buf).await {
                        Ok(()) => {
                            for &byte in &buf {
                                if let Some(fix) = nmea.push(byte) {
                                    gps::publish(fix);
                                }
                            }
                        }
                        Err(_) => Timer::after_millis(10).await,
                    }
                }
            };
            select(read, async { while GPS_ON.wait().await {} }).await;
        }
        gps_hw.reset.set_low();
        gps_hw.enable.set_high();
        vext::want(vext::GPS, false);
        gps::clear();
    }
}
