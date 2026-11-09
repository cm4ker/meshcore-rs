//! A companion's own update from the release server (`fw.server`) while it is on Wi-Fi: the
//! delta file goes from the server's answer into the update session as it comes, a piece at a
//! time, the way pieces over LoRa do (see `meshcore::fwshare::Loader`). Nothing of it is kept in
//! flash, so the node needs no room beside its running image (a repeater's [`crate::fwstore`]
//! does, as it hands the file on). The server is trusted no more than a neighbour: the update
//! session checks the release's signatures before it takes any of the patch.
//!
//! The fetch runs in its own task. It reads the file's head first, for the release it carries,
//! then keeps at most [`BUFFER`] bytes ahead of what the session took: the server waits for the
//! rest on the connection meanwhile.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, Ordering};

use critical_section::Mutex;
use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use meshcore::fwshare::{DeltaHeader, FwQuery, FwStore, HeaderSize, Offer};

use crate::fwstore::{Key, request};

/// Bytes of the file held between the server and the update session.
const BUFFER: usize = 4096;
/// The start of the file read before it is offered (its header, with the release in it): more
/// than any envelope with its signatures.
const HEAD_MAX: usize = 2048;
/// The server said it has no update from an image: believed this long (a check by hand asks
/// again at once).
const MISS_SECS: u64 = 600;
/// After a failed fetch.
const FAIL_SECS: u64 = 600;
/// The session took nothing of the file this long: the transfer is given up.
const STALL_SECS: u64 = 60;
/// What the node is told to wait while the server is asked.
const ASK_RETRY_SECS: u16 = 2;
/// A server that sends nothing this long while the file is not whole has given up.
const SILENT_SECS: u64 = 30;

/// The file the server answered with.
struct File {
    len: u32,
    id: u32,
    counter: u32,
    version: String,
}

enum Job {
    Idle,
    /// The server is asked about `key`.
    Asking { key: Key },
    /// The server's file for `key`: `data` holds the bytes from `taken` on that came and were
    /// not taken yet; `moved` is when the session last took some (seconds of uptime).
    Open { key: Key, file: File, taken: u32, data: VecDeque<u8>, moved: u64 },
}

struct State {
    server: String,
    /// Numbers the fetches asked for: one given up for a newer stops.
    fetch_no: u32,
    job: Job,
    /// (image, release asked from, until): the server had nothing newer.
    misses: Vec<(Key, u32, u64)>,
    failed_until: u64,
}

static STATE: Mutex<RefCell<State>> =
    Mutex::new(RefCell::new(State { server: String::new(), fetch_no: 0, job: Job::Idle, misses: Vec::new(), failed_until: 0 }));
static ONLINE: AtomicBool = AtomicBool::new(false);
/// What to fetch: the image's key, the release it runs, the fetch's number.
static REQUEST: Signal<CriticalSectionRawMutex, (Key, u32, u32)> = Signal::new();

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    critical_section::with(|cs| f(&mut STATE.borrow_ref_mut(cs)))
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

/// The companion's view of the server ([`FwStore`]): an offer once the file's head is in, its
/// bytes in order as they come.
pub struct StreamFwStore;

impl FwStore for StreamFwStore {
    fn offer(&mut self, q: &FwQuery) -> Offer {
        let key = Key::from(q);
        let now = secs();
        with(|s| {
            match &s.job {
                // just in, nothing taken of it yet: it starts from its first byte
                Job::Open { key: k, file, taken: 0, moved, .. } if *k == key && now < moved + STALL_SECS => {
                    return Offer::Ready { len: file.len, id: file.id, counter: file.counter, version: file.version.clone() };
                }
                Job::Asking { key: k } if *k == key => return Offer::Fetching { retry_secs: ASK_RETRY_SECS },
                _ => {}
            }
            s.misses.retain(|m| m.2 > now);
            if s.misses.iter().any(|m| m.0 == key && m.1 <= q.counter) {
                return Offer::NoUpdate;
            }
            if !ONLINE.load(Ordering::Relaxed) || s.server.is_empty() || now < s.failed_until {
                return Offer::Unavailable;
            }
            // whatever came before is given up for this
            s.fetch_no = s.fetch_no.wrapping_add(1);
            s.job = Job::Asking { key };
            REQUEST.signal((key, q.counter, s.fetch_no));
            Offer::Fetching { retry_secs: ASK_RETRY_SECS }
        })
    }

    /// All of `buf`, or nothing (0) while those bytes are still on the way.
    fn read(&mut self, id: u32, offset: u32, buf: &mut [u8]) -> Option<usize> {
        with(|s| {
            let Job::Open { file, taken, data, moved, .. } = &mut s.job else { return None };
            if file.id != id || *taken != offset {
                return None;
            }
            let n = buf.len();
            if data.len() < n {
                return Some(0);
            }
            for (b, d) in buf.iter_mut().zip(data.drain(..n)) {
                *b = d;
            }
            *taken += n as u32;
            *moved = secs();
            if *taken >= file.len {
                s.job = Job::Idle;
            }
            Some(n)
        })
    }

