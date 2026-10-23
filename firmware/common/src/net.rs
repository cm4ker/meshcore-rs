//! Wi-Fi station: the MQTT observer uplink, SNTP time and the LAN address for Wi-Fi updates.
//!
//! The main loop turns heard packets into JSON (see `meshcore::uplink`) and publishes them on
//! [`UPLINK`]; one task per enabled broker (see `broker`) keeps an MQTT session and forwards them.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_net::dns::DnsQueryType;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{Runner, Stack, StackResources};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::pubsub::PubSubChannel;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use esp_hal::peripherals::WIFI;
use esp_radio::wifi::sta::StationConfig;
use esp_radio::wifi::{AuthenticationMethodConfig, Config, ControllerConfig, Interface, WifiController};
use static_cell::StaticCell;

/// A message for every connected broker.
pub struct UplinkMsg {
    /// `packets` or `status`.
    pub leaf: &'static str,
    pub json: String,
    pub retain: bool,
}

pub static UPLINK: PubSubChannel<CriticalSectionRawMutex, Arc<UplinkMsg>, 24, 4, 1> = PubSubChannel::new();
pub type Sub = embassy_sync::pubsub::Subscriber<'static, CriticalSectionRawMutex, Arc<UplinkMsg>, 24, 4, 1>;
/// Epoch seconds from SNTP, applied to the mesh clock by the main loop.
pub static TIME_SYNC: Signal<CriticalSectionRawMutex, u32> = Signal::new();
/// A broker (re)connected: the main loop publishes an `online` status.
pub static BROKER_CONNECTED: AtomicBool = AtomicBool::new(false);
pub static WIFI_UP: AtomicBool = AtomicBool::new(false);
pub static TIME_SYNCED: AtomicBool = AtomicBool::new(false);
/// SNTP time minus uptime, in seconds (0 until the first sync).
static EPOCH_AT_BOOT: AtomicU32 = AtomicU32::new(0);
/// Per broker (index into `meshcore::repeater::MQTT_BROKERS`): see [`BrokerState`].
pub static BROKER_STATE: [AtomicU8; 4] = [const { AtomicU8::new(0) }; 4];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum BrokerState {
    Off = 0,
    Wait = 1,
    Connecting = 2,
    Up = 3,
    Backoff = 4,
}

impl BrokerState {
    pub fn load(i: usize) -> Self {
        match BROKER_STATE[i].load(Ordering::Relaxed) {
            1 => Self::Wait,
            2 => Self::Connecting,
            3 => Self::Up,
            4 => Self::Backoff,
            _ => Self::Off,
        }
    }

    pub fn set(self, i: usize) {
        BROKER_STATE[i].store(self as u8, Ordering::Relaxed);
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Wait => "wait",
            Self::Connecting => "conn",
            Self::Up => "up",
            Self::Backoff => "backoff",
        }
    }
}

pub struct StationParams {
    pub ssid: String,
    pub password: String,
}

/// Bring up the station; returns the network stack (for LAN updates).
pub fn start(spawner: &Spawner, wifi: WIFI<'static>, seed: u64, params: StationParams) -> Result<Stack<'static>, &'static str> {
    let auth = if params.password.is_empty() {
        AuthenticationMethodConfig::Open
    } else {
        AuthenticationMethodConfig::Wpa2Personal(params.password.as_str().try_into().map_err(|_| "bad Wi-Fi password")?)
    };
    let sta = StationConfig::default()
        .with_ssid(params.ssid.as_str().try_into().map_err(|_| "bad SSID")?)
        .with_authentication(auth);
    let controller = WifiController::new(wifi, ControllerConfig::default().with_initial_config(Config::Station(sta)))
        .map_err(|_| "Wi-Fi init failed")?;
    static RESOURCES: StaticCell<StackResources<8>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        Interface::station(),
        embassy_net::Config::dhcpv4(Default::default()),
        RESOURCES.init(StackResources::new()),
        seed,
    );
    spawner.spawn(station_task(controller).map_err(|_| "task busy")?);
    spawner.spawn(net_task(runner).map_err(|_| "task busy")?);
    spawner.spawn(sntp_task(stack).map_err(|_| "task busy")?);
    Ok(stack)
}

