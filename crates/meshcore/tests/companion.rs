//! Companion radio behaviour: apps drive simulated companions over the frame protocol while the
//! nodes exchange real wire frames (directly, or through a repeater).

use meshcore::advert::{ADV_TYPE_CHAT, ADV_TYPE_REPEATER};
use meshcore::companion::*;
use meshcore::identity::{Identity, LocalIdentity};
use meshcore::mesh::{Mesh, MeshApp, MeshCtx};
use meshcore::packet::*;
use meshcore::prefs::NodePrefs;
use meshcore::radio::RadioParams;
use meshcore::repeater::{BoardInfo, Repeater};

const T0: u32 = 1_760_000_000;

trait Node {
    fn rx(&mut self, now: u64, raw: &[u8]);
    fn tx(&mut self, now: u64) -> Vec<Vec<u8>>;
    fn wake(&self) -> u64;
}

impl<A: MeshApp> Node for Mesh<A> {
    fn rx(&mut self, now: u64, raw: &[u8]) {
        self.on_rx_frame(now, raw, 8.0, -70.0);
    }
    fn tx(&mut self, now: u64) -> Vec<Vec<u8>> {
        self.poll(now);
        let mut out = Vec::new();
        while self.tx_pending(now) {
            let f = self.tx_take(now).unwrap();
            out.push(f.bytes().to_vec());
            self.tx_done(now);
        }
        out
    }
    fn wake(&self) -> u64 {
        self.next_wakeup()
    }
}

/// A shared channel: every frame a node sends reaches the nodes linked to it, instantly.
struct Air {
    now: u64,
    links: Vec<(usize, usize)>,
}

impl Air {
    fn new() -> Self {
        Self { now: 1000, links: Vec::new() }
    }

    fn link(&mut self, a: usize, b: usize) {
        self.links.push((a, b));
    }

    fn hears(&self, from: usize, to: usize) -> bool {
        self.links.iter().any(|&(a, b)| (a, b) == (from, to) || (b, a) == (from, to))
    }

    /// Run the network for `ms` milliseconds.
    fn run(&mut self, nodes: &mut [&mut dyn Node], ms: u64) {
        let end = self.now + ms;
        while self.now <= end {
            for i in 0..nodes.len() {
                for raw in nodes[i].tx(self.now) {
                    for (j, node) in nodes.iter_mut().enumerate() {
                        if j != i && self.hears(i, j) {
                            node.rx(self.now, &raw);
                        }
                    }
                }
            }
            let next = nodes.iter().map(|n| n.wake()).min().unwrap_or(self.now + 100);
            self.now = next.clamp(self.now + 1, self.now + 100);
        }
    }
}

fn companion(seed: u8, name: &str) -> Mesh<Companion> {
    let id = LocalIdentity::from_seed(&[seed; 32]);
    let ctx = MeshCtx::new(id, [seed; 16], RadioParams::default(), T0);
    let prefs = CompanionPrefs { node_name: name.into(), ..Default::default() };
    let board = Board { manufacturer: "Test".into(), batt_mv: 3900, ..Default::default() };
    let mut m = Mesh::new(ctx, Companion::new(prefs, board));
    m.begin(0);
    m.app.start(&mut m.ctx);
    m.app.handle_frame(&mut m.ctx, &[CMD_DEVICE_QUERY, 3]); // a v3+ app, like the current ones
    m.app.take_frame();
    m
}

fn repeater(seed: u8) -> Mesh<Repeater> {
    let id = LocalIdentity::from_seed(&[seed; 32]);
    let ctx = MeshCtx::new(id, [seed; 16], RadioParams::default(), T0);
    let board = BoardInfo { manufacturer: "Test".into(), batt_mv: 4100, ..Default::default() };
    let prefs = NodePrefs { advert_interval: 0, flood_advert_interval: 0, ..Default::default() };
    let mut m = Mesh::new(ctx, Repeater::new(prefs, board));
    m.begin(0);
    m
}

/// Send one command frame; return the frames it produced (earlier pending frames are dropped).
fn cmd(m: &mut Mesh<Companion>, frame: &[u8]) -> Vec<Vec<u8>> {
    frames(m);
    m.app.handle_frame(&mut m.ctx, frame);
    frames(m)
}

fn frames(m: &mut Mesh<Companion>) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(f) = m.app.take_frame() {
        out.push(f);
    }
    out
}

/// Frames with this code, ignoring the rest (RX log etc).
fn only(frames: &[Vec<u8>], code: u8) -> Vec<Vec<u8>> {
    frames.iter().filter(|f| f[0] == code).cloned().collect()
}

fn pub_key(m: &Mesh<impl MeshApp>) -> [u8; 32] {
    *m.ctx.self_id.pub_key()
}

fn txt_msg(to: &[u8; 32], text: &str) -> Vec<u8> {
    let mut f = vec![CMD_SEND_TXT_MSG, TXT_TYPE_PLAIN, 0];
    f.extend_from_slice(&(T0 + 5).to_le_bytes());
    f.extend_from_slice(&to[..6]);
    f.extend_from_slice(text.as_bytes());
    f
}

/// Pull the offline queue until NO_MORE_MESSAGES.
fn sync_messages(m: &mut Mesh<Companion>) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let fs = cmd(m, &[CMD_SYNC_NEXT_MESSAGE]);
        let f = fs.into_iter().find(|f| f[0] < 0x80).unwrap(); // not a push
        if f[0] == RESP_CODE_NO_MORE_MESSAGES {
            break;
        }
        out.push(f);
    }
    out
}

