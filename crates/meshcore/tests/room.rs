//! Room server behaviour against simulated clients, exchanging real wire frames.

use meshcore::advert::{ADV_TYPE_ROOM, AdvertData};
use meshcore::crypto::{self, Secret};
use meshcore::identity::{Identity, LocalIdentity};
use meshcore::mesh::{Mesh, MeshApp, MeshCtx};
use meshcore::packet::*;
use meshcore::prefs::NodePrefs;
use meshcore::radio::RadioParams;
use meshcore::repeater::{BoardInfo, Repeater};

const T0: u32 = 1_760_000_000;
const LATER: u64 = 60_000;

fn room_server(prefs: NodePrefs) -> Mesh<Repeater> {
    let id = LocalIdentity::from_seed(&[1; 32]);
    let ctx = MeshCtx::new(id, [1; 16], RadioParams::default(), T0);
    let board = BoardInfo { manufacturer: "Test".into(), batt_mv: 4100, ..Default::default() };
    let mut m = Mesh::new(ctx, Repeater::new_room_server(prefs, board));
    m.begin(0);
    m
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

fn frame(pkt: &Packet) -> Vec<u8> {
    let mut buf = [0u8; 255];
    let n = pkt.write_to(&mut buf).unwrap();
    buf[..n].to_vec()
}

/// A room client: knows the server, records what it receives.
#[derive(Default)]
struct Client {
    peer: Option<(Identity, Secret)>,
    responses: Vec<Vec<u8>>,
    texts: Vec<Vec<u8>>,
    acks: Vec<Vec<u8>>,
}

impl MeshApp for Client {
    fn search_peers_by_hash(&mut self, _ctx: &mut MeshCtx, hash: u8) -> usize {
        self.peer.map(|(id, _)| id.is_hash_match_v1(hash) as usize).unwrap_or(0)
    }
    fn peer_shared_secret(&mut self, _ctx: &mut MeshCtx, _idx: usize) -> Option<Secret> {
        self.peer.map(|(_, s)| s)
    }
    fn on_peer_data_recv(&mut self, _: &mut MeshCtx, _: &Packet, t: u8, _: usize, _: &Secret, data: &[u8]) {
        match t {
            PAYLOAD_TYPE_RESPONSE => self.responses.push(data.to_vec()),
            PAYLOAD_TYPE_TXT_MSG => self.texts.push(data.to_vec()),
            _ => {}
        }
    }
    fn on_peer_path_recv(
        &mut self, _: &mut MeshCtx, _: &Packet, _: usize, _: &Secret, _: u8, _: &[u8], extra_type: u8, extra: &[u8],
    ) -> bool {
        if extra_type == PAYLOAD_TYPE_RESPONSE {
            self.responses.push(extra.to_vec());
        }
        false
    }
    fn on_ack_recv(&mut self, _: &mut MeshCtx, pkt: &Packet, _: u32) -> bool {
        self.acks.push(pkt.payload().to_vec());
        false
    }
}

struct Member {
    m: Mesh<Client>,
    room: Identity,
    secret: Secret,
    ts: u32,
}

impl Member {
    fn new(seed: u8, room: Identity) -> Self {
        let id = LocalIdentity::from_seed(&[seed; 32]);
        let secret = id.calc_shared_secret(&room.pub_key).unwrap();
        let ctx = MeshCtx::new(id, [seed; 16], RadioParams::default(), T0);
        let mut m = Mesh::new(ctx, Client::default());
        m.app.peer = Some((room, secret));
        m.begin(0);
        Self { m, room, secret, ts: T0 + 100 }
    }

    fn pub_key(&self) -> [u8; 32] {
        *self.m.ctx.self_id.pub_key()
    }

    fn next_ts(&mut self) -> u32 {
        self.ts += 10;
        self.ts
    }

    /// Flood ANON_REQ login: `[timestamp][sync_since][password]`.
    fn login(&mut self, sync_since: u32, password: &str) -> Packet {
        let mut data = self.next_ts().to_le_bytes().to_vec();
        data.extend_from_slice(&sync_since.to_le_bytes());
        data.extend_from_slice(password.as_bytes());
        let mut req = self.m.ctx.create_anon_datagram(&self.room, &self.secret, &data).unwrap();
        req.set_route_type(ROUTE_TYPE_FLOOD);
        req.set_path_hash_size_and_count(1, 0);
        *req
    }

    fn zero_hop(&mut self, payload_type: u8, data: &[u8]) -> Packet {
        let mut p = self.m.ctx.create_datagram(payload_type, &self.room, &self.secret, data).unwrap();
        p.set_route_type(ROUTE_TYPE_DIRECT);
        p.path_len = 0;
        *p
    }

    /// TXT_MSG with `flags` (0 = plain post, 3 = CLI command); returns the packet and its data.
    fn text(&mut self, flags: u8, text: &str) -> (Packet, Vec<u8>) {
        let mut data = self.next_ts().to_le_bytes().to_vec();
        data.push(flags << 2);
        data.extend_from_slice(text.as_bytes());
        (self.zero_hop(PAYLOAD_TYPE_TXT_MSG, &data), data)
    }

    fn request(&mut self, body: &[u8]) -> Packet {
        let mut data = self.next_ts().to_le_bytes().to_vec();
        data.extend_from_slice(body);
        self.zero_hop(PAYLOAD_TYPE_REQ, &data)
    }

    /// Tell the server the (zero-hop) path back to us, so it can answer DIRECT.
    fn send_path(&mut self) -> Packet {
        let mut p = self.m.ctx.create_path_return(self.room.hash(), &self.secret, 0, &[], 0, &[]).unwrap();
        p.set_route_type(ROUTE_TYPE_DIRECT);
        p.path_len = 0;
        *p
    }
}

fn ack_of(parts: &[&[u8]]) -> [u8; 4] {
    let mut ack = [0u8; 4];
    crypto::sha256(&mut ack, parts);
    ack
}

fn ack_packet(ack: &[u8]) -> Packet {
    let mut p = Packet::with_type(PAYLOAD_TYPE_ACK);
    p.set_payload(ack);
    p.set_route_type(ROUTE_TYPE_DIRECT);
    p.path_len = 0;
    p
}

fn text_of(data: &[u8], from: usize) -> &[u8] {
    let end = data[from..].iter().position(|&b| b == 0).map_or(data.len(), |p| p + from);
    &data[from..end]
}

/// Deliver every frame in `tx` to `who`.
fn deliver(who: &mut Member, tx: &[Vec<u8>], now: u64) {
    for f in tx {
        who.m.on_rx_frame(now, f, 7.0, -60.0);
    }
}

#[test]
fn room_advert_and_role() {
    let mut r = room_server(NodePrefs::room_server_defaults());
    r.app.start(&mut r.ctx);
    let tx = drain_tx(&mut r, 17_000);
    let pkt = Packet::parse(&tx[0]).unwrap();
    let adv = AdvertData::parse(&pkt.payload()[100..]).unwrap();
    assert_eq!(adv.adv_type, ADV_TYPE_ROOM);
    assert_eq!(adv.name.as_deref(), Some("Rust Room"));
    assert_eq!(r.app.handle_command(&mut r.ctx, 0, "get role"), "> room_server");
    assert_eq!(r.app.handle_command(&mut r.ctx, 0, "get repeat"), "> off");
    assert_eq!(r.app.handle_command(&mut r.ctx, 0, "set repeat on"), "ERR - repeat now deprecated");
    assert_eq!(r.app.handle_command(&mut r.ctx, 0, "neighbors"), "not supported");
}

#[test]
fn posts_are_pushed_to_other_members_until_acked() {
    let mut r = room_server(NodePrefs::room_server_defaults());
    let room_id = r.ctx.self_id.id;
    let mut a = Member::new(2, room_id);
    let mut b = Member::new(3, room_id);

    // public room (no room password): both log in read-write
    let mut now = 1000;
    for who in [&mut a, &mut b] {
        let req = who.login(0, "");
        r.on_rx_frame(now, &frame(&req), 7.0, -60.0);
        now += LATER;
        let tx = drain_tx(&mut r, now);
        assert_eq!(tx.len(), 1, "login reply only");
        deliver(who, &tx, now);
        let resp = who.m.app.responses.pop().expect("login response");
        assert_eq!(resp[4], 0, "RESP_SERVER_LOGIN_OK");
        assert_eq!(resp[6], 0, "not admin");
        assert_eq!(resp[7], 2, "PERM_ACL_READ_WRITE");
        assert_eq!(resp[12], 1, "room firmware level");
    }
    assert_eq!(r.app.clients().len(), 2);

    // A posts; the server ACKs (flood: no path to A yet)
    let (post, data) = a.text(0, "hello room");
    r.on_rx_frame(now, &frame(&post), 7.0, -60.0);
    let tx = drain_tx(&mut r, now + 1000);
    assert_eq!(tx.len(), 1);
    let ack = Packet::parse(&tx[0]).unwrap();
    assert_eq!(ack.payload_type(), PAYLOAD_TYPE_ACK);
    assert_eq!(ack.payload(), ack_of(&[&data, &a.pub_key()]));
    assert_eq!(r.app.room_stats(), Some((1, 0)));

    // a retry (same timestamp) is ACKed again but not stored twice
    r.on_rx_frame(now + 2000, &frame(&post), 7.0, -60.0);
    assert_eq!(r.app.room_stats(), Some((1, 0)));

    // after POST_SYNC_DELAY the post is pushed to B only (never back to its author)
    now += LATER;
    let mut pushed = Vec::new();
    for k in 0..5 {
        pushed.extend(drain_tx(&mut r, now + k * 1000));
    }
    let pushes: Vec<_> = pushed.iter().map(|f| Packet::parse(f).unwrap()).filter(|p| p.payload_type() == PAYLOAD_TYPE_TXT_MSG).collect();
    assert_eq!(pushes.len(), 1, "one push, to B");
    assert_eq!(pushes[0].payload()[0], b.m.ctx.self_id.id.hash());
    deliver(&mut b, &pushed, now + 5000);
    let got = b.m.app.texts.pop().expect("B got the post");
    assert_eq!(got[4] >> 2, 2, "TXT_TYPE_SIGNED_PLAIN");
    assert_eq!(&got[5..9], &a.pub_key()[..4], "author prefix");
    assert_eq!(text_of(&got, 9), b"hello room");
    let post_ts = u32::from_le_bytes(got[..4].try_into().unwrap());

    // B acknowledges: its sync_since moves to the post, nothing more is pushed
    let ack = ack_of(&[&got[..9 + 10], &b.pub_key()]);
    r.on_rx_frame(now + 6000, &frame(&ack_packet(&ack)), 7.0, -60.0);
    let bi = r.app.clients().iter().position(|c| c.id == b.m.ctx.self_id.id).unwrap();
    assert_eq!(r.app.clients()[bi].room.sync_since, post_ts);
    assert_eq!(r.app.clients()[bi].room.pending_ack, 0);
    now += 2 * LATER;
    for k in 0..20 {
        assert!(drain_tx(&mut r, now + k * 1000).is_empty(), "nothing left to push");
    }

    // a system post (serial `room.post`) goes to both members
    assert_eq!(r.app.handle_command(&mut r.ctx, 0, "room.post maintenance at 5"), "OK");
    now += LATER;
    let mut pushed = Vec::new();
    for k in 0..40 {
        pushed.extend(drain_tx(&mut r, now + k * 1000));
    }
    let dests: Vec<u8> = pushed.iter().map(|f| Packet::parse(f).unwrap()).filter(|p| p.payload_type() == PAYLOAD_TYPE_TXT_MSG).map(|p| p.payload()[0]).collect();
    assert!(dests.contains(&a.m.ctx.self_id.id.hash()) && dests.contains(&b.m.ctx.self_id.id.hash()), "{dests:?}");
    assert_eq!(r.app.room_stats().unwrap().0, 2);
}

#[test]
fn unacked_push_gives_up_after_three_attempts() {
    let mut r = room_server(NodePrefs::room_server_defaults());
    let room_id = r.ctx.self_id.id;
    let mut a = Member::new(2, room_id);
    let mut b = Member::new(3, room_id);
    for who in [&mut a, &mut b] {
        let req = who.login(0, "");
        r.on_rx_frame(1000, &frame(&req), 7.0, -60.0);
    }
    drain_tx(&mut r, LATER);
    let (post, _) = a.text(0, "anyone?");
    r.on_rx_frame(LATER, &frame(&post), 7.0, -60.0);
    let mut pushes = 0;
    for k in 0..300u64 {
        pushes += drain_tx(&mut r, 2 * LATER + k * 1000)
            .iter()
            .filter(|f| Packet::parse(f).unwrap().payload_type() == PAYLOAD_TYPE_TXT_MSG)
            .count();
    }
    assert_eq!(pushes, 3, "B never ACKs: 3 attempts (12 s flood timeout each), then evicted");
}

#[test]
fn keep_alive_reports_unsynced_count() {
    let mut r = room_server(NodePrefs::room_server_defaults());
    let room_id = r.ctx.self_id.id;
    let mut a = Member::new(2, room_id);
    let mut b = Member::new(3, room_id);
    for who in [&mut a, &mut b] {
        let req = who.login(0, "");
        r.on_rx_frame(1000, &frame(&req), 7.0, -60.0);
    }
    drain_tx(&mut r, LATER);
    for text in ["one", "two"] {
        let (post, _) = a.text(0, text);
        r.on_rx_frame(LATER, &frame(&post), 7.0, -60.0);
    }
    assert_eq!(drain_tx(&mut r, LATER + 500).len(), 2, "the posts' ACKs");

    // B tells the server its path, then sends a direct keep-alive (REQ type 2)
    let path = b.send_path();
    r.on_rx_frame(LATER + 200, &frame(&path), 7.0, -60.0);
    let ka = b.request(&[0x02]);
    let ka_data = {
        let mut d = b.ts.to_le_bytes().to_vec();
        d.extend_from_slice(&[0x02, 0, 0, 0, 0]);
        d
    };
    r.on_rx_frame(LATER + 300, &frame(&ka), 7.0, -60.0);
    let tx = drain_tx(&mut r, LATER + 1000);
    assert_eq!(tx.len(), 1);
    let ack = Packet::parse(&tx[0]).unwrap();
    assert_eq!(ack.payload_type(), PAYLOAD_TYPE_ACK);
    assert!(ack.is_route_direct());
    assert_eq!(&ack.payload()[..4], &ack_of(&[&ka_data, &b.pub_key()]));
    assert_eq!(ack.payload()[4], 2, "two posts B has not seen");
}

#[test]
fn room_password_admin_cli_and_status() {
    let prefs = NodePrefs { guest_password: "club".into(), ..NodePrefs::room_server_defaults() };
    let mut r = room_server(prefs);
    let room_id = r.ctx.self_id.id;

    // wrong room password: silence
    let mut x = Member::new(4, room_id);
    let req = x.login(0, "nope");
    r.on_rx_frame(1000, &frame(&req), 7.0, -60.0);
    assert!(drain_tx(&mut r, LATER).is_empty());
    assert!(r.app.clients().is_empty());

    // the right one: read-write; the admin password: admin
    let mut m = Member::new(2, room_id);
    let req = m.login(0, "club");
    r.on_rx_frame(LATER, &frame(&req), 7.0, -60.0);
    let mut admin = Member::new(3, room_id);
    let req = admin.login(0, "password");
    r.on_rx_frame(LATER + 10, &frame(&req), 7.0, -60.0);
    let tx = drain_tx(&mut r, 2 * LATER);
    deliver(&mut admin, &tx, 2 * LATER);
    let resp = admin.m.app.responses.pop().unwrap();
    assert_eq!((resp[6], resp[7]), (1, 3), "admin");

    // only admins can run CLI commands; a member's command gets no reply and no ACK
    let (cmd, _) = m.text(3, "get name");
    r.on_rx_frame(3 * LATER, &frame(&cmd), 7.0, -60.0);
    assert!(drain_tx(&mut r, 4 * LATER).is_empty());
    let (cmd, _) = admin.text(3, "get name");
    r.on_rx_frame(4 * LATER, &frame(&cmd), 7.0, -60.0);
    let tx = drain_tx(&mut r, 5 * LATER);
    deliver(&mut admin, &tx, 5 * LATER);
    let reply = admin.m.app.texts.pop().unwrap();
    assert_eq!(reply[4], 1 << 2, "TXT_TYPE_CLI_DATA");
    assert_eq!(text_of(&reply, 5), b"> Rust Room");

    // status: the repeater block with the room counters at the end
    let (post, _) = m.text(0, "hi");
    r.on_rx_frame(5 * LATER, &frame(&post), 7.0, -60.0);
    drain_tx(&mut r, 5 * LATER + 1000);
    let q = admin.request(&[0x01, 0, 0, 0, 0]);
    r.on_rx_frame(6 * LATER, &frame(&q), 7.0, -60.0);
    let tx = drain_tx(&mut r, 7 * LATER);
    deliver(&mut admin, &tx, 7 * LATER);
    let st = admin.m.app.responses.pop().unwrap();
    assert_eq!(u16::from_le_bytes(st[4..6].try_into().unwrap()), 4100, "battery mV");
    assert_eq!(u16::from_le_bytes(st[4 + 48..4 + 50].try_into().unwrap()), 1, "n_posted");

    // only admins are persisted, with their sync_since
    let acl = r.app.acl_serialize();
    assert_eq!(acl.len(), 136);
    assert_eq!(&acl[..32], &admin.pub_key());
    let mut again = room_server(NodePrefs::room_server_defaults());
    let self_id = again.ctx.self_id.clone();
    again.app.acl_load(&acl, &self_id);
    assert!(again.app.clients()[0].is_admin());
}

/// The firmware's hosted room (`set room on`): repeater and room share one radio, so every
/// frame reaches both and neither hears the other's transmissions.
#[test]
fn room_hosted_beside_a_repeater() {
    let mut rep = {
        let id = LocalIdentity::from_seed(&[9; 32]);
        let ctx = MeshCtx::new(id, [9; 16], RadioParams::default(), T0);
        let mut m = Mesh::new(ctx, Repeater::new(NodePrefs::default(), BoardInfo::default()));
        m.begin(0);
        m
    };
    let mut r = room_server(NodePrefs::room_server_defaults());
    assert_ne!(rep.ctx.self_id.id.hash(), r.ctx.self_id.id.hash());
    let mut a = Member::new(2, r.ctx.self_id.id);

    // a login flood that came one hop: the room answers, the repeater re-floods it as usual
    let mut req = a.login(0, "");
    req.set_path_hash_size_and_count(1, 1);
    req.path[0] = 0x42;
    rep.on_rx_frame(1000, &frame(&req), 7.0, -60.0);
    r.on_rx_frame(1000, &frame(&req), 7.0, -60.0);
    let room_tx = drain_tx(&mut r, LATER);
    assert_eq!(room_tx.len(), 1);
    assert_eq!(Packet::parse(&room_tx[0]).unwrap().payload_type(), PAYLOAD_TYPE_PATH);
    let rep_tx = drain_tx(&mut rep, LATER);
    assert_eq!(rep_tx.len(), 1);
    assert_eq!(Packet::parse(&rep_tx[0]).unwrap().path(), &[0x42, rep.ctx.self_id.id.hash()]);

    // a direct post on its last hop is the room's alone
    let (post, _) = a.text(0, "hi");
    rep.on_rx_frame(LATER, &frame(&post), 7.0, -60.0);
    r.on_rx_frame(LATER, &frame(&post), 7.0, -60.0);
    assert!(drain_tx(&mut rep, 2 * LATER).is_empty());
    let room_tx = drain_tx(&mut r, 2 * LATER);
    assert_eq!(Packet::parse(&room_tx[0]).unwrap().payload_type(), PAYLOAD_TYPE_ACK);
    assert_eq!(r.app.room_stats(), Some((1, 0)));
}
