//! A simulated MeshCore network on the PC, for testing companion apps and client libraries.
//!
//! Companions listen on TCP ports (the same `<`/`>` framing as the USB serial interface, so
//! `meshcore-cli -t 127.0.0.1 -p 5000` or the Python `meshcore` library can attach). A repeater
//! (admin password `password`) sits between them: companion 0 and companion 1 only hear each
//! other through it unless `--direct` is given.
//!
//! Usage: `companion-sim [--port 5000] [--companions 2] [--direct]`

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use meshcore::companion::{Board, Companion, CompanionPrefs, MAX_FRAME_SIZE};
use meshcore::identity::LocalIdentity;
use meshcore::mesh::{Mesh, MeshApp, MeshCtx};
use meshcore::prefs::NodePrefs;
use meshcore::radio::RadioParams;
use meshcore::repeater::{BoardInfo, Repeater};
use meshcore::util::to_hex;

/// One TCP client of a companion, with its receive state.
struct Link {
    listener: TcpListener,
    stream: Option<TcpStream>,
    rx: Vec<u8>,
}

impl Link {
    fn new(port: u16) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap_or_else(|e| panic!("bind port {port}: {e}"));
        listener.set_nonblocking(true).unwrap();
        Self { listener, stream: None, rx: Vec::new() }
    }

    /// Accept a client, read whatever arrived; returns complete command frames.
    fn poll(&mut self, name: &str) -> Vec<Vec<u8>> {
        if let Ok((s, addr)) = self.listener.accept() {
            s.set_nonblocking(true).unwrap();
            s.set_nodelay(true).ok();
            println!("[{name}] app connected from {addr}");
            self.stream = Some(s); // a new client replaces the old one
            self.rx.clear();
        }
        let Some(s) = self.stream.as_mut() else {
            return Vec::new();
        };
        let mut buf = [0u8; 1024];
        loop {
            match s.read(&mut buf) {
                Ok(0) => {
                    println!("[{name}] app disconnected");
                    self.stream = None;
                    break;
                }
                Ok(n) => self.rx.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => {
                    println!("[{name}] app link error: {e}");
                    self.stream = None;
                    break;
                }
            }
        }
        // frames: '<' len_lo len_hi data
        let mut frames = Vec::new();
        loop {
            match self.rx.iter().position(|&c| c == b'<') {
                Some(0) => {}
                Some(p) => {
                    self.rx.drain(..p);
                }
                None => {
                    self.rx.clear();
                    break;
                }
            }
            if self.rx.len() < 3 {
                break;
            }
            let len = u16::from_le_bytes([self.rx[1], self.rx[2]]) as usize;
            if self.rx.len() < 3 + len {
                break;
            }
            let frame: Vec<u8> = self.rx[3..3 + len].to_vec();
            self.rx.drain(..3 + len);
            frames.push(frame[..frame.len().min(MAX_FRAME_SIZE)].to_vec());
        }
        frames
    }

    fn send(&mut self, frame: &[u8]) {
        if let Some(s) = self.stream.as_mut() {
            let mut out = vec![b'>', frame.len() as u8, (frame.len() >> 8) as u8];
            out.extend_from_slice(frame);
            s.set_nonblocking(false).ok();
            if s.write_all(&out).is_err() {
                self.stream = None;
            } else {
                s.set_nonblocking(true).ok();
            }
        }
    }
}

fn epoch_now() -> u32 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as u32).unwrap_or(0)
}

fn seed_for(tag: &str) -> [u8; 32] {
    let mut seed = [0u8; 32];
    meshcore::crypto::sha256(&mut seed, &[b"companion-sim", tag.as_bytes()]);
    seed
}