/// Advertise both ways (zero hop) so each side has the other as a contact.
fn exchange_adverts(air: &mut Air, a: &mut Mesh<Companion>, b: &mut Mesh<Companion>) {
    assert_eq!(cmd(a, &[CMD_SEND_SELF_ADVERT, 0]), vec![vec![RESP_CODE_OK]]);
    assert_eq!(cmd(b, &[CMD_SEND_SELF_ADVERT, 0]), vec![vec![RESP_CODE_OK]]);
    air.run(&mut [a, b], 2000);
}

#[test]
fn device_query_and_app_start() {
    let mut a = companion(1, "Alice");
    let f = cmd(&mut a, &[CMD_DEVICE_QUERY, 3]);
    assert_eq!(f.len(), 1);
    let d = &f[0];
    assert_eq!(&d[..4], &[RESP_CODE_DEVICE_INFO, FIRMWARE_VER_CODE, 175, 40]);
    assert_eq!(d.len(), 1 + 3 + 4 + 12 + 40 + 20 + 2);
    assert_eq!(&d[20..24], b"Test");
    assert!(d[60..80].starts_with(b"v1.18"));
    assert_eq!(&d[80..], &[0, 0]); // repeat off, path hash mode 0

    let mut start = vec![CMD_APP_START, 3, 0, 0, 0, 0, 0, 0];
    start.extend_from_slice(b"test-app");
    let f = cmd(&mut a, &start);
    let s = &f[0];
    assert_eq!(s[0], RESP_CODE_SELF_INFO);
    assert_eq!(s[1], ADV_TYPE_CHAT);
    assert_eq!(&s[4..36], &pub_key(&a));
    assert_eq!(u32::from_le_bytes(s[48..52].try_into().unwrap()), 869_618); // kHz
    assert_eq!(u32::from_le_bytes(s[52..56].try_into().unwrap()), 62_500); // Hz
    assert_eq!(&s[56..58], &[8, 5]);
    assert_eq!(&s[58..], b"Alice");

    let f = cmd(&mut a, &[CMD_GET_DEVICE_TIME]);
    assert_eq!(f[0][0], RESP_CODE_CURR_TIME);
    assert!(u32::from_le_bytes(f[0][1..5].try_into().unwrap()) >= T0);
    assert_eq!(cmd(&mut a, &[0xEE]), vec![vec![RESP_CODE_ERR, ERR_CODE_UNSUPPORTED_CMD]]);
}

#[test]
fn adverts_create_contacts_and_messages_are_confirmed() {
    let mut air = Air::new();
    air.link(0, 1);
    let mut a = companion(1, "Alice");
    let mut b = companion(2, "Bob");
    exchange_adverts(&mut air, &mut a, &mut b);

    let pushed = only(&frames(&mut a), PUSH_CODE_ADVERT);
    assert_eq!(pushed.len(), 1);
    assert_eq!(&pushed[0][1..33], &pub_key(&b));
    frames(&mut b);

    // contact list
    let f = cmd(&mut a, &[CMD_GET_CONTACTS]);
    assert_eq!(f[0], vec![RESP_CODE_CONTACTS_START, 1, 0, 0, 0]);
    let c = &f[1];
    assert_eq!(c[0], RESP_CODE_CONTACT);
    assert_eq!(&c[1..33], &pub_key(&b));
    assert_eq!(c[33], ADV_TYPE_CHAT);
    assert_eq!(c[35], OUT_PATH_UNKNOWN);
    assert_eq!(&c[100..103], b"Bob");
    assert_eq!(f[2][0], RESP_CODE_END_OF_CONTACTS);
    assert_eq!(f.len(), 3);

    // first message floods; Bob returns the path together with the ACK
    let sent = cmd(&mut a, &txt_msg(&pub_key(&b), "hi Bob"));
    assert_eq!(sent[0][0], RESP_CODE_SENT);
    assert_eq!(sent[0][1], 1, "flood");
    let ack = sent[0][2..6].to_vec();
    air.run(&mut [&mut a, &mut b], 3000);

    let bf = frames(&mut b);
    assert_eq!(only(&bf, PUSH_CODE_MSG_WAITING).len(), 1);
    let msgs = sync_messages(&mut b);
    assert_eq!(msgs.len(), 1);
    let m = &msgs[0];
    assert_eq!(m[0], RESP_CODE_CONTACT_MSG_RECV_V3);
    assert_eq!(&m[4..10], &pub_key(&a)[..6]);
    assert_eq!(m[10], 0, "zero hops, flood");
    assert_eq!(m[11], TXT_TYPE_PLAIN);
    assert_eq!(&m[16..], b"hi Bob");

    let af = frames(&mut a);
    let confirmed = only(&af, PUSH_CODE_SEND_CONFIRMED);
    assert_eq!(confirmed.len(), 1);
    assert_eq!(confirmed[0][1..5], ack[..]);
    assert_eq!(only(&af, PUSH_CODE_PATH_UPDATED).len(), 1);

    // now Alice knows the (zero-hop) path: the reply goes direct
    let sent = cmd(&mut a, &txt_msg(&pub_key(&b), "direct now"));
    assert_eq!(sent[0][1], 0, "direct");
    air.run(&mut [&mut a, &mut b], 3000);
    assert_eq!(only(&frames(&mut a), PUSH_CODE_SEND_CONFIRMED).len(), 1);
    let msgs = sync_messages(&mut b);
    assert_eq!(&msgs[0][16..], b"direct now");
    assert_eq!(msgs[0][10], 0xFF, "direct");
}

