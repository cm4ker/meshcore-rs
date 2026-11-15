//! Network time end to end: a gateway with a stand-in time server, a repeater, and a node two
//! hops away, exchanging real wire frames (see `meshcore::timesync`).

use std::cell::Cell;
use std::collections::VecDeque;
use std::rc::Rc;

use meshcore::identity::LocalIdentity;
use meshcore::mesh::{Mesh, MeshApp, MeshCtx};
use meshcore::packet::*;
use meshcore::prefs::NodePrefs;
use meshcore::radio::RadioParams;
use meshcore::repeater::{BoardInfo, Repeater};
use meshcore::timesync::{self, Clock, NONCE_LEN, TOKEN_LEN, TimeFetch, Token};

/// The server's clock at simulated time 0: some 37 days ahead of the nodes'.
const SERVER_BASE_MS: u64 = 1_763_200_000_000;
const NODE_EPOCH: u32 = 1_760_000_000;

fn server_key() -> LocalIdentity {
    LocalIdentity::from_seed(&[0x5E; 32])
}

/// The time server as the gateway's firmware would reach it: answers at once.
struct FakeServer {
    key: LocalIdentity,
    now: Rc<Cell<u64>>,
    online: Rc<Cell<bool>>,
    queue: VecDeque<[u8; NONCE_LEN]>,
}

impl TimeFetch for FakeServer {
    fn start(&mut self, nonce: [u8; NONCE_LEN]) -> bool {
        if self.online.get() {
            self.queue.push_back(nonce);
        }
        self.online.get()
    }

    fn take(&mut self) -> Option<([u8; NONCE_LEN], Option<[u8; TOKEN_LEN]>)> {
        let nonce = self.queue.pop_front()?;
        Some((nonce, Some(Token::signed(&self.key, SERVER_BASE_MS + self.now.get(), nonce).to_bytes())))
    }
}

fn node(seed: u8, prefs: NodePrefs) -> Mesh<Repeater> {
    let id = LocalIdentity::from_seed(&[seed; 32]);
    let ctx = MeshCtx::new(id, [seed; 16], RadioParams::default(), NODE_EPOCH);
    let board = BoardInfo { manufacturer: "Test".into(), batt_mv: 4100, ..Default::default() };
    let mut m = Mesh::new(ctx, Repeater::new(prefs, board));
    m.begin(0);
    m.app.start(&mut m.ctx);
    m
}

fn prefs(sync: timesync::Mode) -> NodePrefs {
    NodePrefs { time_sync: sync.code(), time_keys: vec![*server_key().pub_key()], ..NodePrefs::default() }
}

/// Gateway 0 - repeater 1 - node 2, in a line: 2 does not hear 0.
struct Net {
    nodes: Vec<Mesh<Repeater>>,
    now: Rc<Cell<u64>>,
    online: Rc<Cell<bool>>,
    /// Every frame sent: by whom, and the bytes.
    sent: Vec<(usize, Vec<u8>)>,
}

const GATEWAY: usize = 0;
const MIDDLE: usize = 1;
const FAR: usize = 2;

impl Net {
    fn new(far: timesync::Mode) -> Self {
        let now = Rc::new(Cell::new(0));
        let online = Rc::new(Cell::new(true));
        let mut gw = prefs(timesync::Mode::Listen);
        gw.time_serve = true;
        let mut nodes = vec![node(10, gw), node(11, prefs(timesync::Mode::Listen)), node(12, prefs(far))];
        nodes[GATEWAY].app.time_fetch =
            Some(Box::new(FakeServer { key: server_key(), now: now.clone(), online: online.clone(), queue: VecDeque::new() }));
        // so that the far node knows the gateway: its flood advert, as `advert` sends it
        let g = &mut nodes[GATEWAY];
        g.app.send_self_advertisement(&mut g.ctx, 20_000, true);
        Self { nodes, now, online, sent: Vec::new() }
    }