fn drain_tx<A: MeshApp>(m: &mut Mesh<A>, now: u64) -> Vec<Vec<u8>> {
    m.poll(now);
    let mut out = Vec::new();
    while m.tx_pending(now) {
        if let Some(f) = m.tx_take(now) {
            out.push(f.bytes().to_vec());
            m.tx_done(now);
        }
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let base_port: u16 = arg("--port").and_then(|v| v.parse().ok()).unwrap_or(5000);
    let n: usize = arg("--companions").and_then(|v| v.parse().ok()).unwrap_or(2).max(1);
    let direct = args.iter().any(|a| a == "--direct");

    let start = Instant::now();
    let now_ms = || start.elapsed().as_millis() as u64;

    let mut companions: Vec<(Mesh<Companion>, Link, String)> = (0..n)
        .map(|i| {
            let name = format!("Sim{i}");
            let id = LocalIdentity::from_seed(&seed_for(&name));
            let mut rng_seed = [0u8; 16];
            rng_seed.copy_from_slice(&seed_for(&format!("rng{i}"))[..16]);
            let ctx = MeshCtx::new(id, rng_seed, RadioParams::default(), epoch_now());
            let prefs = CompanionPrefs { node_name: name.clone(), ..Default::default() };
            let board = Board { manufacturer: "companion-sim".into(), batt_mv: 3950, mcu_temp_c: Some(31.5), ..Default::default() };
            let mut m = Mesh::new(ctx, Companion::new(prefs, board));
            m.begin(now_ms());
            m.app.start(&mut m.ctx);
            m.app.connected = false;
            let port = base_port + i as u16;
            println!("{name}: tcp 127.0.0.1:{port}  key {}", to_hex(m.ctx.self_id.pub_key()));
            (m, Link::new(port), name)
        })
        .collect();

    let rid = LocalIdentity::from_seed(&seed_for("repeater"));
    let mut rprefs = NodePrefs { node_name: "Sim Repeater".into(), ..Default::default() };
    rprefs.flood_advert_interval = 0;
    let rctx = MeshCtx::new(rid, [7; 16], RadioParams::default(), epoch_now());
    let rboard = BoardInfo { manufacturer: "companion-sim".into(), batt_mv: 4100, ..Default::default() };
    let mut repeater = Mesh::new(rctx, Repeater::new(rprefs, rboard));
    repeater.begin(now_ms());
    repeater.app.start(&mut repeater.ctx);
    println!("Sim Repeater: key {} (admin password: password)", to_hex(repeater.ctx.self_id.pub_key()));
    println!("topology: {}", if direct { "everyone hears everyone" } else { "Sim0 - repeater - Sim1 (Sim2.. hear everyone)" });

    // node index: companions 0..n, repeater n
    let hears = |from: usize, to: usize| -> bool {
        if from == to {
            return false;
        }
        direct || !((from == 0 && to == 1) || (from == 1 && to == 0))
    };

    loop {
        let now = now_ms();
        for (m, link, name) in companions.iter_mut() {
            for f in link.poll(name) {
                m.app.connected = true;
                m.app.handle_frame(&mut m.ctx, &f);
            }
            m.app.connected = link.stream.is_some();
        }

        // air
        let mut sent: Vec<(usize, Vec<u8>)> = Vec::new();
        for (i, (m, _, _)) in companions.iter_mut().enumerate() {
            sent.extend(drain_tx(m, now).into_iter().map(|f| (i, f)));
        }
        sent.extend(drain_tx(&mut repeater, now).into_iter().map(|f| (n, f)));
        for (from, raw) in sent {
            for (j, (m, _, _)) in companions.iter_mut().enumerate() {
                if hears(from, j) {
                    m.on_rx_frame(now, &raw, 9.5, -62.0);
                }
            }
            if hears(from, n) {
                repeater.on_rx_frame(now, &raw, 9.5, -62.0);
            }
        }

        for (m, link, name) in companions.iter_mut() {
            while let Some(f) = m.app.take_frame() {
                link.send(&f);
            }
            let fx = m.app.take_effects();
            if fx.reboot {
                println!("[{name}] reboot requested (ignored)");
            }
            if let Some(r) = fx.apply_radio {
                println!("[{name}] radio -> {r:?}");
            }
        }
        for line in repeater.app.take_effects().console {
            println!("[repeater] {line}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