#[test]
fn messages_cross_a_repeater_and_learn_the_route() {
    // Alice -- Repeater -- Bob; Alice and Bob do not hear each other
    let mut air = Air::new();
    air.link(0, 1);
    air.link(1, 2);
    let mut a = companion(1, "Alice");
    let mut r = repeater(9);
    let mut b = companion(2, "Bob");

    // flood adverts so they reach through the repeater
    cmd(&mut a, &[CMD_SEND_SELF_ADVERT, 1]);
    cmd(&mut b, &[CMD_SEND_SELF_ADVERT, 1]);
    air.run(&mut [&mut a, &mut r, &mut b], 5000);
    assert!(a.app.lookup_contact(&pub_key(&b)).is_some());
    assert!(b.app.lookup_contact(&pub_key(&a)).is_some());
    let f = cmd(&mut a, &[CMD_GET_ADVERT_PATH, 0].iter().copied().chain(pub_key(&b)).collect::<Vec<_>>());
    assert_eq!(f[0][0], RESP_CODE_ADVERT_PATH);
    assert_eq!(&f[0][5..], &[1, pub_key(&r)[0]], "heard via the repeater");
    frames(&mut a);
    frames(&mut b);

    cmd(&mut a, &txt_msg(&pub_key(&b), "via repeater"));
    air.run(&mut [&mut a, &mut r, &mut b], 10_000);
    let msgs = sync_messages(&mut b);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0][10], 1, "one hop");
    assert_eq!(only(&frames(&mut a), PUSH_CODE_SEND_CONFIRMED).len(), 1);

    let f = cmd(&mut a, &[CMD_GET_CONTACT_BY_KEY].iter().copied().chain(pub_key(&b)).collect::<Vec<_>>());
    assert_eq!(f[0][35], 1, "out path: one hop");
    assert_eq!(f[0][36], pub_key(&r)[0]);

    // the next message goes direct through the repeater
    let sent = cmd(&mut a, &txt_msg(&pub_key(&b), "direct via repeater"));
    assert_eq!(sent[0][1], 0);
    air.run(&mut [&mut a, &mut r, &mut b], 10_000);
    assert_eq!(&sync_messages(&mut b)[0][16..], b"direct via repeater");
    assert_eq!(only(&frames(&mut a), PUSH_CODE_SEND_CONFIRMED).len(), 1);

    // trace along the learned path
    let mut trace = vec![CMD_SEND_TRACE_PATH];
    trace.extend_from_slice(&0x1234_5678u32.to_le_bytes());
    trace.extend_from_slice(&0u32.to_le_bytes());
    trace.push(0); // flags: 1-byte hashes
    trace.push(pub_key(&r)[0]);
    let sent = cmd(&mut a, &trace);
    assert_eq!(sent[0][0], RESP_CODE_SENT);
    air.run(&mut [&mut a, &mut r, &mut b], 5000);
    let t = only(&frames(&mut a), PUSH_CODE_TRACE_DATA);
    assert_eq!(t.len(), 1);
    assert_eq!(t[0][2], 1, "one hash");
    assert_eq!(&t[0][4..8], &0x1234_5678u32.to_le_bytes());
    assert_eq!(t[0][12], pub_key(&r)[0]);
}

#[test]
fn public_channel_and_custom_channel() {
    let mut air = Air::new();
    air.link(0, 1);
    let mut a = companion(1, "Alice");
    let mut b = companion(2, "Bob");

    let f = cmd(&mut a, &[CMD_GET_CHANNEL, 0]);
    assert_eq!(f[0][0], RESP_CODE_CHANNEL_INFO);
    assert_eq!(&f[0][2..8], b"Public");
    assert_eq!(&f[0][34..50], &PUBLIC_GROUP_PSK);

    let mut msg = vec![CMD_SEND_CHANNEL_TXT_MSG, TXT_TYPE_PLAIN, 0];
    msg.extend_from_slice(&(T0 + 1).to_le_bytes());
    msg.extend_from_slice(b"hello all");
    assert_eq!(cmd(&mut a, &msg), vec![vec![RESP_CODE_OK]]);
    air.run(&mut [&mut a, &mut b], 3000);
    let msgs = sync_messages(&mut b);
    assert_eq!(msgs.len(), 1);
    let m = &msgs[0];
    assert_eq!(m[0], RESP_CODE_CHANNEL_MSG_RECV_V3);
    assert_eq!(m[4], 0, "channel 0");
    assert_eq!(&m[11..], b"Alice: hello all");

    // a private channel on slot 3, known to both
    let mut set = vec![CMD_SET_CHANNEL, 3];
    let mut name = [0u8; 32];
    name[..5].copy_from_slice(b"#omsk");
    set.extend_from_slice(&name);
    set.extend_from_slice(&[0x42; 16]);
    assert_eq!(cmd(&mut a, &set), vec![vec![RESP_CODE_OK]]);
    assert!(a.app.take_effects().save_channels);
    cmd(&mut b, &set);
    msg[2] = 3;
    cmd(&mut a, &msg);
    air.run(&mut [&mut a, &mut b], 3000);
    let msgs = sync_messages(&mut b);
    assert_eq!(msgs[0][4], 3);

    // channels persist
    let saved = a.app.channels_serialize();
    let mut c = companion(3, "Carol");
    c.app.channels_load(&saved);
    assert_eq!(c.app.channels()[3].name, "#omsk");
    assert_eq!(c.app.channels()[3].hash, a.app.channels()[3].hash);

    // channel data on the private channel
    let mut data = vec![CMD_SEND_CHANNEL_DATA, 3, OUT_PATH_UNKNOWN];
    data.extend_from_slice(&0xFFFFu16.to_le_bytes());
    data.extend_from_slice(b"\x01\x02\x03");
    assert_eq!(cmd(&mut a, &data), vec![vec![RESP_CODE_OK]]);
    air.run(&mut [&mut a, &mut b], 3000);
    let msgs = sync_messages(&mut b);
    assert_eq!(msgs[0][0], RESP_CODE_CHANNEL_DATA_RECV);
    assert_eq!(&msgs[0][4..], &[3, 0, 0xFF, 0xFF, 3, 1, 2, 3]);
}

