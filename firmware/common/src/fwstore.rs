//! The updates a repeater hands on to its neighbours (`set fw.serve on`, see `meshcore::fwshare`):
//! one delta file at a time, fetched over plain HTTP from the release server (`fw.server`) into
//! the last [`CACHE_SIZE`] bytes of the running firmware's slot, past its image (an update of this
//! node writes the other slot), and read from there for the neighbours' FW_GET. What is served is
//! signed and checked on the device that installs it, so neither the server nor the link has to
//! be trusted. Nothing is kept across a reboot.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use critical_section::Mutex;
use embassy_net::Stack;
use embassy_net::dns::DnsQueryType;
use embassy_net::tcp::TcpSocket;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, with_timeout};
use embedded_io_async::Write;
use meshcore::fwshare::{DeltaHeader, FwQuery, FwStore, Offer};
use meshcore::release;
use meshcore::util::to_hex;

use crate::ota::{SharedStore, Slots};

/// Room for one delta: a repeater's from one release to the next is ~50 KB, a companion's less.
pub const CACHE_SIZE: u32 = 512 * 1024;
const SECTOR: usize = 4096;
/// The server said it has no update from an image: believed this long.
const MISS_SECS: u64 = 3600;
/// After a failed fetch.
const FAIL_SECS: u64 = 600;
/// What an asking companion is told to wait while a fetch runs.
const FETCH_RETRY_SECS: u16 = 30;

/// The board, role and image a delta is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Key {
    board: u8,
    role: u8,
    base_len: u32,
    base_sha8: [u8; 8],
    channel: u8,
}

impl From<&FwQuery> for Key {
    fn from(q: &FwQuery) -> Self {
        Self { board: q.board, role: q.role, base_len: q.base_len, base_sha8: q.base_sha8, channel: q.channel }
    }
}

struct File {
    key: Key,
    len: u32,
    id: u32,
    counter: u32,
    version: String,
}

struct State {
    server: String,
    file: Option<File>,
    fetching: Option<Key>,
    /// (image, release asked from, until): the server had nothing newer.
    misses: Vec<(Key, u32, u64)>,
    failed_until: u64,
    status: String,
}

static STATE: Mutex<RefCell<State>> = Mutex::new(RefCell::new(State {
    server: String::new(),
    file: None,
    fetching: None,
    misses: Vec::new(),
    failed_until: 0,
    status: String::new(),
}));
static ONLINE: AtomicBool = AtomicBool::new(false);
static STATUS_GEN: AtomicU32 = AtomicU32::new(0);
static FETCH: Channel<CriticalSectionRawMutex, (Key, u32), 1> = Channel::new();

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    critical_section::with(|cs| f(&mut STATE.borrow_ref_mut(cs)))
}

fn set_status(s: &mut State, text: String) {
    s.status = text;
    STATUS_GEN.fetch_add(1, Ordering::Relaxed);
}

fn secs() -> u64 {
    Instant::now().as_secs()
}

/// The release server and whether the Wi-Fi link is up, from the main loop.
pub fn sync(online: bool, server: &str) {
    ONLINE.store(online, Ordering::Relaxed);
    with(|s| {
        if s.server != server {
            s.server = server.into();
        }
    });
}

/// The status line for `get fw.status`, when it changed since `seen` (updated).
pub fn status_if_changed(seen: &mut u32) -> Option<String> {
    let g = STATUS_GEN.load(Ordering::Relaxed);
    (g != *seen).then(|| {
        *seen = g;
        with(|s| s.status.clone())
    })
}

/// The cache past the running image in its slot, if there is room for one.
pub fn region(slots: &Slots, image_len: u32) -> Option<u32> {
    let (offset, size) = slots.current;
    (image_len + CACHE_SIZE <= size).then_some(offset + size - CACHE_SIZE)
}

pub struct FlashFwStore {
    store: SharedStore<'static>,
    region: u32,
}

impl FlashFwStore {
    pub fn new(store: SharedStore<'static>, region: u32) -> Self {
        Self { store, region }
    }
}

impl FwStore for FlashFwStore {
    fn offer(&mut self, q: &FwQuery) -> Offer {
        let key = Key::from(q);
        let now = secs();
        with(|s| {
            if let Some(f) = s.file.as_ref().filter(|f| f.key == key) {
                return Offer::Ready { len: f.len, id: f.id, counter: f.counter, version: f.version.clone() };
            }
            if s.fetching == Some(key) {
                return Offer::Fetching { retry_secs: FETCH_RETRY_SECS };
            }
            s.misses.retain(|m| m.2 > now);
            if s.misses.iter().any(|m| m.0 == key && m.1 <= q.counter) {
                return Offer::NoUpdate;
            }
            if !ONLINE.load(Ordering::Relaxed) || s.server.is_empty() || now < s.failed_until {
                return Offer::Unavailable;
            }
            if s.fetching.is_none() && FETCH.try_send((key, q.counter)).is_ok() {
                s.fetching = Some(key);
                s.file = None; // its sectors are about to be rewritten
            }
            Offer::Fetching { retry_secs: FETCH_RETRY_SECS * 2 }
        })
    }

    fn read(&mut self, id: u32, offset: u32, buf: &mut [u8]) -> Option<usize> {
        let len = with(|s| s.file.as_ref().filter(|f| f.id == id).map(|f| f.len))?;
        let n = buf.len().min(len.saturating_sub(offset) as usize);
        self.store.borrow_mut().flash().read(self.region + offset, &mut buf[..n]).ok()?;
        Some(n)
    }
}