    fn refresh(&mut self, q: &FwQuery) {
        let key = Key::from(q);
        with(|s| {
            s.misses.retain(|m| m.0 != key);
            s.failed_until = 0;
        });
    }
}

/// How a fetch ended.
enum Fetched {
    /// The whole file came (the session may still be taking the last of it), or the fetch was
    /// given up for a newer one.
    Done,
    /// Nothing newer on the server.
    NoUpdate,
    /// The session stopped taking it.
    Stalled,
}

/// Fetch what the node asks for, one file at a time.
#[embassy_executor::task]
pub async fn fetch_task(stack: Stack<'static>) {
    loop {
        let (key, counter, fetch_no) = REQUEST.wait().await;
        let server = with(|s| s.server.clone());
        let got = fetch(stack, &server, key, counter, fetch_no).await;
        let now = secs();
        with(|s| {
            if s.fetch_no != fetch_no {
                return; // a newer fetch holds the state
            }
            match got {
                Ok(Fetched::Done) => return,
                Ok(Fetched::NoUpdate) => s.misses.push((key, counter, now + MISS_SECS)),
                Ok(Fetched::Stalled) => {}
                Err(e) => {
                    log::warn!("fw: fetching from {server} failed: {e}");
                    s.failed_until = now + FAIL_SECS;
                }
            }
            s.job = Job::Idle;
        });
    }
}

/// GET the newest delta for `key` newer than `counter` and hand it on to the session as it takes
/// it, while this is fetch `fetch_no`.
async fn fetch(stack: Stack<'static>, server: &str, key: Key, counter: u32, fetch_no: u32) -> Result<Fetched, &'static str> {
    let (mut rx, mut tx) = (vec![0u8; BUFFER], vec![0u8; 512]);
    let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
    socket.set_timeout(Some(Duration::from_secs(30)));
    let Some((len, mut head)) = request(stack, &mut socket, server, key, counter).await? else {
        return Ok(Fetched::NoUpdate);
    };

    // the header, for the release the file carries
    let mut buf = vec![0u8; 512];
    let header = loop {
        match DeltaHeader::size(&head) {
            HeaderSize::Known(n) if head.len() >= n => break DeltaHeader::parse(&head).ok_or("not a delta file")?,
            HeaderSize::NotDelta => return Err("not a delta file"),
            _ if head.len() >= HEAD_MAX || head.len() as u32 >= len => return Err("not a delta file"),
            _ => {}
        }
        let want = buf.len().min((len as usize) - head.len());
        let n = read(&mut socket, &mut buf[..want]).await?;
        head.extend_from_slice(&buf[..n]);
    };
    let m = &header.envelope.manifest;
    let file = File {
        len,
        id: u32::from_le_bytes(m.image_sha256[..4].try_into().unwrap()) ^ len,
        counter: m.counter,
        version: m.version.clone(),
    };
    let mut received = head.len() as u32;
    let open = with(|s| {
        let current = s.fetch_no == fetch_no;
        if current {
            s.job = Job::Open { key, file, taken: 0, data: VecDeque::from(head), moved: secs() };
        }
        current
    });
    if !open {
        return Ok(Fetched::Done);
    }

    // the rest, no further ahead of the session than the buffer
    while received < len {
        let room = with(|s| match &s.job {
            Job::Open { data, moved, .. } if s.fetch_no == fetch_no => Some((BUFFER.saturating_sub(data.len()), *moved)),
            _ => None,
        });
        let Some((room, moved)) = room else {
            return Ok(Fetched::Done);
        };
        if room == 0 {
            if secs() > moved + STALL_SECS {
                return Ok(Fetched::Stalled);
            }
            Timer::after_millis(20).await;
            continue;
        }
        let want = room.min(buf.len()).min((len - received) as usize);
        let n = read(&mut socket, &mut buf[..want]).await?;
        with(|s| {
            if s.fetch_no == fetch_no
                && let Job::Open { data, .. } = &mut s.job
            {
                data.extend(&buf[..n]);
            }
        });
        received += n as u32;
    }
    socket.close();
    Ok(Fetched::Done)
}

/// Some bytes of the answer; a server silent for [`SILENT_SECS`] has given up.
async fn read(socket: &mut TcpSocket<'_>, buf: &mut [u8]) -> Result<usize, &'static str> {
    match with_timeout(Duration::from_secs(SILENT_SECS), socket.read(buf)).await {
        Ok(Ok(0) | Err(_)) => Err("cut off"),
        Ok(Ok(n)) => Ok(n),
        Err(_) => Err("the server went silent"),
    }
}