#[test]
fn login_status_and_cli_on_a_repeater() {
    let mut air = Air::new();
    air.link(0, 1);
    let mut a = companion(1, "Alice");
    let mut r = repeater(9);
    r.app.start(&mut r.ctx); // boot advert after 16 s
    air.run(&mut [&mut a, &mut r], 20_000);
    let rk = pub_key(&r);
    let idx = a.app.lookup_contact(&rk).expect("repeater advert added");
    assert_eq!(a.app.contacts()[idx - MAX_ANON_CONTACTS].adv_type, ADV_TYPE_REPEATER);
    frames(&mut a);

    // wrong password, then the right one
    let mut login = vec![CMD_SEND_LOGIN];
    login.extend_from_slice(&rk);
    login.extend_from_slice(b"nope");
    let sent = cmd(&mut a, &login);
    assert_eq!(sent[0][0], RESP_CODE_SENT);
    assert_eq!(&sent[0][2..6], &rk[..4]);
    air.run(&mut [&mut a, &mut r], 10_000);
    assert!(only(&frames(&mut a), PUSH_CODE_LOGIN_SUCCESS).is_empty(), "no reply to a bad password");

    let mut login = vec![CMD_SEND_LOGIN];
    login.extend_from_slice(&rk);
    login.extend_from_slice(b"password");
    cmd(&mut a, &login);
    air.run(&mut [&mut a, &mut r], 10_000);
    let ok = only(&frames(&mut a), PUSH_CODE_LOGIN_SUCCESS);
    assert_eq!(ok.len(), 1);
    assert_eq!(ok[0][1] & 1, 1, "admin");
    assert_eq!(&ok[0][2..8], &rk[..6]);
    // repeaters ask for no keep-alive, so no "connection" is kept
    let has = [CMD_HAS_CONNECTION].iter().copied().chain(rk).collect::<Vec<_>>();
    assert_eq!(cmd(&mut a, &has), vec![vec![RESP_CODE_ERR, ERR_CODE_NOT_FOUND]]);

    // status request
    let sent = cmd(&mut a, &[CMD_SEND_STATUS_REQ].iter().copied().chain(rk).collect::<Vec<_>>());
    assert_eq!(sent[0][0], RESP_CODE_SENT);
    air.run(&mut [&mut a, &mut r], 10_000);
    let st = only(&frames(&mut a), PUSH_CODE_STATUS_RESPONSE);
    assert_eq!(st.len(), 1);
    assert_eq!(&st[0][2..8], &rk[..6]);
    assert_eq!(u16::from_le_bytes(st[0][8..10].try_into().unwrap()), 4100, "battery mV");

    // remote CLI: the reply comes back as CLI_DATA text
    let mut cli = vec![CMD_SEND_TXT_MSG, TXT_TYPE_CLI_COMMAND, 0, 0, 0, 0, 0];
    cli.extend_from_slice(&rk[..6]);
    cli.extend_from_slice(b"get name");
    let sent = cmd(&mut a, &cli);
    assert_eq!(&sent[0][2..6], &[0, 0, 0, 0], "no ACK expected");
    air.run(&mut [&mut a, &mut r], 10_000);
    let msgs = sync_messages(&mut a);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0][11], TXT_TYPE_CLI_DATA);
    assert_eq!(&msgs[0][16..], b"> Rust Repeater");

    // binary request (neighbours list) gets matched by its tag
    let mut req = vec![CMD_SEND_BINARY_REQ];
    req.extend_from_slice(&rk);
    req.extend_from_slice(&[0x01, 0, 0, 0, 0]); // GET_STATUS as a binary request
    let sent = cmd(&mut a, &req);
    let tag = sent[0][2..6].to_vec();
    air.run(&mut [&mut a, &mut r], 10_000);
    let resp = only(&frames(&mut a), PUSH_CODE_BINARY_RESPONSE);
    assert_eq!(resp.len(), 1);
    assert_eq!(resp[0][2..6], tag[..]);
}

#[test]
fn telemetry_and_path_discovery() {
    let mut air = Air::new();
    air.link(0, 1);
    let mut a = companion(1, "Alice");
    let mut b = companion(2, "Bob");
    b.app.prefs.telemetry_mode_base = TELEM_MODE_ALLOW_ALL;
    exchange_adverts(&mut air, &mut a, &mut b);
    frames(&mut a);
    let bk = pub_key(&b);

    let mut req = vec![CMD_SEND_TELEMETRY_REQ, 0, 0, 0];
    req.extend_from_slice(&bk);
    cmd(&mut a, &req);
    air.run(&mut [&mut a, &mut b], 5000);
    let t = only(&frames(&mut a), PUSH_CODE_TELEMETRY_RESPONSE);
    assert_eq!(t.len(), 1);
    assert_eq!(&t[0][2..8], &bk[..6]);
    assert_eq!(&t[0][8..12], &[1, 116, 0x01, 0x86], "3.90 V");

    // own telemetry
    let t = cmd(&mut a, &[CMD_SEND_TELEMETRY_REQ, 0, 0, 0]);
    assert_eq!(&t[0][8..12], &[1, 116, 0x01, 0x86]);

    let mut req = vec![CMD_SEND_PATH_DISCOVERY_REQ, 0];
    req.extend_from_slice(&bk);
    let sent = cmd(&mut a, &req);
    assert_eq!(sent[0][1], 1, "always flooded");
    air.run(&mut [&mut a, &mut b], 5000);
    let d = only(&frames(&mut a), PUSH_CODE_PATH_DISCOVERY_RESPONSE);
    assert_eq!(d.len(), 1);
    assert_eq!(&d[0][2..8], &bk[..6]);
    assert_eq!(&d[0][8..], &[0, 0], "zero hops out and back");

    // telemetry denied by default
    let mut req = vec![CMD_SEND_TELEMETRY_REQ, 0, 0, 0];
    req.extend_from_slice(&pub_key(&a));
    cmd(&mut b, &req);
    air.run(&mut [&mut a, &mut b], 5000);
    assert!(only(&frames(&mut b), PUSH_CODE_TELEMETRY_RESPONSE).is_empty());
}