/// Fetch what the neighbours ask for, one file at a time.
#[embassy_executor::task]
pub async fn fetch_task(stack: Stack<'static>, store: SharedStore<'static>, region: u32) {
    loop {
        let (key, counter) = FETCH.receive().await;
        let server = with(|s| s.server.clone());
        let t0 = Instant::now();
        let got = fetch(stack, &server, key, counter, &store, region).await;
        let now = secs();
        with(|s| {
            s.fetching = None;
            let what = format!("{}/{}", release::board_name(key.board), release::role_name(key.role));
            match got {
                Ok(Some(f)) => {
                    let line = format!(
                        "holds {what} {} #{}: {} bytes, fetched in {} s from {server}",
                        f.version,
                        f.counter,
                        f.len,
                        t0.elapsed().as_secs()
                    );
                    log::info!("fw: {line}");
                    set_status(s, line);
                    s.file = Some(f);
                }
                Ok(None) => {
                    s.misses.push((key, counter, now + MISS_SECS));
                    set_status(s, format!("no update for {what} from image {}", to_hex(&key.base_sha8).to_ascii_lowercase()));
                }
                Err(e) => {
                    log::warn!("fw: fetching from {server} failed: {e}");
                    s.failed_until = now + FAIL_SECS;
                    set_status(s, format!("fetching from {server} failed: {e}"));
                }
            }
        });
    }
}

/// GET the newest delta for `key` newer than `counter` into the cache: `None` if the server has
/// none.
async fn fetch(
    stack: Stack<'static>, server: &str, key: Key, counter: u32, store: &SharedStore<'static>, region: u32,
) -> Result<Option<File>, &'static str> {
    let (host, port) = match server.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() => (h, p.parse().unwrap()),
        _ => (server, 80),
    };
    let addr = stack.dns_query(host, DnsQueryType::A).await.ok().and_then(|a| a.first().copied()).ok_or("no address")?;
    let (mut rx, mut tx) = (vec![0u8; SECTOR], vec![0u8; 512]);
    let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
    socket.set_timeout(Some(Duration::from_secs(30)));
    if !matches!(with_timeout(Duration::from_secs(10), socket.connect((addr, port))).await, Ok(Ok(()))) {
        return Err("no connection");
    }
    let path = format!(
        "/fw/next/{}/{}/{}?counter={counter}&channel={}",
        release::board_name(key.board),
        release::role_name(key.role),
        to_hex(&key.base_sha8).to_ascii_lowercase(),
        release::channel_name(key.channel)
    );
    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: meshcore-rs\r\nConnection: close\r\n\r\n");
    socket.write_all(request.as_bytes()).await.map_err(|_| "request not sent")?;

    // the head, and whatever of the body came with it
    let mut head = Vec::new();
    let body_at = loop {
        let mut buf = [0u8; 512];
        let n = socket.read(&mut buf).await.map_err(|_| "no answer")?;
        if n == 0 {
            return Err("no answer");
        }
        head.extend_from_slice(&buf[..n]);
        if let Some(i) = head.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if head.len() > 2048 {
            return Err("bad answer");
        }
    };
    let text = core::str::from_utf8(&head[..body_at]).map_err(|_| "bad answer")?;
    match text.split(' ').nth(1) {
        Some("200") => {}
        Some("404") => return Ok(None),
        _ => return Err("server error"),
    }
    let len: u32 = text
        .lines()
        .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("content-length")).map(|(_, v)| v.trim()))
        .and_then(|v| v.parse().ok())
        .ok_or("no length")?;
    if len == 0 || len > CACHE_SIZE {
        return Err("too large");
    }

    // the body, a sector at a time: erased, then written
    let mut sector = vec![0xFFu8; SECTOR];
    let mut fill = (head.len() - body_at).min(len as usize);
    sector[..fill].copy_from_slice(&head[body_at..body_at + fill]);
    let (mut received, mut written) = (fill as u32, 0u32);
    loop {
        if fill == SECTOR || received == len {
            let n = fill.next_multiple_of(4);
            sector[fill..n].fill(0xFF);
            crate::reset::feed();
            let mut st = store.borrow_mut();
            let flash = st.flash();
            let at = region + written;
            flash.erase(at, at + SECTOR as u32).map_err(|_| "flash erase failed")?;
            flash.write_nor(at, &sector[..n]).map_err(|_| "flash write failed")?;
            written += fill as u32;
            fill = 0;
            if received == len {
                break;
            }
        }
        let n = socket.read(&mut sector[fill..]).await.map_err(|_| "cut off")?;
        if n == 0 {
            return Err("cut off");
        }
        let n = n.min((len - received) as usize);
        fill += n;
        received += n as u32;
    }
    socket.close();

    // what it is: the release it carries must be for the image asked about
    let mut start = vec![0u8; (len as usize).min(1200)];
    store.borrow_mut().flash().read(region, &mut start).map_err(|_| "flash read failed")?;
    let h = DeltaHeader::parse(&start).ok_or("not a delta file")?;
    let m = &h.envelope.manifest;
    if (m.board, m.role, h.base_len) != (key.board, key.role, key.base_len) || h.base_sha256[..8] != key.base_sha8 {
        return Err("the server sent a delta for another image");
    }
    if !m.fits_channel(key.channel) {
        return Err("the server sent a dev release for a stable node");
    }
    let id = u32::from_le_bytes(m.image_sha256[..4].try_into().unwrap()) ^ len;
    Ok(Some(File { key, len, id, counter: m.counter, version: m.version.clone() }))
}
