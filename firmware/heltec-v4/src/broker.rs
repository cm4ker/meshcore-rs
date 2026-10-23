//! MQTT observer sessions, one task per enabled broker: plain TCP (meshcoretel) or MQTT over
//! WebSocket over TLS 1.3 (ommesh, LetsMesh). Both feed the same MQTT session loop through
//! [`LinkRx`] / [`LinkTx`].

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell, UnsafeCell};
use core::sync::atomic::Ordering;

use embassy_futures::select::{Either3, select, select3};
use embassy_net::Stack;
use embassy_net::dns::DnsQueryType;
use embassy_net::tcp::{TcpReader, TcpSocket, TcpWriter};
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex};
use embassy_sync::mutex::{Mutex, MutexGuard};
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use embedded_io_async::{ErrorType, Read, Write};
use embedded_tls::pki::CertVerifier;
use embedded_tls::{
    Aes128GcmSha256, Certificate, CryptoProvider, TlsClock, TlsConfig, TlsConnection, TlsContext, TlsError, TlsVerifier,
};
use esp_hal::rng::Rng;
use meshcore::identity::LocalIdentity;
use meshcore::uplink::{self, mqtt, ws};

use crate::net::{BROKER_CONNECTED, BrokerState, Sub, TIME_SYNCED, UPLINK, epoch_now};

const KEEPALIVE_SECS: u16 = 30;

/// Trust anchors. embedded-tls checks the served chain against a single anchor (starting from
/// the last certificate sent), so a broker has a list and a session moves to the next anchor
/// when the chain is rejected.
///
/// ommesh: Let's Encrypt, RSA (ISRG Root X1) or ECDSA (ISRG Root X2) chains.
pub const ISRG_ROOTS: &[&[u8]] = &[include_bytes!("../certs/isrg-root-x1.der"), include_bytes!("../certs/isrg-root-x2.der")];
/// LetsMesh: Google Trust Services WE1, served with GTS Root R4 cross-signed by GlobalSign Root CA
/// (valid until 2028); GTS Root R4 itself in case the cross-signature is dropped.
pub const GTS_ROOTS: &[&[u8]] = &[include_bytes!("../certs/globalsign-root-ca.der"), include_bytes!("../certs/gts-root-r4.der")];

