//! MQTT observer sessions, one task per enabled broker, over plain TCP (meshcoretel, ommesh).
//! All feed the same MQTT session loop through [`LinkRx`] / [`LinkTx`].

use alloc::format;
use alloc::string::String;
use alloc::vec;
use core::cell::Cell;
use core::sync::atomic::Ordering;

use embassy_futures::select::{Either3, select, select3};
use embassy_net::Stack;
use embassy_net::dns::DnsQueryType;
use embassy_net::tcp::{TcpReader, TcpSocket, TcpWriter};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::{Mutex, MutexGuard};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use embedded_io_async::Write;
use meshcore::identity::LocalIdentity;
use meshcore::uplink::{self, mqtt};

use crate::net::{BROKER_CONNECTED, BrokerState, Sub, TIME_SYNCED, UPLINK, epoch_now};

const KEEPALIVE_SECS: u16 = 30;

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

/// Only one broker connects at a time (as in the C++ firmware).
static CONNECTING: Mutex<CriticalSectionRawMutex, ()> = Mutex::new(());

#[embassy_executor::task(pool_size = 3)]
async fn broker_task(stack: Stack<'static>, cfg: BrokerConfig) {
    let mut sub = UPLINK.subscriber().unwrap();
    let mut failures = 0u32;
    loop {
        BrokerState::Wait.set(cfg.index);
        stack.wait_config_up().await;
        while !TIME_SYNCED.load(Ordering::Relaxed) {
            Timer::after_secs(2).await;
        }
        let guard = CONNECTING.lock().await;
        BrokerState::Connecting.set(cfg.index);
        if session(stack, &cfg, &mut sub, guard).await {
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

/// One MQTT session over plain TCP; `true` if it got connected before ending.
async fn session(stack: Stack<'static>, cfg: &BrokerConfig, sub: &mut Sub, guard: Guard) -> bool {
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
    mqtt_session(cfg, sub, r, w, &connect, guard).await
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
    // The reader is never cancelled mid-read.
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

    /// Resolves when the link has control traffic of its own to send.
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
