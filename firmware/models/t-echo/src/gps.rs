//! The T-Echo's GPS: a Quectel L76K on UARTE1 at 9600 baud (it sends on P1.09, listens on P1.08),
//! enabled through P1.02 and reset through P1.05 (active low), as upstream's
//! `MicroNMEALocationProvider` drives it.

use embassy_futures::select::select;
use embassy_nrf::buffered_uarte::{self, BufferedUarte};
use embassy_nrf::gpio::Output;
use embassy_nrf::peripherals::{P1_08, P1_09, PPI_CH0, PPI_CH1, PPI_GROUP0, TIMER1, UARTE1};
use embassy_nrf::uarte::{self, Baudrate};
use embassy_nrf::{Peri, bind_interrupts};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::Timer;
use meshcore_fw_core::gps::{self, Nmea};

bind_interrupts!(struct Irqs {
    UARTE1 => buffered_uarte::InterruptHandler<UARTE1>;
});

/// The companion's switch: on or off.
pub static GPS_ON: Signal<CriticalSectionRawMutex, bool> = Signal::new();

pub struct Gps {
    pub uarte: Peri<'static, UARTE1>,
    /// Counts the received bytes for the buffered UART, with two PPI channels and a group.
    pub timer: Peri<'static, TIMER1>,
    pub ppi_ch1: Peri<'static, PPI_CH0>,
    pub ppi_ch2: Peri<'static, PPI_CH1>,
    pub ppi_group: Peri<'static, PPI_GROUP0>,
    pub rx: Peri<'static, P1_09>,
    pub tx: Peri<'static, P1_08>,
    /// High = on.
    pub enable: Output<'static>,
    /// Low = held in reset.
    pub reset: Output<'static>,
}

#[embassy_executor::task]
pub async fn task(mut g: Gps) {
    let mut rx_buf = [0u8; 512];
    let mut tx_buf = [0u8; 32];
    loop {
        while !GPS_ON.wait().await {}
        g.enable.set_high();
        g.reset.set_low();
        Timer::after_millis(10).await;
        g.reset.set_high();
        {
            let mut cfg = uarte::Config::default();
            cfg.baudrate = Baudrate::Baud9600;
            let mut uart = BufferedUarte::new(
                g.uarte.reborrow(),
                g.timer.reborrow(),
                g.ppi_ch1.reborrow(),
                g.ppi_ch2.reborrow(),
                g.ppi_group.reborrow(),
                g.rx.reborrow(),
                g.tx.reborrow(),
                Irqs,
                cfg,
                &mut rx_buf,
                &mut tx_buf,
            );
            // GPS, BeiDou and GLONASS: set again at every start (upstream `configureL76K`)
            Timer::after_millis(250).await;
            let _ = uart.write(b"$PCAS04,7*1E\r\n").await;
            let _ = uart.flush().await;
            let read = async {
                let mut nmea = Nmea::new();
                let mut buf = [0u8; 64];
                loop {
                    match uart.read(&mut buf).await {
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
        }
        g.reset.set_low();
        g.enable.set_low();
        gps::clear();
    }
}