#[test]
fn export_import_share_and_remove_contacts() {
    let mut air = Air::new();
    air.link(0, 1);
    air.link(0, 2);
    let mut a = companion(1, "Alice");
    let mut b = companion(2, "Bob");
    let mut c = companion(3, "Carol");

    // Bob imports Alice's exported card without hearing her
    let card = cmd(&mut a, &[CMD_EXPORT_CONTACT]);
    assert_eq!(card[0][0], RESP_CODE_EXPORT_CONTACT);
    let pkt = Packet::parse(&card[0][1..]).unwrap();
    assert_eq!(pkt.payload_type(), PAYLOAD_TYPE_ADVERT);
    assert_eq!(pkt.route_type(), ROUTE_TYPE_FLOOD);
    let mut import = vec![CMD_IMPORT_CONTACT];
    import.extend_from_slice(&card[0][1..]);
    assert_eq!(cmd(&mut b, &import), vec![vec![RESP_CODE_OK]]);
    air.run(&mut [&mut a, &mut b, &mut c], 100);
    assert!(b.app.lookup_contact(&pub_key(&a)).is_some());
    assert!(air.now < 2000 && only(&frames(&mut b), PUSH_CODE_ADVERT).len() == 1);
    // the stored card can be exported again
    let again = cmd(&mut b, &[CMD_EXPORT_CONTACT].iter().copied().chain(pub_key(&a)).collect::<Vec<_>>());
    assert_eq!(again[0][1..], card[0][1..]);

    // Bob shares Alice's card zero-hop; Carol hears it (Bob links to Alice only... so link Bob-Carol)
    air.link(1, 2);
    assert_eq!(cmd(&mut b, &[CMD_SHARE_CONTACT].iter().copied().chain(pub_key(&a)).collect::<Vec<_>>()), vec![vec![RESP_CODE_OK]]);
    air.run(&mut [&mut a, &mut b, &mut c], 2000);
    assert!(c.app.lookup_contact(&pub_key(&a)).is_some());

    // persistence round trip
    let saved = b.app.contacts_serialize();
    let mut b2 = companion(2, "Bob");
    b2.app.contacts_load(&saved);
    assert_eq!(b2.app.num_contacts(), 1);
    assert_eq!(b2.app.contacts()[0].name_bytes(), b"Alice");

    // remove
    let rm = [CMD_REMOVE_CONTACT].iter().copied().chain(pub_key(&a)).collect::<Vec<_>>();
    assert_eq!(cmd(&mut b, &rm), vec![vec![RESP_CODE_OK]]);
    assert_eq!(b.app.num_contacts(), 0);
    assert_eq!(cmd(&mut b, &rm), vec![vec![RESP_CODE_ERR, ERR_CODE_NOT_FOUND]]);
}

#[test]
fn manual_add_reports_new_adverts_without_adding() {
    let mut air = Air::new();
    air.link(0, 1);
    let mut a = companion(1, "Alice");
    let mut b = companion(2, "Bob");
    assert_eq!(cmd(&mut a, &[CMD_SET_OTHER_PARAMS, 1]), vec![vec![RESP_CODE_OK]]);
    cmd(&mut b, &[CMD_SEND_SELF_ADVERT, 0]);
    air.run(&mut [&mut a, &mut b], 2000);
    let n = only(&frames(&mut a), PUSH_CODE_NEW_ADVERT);
    assert_eq!(n.len(), 1);
    assert_eq!(&n[0][1..33], &pub_key(&b));
    assert_eq!(a.app.num_contacts(), 0);

    // the app adds it explicitly
    let mut add = n[0].clone();
    add[0] = CMD_ADD_UPDATE_CONTACT;
    assert_eq!(cmd(&mut a, &add), vec![vec![RESP_CODE_OK]]);
    assert_eq!(a.app.num_contacts(), 1);

    // auto-add for chat contacts only
    let mut c = companion(3, "Carol");
    air.link(0, 2);
    cmd(&mut a, &[CMD_SET_AUTOADD_CONFIG, AUTO_ADD_CHAT, 0]);
    assert_eq!(cmd(&mut a, &[CMD_GET_AUTOADD_CONFIG]), vec![vec![RESP_CODE_AUTOADD_CONFIG, AUTO_ADD_CHAT, 0]]);
    cmd(&mut c, &[CMD_SEND_SELF_ADVERT, 0]);
    air.run(&mut [&mut a, &mut b, &mut c], 2000);
    assert_eq!(a.app.num_contacts(), 2);
}

