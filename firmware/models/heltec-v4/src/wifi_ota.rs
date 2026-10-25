//! Firmware update over Wi-Fi (CLI `start ota`, also usable remotely over LoRa).
//!
//! Brings up an access point (WPA2 with the admin password when it is long enough), or uses the
//! Wi-Fi station's LAN address, serves an upload page at `/`, streams the image into the inactive
//! slot and reboots into it on trial. Also accepts
//! `curl --data-binary @firmware.bin "http://192.168.4.1/update?code=123456"`.
//!
//! Once a root key is pinned (`set ota.root`), only the `.mcfw` file of a release that root signed
//! is taken: the envelope at its start is checked before the image is written.
//!
//! Uploads must carry the one-time code `start ota` printed: on a LAN anyone could reach the page.
//! A few wrong codes lock uploads until the next `start ota`. The server shuts down (by
//! rebooting) if nothing is uploaded within [`IDLE_TIMEOUT`].

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{IpListenEndpoint, Ipv4Cidr, Runner, Stack, StackResources, StaticConfigV4};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use embedded_io_async::Write;
use esp_hal::peripherals::WIFI;
use esp_radio::wifi::ap::AccessPointConfig;
use esp_radio::wifi::{AuthenticationMethodConfig, Config, ControllerConfig, Interface, WifiController};
use meshcore::ota::{FirmwareTarget, OtaSession};
use meshcore::release::{self, Build, Envelope, Fingerprint, Peek};
use static_cell::StaticCell;

use crate::ota::{FlashTarget, SharedStore, Slots};

pub const AP_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 4, 1);
const IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Write granularity into the slot (whole sectors).
const CHUNK: usize = 4096;
/// Wrong codes before uploads are locked.
const MAX_BAD_CODES: u32 = 3;

/// The code uploads must carry (0 = locked) and the wrong ones seen since it was set.
static UPLOAD_CODE: AtomicU32 = AtomicU32::new(0);
static BAD_CODES: AtomicU32 = AtomicU32::new(0);

/// A new one-time code (also for another `start ota` while the server runs).
pub fn set_code(code: u32) {
    UPLOAD_CODE.store(code, Ordering::Relaxed);
    BAD_CODES.store(0, Ordering::Relaxed);
}

/// Check the `code=` of a request's query string; too many wrong codes lock uploads.
fn check_code(query: &str) -> Result<(), &'static str> {
    let current = UPLOAD_CODE.load(Ordering::Relaxed);
    if current == 0 {
        return Err("uploads locked after wrong codes: run `start ota` again for a new code\n");
    }
    let given = query.split('&').find_map(|kv| kv.strip_prefix("code=")).and_then(|v| v.parse::<u32>().ok());
    if given == Some(current) {
        return Ok(());
    }
    if BAD_CODES.fetch_add(1, Ordering::Relaxed) + 1 >= MAX_BAD_CODES {
        UPLOAD_CODE.store(0, Ordering::Relaxed);
        log::warn!("Wi-Fi OTA: {MAX_BAD_CODES} wrong codes, uploads locked");
    }
    Err("wrong or missing code (the one `start ota` printed)\n")
}

pub struct Params {
    pub ssid: String,
    /// WPA2 passphrase (>= 8 chars), or `None` for an open network.
    pub password: Option<String>,
    pub node_name: String,
    pub version: &'static str,
    /// One-time upload code.
    pub code: u32,
    pub store: SharedStore<'static>,
    pub slots: Slots,
    /// Pinned root keys (`set ota.root`): only releases they signed are installed.
    pub roots: Vec<Fingerprint>,
    /// What a release must be for.
    pub build: Build,
}