pub enum Transport {
    Tcp,
    /// MQTT over WebSocket over TLS: request path and trust anchors.
    Wss(&'static str, &'static [&'static [u8]]),
}

pub enum Credential {
    Fixed(&'static str),
    /// Ed25519 JWT signed with the node key, minted per connection (`aud` = broker host).
    Jwt { id: LocalIdentity, owner: Option<String>, email: Option<String> },
}

/// Settings of one broker session.
pub struct BrokerConfig {
    /// Index into `meshcore::repeater::MQTT_BROKERS`.
    pub index: usize,
    pub host: &'static str,
    pub port: u16,
    pub transport: Transport,
    pub user: String,
    pub password: Credential,
    pub client_id: String,
    /// `meshcore/<IATA>/<KEY>/` prefix.
    pub topic_prefix: String,
    /// Offline status, retained, as last will.
    pub will: String,
}

pub fn spawn(spawner: &embassy_executor::Spawner, stack: Stack<'static>, cfg: BrokerConfig) -> Result<(), &'static str> {
    BrokerState::Wait.set(cfg.index);
    spawner.spawn(broker_task(stack, cfg).map_err(|_| "task busy")?);
    Ok(())
}

/// Reconnect delay: 10 s doubling to 300 s, as in the C++ firmware.
fn backoff(failures: u32) -> Duration {
    Duration::from_secs((10u64 << failures.saturating_sub(1).min(5)).min(300))
}

/// Only one broker connects at a time (as in the C++ firmware); this also keeps a single TLS
/// handshake's buffers in memory.
static CONNECTING: Mutex<CriticalSectionRawMutex, ()> = Mutex::new(());

#[embassy_executor::task(pool_size = 3)]
async fn broker_task(stack: Stack<'static>, cfg: BrokerConfig) {
    let mut sub = UPLINK.subscriber().unwrap();
    let mut failures = 0u32;
    let mut root = 0usize;
    loop {
        BrokerState::Wait.set(cfg.index);
        stack.wait_config_up().await;
        while !TIME_SYNCED.load(Ordering::Relaxed) {
            Timer::after_secs(2).await;
        }
        let guard = CONNECTING.lock().await;
        BrokerState::Connecting.set(cfg.index);
        if session(stack, &cfg, &mut sub, &mut root, guard).await {
            failures = 1; // ran fine, then dropped
        } else {
            failures += 1;
        }
        BrokerState::Backoff.set(cfg.index);
        log::warn!("MQTT {}: disconnected, retry in {} s", cfg.host, backoff(failures).as_secs());
        Timer::after(backoff(failures)).await;
    }
}

type Guard = MutexGuard<'static, CriticalSectionRawMutex, ()>;

/// One MQTT session; `true` if it got connected before ending.
async fn session(stack: Stack<'static>, cfg: &BrokerConfig, sub: &mut Sub, root: &mut usize, guard: Guard) -> bool {
    let Some(addr) = stack.dns_query(cfg.host, DnsQueryType::A).await.ok().and_then(|a| a.first().copied()) else {
        log::warn!("MQTT {}: DNS failed", cfg.host);
        return false;
    };
    let (mut rx_buf, mut tx_buf) = (vec![0u8; 2048], vec![0u8; 2048]);
    let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
    socket.set_timeout(Some(Duration::from_secs(90)));
    if with_timeout(Duration::from_secs(10), socket.connect((addr, cfg.port))).await.map_or(true, |r| r.is_err()) {
        log::warn!("MQTT {}: TCP connect to {addr} failed", cfg.host);
        return false;
    }
    let password = match &cfg.password {
        Credential::Fixed(p) => String::from(*p),
        Credential::Jwt { id, owner, email } => {
            let Some(now) = epoch_now() else { return false };
            uplink::jwt(id, cfg.host, now, owner.as_deref(), email.as_deref())
        }
    };
    let will_topic = format!("{}status", cfg.topic_prefix);
    let will = mqtt::Will { topic: &will_topic, payload: cfg.will.as_bytes(), qos: 1, retain: true };
    let connect = mqtt::connect(&cfg.client_id, &cfg.user, &password, KEEPALIVE_SECS, Some(&will));

    let (r, w) = socket.split();
    let (path, roots) = match cfg.transport {
        Transport::Tcp => return mqtt_session(cfg, sub, r, w, &connect, guard).await,
        Transport::Wss(path, roots) => (path, roots),
    };

    let (r, w) = (UnsafeCell::new(r), UnsafeCell::new(w));
    let (mut rec_rx, mut rec_tx) = (vec![0u8; 16640], vec![0u8; 4096]);
    let mut tls: TlsConnection<'_, Duplex<'_, '_>, Aes128GcmSha256> =
        TlsConnection::new(Duplex { r: &r, w: &w }, &mut rec_rx, &mut rec_tx);
    let anchor = roots[*root % roots.len()];
    let mut provider = Box::new(Provider { rng: HwRng, verifier: Verifier::new(Certificate::X509(anchor)) });
    let config = TlsConfig::new().with_server_name(cfg.host);
    let t0 = Instant::now();
    match with_timeout(Duration::from_secs(30), tls.open(TlsContext::new(&config, &mut *provider))).await {
        Ok(Ok(())) => log::info!(
            "MQTT {}: TLS up in {} ms (heap free {})",
            cfg.host,
            t0.elapsed().as_millis(),
            esp_alloc::HEAP.free()
        ),
        Ok(Err(e)) => {
            log::warn!("MQTT {}: TLS handshake failed: {e:?}", cfg.host);
            if matches!(
                e,
                TlsError::InvalidCertificate | TlsError::InvalidSignature | TlsError::InvalidSignatureScheme | TlsError::DecodeError
            ) {
                *root = (*root + 1) % roots.len();
            }
            return false;
        }
        Err(_) => {
            log::warn!("MQTT {}: TLS handshake timed out", cfg.host);
            return false;
        }
    }
    drop(provider);

    let mut nonce = [0u8; 16];
    Rng::new().read(&mut nonce);
    let request = ws::upgrade_request(cfg.host, path, &nonce);
    if tls.write_all(request.as_bytes()).await.is_err() || tls.flush().await.is_err() {
        return false;
    }
    match with_timeout(Duration::from_secs(10), read_http_head(&mut tls)).await {
        Ok(Some(head)) if ws::upgrade_accepted(&head) => {}
        _ => {
            log::warn!("MQTT {}: WebSocket upgrade refused", cfg.host);
            return false;
        }
    }
    let control = Control { pong: RefCell::new(None), ready: Signal::new() };
    let (tr, tw) = tls.split();
    let rx = WsRx { inner: tr, control: &control, left: 0, mask: None, pos: 0 };
    mqtt_session(cfg, sub, rx, WsTx { inner: tw, control: &control }, &connect, guard).await
}

/// The HTTP response head, byte by byte so nothing after it is consumed.
async fn read_http_head(r: &mut impl Read) -> Option<Vec<u8>> {
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 2048 {
            return None;
        }
        match r.read(&mut b).await {
            Ok(1) => head.push(b[0]),
            _ => return None,
        }
    }
    Some(head)
}

/// CONNECT, wait for CONNACK, then publish uplink messages and keep the session alive.
async fn mqtt_session(cfg: &BrokerConfig, sub: &mut Sub, mut rx: impl LinkRx, mut tx: impl LinkTx, connect: &[u8], guard: Guard) -> bool {
    if !tx.send(connect).await {
        return false;
    }
    let mut inbuf = [0u8; 256];
    let mut fill = 0usize;
    let connack = with_timeout(Duration::from_secs(10), async {
        loop {
            fill += rx.recv(&mut inbuf[fill..]).await?;
            if let Some((p, used)) = mqtt::parse(&inbuf[..fill]) {
                inbuf.copy_within(used..fill, 0);
                fill -= used;
                return Some(p);
            }
        }
    })
    .await;
    match connack {
        Ok(Some(mqtt::Incoming::ConnAck { code: 0 })) => {}
        other => {
            log::warn!("MQTT {}: connect refused: {other:?}", cfg.host);
            return false;
        }
    }
    drop(guard);
    log::info!("MQTT {}: connected", cfg.host);
    BrokerState::Up.set(cfg.index);
    BROKER_CONNECTED.store(true, Ordering::Relaxed);

    let ping_every = Duration::from_secs(KEEPALIVE_SECS as u64 * 2 / 3);
    let last_rx = Cell::new(Instant::now());
    // The reader is never cancelled mid-read (TLS record reads are not cancel-safe).
    let reader = async {
        while let Some(n) = rx.recv(&mut inbuf[fill..]).await {
            fill += n;
            last_rx.set(Instant::now());
            while let Some((_, used)) = mqtt::parse(&inbuf[..fill]) {
                inbuf.copy_within(used..fill, 0);
                fill -= used;
            }
            if fill == inbuf.len() {
                fill = 0; // oversized packet: we never subscribe, drop it
            }
        }
    };
    let writer = async {
        let mut next_id: u16 = 1;
        let mut next_ping = Instant::now() + ping_every;
        loop {
            let event = select3(sub.next_message_pure(), Timer::at(next_ping), tx.control_ready()).await;
            let ok = match event {
                Either3::First(msg) => {
                    let topic = format!("{}{}", cfg.topic_prefix, msg.leaf);
                    let p = mqtt::publish(&topic, msg.json.as_bytes(), 1, msg.retain, next_id);
                    next_id = next_id.wrapping_add(1).max(1);
                    tx.send(&p).await
                }
                Either3::Second(()) => {
                    if last_rx.get().elapsed() > Duration::from_secs(KEEPALIVE_SECS as u64 * 2) {
                        log::warn!("MQTT {}: no answer to pings", cfg.host);
                        return;
                    }
                    next_ping = Instant::now() + ping_every;
                    tx.send(&mqtt::pingreq()).await
                }
                Either3::Third(()) => tx.send_control().await,
            };
            if !ok {
                return;
            }
        }
    };
    select(reader, writer).await;
    true
}

/// Incoming MQTT bytes; `None` once the link is closed or broken.
trait LinkRx {
    async fn recv(&mut self, buf: &mut [u8]) -> Option<usize>;
}

/// Outgoing MQTT bytes.
trait LinkTx {
    async fn send(&mut self, data: &[u8]) -> bool;

    /// Resolves when the link has control traffic of its own to send (WebSocket pongs).
    async fn control_ready(&self) {
        core::future::pending().await
    }

    async fn send_control(&mut self) -> bool {
        true
    }
}

impl LinkRx for TcpReader<'_> {
    async fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
        self.read(buf).await.ok().filter(|&n| n > 0)
    }
}

impl LinkTx for TcpWriter<'_> {
    async fn send(&mut self, data: &[u8]) -> bool {
        self.write_all(data).await.is_ok()
    }
}