#[embassy_executor::task]
async fn station_task(mut controller: WifiController<'static>) {
    // modem sleep between beacons, the ESP-IDF station default (esp-radio keeps the radio on)
    if let Err(e) = controller.set_power_saving(esp_radio::wifi::PowerSaveMode::Minimum) {
        log::warn!("Wi-Fi: power saving not set: {e:?}");
    }
    loop {
        log::info!("Wi-Fi: connecting");
        match controller.connect_async().await {
            Ok(_) => {
                log::info!("Wi-Fi: connected");
                WIFI_UP.store(true, Ordering::Relaxed);
                let _ = controller.wait_for_disconnect_async().await;
                WIFI_UP.store(false, Ordering::Relaxed);
                log::warn!("Wi-Fi: disconnected");
            }
            Err(e) => log::warn!("Wi-Fi: connect failed: {e:?}"),
        }
        Timer::after_secs(5).await;
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface>) {
    runner.run().await
}

/// Seconds between 1900-01-01 and 1970-01-01.
const NTP_UNIX_OFFSET: u32 = 2_208_988_800;

async fn sntp_query(stack: Stack<'static>, server: &str) -> Option<u32> {
    let addr = *stack.dns_query(server, DnsQueryType::A).await.ok()?.first()?;
    let mut rx_meta = [PacketMetadata::EMPTY; 2];
    let mut tx_meta = [PacketMetadata::EMPTY; 2];
    let (mut rx, mut tx) = ([0u8; 128], [0u8; 128]);
    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx, &mut tx_meta, &mut tx);
    socket.bind(0).ok()?;
    let mut req = [0u8; 48];
    req[0] = 0x23; // LI 0, version 4, client mode
    socket.send_to(&req, (addr, 123)).await.ok()?;
    let mut resp = [0u8; 48];
    let (n, _) = with_timeout(Duration::from_secs(5), socket.recv_from(&mut resp)).await.ok()?.ok()?;
    // reject short replies, kiss-of-death (stratum 0) and unsynchronised servers (LI 3)
    if n < 48 || resp[1] == 0 || resp[0] >> 6 == 3 {
        return None;
    }
    let secs = u32::from_be_bytes(resp[40..44].try_into().ok()?);
    Some(secs.wrapping_sub(NTP_UNIX_OFFSET))
}

#[embassy_executor::task]
async fn sntp_task(stack: Stack<'static>) {
    const SERVERS: [&str; 3] = ["0.ru.pool.ntp.org", "ntp.ix.ru", "pool.ntp.org"];
    let mut k = 0;
    loop {
        stack.wait_config_up().await;
        match sntp_query(stack, SERVERS[k % SERVERS.len()]).await {
            Some(t) if t > meshcore::uplink::MIN_TOKEN_TIME => {
                EPOCH_AT_BOOT.store(t - Instant::now().as_secs() as u32, Ordering::Relaxed);
                TIME_SYNC.signal(t);
                TIME_SYNCED.store(true, Ordering::Relaxed);
                Timer::after_secs(3600).await;
            }
            _ => {
                k += 1;
                Timer::after_secs(30).await;
            }
        }
    }
}

/// Wall-clock seconds from SNTP, once synced.
pub fn epoch_now() -> Option<u32> {
    match EPOCH_AT_BOOT.load(Ordering::Relaxed) {
        0 => None,
        base => Some(base + Instant::now().as_secs() as u32),
    }
}

/// `ip:a.b.c.d` of the station, if it has one.
pub fn ipv4(stack: &Stack<'static>) -> Option<String> {
    stack.config_v4().map(|c| format!("{}", c.address.address()))
}