#[test]
fn offline_queue_drops_channel_messages_first() {
    let mut air = Air::new();
    air.link(0, 1);
    let mut a = companion(1, "Alice");
    let mut b = companion(2, "Bob");
    exchange_adverts(&mut air, &mut a, &mut b);
    b.app.connected = false; // app away: only the queue fills
    let mut msg = vec![CMD_SEND_CHANNEL_TXT_MSG, TXT_TYPE_PLAIN, 0];
    msg.extend_from_slice(&(T0 + 1).to_le_bytes());
    msg.extend_from_slice(b"chan");
    for i in 0..OFFLINE_QUEUE_SIZE {
        let mut m = msg.clone();
        m[3] = i as u8; // distinct timestamps, distinct packets
        m[4] = (i >> 8) as u8;
        cmd(&mut a, &m);
        air.run(&mut [&mut a, &mut b], 500);
    }
    assert_eq!(b.app.queued_messages(), OFFLINE_QUEUE_SIZE);
    cmd(&mut a, &txt_msg(&pub_key(&b), "important"));
    air.run(&mut [&mut a, &mut b], 3000);
    assert_eq!(b.app.queued_messages(), OFFLINE_QUEUE_SIZE);
    b.app.connected = true;
    let msgs = sync_messages(&mut b);
    assert_eq!(msgs.last().unwrap()[0], RESP_CODE_CONTACT_MSG_RECV_V3);
    assert_eq!(&msgs.last().unwrap()[16..], b"important");
}

#[test]
fn cli_settings_signing_and_keys() {
    let mut a = companion(1, "Alice");
    let cli = |m: &mut Mesh<Companion>, s: &str| {
        let mut f = vec![CMD_RUN_CLI_COMMAND];
        f.extend_from_slice(s.as_bytes());
        let r = cmd(m, &f);
        assert_eq!(r[0][0], RESP_CODE_CLI_REPLY);
        String::from_utf8(r[0][1..].to_vec()).unwrap()
    };
    assert_eq!(cli(&mut a, "get radio"), "> 869.618,62.5,8,5");
    assert_eq!(cli(&mut a, "set name Alice2"), "OK");
    assert_eq!(cli(&mut a, "01|get name"), "01|> Alice2");
    assert_eq!(cli(&mut a, "set tx 30"), "OK");
    assert_eq!(cli(&mut a, "get tx"), "> 22");
    assert_eq!(cli(&mut a, "bogus"), "Unknown command");
    assert!(a.app.take_effects().save_prefs);

    // radio params from the app
    let mut f = vec![CMD_SET_RADIO_PARAMS];
    f.extend_from_slice(&869_525u32.to_le_bytes());
    f.extend_from_slice(&250_000u32.to_le_bytes());
    f.extend_from_slice(&[11, 5]);
    assert_eq!(cmd(&mut a, &f), vec![vec![RESP_CODE_OK]]);
    let fx = a.app.take_effects();
    assert_eq!(fx.apply_radio.unwrap().sf, 11);
    f.push(1); // repeat mode is only allowed on a few frequencies
    assert_eq!(cmd(&mut a, &f), vec![vec![RESP_CODE_ERR, ERR_CODE_ILLEGAL_ARG]]);

    // signing
    let s = cmd(&mut a, &[CMD_SIGN_START]);
    assert_eq!(s[0][0], RESP_CODE_SIGN_START);
    assert_eq!(cmd(&mut a, &[CMD_SIGN_DATA, b'a', b'b']), vec![vec![RESP_CODE_OK]]);
    assert_eq!(cmd(&mut a, &[CMD_SIGN_DATA, b'c']), vec![vec![RESP_CODE_OK]]);
    let sig = cmd(&mut a, &[CMD_SIGN_FINISH]);
    assert_eq!(sig[0][0], RESP_CODE_SIGNATURE);
    let id = Identity::new(pub_key(&a));
    assert!(id.verify(&sig[0][1..65].try_into().unwrap(), b"abc"));
    assert_eq!(cmd(&mut a, &[CMD_SIGN_FINISH]), vec![vec![RESP_CODE_ERR, ERR_CODE_BAD_STATE]]);

    // private key export / import
    let k = cmd(&mut a, &[CMD_EXPORT_PRIVATE_KEY]);
    assert_eq!(k[0][0], RESP_CODE_PRIVATE_KEY);
    let other = LocalIdentity::from_seed(&[7; 32]);
    let mut imp = vec![CMD_IMPORT_PRIVATE_KEY];
    imp.extend_from_slice(other.private_key());
    assert_eq!(cmd(&mut a, &imp), vec![vec![RESP_CODE_OK]]);
    assert_eq!(pub_key(&a), *other.pub_key());
    assert!(a.app.take_effects().save_identity.is_some());

    // stats
    let s = cmd(&mut a, &[CMD_GET_STATS, STATS_TYPE_PACKETS]);
    assert_eq!(s[0].len(), 2 + 7 * 4);
    assert_eq!(cmd(&mut a, &[CMD_GET_STATS, 9]), vec![vec![RESP_CODE_ERR, ERR_CODE_ILLEGAL_ARG]]);
}

/// In-RAM firmware slot for the update tests.
struct Flash {
    mem: Vec<u8>,
    activated: bool,
}

struct RamTarget(std::rc::Rc<std::cell::RefCell<Flash>>, u16);

impl meshcore::ota::FirmwareTarget for RamTarget {
    fn capacity(&self) -> u32 {
        self.0.borrow().mem.len() as u32
    }
    fn erase(&mut self, o: u32, l: u32) -> bool {
        self.0.borrow_mut().mem[o as usize..(o + l) as usize].fill(0xFF);
        true
    }
    fn write(&mut self, o: u32, d: &[u8]) -> bool {
        self.0.borrow_mut().mem[o as usize..o as usize + d.len()].copy_from_slice(d);
        true
    }
    fn read(&mut self, o: u32, b: &mut [u8]) -> bool {
        b.copy_from_slice(&self.0.borrow().mem[o as usize..o as usize + b.len()]);
        true
    }
    fn activate(&mut self, _size: u32) -> bool {
        self.0.borrow_mut().activated = true;
        true
    }
    fn chip_id(&self) -> u16 {
        self.1
    }
}

