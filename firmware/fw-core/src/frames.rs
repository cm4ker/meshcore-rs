//! Companion frames between the transports (USB, BLE, TCP) and the main loop. Serial links use
//! upstream's framing: the app sends `<` + length (u16 LE) + frame, the radio answers `>` +
//! length + frame.

use alloc::vec::Vec;
use core::sync::atomic::AtomicBool;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use meshcore::companion::MAX_FRAME_SIZE;

/// The transport a command frame came in on (its replies go back there).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    Usb,
    Ble,
    Tcp,
}

/// Command frames from any transport, for the main loop.
pub static FRAMES_IN: Channel<CriticalSectionRawMutex, (Link, Vec<u8>), 8> = Channel::new();
/// Frames for the TCP client.
pub static TCP_OUT: Channel<CriticalSectionRawMutex, Vec<u8>, 32> = Channel::new();
pub static TCP_CONNECTED: AtomicBool = AtomicBool::new(false);

/// Incremental parser for `<` frames.
#[derive(Default)]
pub struct FrameParser {
    state: u8,
    len: usize,
    buf: Vec<u8>,
}

impl FrameParser {
    /// In the middle of a frame.
    pub fn partial(&self) -> bool {
        self.state != 0
    }

    /// Drop a frame that stopped coming: a link that lost a byte would otherwise take the next
    /// frames for the rest of it.
    pub fn reset(&mut self) {
        self.state = 0;
        self.buf.clear();
    }

    pub fn push(&mut self, c: u8) -> Option<Vec<u8>> {
        match self.state {
            0 => {
                if c == b'<' {
                    self.state = 1;
                }
            }
            1 => {
                self.len = c as usize;
                self.state = 2;
            }
            2 => {
                self.len |= (c as usize) << 8;
                self.buf.clear();
                self.state = if self.len > 0 { 3 } else { 0 };
            }
            _ => {
                if self.buf.len() < MAX_FRAME_SIZE {
                    self.buf.push(c); // the rest of an oversized frame is dropped
                }
                self.len -= 1;
                if self.len == 0 {
                    self.state = 0;
                    return Some(core::mem::take(&mut self.buf));
                }
            }
        }
        None
    }
}

/// `>` + length + frame.
pub fn encode(frame: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(frame.len() + 3);
    out.push(b'>');
    out.extend_from_slice(&(frame.len() as u16).to_le_bytes());
    out.extend_from_slice(frame);
    out
}

#[cfg(feature = "net")]
pub use tcp::tcp_server_task;

#[cfg(feature = "net")]
mod tcp {
    use embassy_futures::select::{Either, select};
    use embassy_net::Stack;
    use embassy_net::tcp::TcpSocket;
    use embassy_time::{Duration, Timer, with_timeout};
    use core::sync::atomic::Ordering;

    use super::{FRAMES_IN, FrameParser, Link, TCP_CONNECTED, TCP_OUT, encode};

    /// TCP server for one app at a time (upstream `SerialWifiInterface`, port 5000).
    #[embassy_executor::task]
    pub async fn tcp_server_task(stack: Stack<'static>, port: u16) {
        let mut rx_buf = [0u8; 1024];
        let mut tx_buf = [0u8; 2048];
        loop {
            stack.wait_config_up().await;
            let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
            // keep-alives let a vanished client (its FIN lost, e.g. on a flaky Wi-Fi) time out in a
            // minute instead of keeping the only listening socket busy
            socket.set_timeout(Some(Duration::from_secs(60)));
            socket.set_keep_alive(Some(Duration::from_secs(15)));
            if socket.accept(port).await.is_err() {
                Timer::after_secs(1).await;
                continue;
            }
            while TCP_OUT.try_receive().is_ok() {} // stale frames from an earlier client
            TCP_CONNECTED.store(true, Ordering::Relaxed);
            let mut parser = FrameParser::default();
            let mut buf = [0u8; 256];
            loop {
                match select(socket.read(&mut buf), TCP_OUT.receive()).await {
                    Either::First(Ok(n)) if n > 0 => {
                        for &c in &buf[..n] {
                            if let Some(f) = parser.push(c) {
                                FRAMES_IN.send((Link::Tcp, f)).await;
                            }
                        }
                    }
                    Either::First(_) => break, // closed or failed
                    Either::Second(frame) => {
                        if embedded_io_async::Write::write_all(&mut socket, &encode(&frame)).await.is_err() {
                            break;
                        }
                    }
                }
            }
            TCP_CONNECTED.store(false, Ordering::Relaxed);
            // a vanished client never acknowledges: flushing without a limit kept the port closed for
            // the socket timeout (10 minutes)
            socket.close();
            let _ = with_timeout(Duration::from_secs(2), socket.flush()).await;
            socket.abort();
            let _ = with_timeout(Duration::from_millis(500), socket.flush()).await;
        }
    }
}