    fn neighbours(i: usize) -> &'static [usize] {
        match i {
            GATEWAY => &[MIDDLE],
            MIDDLE => &[GATEWAY, FAR],
            _ => &[MIDDLE],
        }
    }

    fn run(&mut self, ms: u64) {
        let end = self.now.get() + ms;
        while self.now.get() < end {
            let now = self.now.get() + 100;
            self.now.set(now);
            for i in 0..self.nodes.len() {
                for f in drain_tx(&mut self.nodes[i], now) {
                    for &j in Self::neighbours(i) {
                        self.nodes[j].on_rx_frame(now, &f, 10.0, -60.0);
                    }
                    self.sent.push((i, f));
                }
            }
        }
    }

    fn server_ms(&self) -> u64 {
        SERVER_BASE_MS + self.now.get()
    }

    /// How far node `i`'s clock is from the server's (ms, + = ahead).
    fn off(&self, i: usize) -> i64 {
        self.nodes[i].ctx.current_time_ms() as i64 - self.server_ms() as i64
    }

    fn beacons(&self) -> Vec<Vec<u8>> {
        self.sent
            .iter()
            // its own: a beacon of another gateway it repeats has a path
            .filter(|(i, f)| {
                *i == GATEWAY && Packet::parse(f).is_some_and(|p| p.payload_type() == PAYLOAD_TYPE_GRP_DATA && p.path_hash_count() == 0)
            })
            .map(|(_, f)| f.clone())
            .collect()
    }

    fn requests_from(&self, i: usize) -> usize {
        self.sent.iter().filter(|(n, f)| *n == i && Packet::parse(f).is_some_and(|p| p.payload_type() == PAYLOAD_TYPE_ANON_REQ)).count()
    }

    /// A frame heard again later, as a replay would be (the duplicate table forgets in time).
    fn replay_to(&mut self, i: usize, f: &[u8]) {
        let now = self.now.get();
        let n = &mut self.nodes[i];
        n.ctx.tables.clear(&Packet::parse(f).unwrap());
        n.on_rx_frame(now, f, 10.0, -60.0);
        n.poll(now);
    }
}

fn drain_tx<A: MeshApp>(m: &mut Mesh<A>, now: u64) -> Vec<Vec<u8>> {
    m.poll(now);
    let mut out = Vec::new();
    while m.tx_pending(now) {
        let f = m.tx_take(now).unwrap();
        out.push(f.bytes().to_vec());
        m.tx_done(now);
    }
    out
}

#[test]
fn the_first_beacon_sets_every_clock() {
    let mut net = Net::new(timesync::Mode::Listen);
    net.run(4 * 60_000);
    assert!(net.beacons().is_empty(), "the first beacon comes 5 minutes after boot");
    assert_eq!(net.nodes[FAR].app.timesync.clock(), Clock::Unknown);
    net.run(3 * 60_000);
    assert_eq!(net.beacons().len(), 1);
    for i in [GATEWAY, MIDDLE, FAR] {
        assert_eq!(net.nodes[i].app.timesync.clock(), Clock::Trusted, "node {i}");
        // the sim sends without air time, so the correction for it shows: at most two transmissions
        assert!(net.off(i).abs() < 2_000, "node {i} is {} ms off", net.off(i));
    }
    assert_eq!(net.requests_from(FAR), 0, "listen: nothing sent");
}

#[test]
fn a_played_beacon_never_moves_a_clock_back() {
    let mut net = Net::new(timesync::Mode::Listen);
    net.run(7 * 60_000);
    let beacon = net.beacons()[0].clone();
    net.run(2 * 3_600_000);
    let before = net.off(FAR);
    net.replay_to(FAR, &beacon);
    assert_eq!(net.off(FAR), before, "two hours behind: not taken");
    assert_eq!(net.requests_from(FAR), 0, "listen: does not ask either");
}

#[test]
fn a_beacon_far_behind_makes_an_auto_node_ask_and_go_back() {
    let mut net = Net::new(timesync::Mode::Auto);
    net.run(7 * 60_000);
    assert!(net.off(FAR).abs() < 2_000);
    // someone set it an hour ahead by hand
    let far = &mut net.nodes[FAR];
    let ahead = far.ctx.current_time() + 3600;
    let r = far.app.handle_command(&mut far.ctx, 0, &format!("time {ahead}"));
    assert!(r.starts_with("OK"), "{r}");
    assert!(net.off(FAR) > 3_500_000);
    // the next beacon is behind it by an hour: it asks, and the answer may go back
    net.run(6 * 3_600_000 + 60 * 60_000);
    assert!(net.requests_from(FAR) >= 1);
    assert!(net.off(FAR).abs() < 2_000, "{} ms off", net.off(FAR));
    let status = { let n = &net.nodes[FAR]; n.app.timesync.status(&n.ctx) };
    assert!(status.contains("answer from"), "{status}");
}

#[test]
fn a_forged_or_far_jumping_beacon_is_not_taken() {
    let mut net = Net::new(timesync::Mode::Listen);
    net.run(7 * 60_000);
    let before = net.off(FAR);
    let g = timesync::channel();
    for (key, ahead) in [(LocalIdentity::from_seed(&[1; 32]), 1_000), (server_key(), 86_400_000)] {
        let token = Token::signed(&key, net.server_ms() + ahead, [3; 8]);
        let data = timesync::beacon(&net.nodes[GATEWAY].ctx.self_id.id, &token);
        let gw = &mut net.nodes[GATEWAY];
        let mut pkt = gw.ctx.create_group_datagram(PAYLOAD_TYPE_GRP_DATA, &g, &data).unwrap();
        pkt.set_route_type(ROUTE_TYPE_FLOOD);
        let mut raw = [0u8; 255];
        let n = pkt.write_to(&mut raw).unwrap();
        net.replay_to(FAR, &raw[..n]);
        assert!((net.off(FAR) - before).abs() < 200, "forged or a day ahead: not taken");
    }
    let status = { let n = &net.nodes[FAR]; n.app.timesync.status(&n.ctx) };
    assert!(status.contains("refused"), "{status}");
}

