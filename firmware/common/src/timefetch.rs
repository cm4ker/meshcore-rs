//! The time server, for a gateway (`set time.serve on`, see `meshcore::timesync`): its token over
//! a nonce, fetched over plain HTTP (`GET /time/<nonce>`, 85 bytes) from `time.server`. The token
//! is signed and checked by the nodes it goes to, so neither the server nor the link has to be
//! trusted. One request at a time, a few waiting.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, Ordering};

use critical_section::Mutex;
use embassy_net::Stack;
use embassy_net::dns::DnsQueryType;
use embassy_net::tcp::TcpSocket;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, with_timeout};
use embedded_io_async::Write;
use meshcore::timesync::{NONCE_LEN, TOKEN_LEN, TimeFetch};
use meshcore::util::to_hex;

/// A whole fetch, connection and answer; the node waits 10 s for it (`timesync`'s gateway).
const FETCH_TIMEOUT: Duration = Duration::from_secs(8);

static ONLINE: AtomicBool = AtomicBool::new(false);
static SERVER: Mutex<RefCell<String>> = Mutex::new(RefCell::new(String::new()));
static ASK: Channel<CriticalSectionRawMutex, [u8; NONCE_LEN], 4> = Channel::new();
static DONE: Channel<CriticalSectionRawMutex, ([u8; NONCE_LEN], Option<[u8; TOKEN_LEN]>), 4> = Channel::new();

/// The time server and whether the Wi-Fi link is up, from the main loop.
pub fn sync(online: bool, server: &str) {
    ONLINE.store(online, Ordering::Relaxed);
    critical_section::with(|cs| {
        let mut s = SERVER.borrow_ref_mut(cs);
        if *s != server {
            *s = server.into();
        }
    });
}

/// The repeater's way to the server ([`meshcore::repeater::Repeater::time_fetch`]).
pub struct HttpTimeFetch;

impl TimeFetch for HttpTimeFetch {
    fn start(&mut self, nonce: [u8; NONCE_LEN]) -> bool {
        ONLINE.load(Ordering::Relaxed) && ASK.try_send(nonce).is_ok()
    }

    fn take(&mut self) -> Option<([u8; NONCE_LEN], Option<[u8; TOKEN_LEN]>)> {
        DONE.try_receive().ok()
    }
}

#[embassy_executor::task]
pub async fn fetch_task(stack: Stack<'static>) {
    loop {
        let nonce = ASK.receive().await;
        let server = critical_section::with(|cs| SERVER.borrow_ref(cs).clone());
        let t0 = Instant::now();
        let token = match with_timeout(FETCH_TIMEOUT, fetch(stack, &server, &nonce)).await {
            Ok(Ok(t)) => {
                log::info!("time: token from {server} in {} ms", t0.elapsed().as_millis());
                Some(t)
            }
            Ok(Err(e)) => {
                log::warn!("time: {server}: {e}");
                None
            }
            Err(_) => {
                log::warn!("time: {server} did not answer in {} s", FETCH_TIMEOUT.as_secs());
                None
            }
        };
        DONE.send((nonce, token)).await;
    }
}

/// GET `/time/<nonce>` from `server` (`host` or `host:port`).
async fn fetch(stack: Stack<'static>, server: &str, nonce: &[u8; NONCE_LEN]) -> Result<[u8; TOKEN_LEN], &'static str> {
    let (host, port) = match server.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() => (h, p.parse().unwrap()),
        _ => (server, 80),
    };
    let addr = stack.dns_query(host, DnsQueryType::A).await.ok().and_then(|a| a.first().copied()).ok_or("no address")?;
    let (mut rx, mut tx) = (vec![0u8; 1024], vec![0u8; 256]);
    let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
    socket.connect((addr, port)).await.map_err(|_| "no connection")?;
    let request = format!(
        "GET /time/{} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: meshcore-rs\r\nConnection: close\r\n\r\n",
        to_hex(nonce).to_ascii_lowercase()
    );
    socket.write_all(request.as_bytes()).await.map_err(|_| "request not sent")?;

    // the whole answer is small: read it to the end
    let mut answer = vec![0u8; 1024];
    let mut n = 0;
    loop {
        let got = socket.read(&mut answer[n..]).await.map_err(|_| "cut off")?;
        if got == 0 {
            break;
        }
        n += got;
        if n == answer.len() {
            return Err("answer too long");
        }
    }
    socket.close();
    let answer = &answer[..n];
    let body_at = answer.windows(4).position(|w| w == b"\r\n\r\n").ok_or("bad answer")? + 4;
    let head = core::str::from_utf8(&answer[..body_at]).map_err(|_| "bad answer")?;
    match head.split(' ').nth(1) {
        Some("200") => {}
        Some("503") => return Err("the server's clock is not checked (503)"),
        Some("429") => return Err("too many requests (429)"),
        _ => return Err("server error"),
    }
    answer[body_at..].get(..TOKEN_LEN).and_then(|b| b.try_into().ok()).ok_or("short answer")
}