/// A pong owed to the server: the reader stores the ping payload, the writer answers it.
struct Control {
    pong: RefCell<Option<Vec<u8>>>,
    ready: Signal<NoopRawMutex, ()>,
}

/// WebSocket data frames from the server as a byte stream; answers pings via [`Control`].
struct WsRx<'c, R> {
    inner: R,
    control: &'c Control,
    /// Payload bytes left in the current data frame.
    left: u64,
    mask: Option<[u8; 4]>,
    pos: usize,
}

impl<R: Read> WsRx<'_, R> {
    async fn read_exact(&mut self, buf: &mut [u8]) -> Option<()> {
        let mut i = 0;
        while i < buf.len() {
            i += self.inner.read(&mut buf[i..]).await.ok().filter(|&n| n > 0)?;
        }
        Some(())
    }
}

impl<R: Read> LinkRx for WsRx<'_, R> {
    async fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
        loop {
            if self.left > 0 {
                let want = buf.len().min(self.left.min(usize::MAX as u64) as usize);
                let n = self.inner.read(&mut buf[..want]).await.ok().filter(|&n| n > 0)?;
                if let Some(m) = self.mask {
                    for (i, b) in buf[..n].iter_mut().enumerate() {
                        *b ^= m[(self.pos + i) % 4];
                    }
                }
                self.pos += n;
                self.left -= n as u64;
                return Some(n);
            }
            let mut h = [0u8; 14];
            self.read_exact(&mut h[..2]).await?;
            let extra = ws::Header::extra_len(h[1]);
            self.read_exact(&mut h[2..2 + extra]).await?;
            let header = ws::Header::parse(&h[..2 + extra])?;
            match header.opcode {
                ws::OP_CONT | ws::OP_TEXT | ws::OP_BINARY => {
                    (self.left, self.mask, self.pos) = (header.len, header.mask, 0);
                }
                ws::OP_PING | ws::OP_PONG if header.len <= 125 => {
                    let mut payload = vec![0u8; header.len as usize];
                    self.read_exact(&mut payload).await?;
                    if let Some(m) = header.mask {
                        payload.iter_mut().enumerate().for_each(|(i, b)| *b ^= m[i % 4]);
                    }
                    if header.opcode == ws::OP_PING {
                        *self.control.pong.borrow_mut() = Some(payload);
                        self.control.ready.signal(());
                    }
                }
                _ => return None, // close, or a malformed control frame
            }
        }
    }
}