#[test]
fn time_sync_by_hand_asks_through_the_middle_repeater() {
    let mut net = Net::new(timesync::Mode::Listen);
    net.run(60_000); // the gateway's flood advert came through, no beacon yet
    let far = &mut net.nodes[FAR];
    let r = far.app.handle_command(&mut far.ctx, 0, "time sync");
    assert!(r.starts_with("OK - asking"), "{r}");
    net.run(5_000);
    assert_eq!(net.nodes[FAR].app.timesync.clock(), Clock::Trusted);
    assert!(net.off(FAR).abs() < 1_000, "{} ms off", net.off(FAR));
    assert!(!net.nodes[FAR].app.prefs.time_gateway.is_empty(), "the gateway is kept for the next boot");
}

#[test]
fn a_gateway_without_its_link_answers_unavailable() {
    let mut net = Net::new(timesync::Mode::Listen);
    net.run(60_000);
    net.online.set(false);
    let far = &mut net.nodes[FAR];
    far.app.handle_command(&mut far.ctx, 0, "time sync");
    net.run(5_000);
    assert_eq!(net.nodes[FAR].app.timesync.clock(), Clock::Unknown);
    let status = { let n = &net.nodes[FAR]; n.app.timesync.status(&n.ctx) };
    assert!(status.contains("can't reach the time server"), "{status}");
}

#[test]
fn a_kept_gateway_is_asked_soon_after_boot() {
    let mut net = Net::new(timesync::Mode::Listen);
    net.run(60_000);
    let far = &mut net.nodes[FAR];
    far.app.handle_command(&mut far.ctx, 0, "time sync");
    net.run(5_000);
    // the far node loses power and comes back in auto mode, with what it kept
    let mut kept = net.nodes[FAR].app.prefs.clone();
    kept.time_sync = timesync::Mode::Auto.code();
    net.nodes[FAR] = node(12, kept);
    net.nodes[GATEWAY].app.prefs.time_beacons = 0; // no beacon to help
    net.run(11 * 60_000);
    assert_eq!(net.nodes[FAR].app.timesync.clock(), Clock::Trusted, "asked within 10 minutes");
}

#[test]
fn a_gateway_advertises_itself_and_takes_another_gateways_beacon_for_its_own() {
    let mut net = Net::new(timesync::Mode::Listen);
    net.run(30_000);
    let advert = net
        .sent
        .iter()
        .find(|(i, f)| *i == GATEWAY && Packet::parse(f).is_some_and(|p| p.payload_type() == PAYLOAD_TYPE_ADVERT && p.is_route_flood()))
        .map(|(_, f)| Packet::parse(f).unwrap())
        .unwrap();
    let adv = meshcore::advert::AdvertData::parse(&advert.payload()[100..]).unwrap();
    assert_eq!(adv.feat1 & timesync::FEAT1_TIME_SERVER, timesync::FEAT1_TIME_SERVER);

    // a second gateway's beacon heard first: this one sends none in that slot
    let other = LocalIdentity::from_seed(&[0x77; 32]);
    let token = Token::signed(&server_key(), net.server_ms(), [4; 8]);
    let data = timesync::beacon(&other.id, &token);
    let gw = &mut net.nodes[MIDDLE];
    let mut pkt = gw.ctx.create_group_datagram(PAYLOAD_TYPE_GRP_DATA, &timesync::channel(), &data).unwrap();
    pkt.set_route_type(ROUTE_TYPE_FLOOD);
    let mut raw = [0u8; 255];
    let n = pkt.write_to(&mut raw).unwrap();
    net.replay_to(GATEWAY, &raw[..n]);
    net.run(10 * 60_000);
    assert!(net.beacons().is_empty(), "another gateway's beacon stands for this one's");
}

#[test]
fn time_prefs_round_trip() {
    let mut p = NodePrefs::default();
    assert_eq!(p.time_sync, timesync::Mode::Listen.code(), "beacons are taken by default");
    assert!(!p.time_serve);
    p.time_sync = timesync::Mode::Auto.code();
    p.time_serve = true;
    p.time_server = "time.example:8080".into();
    p.time_beacons = 6;
    p.time_keys = vec![[0xAB; 32], [0xCD; 32]];
    p.time_gateway = "ab/00/".into();
    let q = NodePrefs::deserialize(&p.serialize());
    assert_eq!(
        (q.time_sync, q.time_serve, q.time_server.as_str(), q.time_beacons, q.time_keys.clone(), q.time_gateway.as_str()),
        (p.time_sync, true, "time.example:8080", 6, p.time_keys.clone(), "ab/00/")
    );
}