fn ram_target(chip: u16) -> (std::rc::Rc<std::cell::RefCell<Flash>>, Box<RamTarget>) {
    let flash = std::rc::Rc::new(std::cell::RefCell::new(Flash { mem: vec![0; 64 * 1024], activated: false }));
    (flash.clone(), Box::new(RamTarget(flash, chip)))
}

/// A 20 KB image for `chip` that passes the ESP image checks.
fn test_image(chip: u16) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut img: Vec<u8> = (0..20_000).map(|i| (i % 251) as u8).collect();
    img[0] = 0xE9;
    img[12..14].copy_from_slice(&chip.to_le_bytes());
    img[23] = 1; // hash appended, like espflash images
    let h: [u8; 32] = Sha256::digest(&img[..20_000 - 32]).into();
    img[20_000 - 32..].copy_from_slice(&h);
    img
}

/// Upload `img` zlib-compressed through `send` (a request frame -> the frames it produced), in
/// chunks as large as a frame allows; returns the commit status.
fn upload(send: &mut dyn FnMut(&[u8]) -> Vec<Vec<u8>>, img: &[u8]) -> u8 {
    use meshcore::ota::*;
    use sha2::{Digest, Sha256};
    const CHUNK: usize = 172; // 176-byte frame - command - index
    let sha: [u8; 32] = Sha256::digest(img).into();
    let z = miniz_oxide::deflate::compress_to_vec_zlib(img, 9);
    let mut begin = vec![REQ_TYPE_OTA_BEGIN];
    begin.extend_from_slice(&(z.len() as u32).to_le_bytes());
    begin.extend_from_slice(&sha);
    begin.extend_from_slice(&(CHUNK as u16).to_le_bytes());
    begin.push(1); // zlib
    begin.extend_from_slice(&(img.len() as u32).to_le_bytes());
    let b = send(&begin);
    assert_eq!(b[0][..2], [REQ_TYPE_OTA_BEGIN, 0]);
    let total = u16::from_le_bytes([b[0][2], b[0][3]]) as usize;
    assert_eq!(total, z.len().div_ceil(CHUNK));
    for i in 0..total {
        let mut d = vec![REQ_TYPE_OTA_DATA];
        d.extend_from_slice(&(i as u16).to_le_bytes());
        d.extend_from_slice(&z[i * CHUNK..((i + 1) * CHUNK).min(z.len())]);
        assert!(d.len() <= MAX_FRAME_SIZE);
        assert!(send(&d).is_empty(), "no reply to DATA");
    }
    let st = send(&[REQ_TYPE_OTA_STATUS, 0, 0]);
    assert_eq!(st[0][1], 0);
    assert_eq!(st[0][6], 0, "nothing missing");
    let c = send(&[REQ_TYPE_OTA_COMMIT]);
    assert_eq!(c[0][0], REQ_TYPE_OTA_COMMIT);
    c[0][1]
}

/// Firmware update over the app link: zlib-compressed, chunks as large as a frame allows, and an
/// image for another chip refused.
#[test]
fn ota_over_the_app_link() {
    use meshcore::ota::*;

    let (flash, target) = ram_target(ESP_CHIP_ID_ESP32C6);
    let mut a = companion(1, "Alice");
    a.app.ota.target = Some(target);

    let info = cmd(&mut a, &[REQ_TYPE_OTA_INFO]);
    assert_eq!(info[0][0], REQ_TYPE_OTA_INFO);
    assert_eq!(info[0][1], 1, "has target");

    // an ESP32-S3 image is refused by a C6
    assert_eq!(upload(&mut |f| cmd(&mut a, f), &test_image(ESP_CHIP_ID_ESP32S3)), OtaError::BadImage as u8);
    assert!(!flash.borrow().activated);

    let img = test_image(ESP_CHIP_ID_ESP32C6);
    assert_eq!(upload(&mut |f| cmd(&mut a, f), &img), 0, "commit ok");
    assert!(flash.borrow().activated);
    assert_eq!(&flash.borrow().mem[..img.len()], &img[..]);
    assert!(a.app.take_effects().reboot);
}

/// `start ota ble` on a repeater: a fresh PIN for the admin, then the update over the BLE link
/// with the companion's requests (and its device query, which the update tool asks first).
#[test]
fn repeater_ble_update_window() {
    use meshcore::ota::*;

    let mut r = repeater(2);
    let reply = r.app.handle_command(&mut r.ctx, 0, "start ota ble");
    assert_eq!(reply, "Error", "board without BLE updates");

    r.app.board.ble_ota_name = Some("MeshCore-OTA-R".into());
    let reply = r.app.handle_command(&mut r.ctx, 0, "start ota ble");
    let pin = r.app.take_effects().start_ble_ota.expect("window opened");
    assert!((100_000..1_000_000).contains(&pin));
    assert_eq!(reply, format!("Started: BLE 'MeshCore-OTA-R' PIN {pin} for 15 min"));

    let dev = r.app.handle_link_frame(&[CMD_DEVICE_QUERY, 3]).unwrap();
    assert_eq!(dev[0], RESP_CODE_DEVICE_INFO);
    assert_eq!(&dev[20..25], b"Test\0", "model where the companion has it");
    assert!(dev[60..].starts_with(meshcore::FIRMWARE_VERSION.as_bytes()));
    assert_eq!(r.app.handle_link_frame(&[CMD_APP_START, 0, 0, 0, 0, 0, 0, 0]), None, "not a companion");

    let (flash, target) = ram_target(ESP_CHIP_ID_ESP32S3);
    r.app.ota.target = Some(target);
    let img = test_image(ESP_CHIP_ID_ESP32S3);
    assert_eq!(upload(&mut |f| r.app.handle_link_frame(f).into_iter().collect(), &img), 0, "commit ok");
    assert!(flash.borrow().activated);
    assert!(r.app.take_effects().reboot);
}