/// MQTT bytes out as masked binary frames, one per packet.
struct WsTx<'c, W> {
    inner: W,
    control: &'c Control,
}

impl<W: Write> WsTx<'_, W> {
    async fn frame(&mut self, opcode: u8, payload: &[u8]) -> bool {
        let mut mask = [0u8; 4];
        Rng::new().read(&mut mask);
        let f = ws::frame(opcode, payload, mask);
        self.inner.write_all(&f).await.is_ok() && self.inner.flush().await.is_ok()
    }
}

impl<W: Write> LinkTx for WsTx<'_, W> {
    async fn send(&mut self, data: &[u8]) -> bool {
        self.frame(ws::OP_BINARY, data).await
    }

    async fn control_ready(&self) {
        self.control.ready.wait().await
    }

    async fn send_control(&mut self) -> bool {
        let pong = self.control.pong.borrow_mut().take();
        match pong {
            Some(p) => self.frame(ws::OP_PONG, &p).await,
            None => true,
        }
    }
}

/// Both TCP halves behind one `Clone` handle, as `TlsConnection::split` requires.
#[derive(Clone, Copy)]
struct Duplex<'s, 'a> {
    r: &'s UnsafeCell<TcpReader<'a>>,
    w: &'s UnsafeCell<TcpWriter<'a>>,
}

impl ErrorType for Duplex<'_, '_> {
    type Error = embassy_net::tcp::Error;
}

impl Read for Duplex<'_, '_> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        // SAFETY: the handshake uses one handle sequentially; after `split` only the TLS reader
        // reads and only the TLS writer writes, so neither half is ever borrowed twice.
        unsafe { &mut *self.r.get() }.read(buf).await
    }
}

impl Write for Duplex<'_, '_> {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        // SAFETY: see `read`.
        unsafe { &mut *self.w.get() }.write(buf).await
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        // SAFETY: see `read`.
        unsafe { &mut *self.w.get() }.flush().await
    }
}

/// Certificate validity is checked once SNTP has set the clock.
struct EpochClock;

impl TlsClock for EpochClock {
    fn now() -> Option<u64> {
        epoch_now().map(u64::from)
    }
}

/// The hardware RNG; it is a true RNG while the Wi-Fi radio runs, which it does whenever TLS is used.
struct HwRng;

impl rand_core::RngCore for HwRng {
    fn next_u32(&mut self) -> u32 {
        Rng::new().random()
    }

    fn next_u64(&mut self) -> u64 {
        (self.next_u32() as u64) << 32 | self.next_u32() as u64
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        Rng::new().read(dest)
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

impl rand_core::CryptoRng for HwRng {}

/// Room for the server's whole certificate chain (Let's Encrypt: ~4 KB).
type Verifier = CertVerifier<'static, Aes128GcmSha256, EpochClock, 6144>;

struct Provider {
    rng: HwRng,
    verifier: Verifier,
}

impl CryptoProvider for Provider {
    type CipherSuite = Aes128GcmSha256;
    type Signature = [u8; 0];

    fn rng(&mut self) -> impl embedded_tls::CryptoRngCore {
        &mut self.rng
    }

    fn verifier(&mut self) -> Result<&mut impl TlsVerifier<Aes128GcmSha256>, TlsError> {
        Ok(&mut self.verifier)
    }
}