/// Serve the upload page on an already running network (the Wi-Fi station's LAN address).
pub fn start_http(spawner: &Spawner, stack: Stack<'static>, params: Params) -> Result<(), &'static str> {
    set_code(params.code);
    spawner.spawn(http_task(stack, params).map_err(|_| "already running, new code set")?);
    Ok(())
}

/// Start the access point and the upload server. Can only be called once per boot.
pub fn start(spawner: &Spawner, wifi: WIFI<'static>, seed: u64, params: Params) -> Result<(), &'static str> {
    set_code(params.code);
    let auth = match params.password.as_deref() {
        Some(pw) => AuthenticationMethodConfig::Wpa2Personal(pw.try_into().map_err(|_| "bad Wi-Fi password")?),
        None => AuthenticationMethodConfig::Open,
    };
    let ap = AccessPointConfig::default()
        .with_ssid(params.ssid.as_str().try_into().map_err(|_| "bad SSID")?)
        .with_authentication(auth);
    let controller = WifiController::new(wifi, ControllerConfig::default().with_initial_config(Config::AccessPoint(ap)))
        .map_err(|_| "Wi-Fi init failed")?;

    let net = embassy_net::Config::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(AP_IP, 24),
        gateway: Some(AP_IP),
        dns_servers: Default::default(),
    });
    static RESOURCES: StaticCell<StackResources<4>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(Interface::access_point(), net, RESOURCES.init(StackResources::new()), seed);

    spawner.spawn(wifi_task(controller).map_err(|_| "task busy")?);
    spawner.spawn(net_task(runner).map_err(|_| "task busy")?);
    spawner.spawn(dhcp_task(stack).map_err(|_| "task busy")?);
    spawner.spawn(http_task(stack, params).map_err(|_| "task busy")?);
    Ok(())
}

#[embassy_executor::task]
async fn wifi_task(controller: WifiController<'static>) {
    loop {
        if let Ok(ev) = controller.wait_for_access_point_connected_event_async().await {
            log::info!("Wi-Fi OTA: {ev:?}");
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface>) {
    runner.run().await
}

#[embassy_executor::task]
async fn dhcp_task(stack: Stack<'static>) {
    use edge_dhcp::io::{self, DEFAULT_SERVER_PORT};
    use edge_dhcp::server::{Server, ServerOptions};
    use edge_nal::UdpBind;
    use edge_nal_embassy::{Udp, UdpBuffers};

    let mut buf = [0u8; 1500];
    let mut gw = [Ipv4Addr::UNSPECIFIED];
    let buffers = UdpBuffers::<2, 1024, 1024, 4>::new();
    let udp = Udp::new(stack, &buffers);
    let Ok(mut socket) = udp.bind(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, DEFAULT_SERVER_PORT))).await
    else {
        log::error!("Wi-Fi OTA: DHCP bind failed");
        return;
    };
    loop {
        let _ = io::server::run(&mut Server::<_, 8>::new_with_et(AP_IP), &ServerOptions::new(AP_IP, Some(&mut gw)), &mut socket, &mut buf)
            .await;
        Timer::after_millis(500).await;
    }
}

fn page(params: &Params) -> String {
    format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content='width=device-width'>\
<title>{name} OTA</title><body style='font-family:sans-serif;max-width:32em;margin:2em auto'>\
<h2>MeshCore repeater: {name}</h2><p>Running <b>{ver}</b> (slot {slot}). Upload a new <code>.bin</code> \
(from <code>espflash save-image</code>) or the <code>.mcfw</code> of a signed release{signed} with the code \
<code>start ota</code> printed:</p>\
<p><input id=c inputmode=numeric placeholder=code size=8> <input type=file id=f accept=.bin,.mcfw> \
<button onclick=up()>Upload</button></p>\
<p><progress id=p max=100 value=0 style='width:100%'></progress></p><pre id=s></pre>\
<script>function up(){{var f=document.getElementById('f').files[0];if(!f)return;\
var x=new XMLHttpRequest(),s=document.getElementById('s');x.open('POST','/update?code='+encodeURIComponent(document.getElementById('c').value));\
x.upload.onprogress=function(e){{document.getElementById('p').value=e.loaded*100/e.total}};\
x.onload=function(){{s.textContent=x.responseText}};x.onerror=function(){{s.textContent='upload failed'}};\
s.textContent='uploading...';x.send(f)}}</script>",
        name = params.node_name,
        ver = params.version,
        slot = params.slots.booted,
        signed = if params.roots.is_empty() { "" } else { " (a root key is pinned: only signed releases)" }
    )
}