/// `start ota`: the Wi-Fi upload page wants a fresh one-time code (on a LAN anyone could reach it).
#[test]
fn repeater_wifi_update_code() {
    let mut r = repeater(3);
    assert_eq!(r.app.handle_command(&mut r.ctx, 0, "start ota"), "Error", "no update slots");

    r.app.board.wifi_ota_ssid = Some("MeshCore-OTA-R".into());
    let reply = r.app.handle_command(&mut r.ctx, 0, "start ota");
    let code = r.app.take_effects().start_ota.expect("started");
    assert!((100_000..1_000_000).contains(&code));
    assert_eq!(reply, format!("Started: http://192.168.4.1/update (Wi-Fi 'MeshCore-OTA-R') code {code}"));

    r.app.board.wifi_ip = Some("10.0.0.5".into());
    let reply = r.app.handle_command(&mut r.ctx, 0, "start ota");
    let lan_code = r.app.take_effects().start_ota.expect("started");
    assert_eq!(reply, format!("Started: http://10.0.0.5/update code {lan_code}"));
    assert_ne!(lan_code, code, "a new code each time");
}

/// `powersaving`: upstream's replies, only on a board that can sleep; the firmware's sleep summary.
#[test]
fn repeater_powersaving_cli() {
    let mut r = repeater(4);
    let cli = |r: &mut Mesh<Repeater>, c: &str| r.app.handle_command(&mut r.ctx, 0, c);
    assert_eq!(cli(&mut r, "powersaving on"), "Board not supported");
    assert_eq!(cli(&mut r, "powersaving"), "off");

    r.app.board.powersaving_supported = true;
    assert_eq!(cli(&mut r, "powersaving on"), "on - After 2 minutes");
    assert!(r.app.prefs.powersaving_enabled);
    assert!(r.app.take_effects().save_prefs);
    assert_eq!(cli(&mut r, "powersaving"), "on");
    assert_eq!(cli(&mut r, "powersaving stats"), "> no sleep yet");
    r.app.board.power_status = "asleep 50%".into();
    assert_eq!(cli(&mut r, "powersaving stats"), "> asleep 50%");
    assert_eq!(cli(&mut r, "powersaving off"), "off");
    assert_eq!(cli(&mut r, "powersaving"), "off");
}

#[test]
fn gps_setting_position_and_telemetry() {
    // no GPS: no settings to offer
    let mut a = companion(1, "Alice");
    assert_eq!(cmd(&mut a, &[CMD_GET_CUSTOM_VARS]), vec![vec![RESP_CODE_CUSTOM_VARS]]);
    assert_eq!(cmd(&mut a, b"\x29gps:1")[0][0], RESP_CODE_ERR);

    let mut air = Air::new();
    air.link(0, 1);
    let mut b = companion(2, "Bob");
    b.app.board.has_gps = true;
    b.app.prefs.telemetry_mode_base = TELEM_MODE_ALLOW_ALL;
    b.app.prefs.telemetry_mode_loc = TELEM_MODE_ALLOW_ALL;
    assert_eq!(&cmd(&mut b, &[CMD_GET_CUSTOM_VARS])[0][1..], b"gps:0");
    assert_eq!(cmd(&mut b, b"\x29gps:1")[0][0], RESP_CODE_OK);
    let fx = b.app.take_effects();
    assert!(fx.gps && fx.save_prefs && b.app.prefs.gps_enabled);
    assert_eq!(&cmd(&mut b, &[CMD_GET_CUSTOM_VARS])[0][1..], b"gps:1");
    assert_eq!(cmd(&mut b, b"\x29gps_interval:30")[0][0], RESP_CODE_OK);
    assert_eq!(b.app.prefs.gps_interval, 30);
    assert_eq!(cmd(&mut b, b"\x29alt:1")[0][0], RESP_CODE_ERR);

    // own telemetry: voltage, then the position (1/10000 degree, centimetres)
    b.app.set_gps_position(55_000_000, -33_500_000, 120_500);
    let t = cmd(&mut b, &[CMD_SEND_TELEMETRY_REQ, 0, 0, 0]);
    assert_eq!(&t[0][8..], &[1, 116, 0x01, 0x86, 1, 136, 0x08, 0x64, 0x70, 0xFA, 0xE3, 0x68, 0x00, 0x2F, 0x12]);

    // another client's request: the position too, as location is allowed
    exchange_adverts(&mut air, &mut a, &mut b);
    frames(&mut a);
    let mut req = vec![CMD_SEND_TELEMETRY_REQ, 0, 0, 0];
    req.extend_from_slice(&pub_key(&b));
    cmd(&mut a, &req);
    air.run(&mut [&mut a, &mut b], 5000);
    let t = only(&frames(&mut a), PUSH_CODE_TELEMETRY_RESPONSE);
    // (the reply is padded to the cipher's block)
    assert_eq!(&t[0][8..23], &[1, 116, 0x01, 0x86, 1, 136, 0x08, 0x64, 0x70, 0xFA, 0xE3, 0x68, 0x00, 0x2F, 0x12]);

    // GPS off: no position
    cmd(&mut b, b"\x29gps:0");
    let t = cmd(&mut b, &[CMD_SEND_TELEMETRY_REQ, 0, 0, 0]);
    assert_eq!(&t[0][8..], &[1, 116, 0x01, 0x86]);
}