async fn respond(socket: &mut TcpSocket<'_>, status: &str, content_type: &str, body: &str) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = socket.write_all(head.as_bytes()).await;
    let _ = socket.write_all(body.as_bytes()).await;
    let _ = socket.flush().await;
}

/// Find `Content-Length` (case-insensitive) in a request head.
fn content_length(head: &str) -> Option<usize> {
    head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case("content-length").then(|| v.trim().parse().ok())?
    })
}

/// Read body bytes into `buf` until it holds `want`.
async fn read_more(socket: &mut TcpSocket<'_>, buf: &mut Vec<u8>, want: usize) -> Result<(), String> {
    let mut tmp = [0u8; 256];
    while buf.len() < want {
        let k = (want - buf.len()).min(tmp.len());
        let n = with_timeout(Duration::from_secs(20), socket.read(&mut tmp[..k]))
            .await
            .map_err(|_| String::from("upload stalled"))?
            .map_err(|_| String::from("connection error"))?;
        if n == 0 {
            return Err(String::from("connection closed"));
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    Ok(())
}

/// A `.mcfw` upload starts with the envelope of its release: read it into `body` (what has
/// arrived of the `len`-byte body so far) and check it. Returns the envelope's length and the
/// image hash it names; an unsigned image passes only while no root key is pinned.
async fn check_envelope(
    socket: &mut TcpSocket<'_>, body: &mut Vec<u8>, len: usize, params: &Params,
) -> Result<(usize, Option<[u8; 32]>), String> {
    loop {
        let need = match release::peek(body) {
            Peek::NotEnvelope if params.roots.is_empty() => return Ok((0, None)),
            Peek::NotEnvelope => return Err(String::from("unsigned image: a root key is pinned, upload the release's .mcfw")),
            Peek::NeedMore(n) => n,
            Peek::Envelope { len: n } if body.len() < n => n,
            Peek::Envelope { len: env_len } => {
                let (envelope, _) =
                    Envelope::parse(&body[..env_len]).ok_or_else(|| String::from("unreadable release envelope"))?;
                if !params.roots.is_empty() {
                    envelope.check(&params.roots, &params.build).map_err(|r| format!("release refused: {r:?}"))?;
                }
                let m = &envelope.manifest;
                if m.image_len as usize != len - env_len {
                    return Err(String::from("the image is not the size its release names"));
                }
                log::info!("Wi-Fi OTA: release {} #{}", m.version, m.counter);
                return Ok((env_len, Some(m.image_sha256)));
            }
        };
        if need > len {
            return Err(String::from("upload too short"));
        }
        read_more(socket, body, need).await?;
    }
}

/// Stream `len` body bytes (some already in `pending`) into the inactive slot and commit;
/// `sha256` is the hash its release names, if it came with one.
async fn receive_image(
    socket: &mut TcpSocket<'_>, target: &mut FlashTarget<'static>, len: usize, pending: &[u8], sha256: Option<[u8; 32]>,
) -> Result<(), String> {
    let mut session = OtaSession::begin(target, len as u32, sha256, CHUNK as u16).map_err(|e| format!("rejected: {e:?}"))?;
    let mut buf = alloc::vec![0u8; CHUNK];
    let mut fill = pending.len().min(len.min(CHUNK));
    buf[..fill].copy_from_slice(&pending[..fill]);
    let mut received = fill;
    let mut index = 0u32;
    let started = Instant::now();
    loop {
        let expected = (len - index as usize * CHUNK).min(CHUNK);
        if fill == expected {
            session.write_chunk(target, index, &buf[..fill]).map_err(|e| format!("write failed: {e:?}"))?;
            index += 1;
            fill = 0;
            if index as usize * CHUNK >= len {
                break;
            }
            continue;
        }
        let want = expected - fill;
        let n = with_timeout(Duration::from_secs(20), socket.read(&mut buf[fill..fill + want]))
            .await
            .map_err(|_| String::from("upload stalled"))?
            .map_err(|_| String::from("connection error"))?;
        if n == 0 {
            return Err(format!("connection closed after {received} of {len} bytes"));
        }
        fill += n;
        received += n;
    }
    session.commit(target).map_err(|e| format!("verification failed: {e:?}"))?;
    log::info!("Wi-Fi OTA: {len} bytes in {} s, verified", started.elapsed().as_secs());
    Ok(())
}

#[embassy_executor::task]
async fn http_task(stack: Stack<'static>, params: Params) {
    let mut target = FlashTarget::new(params.store.clone(), params.slots);
    let mut rx = alloc::vec![0u8; 4096];
    let mut tx = alloc::vec![0u8; 2048];
    let deadline = Instant::now() + IDLE_TIMEOUT;
    log::info!("Wi-Fi OTA: upload server up ({})", params.ssid);
    loop {
        let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
        socket.set_timeout(Some(Duration::from_secs(30)));
        let left = deadline.saturating_duration_since(Instant::now());
        match with_timeout(left, socket.accept(IpListenEndpoint { addr: None, port: 80 })).await {
            Err(_) => {
                log::warn!("Wi-Fi OTA: no upload within {} min, shutting down", IDLE_TIMEOUT.as_secs() / 60);
                crate::reset::hard_reset();
            }
            Ok(Err(_)) => continue,
            Ok(Ok(())) => {}
        }

        // request head
        let mut head = [0u8; 1024];
        let mut n = 0;
        let split = loop {
            match with_timeout(Duration::from_secs(10), socket.read(&mut head[n..])).await {
                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break None,
                Ok(Ok(k)) => n += k,
            }
            if let Some(p) = head[..n].windows(4).position(|w| w == b"\r\n\r\n") {
                break Some(p + 4);
            }
            if n == head.len() {
                break None;
            }
        };
        let Some(split) = split else {
            socket.abort();
            continue;
        };
        let text = core::str::from_utf8(&head[..split]).unwrap_or("");
        let mut words = text.split_whitespace();
        let (method, target_path) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
        let (path, query) = target_path.split_once('?').unwrap_or((target_path, ""));

        match (method, path) {
            ("GET", "/") | ("GET", "/update") => respond(&mut socket, "200 OK", "text/html", &page(&params)).await,
            ("POST", "/update") => match content_length(text) {
                // before any of the body is taken
                _ if let Err(e) = check_code(query) => respond(&mut socket, "403 Forbidden", "text/plain", e).await,
                None => respond(&mut socket, "411 Length Required", "text/plain", "Content-Length required\n").await,
                Some(len) if len as u32 > target.capacity() => {
                    respond(&mut socket, "413 Payload Too Large", "text/plain", "image larger than the slot\n").await
                }
                Some(len) => match async {
                    let mut body = head[split..n].to_vec();
                    let (env_len, sha256) = check_envelope(&mut socket, &mut body, len, &params).await?;
                    receive_image(&mut socket, &mut target, len - env_len, &body[env_len..], sha256).await
                }
                .await
                {
                    Ok(()) => {
                        respond(&mut socket, "200 OK", "text/plain", "OK: image verified, rebooting into it\n").await;
                        Timer::after_secs(2).await;
                        crate::reset::hard_reset();
                    }
                    Err(e) => {
                        log::warn!("Wi-Fi OTA: {e}");
                        respond(&mut socket, "400 Bad Request", "text/plain", &format!("{e}\n")).await;
                    }
                },
            },
            _ => respond(&mut socket, "404 Not Found", "text/plain", "not found\n").await,
        }
        socket.close();
        Timer::after_millis(200).await;
        socket.abort();
    }
}
