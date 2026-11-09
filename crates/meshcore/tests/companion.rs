//! Companion radio behaviour: apps drive simulated companions over the frame protocol while the
//! nodes exchange real wire frames (directly, or through a repeater).

use meshcore::advert::{ADV_TYPE_CHAT, ADV_TYPE_REPEATER};
use meshcore::companion::*;
use meshcore::fwstate::FwState;
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
fn telemetry_carries_the_environment_sensors_when_allowed() {
    let mut air = Air::new();
    air.link(0, 1);
    let mut a = companion(1, "Alice");
    let mut b = companion(2, "Bob");
    b.app.prefs.telemetry_mode_base = TELEM_MODE_ALLOW_ALL;
    // an INA219 on channel 2: 4.10 V
    let sensors = vec![2, 116, 0x01, 0x9A];
    b.app.board.sensor_lpp = sensors.clone();
    exchange_adverts(&mut air, &mut a, &mut b);
    frames(&mut a);
    let mut req = vec![CMD_SEND_TELEMETRY_REQ, 0, 0, 0];
    req.extend_from_slice(&pub_key(&b));
    let carries = |t: &[u8]| t.windows(sensors.len()).any(|w| w == sensors);

    // the environment is denied by default
    cmd(&mut a, &req);
    air.run(&mut [&mut a, &mut b], 5000);
    let t = only(&frames(&mut a), PUSH_CODE_TELEMETRY_RESPONSE);
    assert_eq!(t.len(), 1);
    assert!(!carries(&t[0]));

    b.app.prefs.telemetry_mode_env = TELEM_MODE_ALLOW_ALL;
    cmd(&mut a, &req);
    air.run(&mut [&mut a, &mut b], 5000);
    let t = only(&frames(&mut a), PUSH_CODE_TELEMETRY_RESPONSE);
    assert_eq!(t.len(), 1);
    assert!(carries(&t[0]));

    // the app sees all of its own
    let t = cmd(&mut b, &[CMD_SEND_TELEMETRY_REQ, 0, 0, 0]);
    assert!(t[0].ends_with(&sensors));
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
    // above the chip's limit: said so, and nothing changes
    assert_eq!(cli(&mut a, "set tx 30"), "Error: 30 dBm is out of range: the radio chip gives -9 to 22 dBm");
    assert_eq!(cli(&mut a, "set tx high"), "Error: a number, -9 to 22 dBm");
    assert_eq!(cli(&mut a, "set tx 17"), "OK");
    assert_eq!(cli(&mut a, "get tx"), "> 17");
    // on a Heltec V4 the amplifier after the chip is named too
    a.app.ota.build.board = meshcore::release::board::HELTEC_V4;
    assert_eq!(
        cli(&mut a, "set tx 28"),
        "Error: 28 dBm is out of range: the radio chip gives -9 to 22 dBm; the amplifier after it adds about 12 dB (tx 16 gives about 28 dBm)"
    );
    assert_eq!(cli(&mut a, "get tx"), "> 17");
    assert_eq!(cli(&mut a, "bogus"), "Unknown command");
    assert!(a.app.take_effects().save_prefs);
    // a key prefix whose hex half ends inside a letter of two bytes: an error, not a panic
    assert!(cli(&mut a, "set fw.source aaaaaaaé").starts_with("Error"));
    let mut f = vec![CMD_SET_CUSTOM_VAR];
    f.extend_from_slice("fw_source:aaaaaaaé".as_bytes());
    assert_eq!(cmd(&mut a, &f), vec![vec![RESP_CODE_ERR, ERR_CODE_ILLEGAL_ARG]]);

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

/// Remote CLI from a contact allowed it: a command from before the node started is not taken, so
/// a recorded `reboot` played again can't keep restarting it (upstream takes any).
#[test]
fn remote_cli_on_a_companion_is_not_played_again() {
    let mut air = Air::new();
    air.link(0, 1);
    let mut a = companion(1, "Alice");
    let mut b = companion(2, "Bob");
    exchange_adverts(&mut air, &mut a, &mut b);
    let mut bob = cmd(&mut a, &[CMD_GET_CONTACT_BY_KEY].iter().copied().chain(pub_key(&b)).collect::<Vec<_>>()).remove(0);
    bob[0] = CMD_ADD_UPDATE_CONTACT;
    bob[34] |= 0x10; // remote CLI allowed
    assert_eq!(cmd(&mut a, &bob), vec![vec![RESP_CODE_OK]]);

    let mut cli = vec![CMD_SEND_TXT_MSG, TXT_TYPE_CLI_COMMAND, 0, 0, 0, 0, 0];
    cli.extend_from_slice(&pub_key(&a)[..6]);
    cli.extend_from_slice(b"get name");
    cmd(&mut b, &cli);
    let recorded: Vec<Vec<u8>> = (0..50).flat_map(|k| b.tx(air.now + k * 100)).collect();
    assert_eq!(recorded.len(), 1);
    air.now += 5000;
    a.rx(air.now, &recorded[0]);
    air.run(&mut [&mut a, &mut b], 5000);
    let replies = sync_messages(&mut b);
    assert_eq!(replies.len(), 1);
    assert_eq!(&replies[0][16..], b"> Alice");

    // Alice restarts, her clock where it stopped (the firmware keeps it); Bob's old command is heard again
    let started = a.ctx.current_time() + 1;
    let mut a = Mesh::new(
        MeshCtx::new(LocalIdentity::from_seed(&[1; 32]), [1; 16], RadioParams::default(), started),
        Companion::new(CompanionPrefs { node_name: "Alice".into(), ..Default::default() }, Board::default()),
    );
    a.begin(0);
    a.app.start(&mut a.ctx);
    assert_eq!(cmd(&mut a, &bob), vec![vec![RESP_CODE_OK]]);
    a.rx(air.now, &recorded[0]);
    air.run(&mut [&mut a, &mut b], 5000);
    assert!(sync_messages(&mut b).is_empty(), "the command from before the restart is ignored");

    // a new one is taken (sent after the restart: Bob's clock has passed Alice's start)
    cmd(&mut b, &cli);
    air.run(&mut [&mut a, &mut b], 5000);
    assert_eq!(&sync_messages(&mut b)[0][16..], b"> Alice");

    // as CMD_SET_DEVICE_PIN: 0 or six digits
    let pin = |m: &mut Mesh<Companion>, v: &str| {
        let r = cmd(m, &[&[CMD_RUN_CLI_COMMAND][..], format!("set pin {v}").as_bytes()].concat());
        String::from_utf8(r[0][1..].to_vec()).unwrap()
    };
    assert_eq!(pin(&mut a, "654321"), "> pin is now 654321");
    assert_eq!(pin(&mut a, "0"), "> pin is now 000000");
    assert!(pin(&mut a, "1234").starts_with("Error"));
    assert!(pin(&mut a, "1234567").starts_with("Error"));
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

/// Anyone on the LAN reaches the TCP port, as upstream's: it serves the app as usual, but with no
/// root pinned an update over it doesn't begin.
#[test]
fn ota_over_tcp_needs_a_pinned_root() {
    use meshcore::ota::*;

    let (flash, target) = ram_target(ESP_CHIP_ID_ESP32C6);
    let mut a = companion(1, "Alice");
    a.app.ota.target = Some(target);
    let mut open = |f: &[u8]| {
        frames(&mut a);
        a.app.handle_open_frame(&mut a.ctx, f);
        frames(&mut a)
    };
    assert_eq!(open(&[REQ_TYPE_OTA_INFO])[0][1], 1, "has target");
    assert_eq!(open(&[CMD_DEVICE_QUERY, 3])[0][0], RESP_CODE_DEVICE_INFO, "the app is served");

    let img = test_image(ESP_CHIP_ID_ESP32C6);
    let mut begin = vec![REQ_TYPE_OTA_BEGIN];
    begin.extend_from_slice(&(img.len() as u32).to_le_bytes());
    begin.extend_from_slice(&[0; 32]);
    begin.extend_from_slice(&172u16.to_le_bytes());
    assert_eq!(open(&begin), [vec![REQ_TYPE_OTA_BEGIN, OtaError::NoRoot as u8, 0, 0]]);
    assert_eq!(open(&[REQ_TYPE_OTA_STATUS, 0, 0])[0][1], OtaError::NoSession as u8);
    assert!(!flash.borrow().activated);
}

/// `fw ble` on a repeater (`start ota ble`, as upstream calls it): a fresh PIN for the admin, then the update over the BLE link
/// with the companion's requests (and its device query, which the update tool asks first).
#[test]
fn repeater_ble_update_window() {
    use meshcore::ota::*;

    let mut r = repeater(2);
    let reply = r.app.handle_command(&mut r.ctx, 0, "fw ble");
    assert_eq!(reply, "Error: no Bluetooth update window on this board");

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

/// `fw wifi` (`start ota`): the Wi-Fi upload page wants a fresh one-time code (on a LAN anyone could reach it);
/// the access point has a random passphrase, not the admin password.
#[test]
fn repeater_wifi_update_code() {
    let mut r = repeater(3);
    assert_eq!(r.app.handle_command(&mut r.ctx, 0, "fw wifi"), "Error: no Wi-Fi update page on this board");

    r.app.board.wifi_ota_ssid = Some("MeshCore-OTA-R".into());
    let reply = r.app.handle_command(&mut r.ctx, 0, "fw wifi");
    let (code, key) = r.app.take_effects().start_ota.expect("started");
    assert!((100_000..1_000_000).contains(&code));
    assert_eq!(key.len(), 10);
    assert_ne!(key, r.app.prefs.password, "the access point has a passphrase of its own");
    assert_eq!(reply, format!("Started: http://192.168.4.1/update (Wi-Fi 'MeshCore-OTA-R' password {key}) code {code}"));

    r.app.handle_command(&mut r.ctx, 0, "start ota");
    assert_eq!(r.app.take_effects().start_ota.unwrap().1, key, "the point comes up once a boot");

    r.app.board.wifi_ip = Some("10.0.0.5".into());
    let reply = r.app.handle_command(&mut r.ctx, 0, "start ota");
    let (lan_code, _) = r.app.take_effects().start_ota.expect("started");
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

/// `sensor scan` says what the firmware found on the I2C at boot, on a repeater and a companion (#8).
#[test]
fn sensor_scan_tells_what_answers_on_the_bus() {
    let mut r = repeater(4);
    let rcli = |r: &mut Mesh<Repeater>, c: &str| r.app.handle_command(&mut r.ctx, 0, c);
    assert_eq!(rcli(&mut r, "sensor scan"), "> no sensor bus on this board");
    r.app.board.sensor_scan = "ch2 BMP280 0x77, not known: 0x38".into();
    assert_eq!(rcli(&mut r, "sensor scan"), "> ch2 BMP280 0x77, not known: 0x38");

    let mut c = companion(1, "Alice");
    let ccli = |m: &mut Mesh<Companion>, s: &str| m.app.handle_command(&mut m.ctx, false, s);
    assert_eq!(ccli(&mut c, "sensor scan"), "> no sensor bus on this board");
    c.app.board.sensor_scan = "nothing answers on the I2C".into();
    assert_eq!(ccli(&mut c, "sensor scan"), "> nothing answers on the I2C");
}

/// `set tx` beyond what the radio chip gives is refused with the reason, rather than cut to 22 (#1).
#[test]
fn repeater_tx_power_out_of_range_is_an_error() {
    let mut r = repeater(4);
    r.app.ota.build.board = meshcore::release::board::HELTEC_V4;
    let cli = |r: &mut Mesh<Repeater>, c: &str| r.app.handle_command(&mut r.ctx, 0, c);
    assert_eq!(cli(&mut r, "set tx 16"), "OK");
    assert_eq!(r.app.take_effects().tx_power, Some(16));
    assert_eq!(
        cli(&mut r, "set tx 25"),
        "Error: 25 dBm is out of range: the radio chip gives -9 to 22 dBm; the amplifier after it adds about 12 dB (tx 16 gives about 28 dBm)"
    );
    assert_eq!(cli(&mut r, "set tx -10"), "Error: -10 dBm is out of range: the radio chip gives -9 to 22 dBm; the amplifier after it adds about 12 dB (tx 16 gives about 28 dBm)");
    assert_eq!(r.app.take_effects().tx_power, None, "the radio is left as it was");
    assert_eq!(cli(&mut r, "get tx"), "> 16");
    assert_eq!(cli(&mut r, "set tx 22"), "OK");
    assert_eq!(cli(&mut r, "get tx"), "> 22");
}

/// `fw.root`: an admin may pin the first root (an admin could install any firmware until then);
/// changing or removing a pinned root takes the serial console.
#[test]
fn repeater_ota_root_cli() {
    let mut r = repeater(4);
    let serial = |r: &mut Mesh<Repeater>, c: &str| r.app.handle_command(&mut r.ctx, 0, c);
    let remote = |r: &mut Mesh<Repeater>, c: &str| r.app.handle_command(&mut r.ctx, 1_800_000_000, c);
    let (a, b) = ("9a".repeat(32), "0b".repeat(32));
    assert!(serial(&mut r, "get fw.root").starts_with("> none, unsigned updates accepted over BLE, Wi-Fi and USB, none over LoRa (?/repeater #"));
    assert_eq!(remote(&mut r, "set fw.root 12ab"), "Error: up to 4 fingerprints of 64 hex digits, or off");
    assert_eq!(remote(&mut r, &format!("set fw.root {a}")), format!("OK - only releases signed by {a} are installed"));
    assert_eq!(r.app.ota.roots, vec![[0x9a; 32]]);
    assert!(r.app.prefs.serialize().contains(&format!("ota_root={a}
")));
    let refused = "Error: a pinned root can only be changed on the serial console";
    assert_eq!(remote(&mut r, &format!("set fw.root {b}")), refused);
    assert_eq!(remote(&mut r, "set fw.root off"), refused);
    assert_eq!(remote(&mut r, &format!("set fw.root {a}")), "OK", "unchanged");
    assert_eq!(serial(&mut r, &format!("set fw.root {a},{b}")), format!("OK - only releases signed by {a}, {b} are installed"));
    assert!(serial(&mut r, "get fw.root").starts_with(&format!("> {a}, {b} (")));
    assert_eq!(serial(&mut r, "set fw.root off"), "OK - unsigned updates accepted over BLE, Wi-Fi and USB, none over LoRa");
    assert!(r.app.ota.roots.is_empty());
}

/// The companion's pinned root keys are a custom variable, so an app can set them.
#[test]
fn companion_ota_root_custom_var() {
    let mut a = companion(1, "Alice");
    let fp = "9a".repeat(32);
    let set = |m: &mut Mesh<Companion>, v: &str| cmd(m, &[&b")fw_root:"[..], v.as_bytes()].concat())[0][0];
    assert_eq!(set(&mut a, "12"), RESP_CODE_ERR);
    assert_eq!(set(&mut a, &fp), RESP_CODE_OK);
    assert!(a.app.take_effects().save_prefs);
    assert_eq!(a.app.ota.roots, vec![[0x9a; 32]]);
    // a whole key and the status would not fit in a frame: the key's first 16 hex digits
    let vars = format!("fw_root:{}..,fw_owner:off,{NO_SLOT_VARS}", &fp[..16]);
    assert_eq!(&cmd(&mut a, &[CMD_GET_CUSTOM_VARS])[0][1..], vars.as_bytes());
    assert_eq!(set(&mut a, "off"), RESP_CODE_OK);
    assert!(a.app.ota.roots.is_empty());

    // an owner key: only with a root pinned, as it means nothing without one
    let owner = "3c".repeat(32);
    let set_owner = |m: &mut Mesh<Companion>, v: &str| cmd(m, &[&b")fw_owner:"[..], v.as_bytes()].concat())[0][0];
    assert_eq!(set_owner(&mut a, &owner), RESP_CODE_ERR);
    assert_eq!(set(&mut a, &fp), RESP_CODE_OK);
    assert_eq!(set_owner(&mut a, "3c3c"), RESP_CODE_ERR);
    assert_eq!(set_owner(&mut a, &owner), RESP_CODE_OK);
    assert_eq!(a.app.ota.owners, vec![[0x3c; 32]]);
    assert!(a.app.prefs.serialize().contains(&format!("ota_owner={owner}
")));
    let short = |fp: &str| format!("{}..", &fp[..16]);
    let vars = format!("fw_root:{},fw_owner:{},{NO_SLOT_VARS}", short(&fp), short(&owner));
    assert_eq!(&cmd(&mut a, &[CMD_GET_CUSTOM_VARS])[0][1..], vars.as_bytes());
    assert_eq!(set_owner(&mut a, "off"), RESP_CODE_OK);
    assert!(a.app.ota.owners.is_empty());
}

/// The same keys from the companion's CLI, with a repeater's commands; never from a contact's
/// remote CLI, as pinning a root is what lets a repeater hand the companion firmware.
#[test]
fn companion_ota_root_cli() {
    let mut c = companion(1, "Alice");
    let local = |m: &mut Mesh<Companion>, s: &str| m.app.handle_command(&mut m.ctx, false, s);
    let remote = |m: &mut Mesh<Companion>, s: &str| m.app.handle_command(&mut m.ctx, true, s);
    let (root, a, b) = ("9a".repeat(32), "3c".repeat(32), "0b".repeat(32));
    assert!(local(&mut c, "get fw.root").starts_with("> none, unsigned updates accepted over BLE and USB, none pulled ("));
    assert_eq!(local(&mut c, "set fw.root 12ab"), "Error: up to 4 fingerprints of 64 hex digits, or off");
    let refused = "Error: release keys are set only in the app or over USB";
    assert_eq!(remote(&mut c, &format!("set fw.root {root}")), refused);
    assert!(c.app.ota.roots.is_empty());
    assert_eq!(local(&mut c, &format!("set fw.owner {a}")), "Error: pin the root key first (set fw.root)");

    assert_eq!(local(&mut c, &format!("set fw.root {root}")), format!("OK - only releases signed by {root} are installed"));
    assert!(c.app.take_effects().save_prefs);
    assert_eq!(c.app.ota.roots, vec![[0x9a; 32]]);
    assert!(c.app.prefs.serialize().contains(&format!("ota_root={root}\n")));
    assert!(remote(&mut c, "get fw.root").starts_with(&format!("> {root} (")));
    assert_eq!(remote(&mut c, "set fw.root off"), refused);
    assert_eq!(local(&mut c, "fw update"), "Error: no update slot: update over USB", "the root is in: only the slot is missing");

    assert_eq!(remote(&mut c, &format!("set fw.owner {a}")), refused);
    assert_eq!(local(&mut c, &format!("set fw.owner {a},{b}")), format!("OK - a release must be signed by {a}, {b} too"));
    assert_eq!(c.app.ota.owners, vec![[0x3c; 32], [0x0b; 32]]);
    assert_eq!(local(&mut c, "get fw.owner"), format!("> {a}, {b}"));
    assert_eq!(local(&mut c, "set fw.owner off"), "OK - the root's signature is enough");
    assert_eq!(local(&mut c, "get fw.owner"), "> none, the root's signature is enough");
    assert_eq!(local(&mut c, "set fw.root off"), "OK - unsigned updates accepted over BLE and USB, none pulled");
    assert!(c.app.ota.roots.is_empty());
}

/// `fw.owner`: a release must carry an owner's signature as well as the root's. The same rule as
/// for the root: an admin may pin the first one, a change takes the serial console.
#[test]
fn repeater_ota_owner_cli() {
    let mut r = repeater(4);
    let serial = |r: &mut Mesh<Repeater>, c: &str| r.app.handle_command(&mut r.ctx, 0, c);
    let remote = |r: &mut Mesh<Repeater>, c: &str| r.app.handle_command(&mut r.ctx, 1_800_000_000, c);
    let (root, a, b) = ("9a".repeat(32), "3c".repeat(32), "0b".repeat(32));
    assert_eq!(serial(&mut r, "get fw.owner"), "> none, the root's signature is enough");
    assert_eq!(remote(&mut r, &format!("set fw.owner {a}")), "Error: pin the root key first (set fw.root)");
    serial(&mut r, &format!("set fw.root {root}"));
    assert_eq!(remote(&mut r, "set fw.owner 3c"), "Error: up to 4 fingerprints of 64 hex digits, or off");
    assert_eq!(remote(&mut r, &format!("set fw.owner {a}")), format!("OK - a release must be signed by {a} too"));
    assert_eq!(r.app.ota.owners, vec![[0x3c; 32]]);
    assert!(r.app.prefs.serialize().contains(&format!("ota_owner={a}
")));
    assert_eq!(serial(&mut r, "get fw.owner"), format!("> {a}"));
    let refused = "Error: a pinned owner key can only be changed on the serial console";
    assert_eq!(remote(&mut r, &format!("set fw.owner {b}")), refused);
    assert_eq!(remote(&mut r, "set fw.owner off"), refused);
    assert_eq!(serial(&mut r, &format!("set fw.owner {a},{b}")), format!("OK - a release must be signed by {a}, {b} too"));
    assert_eq!(serial(&mut r, "set fw.owner off"), "OK - the root's signature is enough");
    assert!(r.app.ota.owners.is_empty());
}

/// The update variables of a companion that pins no root key, after the keys.
const NO_ROOT_VARS: &str = "fw_auto:off,fw_source:auto,fw_channel:stable,fw_status:needs a release key: fw_root,fw_do:-";
/// And of one that pins one, but has no slot to update into.
const NO_SLOT_VARS: &str = "fw_auto:off,fw_source:auto,fw_channel:stable,fw_status:no update slot: update over USB,fw_do:-";

#[test]
fn gps_setting_position_and_telemetry() {
    // no GPS: no GPS settings to offer
    let mut a = companion(1, "Alice");
    assert_eq!(&cmd(&mut a, &[CMD_GET_CUSTOM_VARS])[0][1..], format!("fw_root:off,fw_owner:off,{NO_ROOT_VARS}").as_bytes());
    assert_eq!(cmd(&mut a, b"\x29gps:1")[0][0], RESP_CODE_ERR);

    let mut air = Air::new();
    air.link(0, 1);
    let mut b = companion(2, "Bob");
    b.app.board.has_gps = true;
    b.app.prefs.telemetry_mode_base = TELEM_MODE_ALLOW_ALL;
    b.app.prefs.telemetry_mode_loc = TELEM_MODE_ALLOW_ALL;
    assert_eq!(&cmd(&mut b, &[CMD_GET_CUSTOM_VARS])[0][1..], format!("gps:0,fw_root:off,fw_owner:off,{NO_ROOT_VARS}").as_bytes());
    assert_eq!(cmd(&mut b, b"\x29gps:1")[0][0], RESP_CODE_OK);
    let fx = b.app.take_effects();
    assert!(fx.gps && fx.save_prefs && b.app.prefs.gps_enabled);
    assert_eq!(&cmd(&mut b, &[CMD_GET_CUSTOM_VARS])[0][1..], format!("gps:1,fw_root:off,fw_owner:off,{NO_ROOT_VARS}").as_bytes());
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

/// The `gps …` text CLI (upstream `CommonCLI`), gated on a receiver; `gps advert` is a bare
/// preference and answers without one.
#[test]
fn gps_cli_on_a_companion() {
    use meshcore::gps::GpsReading;
    use meshcore::prefs::{ADVERT_LOC_NONE, ADVERT_LOC_SHARE};
    let mut a = companion(1, "Alice");
    let cli = |m: &mut Mesh<Companion>, s: &str| m.app.handle_command(&mut m.ctx, false, s);

    // advert policy answers without a receiver
    assert_eq!(cli(&mut a, "gps advert share"), "ok");
    assert_eq!(a.app.prefs.advert_loc_policy, ADVERT_LOC_SHARE);
    assert_eq!(cli(&mut a, "gps advert"), "> share");
    assert_eq!(cli(&mut a, "gps advert none"), "ok");
    assert_eq!(a.app.prefs.advert_loc_policy, ADVERT_LOC_NONE);

    // no receiver: the hardware commands report absence
    assert_eq!(cli(&mut a, "gps"), "Can't find GPS");
    assert_eq!(cli(&mut a, "gps on"), "Can't find GPS");

    // a board with a receiver, before and after a fix
    a.app.board.has_gps = true;
    assert_eq!(cli(&mut a, "gps"), "off");
    assert_eq!(cli(&mut a, "gps on"), "ok");
    assert!(a.app.prefs.gps_enabled);
    let fx = a.app.take_effects();
    assert!(fx.gps && fx.save_prefs);
    assert_eq!(cli(&mut a, "gps"), "on, active, no fix, 0 sats");
    assert_eq!(cli(&mut a, "gps setloc"), "no fix");

    a.app.board.gps = Some(GpsReading { in_view: 11, time: Some(2_000_000_000), ..Default::default() });
    assert_eq!(cli(&mut a, "gps sync"), "no fix");

    let fix = GpsReading {
        valid: true,
        lat: 55_000_000,
        lon: -33_500_000,
        alt_mm: 120_500,
        satellites: 8,
        in_view: 11,
        heard: 9,
        best_snr: 38,
        time: Some(2_000_000_000),
    };
    a.app.board.gps = Some(fix);
    assert_eq!(cli(&mut a, "gps"), "on, active, fix, 8 sats");
    assert_eq!(cli(&mut a, "gps diag"), "req:1 fix:1 sats:8 inview:11 heard:9 snr:38");
    assert_eq!(cli(&mut a, "gps setloc"), "ok");
    assert!((a.app.prefs.node_lat - 55.0).abs() < 1e-6);
    assert!((a.app.prefs.node_lon + 33.5).abs() < 1e-6);
    assert_eq!(cli(&mut a, "gps sync"), "ok");
    assert_eq!(a.ctx.current_time(), 2_000_000_000);
    a.app.board.gps = Some(GpsReading { time: Some(1_900_000_000), ..fix });
    assert_eq!(cli(&mut a, "gps sync"), "ok");
    assert_eq!(a.ctx.current_time(), 1_900_000_000);

    assert_eq!(cli(&mut a, "gps bogus"), "error");
    assert_eq!(cli(&mut a, "gps off"), "ok");
    assert!(!a.app.prefs.gps_enabled);
}

/// `adc.multiplier` as on a repeater: only on boards that take one, over the CLI and as a custom
/// variable, 0 = the board's own, kept in prefs.
#[test]
fn adc_multiplier_on_a_companion() {
    let mut a = companion(1, "Alice");
    let cli = |m: &mut Mesh<Companion>, s: &str| m.app.handle_command(&mut m.ctx, false, s);
    assert_eq!(cli(&mut a, "get adc.multiplier"), "Error: unsupported");
    assert_eq!(cli(&mut a, "set adc.multiplier 1.7"), "Error: unsupported");
    assert_eq!(cmd(&mut a, b"\x29adc_multiplier:1.7")[0][0], RESP_CODE_ERR);
    assert_eq!(a.app.prefs.adc_multiplier, 0.0);

    a.app.board.adc_multiplier = Some(5.42);
    assert_eq!(cli(&mut a, "get adc.multiplier"), "> 5.420");
    assert!(cmd(&mut a, &[CMD_GET_CUSTOM_VARS])[0][1..].starts_with(b"adc_multiplier:5.420,fw_root:"));
    assert_eq!(cli(&mut a, "set adc.multiplier 5.6"), "OK - multiplier set to 5.600");
    assert!(a.app.take_effects().save_prefs);
    assert_eq!(cli(&mut a, "get adc.multiplier"), "> 5.600");
    let saved = CompanionPrefs::deserialize(&a.app.prefs.serialize(), 22);
    assert_eq!(saved.adc_multiplier, 5.6);

    assert_eq!(cmd(&mut a, b"\x29adc_multiplier:5.3")[0][0], RESP_CODE_OK);
    assert!(cmd(&mut a, &[CMD_GET_CUSTOM_VARS])[0][1..].starts_with(b"adc_multiplier:5.300,"));
    assert_eq!(cli(&mut a, "set adc.multiplier 0"), "OK - using default board multiplier");
    assert_eq!(a.app.adc_multiplier(), Some(5.42));
}

/// `radio.fem.rxgain` as on a repeater: only on boards whose front end can switch its LNA, over
/// the CLI and as a custom variable, kept in prefs; off until turned on.
#[test]
fn fem_rxgain_on_a_companion() {
    let mut a = companion(1, "Alice");
    let cli = |m: &mut Mesh<Companion>, s: &str| m.app.handle_command(&mut m.ctx, false, s);
    assert_eq!(cli(&mut a, "get radio.fem.rxgain"), "Error: unsupported");
    assert_eq!(cli(&mut a, "set radio.fem.rxgain on"), "Error: unsupported");
    assert_eq!(cmd(&mut a, b"\x29fem_rxgain:1")[0][0], RESP_CODE_ERR);
    assert!(!cmd(&mut a, &[CMD_GET_CUSTOM_VARS])[0][1..].starts_with(b"fem_rxgain"));
    assert!(!a.app.prefs.radio_fem_rxgain);
    assert_eq!(a.app.take_effects().fem_lna, None);

    a.app.board.fem_lna_controllable = true;
    assert_eq!(cli(&mut a, "get radio.fem.rxgain"), "> off");
    assert!(cmd(&mut a, &[CMD_GET_CUSTOM_VARS])[0][1..].starts_with(b"fem_rxgain:0,fw_root:"));
    assert_eq!(cli(&mut a, "set radio.fem.rxgain on"), "OK - LoRa FEM RX gain on");
    let fx = a.app.take_effects();
    assert_eq!(fx.fem_lna, Some(true));
    assert!(fx.save_prefs);
    assert_eq!(cli(&mut a, "get radio.fem.rxgain"), "> on");
    assert_eq!(cli(&mut a, "set radio.fem.rxgain loud"), "Error: state must be on or off");
    let saved = CompanionPrefs::deserialize(&a.app.prefs.serialize(), 22);
    assert!(saved.radio_fem_rxgain);

    assert_eq!(cmd(&mut a, b"\x29fem_rxgain:0")[0][0], RESP_CODE_OK);
    assert_eq!(a.app.take_effects().fem_lna, Some(false));
    assert!(cmd(&mut a, &[CMD_GET_CUSTOM_VARS])[0][1..].starts_with(b"fem_rxgain:0,"));
}

/// Periodic self-advert on a companion, as on a repeater: `advert.interval` (local, minutes) and
/// `flood.advert.interval` (hours), off by default, kept in prefs.
#[test]
fn periodic_self_advert() {
    let mut air = Air::new();
    air.link(0, 1);
    let mut a = companion(1, "Alice");
    let mut b = companion(2, "Bob");
    let cli = |m: &mut Mesh<Companion>, s: &str| m.app.handle_command(&mut m.ctx, false, s);

    // off by default
    assert_eq!(cli(&mut a, "get advert.interval"), "> 0");
    assert_eq!(cli(&mut a, "get flood.advert.interval"), "> 0");

    // ranges rejected, as on a repeater
    assert_eq!(cli(&mut a, "set advert.interval 30"), "Error: interval range is 60-240 minutes");
    assert_eq!(cli(&mut a, "set flood.advert.interval 2"), "Error: interval range is 3-168 hours");

    // local zero-hop advert every hour; stored as minutes / 2
    assert_eq!(cli(&mut a, "set advert.interval 60"), "OK");
    assert!(a.app.take_effects().save_prefs);
    assert_eq!(cli(&mut a, "get advert.interval"), "> 60");
    assert_eq!(cli(&mut a, "set flood.advert.interval 6"), "OK");
    let saved = CompanionPrefs::deserialize(&a.app.prefs.serialize(), 22);
    assert_eq!((saved.advert_interval, saved.flood_advert_interval), (30, 6));

    // Bob hears Alice advertise herself without any prompting
    frames(&mut b);
    air.run(&mut [&mut a, &mut b], 61 * 60 * 1000);
    assert!(!only(&frames(&mut b), PUSH_CODE_ADVERT).is_empty());
}

#[test]
fn round_trip_manual_path() {
    // Alice -- Repeater -- Bob; the app gives Alice a path to Bob, and Bob learns the way back
    for on in [false, true] {
        let mut air = Air::new();
        air.link(0, 1);
        air.link(1, 2);
        let mut a = companion(1, "Alice");
        let mut r = repeater(9);
        let mut b = companion(2, "Bob");
        cmd(&mut a, &[CMD_SEND_SELF_ADVERT, 1]);
        cmd(&mut b, &[CMD_SEND_SELF_ADVERT, 1]);
        air.run(&mut [&mut a, &mut r, &mut b], 5000);
        frames(&mut a);
        frames(&mut b);
        let mut cli = vec![CMD_RUN_CLI_COMMAND];
        cli.extend_from_slice(if on { b"set rtpath on" } else { b"set rtpath off" });
        assert_eq!(cmd(&mut a, &cli)[0][1..], *b"OK");

        let contact = |m: &mut Mesh<Companion>, key: [u8; 32]| {
            cmd(m, &[CMD_GET_CONTACT_BY_KEY].iter().copied().chain(key).collect::<Vec<_>>()).remove(0)
        };
        let mut c = contact(&mut a, pub_key(&b));
        c[0] = CMD_ADD_UPDATE_CONTACT;
        (c[35], c[36]) = (1, pub_key(&r)[0]);
        assert_eq!(cmd(&mut a, &c), vec![vec![RESP_CODE_OK]]);
        assert_eq!(contact(&mut b, pub_key(&a))[35], 0xFF, "Bob has no way back yet");

        let floods = r.ctx.stats.n_sent_flood;
        cmd(&mut a, &txt_msg(&pub_key(&b), "round trip"));
        air.run(&mut [&mut a, &mut r, &mut b], 10_000);
        assert_eq!(&sync_messages(&mut b)[0][16..], b"round trip");
        assert_eq!(only(&frames(&mut a), PUSH_CODE_SEND_CONFIRMED).len(), 1);
        let back = contact(&mut b, pub_key(&a));
        if on {
            assert_eq!((back[35], back[36]), (1, pub_key(&r)[0]), "Bob learnt the way back");
            assert_eq!(r.ctx.stats.n_sent_flood, floods, "and the ACK came back direct");
        } else {
            // upstream behaviour: the ACK floods, and Alice then sends Bob the path it came by
            // (`handleReturnPathRetry`)
            assert!(r.ctx.stats.n_sent_flood > floods, "the ACK floods");
            assert_eq!(back[35], 1, "Bob learns the way back after the flood");
        }
    }
}

#[test]
fn reverse_path_keeps_the_hash_size() {
    assert_eq!(reverse_path(3, &[1, 2, 3]), (3, vec![3, 2, 1]));
    // two 2-byte hashes: path_len = (2 - 1) << 6 | 2
    assert_eq!(reverse_path(0x42, &[1, 2, 3, 4]), (0x42, vec![3, 4, 1, 2]));
    assert_eq!(reverse_path(0, &[]), (0, vec![]));
}

/// An ESP app image as espflash makes it: one segment of `data`, the checksum block, SHA-256.
fn esp_image(data: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut img = vec![0u8; 24];
    img[0] = 0xE9;
    img[1] = 1; // segments
    img[12..14].copy_from_slice(&9u16.to_le_bytes()); // ESP32-S3
    img[23] = 1; // hash appended
    img.extend_from_slice(&0x4200_0000u32.to_le_bytes());
    img.extend_from_slice(&(data.len() as u32).to_le_bytes());
    img.extend_from_slice(data);
    img.resize((img.len() / 16 + 1) * 16, 0);
    let h: [u8; 32] = Sha256::digest(&img).into();
    img.extend_from_slice(&h);
    img
}

/// The update slot's bytes, and whether its image was selected for the next boot.
type Slot = std::rc::Rc<std::cell::RefCell<(Vec<u8>, bool)>>;

/// A slot pair in RAM: the running image (a delta's base) and the update slot.
struct RamSlots {
    running: Vec<u8>,
    slot: Slot,
}

impl meshcore::ota::FirmwareTarget for RamSlots {
    fn capacity(&self) -> u32 {
        self.slot.borrow().0.len() as u32
    }
    fn erase(&mut self, o: u32, l: u32) -> bool {
        self.slot.borrow_mut().0[o as usize..(o + l) as usize].fill(0xFF);
        true
    }
    fn write(&mut self, o: u32, d: &[u8]) -> bool {
        self.slot.borrow_mut().0[o as usize..o as usize + d.len()].copy_from_slice(d);
        true
    }
    fn read(&mut self, o: u32, b: &mut [u8]) -> bool {
        b.copy_from_slice(&self.slot.borrow().0[o as usize..o as usize + b.len()]);
        true
    }
    fn activate(&mut self, _size: u32) -> bool {
        self.slot.borrow_mut().1 = true;
        true
    }
    fn running_capacity(&self) -> u32 {
        self.running.len() as u32
    }
    fn read_running(&mut self, o: u32, b: &mut [u8]) -> bool {
        let Some(src) = self.running.get(o as usize..o as usize + b.len()) else { return false };
        b.copy_from_slice(src);
        true
    }
}

/// A release of `image` for a T-Echo companion, signed by `key`, as a delta from `base`.
fn delta_release(key: &LocalIdentity, counter: u32, base: &[u8], image: &[u8]) -> Vec<u8> {
    use meshcore::release::{board, role};
    delta_release_for(key, board::T_ECHO, role::COMPANION, 0, counter, base, image)
}

/// A release of `image` for `board` and `role` with manifest `flags`, as a delta from `base`.
fn delta_release_for(key: &LocalIdentity, board: u8, role: u8, flags: u8, counter: u32, base: &[u8], image: &[u8]) -> Vec<u8> {
    use meshcore::release::{Envelope, Manifest};
    use sha2::{Digest, Sha256};
    let mut patch = Vec::new();
    bsdiff::diff(base, image, &mut patch).unwrap();
    let patch = miniz_oxide::deflate::compress_to_vec_zlib(&patch, 9);
    let manifest = Manifest {
        board,
        role,
        flags,
        counter,
        image_len: image.len() as u32,
        image_sha256: Sha256::digest(image).into(),
        issued: T0,
        version: "0.5.2".into(),
    };
    let mut env = Envelope { manifest, signatures: Vec::new() };
    let sig = key.sign(&Envelope::signed_message(&env.manifest));
    env.add_signature(*key.pub_key(), sig);
    let base_sha: [u8; 32] = Sha256::digest(base).into();
    meshcore::fwshare::delta_file(&env.to_bytes(), base.len() as u32, &base_sha, &patch)
}

/// `file` (a delta) with `key`'s signature added, as an owner adds theirs on the release server.
fn cosigned(file: &[u8], key: &LocalIdentity) -> Vec<u8> {
    use meshcore::fwshare::DeltaHeader;
    use meshcore::release::Envelope;
    let h = DeltaHeader::parse(file).unwrap();
    let mut env = h.envelope.clone();
    env.add_signature(*key.pub_key(), key.sign(&Envelope::signed_message(&env.manifest)));
    meshcore::fwshare::delta_file(&env.to_bytes(), h.base_len, &h.base_sha256, &file[h.patch_offset()..])
}

/// A T-Echo-like companion pinning `root`, running `base`, with `fw.auto download`.
fn pulling_companion(root: &LocalIdentity, base: &[u8]) -> (Mesh<Companion>, Slot) {
    let slot = std::rc::Rc::new(std::cell::RefCell::new((vec![0u8; 64 * 1024], false)));
    let mut c = companion(2, "Echo");
    c.app.prefs.fw_mode = meshcore::fwstate::mode::DOWNLOAD;
    let fp = meshcore::release::fingerprint(root.pub_key());
    c.app.prefs.ota_roots = vec![fp];
    c.app.ota.roots = vec![fp];
    c.app.ota.build.board = meshcore::release::board::T_ECHO;
    c.app.ota.build.counter = 4; // whatever MCFW_COUNTER this test was built with
    c.app.ota.target = Some(Box::new(RamSlots { running: base.to_vec(), slot: slot.clone() }));
    (c, slot)
}

#[test]
fn fw_tells_what_runs_where_the_update_stands_and_what_the_store_holds() {
    let mut r = repeater(1);
    let fw = |r: &mut Mesh<Repeater>| r.app.handle_command(&mut r.ctx, 0, "fw");
    let head = format!("{} stable, auto off", meshcore::release::running_label(""));
    assert_eq!(fw(&mut r), format!("{head}\nneeds a release key: set fw.root"));
    r.app.ota.roots = vec![[1; 32]];
    assert_eq!(fw(&mut r), format!("{head}\nno update slot: update over USB"));
    let slot: Slot = std::rc::Rc::new(std::cell::RefCell::new((vec![0u8; 1024], false)));
    r.app.ota.target = Some(Box::new(RamSlots { running: esp_image(&[1; 100]), slot }));
    assert_eq!(fw(&mut r), format!("{head}\nauto off; check by hand: fw check"));

    // a node that hands updates on: what its store holds, or why it holds nothing
    r.app.prefs.fw_serve = true;
    r.app.fw_store = Some(Box::new(meshcore::fwshare::MemStore::new(Vec::new(), 77)));
    assert_eq!(fw(&mut r).lines().nth(2), Some("serves nothing yet"));
    r.app.board.fw_store = "serve: nw.ommesh.ru not reachable (cut off)".into();
    assert_eq!(fw(&mut r).lines().nth(2), Some("serve: nw.ommesh.ru not reachable (cut off)"));

    // after an update, on trial
    r.app.board.fw_trial_until = Some(r.ctx.now + 90_000);
    r.app.board.fw_prev = "0.8.2 #42".into();
    assert_eq!(fw(&mut r).lines().nth(1), Some("on trial, kept in 2 min (fw rollback: back to 0.8.2 #42)"));
    assert_eq!(r.app.handle_command(&mut r.ctx, 0, "fw rollback"), "Error: no previous firmware on this node");
    r.app.board.fw_rollback = true;
    assert_eq!(r.app.handle_command(&mut r.ctx, 0, "fw rollback"), "OK - back to 0.8.2 #42: restarting");
    assert!(r.app.take_effects().fw_rollback);

    // back by itself: the update it left did not start
    r.app.board.fw_trial_until = None;
    r.app.board.fw_prev = "0.8.4 #60".into();
    r.app.board.fw_prev_failed = true;
    assert_eq!(fw(&mut r).lines().nth(1), Some("rolled back: 0.8.4 #60 did not start"));
}

fn serving_repeater(file: Vec<u8>) -> Mesh<Repeater> {
    let mut r = repeater(1);
    r.app.prefs.fw_serve = true;
    r.app.fw_store = Some(Box::new(meshcore::fwshare::MemStore::new(file, 77)));
    r.app.start(&mut r.ctx); // boot advert after 16 s
    r
}

#[test]
fn companion_pulls_an_update_from_a_neighbouring_repeater() {
    let root = LocalIdentity::from_seed(&[42; 32]);
    let base_data: Vec<u8> = (0..20_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let mut new_data = base_data.clone();
    new_data[5000..5100].fill(0x5A); // a change, and a bit more code
    new_data.extend_from_slice(b"new feature");
    let (base, image) = (esp_image(&base_data), esp_image(&new_data));
    let file = delta_release(&root, 5, &base, &image);
    assert!(file.len() < 1500, "a small delta: {} bytes", file.len());

    let mut r = serving_repeater(file.clone());
    let (mut c, slot) = pulling_companion(&root, &base);
    let mut air = Air::new();
    air.link(0, 1);
    air.run(&mut [&mut r, &mut c], 20_000); // the repeater's boot advert: a contact, heard directly
    assert_eq!(c.app.fw_state(), FwState::Waiting { at: 0 });

    let _ = c.app.fw_check_now(&c.ctx, false);
    air.run(&mut [&mut r, &mut c], 120_000);
    let kb = (file.len() as u32).div_ceil(1024);
    assert_eq!(c.app.fw_line(c.ctx.now), format!("0.5.2 #5 ready ({kb} KB): fw install"));
    assert!(!slot.borrow().1, "fw.auto download: nothing installed until the user says so");
    let fp = meshcore::release::fingerprint_hex(&meshcore::release::fingerprint(root.pub_key()));
    let vars = format!("fw_root:{}..,fw_owner:off,fw_auto:download,fw_source:auto,fw_channel:stable,fw_status:0.5.2 #5 ready ({kb} KB): fw_do install,fw_do:-", &fp[..16]);
    assert_eq!(String::from_utf8_lossy(&cmd(&mut c, &[CMD_GET_CUSTOM_VARS])[0][1..]), vars);

    assert_eq!(cmd(&mut c, b"\x29fw_do:install")[0][0], RESP_CODE_OK);
    assert!(slot.borrow().1, "activated");
    assert_eq!(&slot.borrow().0[..image.len()], &image[..], "the slot holds the release's image");
    assert!(c.app.take_effects().reboot);
}

#[test]
fn a_pulled_update_is_only_what_the_pinned_root_signed() {
    let root = LocalIdentity::from_seed(&[42; 32]);
    let stranger = LocalIdentity::from_seed(&[43; 32]);
    let base = esp_image(&[1u8; 3000]);
    let image = esp_image(&[2u8; 3100]);

    // a repeater (or its server) hands on someone else's build: refused before any patch is fetched
    let mut r = serving_repeater(delta_release(&stranger, 5, &base, &image));
    let (mut c, slot) = pulling_companion(&root, &base);
    let mut air = Air::new();
    air.link(0, 1);
    air.run(&mut [&mut r, &mut c], 20_000);
    let _ = c.app.fw_check_now(&c.ctx, false);
    air.run(&mut [&mut r, &mut c], 60_000);
    assert!(c.app.fw_line(c.ctx.now).starts_with("failed: not signed by the pinned root key; next try "), "{}", c.app.fw_line(c.ctx.now));
    assert!(!slot.borrow().1);

    // with no root pinned the node asks nobody: it would install anything
    let mut r = serving_repeater(delta_release(&root, 5, &base, &image));
    let (mut c, _) = pulling_companion(&root, &base);
    c.app.ota.roots.clear();
    let mut air = Air::new();
    air.link(0, 1);
    air.run(&mut [&mut r, &mut c], 20_000);
    let _ = c.app.fw_check_now(&c.ctx, false);
    air.run(&mut [&mut r, &mut c], 60_000);
    assert_eq!(c.app.fw_state(), FwState::NoRoot);
}

#[test]
fn an_owner_key_holds_updates_until_the_owner_signs() {
    let root = LocalIdentity::from_seed(&[42; 32]);
    let owner = LocalIdentity::from_seed(&[44; 32]);
    let base = esp_image(&[5u8; 3000]);
    let image = esp_image(&[6u8; 3100]);
    let release = delta_release(&root, 5, &base, &image);
    let pin_owner = |c: &mut Mesh<Companion>| {
        let fp = meshcore::release::fingerprint(owner.pub_key());
        c.app.prefs.ota_owners = vec![fp];
        c.app.ota.owners = vec![fp];
    };

    // the root's release alone: no update for a node whose owner has not signed it
    let mut r = serving_repeater(release.clone());
    let (mut c, slot) = pulling_companion(&root, &base);
    pin_owner(&mut c);
    let mut air = Air::new();
    air.link(0, 1);
    air.run(&mut [&mut r, &mut c], 20_000);
    let _ = c.app.fw_check_now(&c.ctx, false);
    air.run(&mut [&mut r, &mut c], 60_000);
    assert!(matches!(c.app.fw_state(), FwState::UpToDate { .. }), "{:?}", c.app.fw_state());

    // a repeater from before owner keys hands it on anyway: the node refuses it
    let mut r = serving_repeater(release.clone());
    r.app.fw_store = Some(Box::new(IgnoresOwner(meshcore::fwshare::MemStore::new(release.clone(), 77))));
    let (mut c, _) = pulling_companion(&root, &base);
    pin_owner(&mut c);
    let mut air = Air::new();
    air.link(0, 1);
    air.run(&mut [&mut r, &mut c], 20_000);
    let _ = c.app.fw_check_now(&c.ctx, false);
    air.run(&mut [&mut r, &mut c], 60_000);
    assert!(c.app.fw_line(c.ctx.now).starts_with("failed: not signed by the owner key"), "{}", c.app.fw_line(c.ctx.now));

    // signed by the owner too: it comes and installs
    let mut r = serving_repeater(cosigned(&release, &owner));
    let (mut c2, slot2) = pulling_companion(&root, &base);
    pin_owner(&mut c2);
    let mut air = Air::new();
    air.link(0, 1);
    air.run(&mut [&mut r, &mut c2], 20_000);
    let _ = c2.app.fw_check_now(&c2.ctx, false);
    air.run(&mut [&mut r, &mut c2], 120_000);
    assert!(c2.app.fw_line(c2.ctx.now).starts_with("0.5.2 #5 ready"));
    assert_eq!(cmd(&mut c2, b")fw_do:install")[0][0], RESP_CODE_OK);
    assert!(slot2.borrow().1 && !slot.borrow().1);
}

/// A store that hands on whatever it holds, whoever signed it (a repeater from before owner keys).
struct IgnoresOwner(meshcore::fwshare::MemStore);

impl meshcore::fwshare::FwStore for IgnoresOwner {
    fn offer(&mut self, q: &meshcore::fwshare::FwQuery) -> meshcore::fwshare::Offer {
        self.0.offer(&meshcore::fwshare::FwQuery { owner8: meshcore::fwshare::FwQuery::NO_OWNER, ..*q })
    }
    fn read(&mut self, id: u32, offset: u32, buf: &mut [u8]) -> Option<usize> {
        self.0.read(id, offset, buf)
    }
}

#[test]
fn a_repeater_updates_itself_from_its_store() {
    use meshcore::release::{board, fingerprint, role};
    let root = LocalIdentity::from_seed(&[42; 32]);
    let base = esp_image(&(0..30_000u32).map(|i| (i * 13 % 251) as u8).collect::<Vec<_>>());
    let mut data: Vec<u8> = (0..30_000u32).map(|i| (i * 13 % 251) as u8).collect();
    data[100..140].fill(7);
    let image = esp_image(&data);

    let slot: Slot = std::rc::Rc::new(std::cell::RefCell::new((vec![0u8; 64 * 1024], false)));
    let mut r = repeater(1);
    r.app.ota.target = Some(Box::new(RamSlots { running: base.clone(), slot: slot.clone() }));
    r.app.ota.build.board = board::HELTEC_V4;
    r.app.ota.build.counter = 4;
    // the store's server had nothing newer when it last asked: `fw update` makes it ask again
    let inner = meshcore::fwshare::MemStore::new(delta_release_for(&root, board::HELTEC_V4, role::REPEATER, 0, 5, &base, &image), 9);
    r.app.fw_store = Some(Box::new(StaleStore { inner, stale: true }));
    let cli = |r: &mut Mesh<Repeater>, c: &str| r.app.handle_command(&mut r.ctx, 0, c);

    assert_eq!(cli(&mut r, "get fw.auto"), "> off");
    assert_eq!(cli(&mut r, "fw update"), "Error: needs a release key: set fw.root", "the server is not trusted");
    let fp = meshcore::release::fingerprint_hex(&fingerprint(root.pub_key()));
    cli(&mut r, &format!("set fw.root {fp}"));
    assert_eq!(cli(&mut r, "set fw.auto on"), "Error: off, download or install");
    assert_eq!(cli(&mut r, "set fw.auto install"), "OK - installs stable releases by itself, from nw.ommesh.ru or a neighbour");

    // `fw update` checks now rather than 10 minutes after boot
    assert!(cli(&mut r, "fw update").starts_with("OK"));
    let mut air = Air::new();
    air.run(&mut [&mut r], 5_000);
    assert!(r.app.take_effects().reboot, "installed: restarting into it");
    assert!(slot.borrow().1);
    assert_eq!(&slot.borrow().0[..image.len()], &image[..]);
    assert_eq!(cli(&mut r, "fw").lines().nth(1), Some("installing 0.5.2 #5: restarting, silent for 1-2 min"));
    assert_eq!(cli(&mut r, "get fw.auto"), "> install");
}

#[test]
fn a_repeater_in_download_mode_waits_for_fw_install_and_skips_what_it_rolled_back() {
    use meshcore::release::{board, role};
    let root = LocalIdentity::from_seed(&[42; 32]);
    let base = esp_image(&(0..30_000u32).map(|i| (i * 13 % 251) as u8).collect::<Vec<_>>());
    let mut data: Vec<u8> = (0..30_000u32).map(|i| (i * 13 % 251) as u8).collect();
    data[100..140].fill(7);
    let image = esp_image(&data);
    let file = delta_release_for(&root, board::HELTEC_V4, role::REPEATER, 0, 5, &base, &image);
    let (mut r, slot) = self_updating_repeater(1, &root, &base);
    r.app.fw_store = Some(Box::new(meshcore::fwshare::MemStore::new(file, 9)));
    let cli = |r: &mut Mesh<Repeater>, c: &str| r.app.handle_command(&mut r.ctx, 0, c);
    let mut air = Air::new();

    // rolled back from #5 before: the check by itself leaves it
    r.app.board.fw_skip = 5;
    air.run(&mut [&mut r], 11 * 60_000);
    assert_eq!(cli(&mut r, "fw").lines().nth(1), Some("0.5.2 #5 was rolled back: waiting for a newer release"));
    assert!(!slot.borrow().1);

    // asked by hand it comes, and in download mode it waits for `fw install`
    assert_eq!(cli(&mut r, "set fw.auto download"), "OK - downloads stable releases by itself; fw install installs them");
    assert_eq!(cli(&mut r, "fw install"), "Error: nothing downloaded: fw check first");
    assert_eq!(cli(&mut r, "fw check"), "OK - asking nw.ommesh.ru, see fw");
    air.run(&mut [&mut r], 5_000);
    assert!(cli(&mut r, "fw").lines().nth(1).unwrap().starts_with("0.5.2 #5 ready ("), "{}", cli(&mut r, "fw"));
    assert!(!slot.borrow().1 && !r.app.take_effects().reboot);
    assert_eq!(cli(&mut r, "fw install"), "OK - installing 0.5.2 #5: restarting, silent for 1-2 min");
    assert!(slot.borrow().1 && r.app.take_effects().reboot);
}

/// The companion's update variables are its CLI's settings and actions, with the same checks.
#[test]
fn companion_update_variables_are_the_cli_settings() {
    let mut c = companion(1, "Alice");
    let set = |m: &mut Mesh<Companion>, v: &str| cmd(m, v.as_bytes())[0].clone();
    let cli = |m: &mut Mesh<Companion>, s: &str| m.app.handle_command(&mut m.ctx, false, s);
    assert_eq!(set(&mut c, ")fw_auto:on"), vec![RESP_CODE_ERR, ERR_CODE_ILLEGAL_ARG]);
    assert_eq!(set(&mut c, ")fw_auto:install"), vec![RESP_CODE_OK]);
    assert_eq!(cli(&mut c, "get fw.auto"), "> install");
    assert_eq!(set(&mut c, ")fw_do:dance"), vec![RESP_CODE_ERR, ERR_CODE_ILLEGAL_ARG]);
    assert_eq!(set(&mut c, ")fw_do:check"), vec![RESP_CODE_ERR, ERR_CODE_BAD_STATE], "no root pinned");
    assert!(String::from_utf8_lossy(&cmd(&mut c, &[CMD_GET_CUSTOM_VARS])[0][1..]).ends_with(",fw_do:check: failed"));
    assert_eq!(cli(&mut c, "fw check"), "Error: needs a release key: set fw.root");
    assert_eq!(cli(&mut c, "fw install"), "Error: nothing downloaded: fw check first");
    assert_eq!(cli(&mut c, "fw rollback"), "Error: no previous firmware on this device");
    assert_eq!(cli(&mut c, "set fw.source nobody"), "Error: no repeater nobody in the contacts; a name, the start of a key (8+ hex digits) or auto");
    assert_eq!(cli(&mut c, "set fw.auto off"), "OK - no updates by itself");
}

#[test]
fn a_dev_release_reaches_only_the_nodes_that_follow_dev() {
    use meshcore::release::{board, flags, role};
    let root = LocalIdentity::from_seed(&[42; 32]);
    let base = esp_image(&[3u8; 4000]);
    let image = esp_image(&[4u8; 4100]);
    let mut r = serving_repeater(delta_release_for(&root, board::T_ECHO, role::COMPANION, flags::DEV, 5, &base, &image));
    let (mut c, slot) = pulling_companion(&root, &base);
    let mut air = Air::new();
    air.link(0, 1);
    air.run(&mut [&mut r, &mut c], 20_000);

    let _ = c.app.fw_check_now(&c.ctx, false);
    air.run(&mut [&mut r, &mut c], 60_000);
    assert!(matches!(c.app.fw_state(), FwState::UpToDate { .. }), "stable: a dev release is no update");

    // following dev, and with `fw.auto off`: `fw update` fetches it and installs it at once
    assert_eq!(c.app.handle_command(&mut c.ctx, false, "set fw.channel dev"), "OK - following dev releases (and stable ones)");
    assert_eq!(c.app.handle_command(&mut c.ctx, false, "set fw.auto off"), "OK - no updates by itself");
    let name = r.app.prefs.node_name.clone();
    assert_eq!(
        c.app.handle_command(&mut c.ctx, false, "fw update"),
        format!("OK - asking {name}; if there is an update the device installs it and restarts")
    );
    air.run(&mut [&mut r, &mut c], 60_000);
    assert!(slot.borrow().1, "installed");
    assert!(c.app.take_effects().reboot);
}

/// The release server as a companion on Wi-Fi sees it: asked first, it fetches; then the file
/// comes in order as the server sends it, a little at a time, at most 4 KB ahead of the reader
/// (as the firmware's `fwstream`), and can't be read anywhere else.
struct Streamed {
    inner: meshcore::fwshare::MemStore,
    stream: Option<meshcore::fwshare::Stream>,
}

impl meshcore::fwshare::FwStore for Streamed {
    fn offer(&mut self, q: &meshcore::fwshare::FwQuery) -> meshcore::fwshare::Offer {
        if self.stream.is_none() {
            self.stream = Some(meshcore::fwshare::Stream::new(self.inner.file.len() as u32));
            return meshcore::fwshare::Offer::Fetching { retry_secs: 2 };
        }
        self.inner.offer(q)
    }
    fn read(&mut self, id: u32, offset: u32, buf: &mut [u8]) -> Option<usize> {
        let s = self.stream.as_mut()?;
        if id != self.inner.id {
            return None;
        }
        // 100 more bytes came since the last read
        let at = s.received() as usize;
        let more = 100.min(4096 - s.held().min(4096)).min(self.inner.file.len() - at);
        s.push(&self.inner.file[at..at + more]);
        s.read(offset, buf)
    }
}

#[test]
fn a_companion_on_wifi_takes_its_update_from_the_release_server() {
    let root = LocalIdentity::from_seed(&[42; 32]);
    let base = esp_image(&[5u8; 4000]);
    let image = esp_image(&[6u8; 4100]);
    let file = delta_release(&root, 5, &base, &image);
    let (mut c, slot) = pulling_companion(&root, &base);
    c.app.fw_store = Some(Box::new(Streamed { inner: meshcore::fwshare::MemStore::new(file.clone(), 9), stream: None }));
    let cli = |m: &mut Mesh<Companion>, s: &str| m.app.handle_command(&mut m.ctx, false, s);

    assert_eq!(cli(&mut c, "get fw.server"), "> nw.ommesh.ru");
    assert_eq!(cli(&mut c, "set fw.server a b"), "Error: a host name, or host:port");
    assert_eq!(cli(&mut c, "set fw.server server.lan:8095"), "OK - release server server.lan:8095");
    assert_eq!(c.app.prefs.fw_server, "server.lan:8095");
    assert_eq!(cli(&mut c, "set fw.auto install"), "OK - installs stable releases by itself, from server.lan:8095 or a repeater nearby");
    assert_eq!(
        cli(&mut c, "set fw.source auto"),
        "OK - updates from server.lan:8095, or from the best repeater heard directly without it"
    );
    assert_eq!(cli(&mut c, "set fw.auto download"), "OK - downloads stable releases by itself; fw install installs them");

    // no repeater anywhere: the store fetches it from the server, the node loads it from there
    assert_eq!(cli(&mut c, "fw check"), "OK - asking server.lan:8095, see fw");
    let mut air = Air::new();
    air.run(&mut [&mut c], 1_000);
    assert_eq!(c.app.fw_line(c.ctx.now), "asking server.lan:8095");
    air.run(&mut [&mut c], 40_000);
    let kb = (file.len() as u32).div_ceil(1024);
    assert_eq!(c.app.fw_line(c.ctx.now), format!("0.5.2 #5 ready ({kb} KB): fw install"));
    assert!(!slot.borrow().1, "fw.auto download: nothing installed until the user says so");
    assert_eq!(cli(&mut c, "fw install"), "OK - installing 0.5.2 #5: restarting");
    assert!(slot.borrow().1 && c.app.take_effects().reboot);
    assert_eq!(&slot.borrow().0[..image.len()], &image[..]);
}

#[test]
fn a_companion_whose_server_is_out_of_reach_asks_a_repeater() {
    let root = LocalIdentity::from_seed(&[42; 32]);
    let base = esp_image(&[5u8; 4000]);
    let image = esp_image(&[6u8; 4100]);
    let file = delta_release(&root, 5, &base, &image);
    let cli = |m: &mut Mesh<Companion>, s: &str| m.app.handle_command(&mut m.ctx, false, s);

    // Wi-Fi down and no repeater heard: both said
    let (mut c, _) = pulling_companion(&root, &base);
    c.app.fw_store = Some(Box::new(Offline));
    assert_eq!(cli(&mut c, "fw check"), "OK - asking nw.ommesh.ru, see fw");
    Air::new().run(&mut [&mut c], 5_000);
    let line = c.app.fw_line(c.ctx.now);
    assert!(line.starts_with("failed: nw.ommesh.ru not reachable, no repeater heard directly; one with fw.serve on is needed"), "{line}");

    // with a repeater that hands updates on, the update comes over LoRa
    let mut r = serving_repeater(file);
    let (mut c, slot) = pulling_companion(&root, &base);
    c.app.fw_store = Some(Box::new(Offline));
    let mut air = Air::new();
    air.link(0, 1);
    air.run(&mut [&mut r, &mut c], 20_000);
    let _ = c.app.fw_check_now(&c.ctx, true);
    air.run(&mut [&mut r, &mut c], 120_000);
    assert!(slot.borrow().1 && c.app.take_effects().reboot, "{}", c.app.fw_line(c.ctx.now));

    // a repeater named in fw.source is asked over LoRa even with the server at hand
    let mut r = serving_repeater(Vec::new());
    let (mut c, _) = pulling_companion(&root, &base);
    c.app.fw_store = Some(Box::new(meshcore::fwshare::MemStore::new(Vec::new(), 9)));
    let mut air = Air::new();
    air.link(0, 1);
    air.run(&mut [&mut r, &mut c], 20_000);
    let name = r.app.prefs.node_name.clone();
    assert_eq!(cli(&mut c, &format!("set fw.source {name}")), format!("OK - updates from {name} ({}) over LoRa", meshcore::util::to_hex(&pub_key(&r)[..4])));
    assert_eq!(cli(&mut c, "fw check"), format!("OK - asking {name}, see fw"));
}

/// A store whose server had nothing newer when it last asked, and which believes that until it is
/// told to ask again.
struct StaleStore {
    inner: meshcore::fwshare::MemStore,
    stale: bool,
}

impl meshcore::fwshare::FwStore for StaleStore {
    fn offer(&mut self, q: &meshcore::fwshare::FwQuery) -> meshcore::fwshare::Offer {
        if self.stale { meshcore::fwshare::Offer::NoUpdate } else { self.inner.offer(q) }
    }
    fn read(&mut self, id: u32, offset: u32, buf: &mut [u8]) -> Option<usize> {
        self.inner.read(id, offset, buf)
    }
    fn refresh(&mut self, _q: &meshcore::fwshare::FwQuery) {
        self.stale = false;
    }
}

#[test]
fn fw_update_by_hand_does_not_take_the_repeaters_old_answer() {
    let root = LocalIdentity::from_seed(&[42; 32]);
    let base = esp_image(&[5u8; 4000]);
    let image = esp_image(&[6u8; 4100]);
    let mut r = repeater(1);
    r.app.prefs.fw_serve = true;
    let inner = meshcore::fwshare::MemStore::new(delta_release(&root, 5, &base, &image), 77);
    r.app.fw_store = Some(Box::new(StaleStore { inner, stale: true }));
    r.app.start(&mut r.ctx);
    let (mut c, slot) = pulling_companion(&root, &base);
    let mut air = Air::new();
    air.link(0, 1);

    // the scheduled check, 10 minutes after boot, believes what the repeater last heard
    air.run(&mut [&mut r, &mut c], 11 * 60_000);
    assert!(matches!(c.app.fw_state(), FwState::UpToDate { .. }), "{:?}", c.app.fw_state());

    // asked by hand, the repeater asks its server again: the update comes and installs itself
    assert!(c.app.handle_command(&mut c.ctx, false, "fw update").starts_with("OK"));
    air.run(&mut [&mut r, &mut c], 60_000);
    assert!(slot.borrow().1, "installed");
    assert!(c.app.take_effects().reboot);
}

/// A store with no link to its server, as on a repeater whose Wi-Fi is down.
struct Offline;

impl meshcore::fwshare::FwStore for Offline {
    fn offer(&mut self, _q: &meshcore::fwshare::FwQuery) -> meshcore::fwshare::Offer {
        meshcore::fwshare::Offer::Unavailable
    }
    fn read(&mut self, _id: u32, _offset: u32, _buf: &mut [u8]) -> Option<usize> {
        None
    }
}

/// A Heltec V4 repeater running `base`, pinning `root`, with `fw.auto install`: what the tests below pull into.
fn self_updating_repeater(seed: u8, root: &LocalIdentity, base: &[u8]) -> (Mesh<Repeater>, Slot) {
    use meshcore::release::{board, fingerprint, fingerprint_hex};
    let slot: Slot = std::rc::Rc::new(std::cell::RefCell::new((vec![0u8; 64 * 1024], false)));
    let mut r = repeater(seed);
    r.app.ota.target = Some(Box::new(RamSlots { running: base.to_vec(), slot: slot.clone() }));
    r.app.ota.build.board = board::HELTEC_V4;
    r.app.ota.build.counter = 4; // whatever MCFW_COUNTER this test was built with
    let fp = fingerprint_hex(&fingerprint(root.pub_key()));
    assert!(r.app.handle_command(&mut r.ctx, 0, &format!("set fw.root {fp}")).starts_with("OK"));
    assert!(r.app.handle_command(&mut r.ctx, 0, "set fw.auto install").starts_with("OK"));
    (r, slot)
}

#[test]
fn a_repeater_without_internet_pulls_its_update_from_a_neighbour() {
    use meshcore::release::{board, role};
    let root = LocalIdentity::from_seed(&[42; 32]);
    let base_data: Vec<u8> = (0..30_000u32).map(|i| (i * 13 % 251) as u8).collect();
    let mut new_data = base_data.clone();
    new_data[100..140].fill(7);
    new_data.extend_from_slice(b"new feature");
    let (base, image) = (esp_image(&base_data), esp_image(&new_data));
    let file = delta_release_for(&root, board::HELTEC_V4, role::REPEATER, 0, 5, &base, &image);

    // a neighbour that hands updates on, one that does not, and the node, whose Wi-Fi is down
    let mut serving = serving_repeater(file);
    let mut quiet = repeater(5);
    quiet.app.start(&mut quiet.ctx);
    let (mut r, slot) = self_updating_repeater(3, &root, &base);
    r.app.fw_store = Some(Box::new(Offline));
    let mut air = Air::new();
    air.link(0, 2);
    air.link(1, 2);
    air.run(&mut [&mut serving, &mut quiet, &mut r], 20_000); // the boot adverts: both neighbours heard

    assert!(r.app.handle_command(&mut r.ctx, 0, "fw update").starts_with("OK"));
    air.run(&mut [&mut serving, &mut quiet, &mut r], 120_000);
    let fx = r.app.take_effects();
    assert!(fx.reboot, "installed: restarting into it");
    assert!(slot.borrow().1);
    assert_eq!(&slot.borrow().0[..image.len()], &image[..], "the slot holds the release's image");
    assert_eq!(r.app.handle_command(&mut r.ctx, 0, "fw").lines().nth(1), Some("installing 0.5.2 #5: restarting, silent for 1-2 min"));
    let name = serving.app.prefs.node_name.clone();
    assert!(fx.console.iter().any(|l| l == &format!("fw: installing 0.5.2 #5 from neighbour {name}")), "{:?}", fx.console);
}

/// A store that asks its server about what it has no answer for, and hears there is nothing
/// newer; asked by hand, it forgets that answer (as the firmware's does a minute after the last
/// time it did).
struct NothingNewer {
    known: bool,
    fetches: std::rc::Rc<std::cell::Cell<u32>>,
}

impl meshcore::fwshare::FwStore for NothingNewer {
    fn offer(&mut self, _q: &meshcore::fwshare::FwQuery) -> meshcore::fwshare::Offer {
        if self.known {
            return meshcore::fwshare::Offer::NoUpdate;
        }
        self.known = true;
        self.fetches.set(self.fetches.get() + 1);
        meshcore::fwshare::Offer::Fetching { retry_secs: 60 }
    }
    fn read(&mut self, _id: u32, _offset: u32, _buf: &mut [u8]) -> Option<usize> {
        None
    }
    fn refresh(&mut self, _q: &meshcore::fwshare::FwQuery) {
        self.known = false;
    }
}

#[test]
fn a_check_by_hand_ends_when_the_neighbours_server_has_nothing_newer() {
    let root = LocalIdentity::from_seed(&[42; 32]);
    let base = esp_image(&[5u8; 4000]);
    let fetches = std::rc::Rc::new(std::cell::Cell::new(0));
    let mut serving = repeater(1);
    serving.app.prefs.fw_serve = true;
    serving.app.fw_store = Some(Box::new(NothingNewer { known: false, fetches: fetches.clone() }));
    serving.app.start(&mut serving.ctx);
    let (mut r, _slot) = self_updating_repeater(3, &root, &base);
    r.app.fw_store = Some(Box::new(Offline));
    let mut air = Air::new();
    air.link(0, 1);
    air.run(&mut [&mut serving, &mut r], 20_000);

    // the re-ask after "fetching" is not by hand any more: it takes the server's answer
    assert!(r.app.handle_command(&mut r.ctx, 0, "fw check").starts_with("OK"));
    air.run(&mut [&mut serving, &mut r], 5 * 60_000);
    assert!(matches!(r.app.fw_state(), FwState::UpToDate { .. }), "{:?}", r.app.fw_state());
    assert_eq!(fetches.get(), 1, "the neighbour asked its server once");
}

#[test]
fn fw_source_names_the_neighbour_a_repeater_pulls_from() {
    let root = LocalIdentity::from_seed(&[42; 32]);
    let base = esp_image(&[5u8; 4000]);
    let mut quiet = repeater(5);
    quiet.app.start(&mut quiet.ctx);
    let (mut r, slot) = self_updating_repeater(3, &root, &base);
    let cli = |r: &mut Mesh<Repeater>, c: &str| r.app.handle_command(&mut r.ctx, 0, c);
    assert_eq!(cli(&mut r, "get fw.source"), "> auto");
    assert!(cli(&mut r, "set fw.source xyz").starts_with("Error"));
    let quiet_key = meshcore::util::to_hex(&quiet.ctx.self_id.pub_key()[..4]);
    assert!(cli(&mut r, &format!("set fw.source {}", &quiet_key[..6])).starts_with("Error"), "8 hex digits at least");
    assert_eq!(cli(&mut r, &format!("set fw.source {quiet_key}")), format!("OK - updates from {quiet_key} over LoRa"));
    assert_eq!(cli(&mut r, "get fw.source"), format!("> {quiet_key}"));

    // not heard yet
    let mut air = Air::new();
    air.link(0, 1);
    assert!(cli(&mut r, "fw update").starts_with("OK"));
    air.run(&mut [&mut quiet, &mut r], 5_000);
    assert!(cli(&mut r, "fw").contains(&format!("failed: {quiet_key} not heard lately; next try ")), "{}", cli(&mut r, "fw"));

    // heard, but it does not hand updates on: asked twice, no answer
    air.run(&mut [&mut quiet, &mut r], 20_000);
    assert!(cli(&mut r, "fw update").starts_with("OK"));
    air.run(&mut [&mut quiet, &mut r], 30_000);
    assert!(cli(&mut r, "fw").contains("failed: no neighbour hands updates on (fw.serve)"), "{}", cli(&mut r, "fw"));
    assert!(!slot.borrow().1);
    // heard now: by its name too
    let name = quiet.app.prefs.node_name.clone();
    assert_eq!(cli(&mut r, "get fw.source"), format!("> {name} ({quiet_key})"));
    assert_eq!(cli(&mut r, "set fw.source auto"), "OK - updates from the release server, or from the best neighbour over LoRa without it");
    assert_eq!(cli(&mut r, &format!("set fw.source {name}")), format!("OK - updates from {name} ({quiet_key}) over LoRa"));

    assert_eq!(cli(&mut r, "set fw.source auto"), "OK - updates from the release server, or from the best neighbour over LoRa without it");
    assert_eq!(cli(&mut r, "get fw.source"), "> auto");
}

/// New code that does not compress: its delta is about as large as itself.
fn noise(len: usize) -> Vec<u8> {
    let mut x = 1u32;
    (0..len)
        .map(|_| {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (x >> 16) as u8
        })
        .collect()
}

#[test]
fn a_delta_larger_than_the_node_takes_is_not_pulled() {
    let root = LocalIdentity::from_seed(&[42; 32]);
    let base = esp_image(&[1u8; 3000]);
    let file = delta_release(&root, 5, &base, &esp_image(&noise(70_000)));
    // under the ceiling, over what this node's 64 KB slot holds
    assert!((64 * 1024..meshcore::fwshare::MAX_LORA_FILE).contains(&(file.len() as u32)), "{} bytes", file.len());

    let mut r = serving_repeater(file.clone());
    let (mut c, slot) = pulling_companion(&root, &base);
    let mut air = Air::new();
    air.link(0, 1);
    air.run(&mut [&mut r, &mut c], 20_000);
    let _ = c.app.fw_check_now(&c.ctx, false);
    air.run(&mut [&mut r, &mut c], 60_000);
    let kb = (file.len() as u32).div_ceil(1024);
    let why = format!("the update is {kb} KB, more than this node takes over LoRa (64 KB): install it over USB or BLE");
    assert!(c.app.fw_line(c.ctx.now).starts_with(&format!("failed: {why}; next try ")), "{}", c.app.fw_line(c.ctx.now));
    assert!(!slot.borrow().1);
}

/// A slot that holds the new image but not the delta beside it (a Pro Micro with a large build):
/// the delta is applied as it comes.
#[test]
fn a_delta_with_no_room_beside_its_image_is_applied_as_it_comes() {
    let root = LocalIdentity::from_seed(&[42; 32]);
    let (base, image) = (esp_image(&noise(50_000)), esp_image(&noise(60_000)));
    let file = delta_release(&root, 5, &base, &image);
    let sectors = |n: usize| n.div_ceil(4096);
    assert!(sectors(image.len()) + sectors(file.len()) > 16, "64 KB slot: {} + {} bytes", image.len(), file.len());

    let mut r = serving_repeater(file.clone());
    let (mut c, slot) = pulling_companion(&root, &base);
    c.app.prefs.fw_mode = meshcore::fwstate::mode::INSTALL;
    let mut air = Air::new();
    air.link(0, 1);
    air.run(&mut [&mut r, &mut c], 20_000);
    let _ = c.app.fw_check_now(&c.ctx, false);
    air.run(&mut [&mut r, &mut c], 3_600_000);
    assert!(slot.borrow().1, "installed: {}", c.app.fw_line(c.ctx.now));
    assert_eq!(&slot.borrow().0[..image.len()], &image[..]);
}

/// A delta over the ceiling, with room for it: a check leaves it, `fw update` takes it.
#[test]
fn a_companion_takes_a_delta_over_the_ceiling_only_by_hand() {
    let root = LocalIdentity::from_seed(&[42; 32]);
    let base = esp_image(&[1u8; 3000]);
    let image = esp_image(&noise(170_000));
    let file = delta_release(&root, 5, &base, &image);
    assert!(file.len() as u32 > meshcore::fwshare::MAX_LORA_FILE, "{} bytes", file.len());

    let mut r = serving_repeater(file.clone());
    let (mut c, slot) = pulling_companion(&root, &base);
    slot.borrow_mut().0 = vec![0u8; 512 * 1024];
    let mut air = Air::new();
    air.link(0, 1);
    air.run(&mut [&mut r, &mut c], 20_000);
    let _ = c.app.fw_check_now(&c.ctx, false);
    air.run(&mut [&mut r, &mut c], 60_000);
    let kb = (file.len() as u32).div_ceil(1024);
    let why = format!("the update is {kb} KB, over the 160 KB a node takes over LoRa by itself: fw update takes it");
    assert!(c.app.fw_line(c.ctx.now).starts_with(&format!("failed: {why}; next try ")), "{}", c.app.fw_line(c.ctx.now));

    assert!(c.app.fw_check_now(&c.ctx, true).is_ok());
    air.run(&mut [&mut r, &mut c], 4 * 3_600_000);
    assert!(slot.borrow().1, "installed: {}", c.app.fw_line(c.ctx.now));
    assert_eq!(&slot.borrow().0[..image.len()], &image[..]);
}

/// The same for a repeater that pulls its update from a neighbour.
#[test]
fn a_repeater_takes_a_delta_over_the_ceiling_only_by_hand() {
    use meshcore::release::{board, role};
    let root = LocalIdentity::from_seed(&[42; 32]);
    let base = esp_image(&[1u8; 3000]);
    let image = esp_image(&noise(170_000));
    let file = delta_release_for(&root, board::HELTEC_V4, role::REPEATER, 0, 5, &base, &image);
    assert!(file.len() as u32 > meshcore::fwshare::MAX_LORA_FILE, "{} bytes", file.len());

    let mut serving = serving_repeater(file.clone());
    let (mut r, slot) = self_updating_repeater(3, &root, &base);
    slot.borrow_mut().0 = vec![0u8; 512 * 1024];
    r.app.fw_store = Some(Box::new(Offline));
    let mut air = Air::new();
    air.link(0, 1);
    air.run(&mut [&mut serving, &mut r], 20_000);
    assert!(r.app.handle_command(&mut r.ctx, 0, "fw check").starts_with("OK"));
    air.run(&mut [&mut serving, &mut r], 60_000);
    let kb = (file.len() as u32).div_ceil(1024);
    let line = r.app.handle_command(&mut r.ctx, 0, "fw").lines().nth(1).unwrap_or_default().to_string();
    let why = format!("the update is {kb} KB, over the 160 KB a node takes over LoRa by itself: fw update takes it");
    assert!(line.starts_with(&format!("failed: {why}; next try ")), "{line}");

    assert!(r.app.handle_command(&mut r.ctx, 0, "fw update").starts_with("OK"));
    air.run(&mut [&mut serving, &mut r], 4 * 3_600_000);
    assert!(r.app.take_effects().reboot, "installed: restarting into it");
    assert_eq!(&slot.borrow().0[..image.len()], &image[..]);
}

/// A store holding two files, as a repeater's does: its own update and a neighbour's.
struct TwoFiles(meshcore::fwshare::MemStore, meshcore::fwshare::MemStore);

impl meshcore::fwshare::FwStore for TwoFiles {
    fn offer(&mut self, q: &meshcore::fwshare::FwQuery) -> meshcore::fwshare::Offer {
        match self.0.offer(q) {
            meshcore::fwshare::Offer::NoUpdate => self.1.offer(q),
            offer => offer,
        }
    }
    fn read(&mut self, id: u32, offset: u32, buf: &mut [u8]) -> Option<usize> {
        if id == self.0.id { self.0.read(id, offset, buf) } else { self.1.read(id, offset, buf) }
    }
}

#[test]
fn a_repeater_restarts_into_its_update_only_once_a_neighbour_has_loaded_its_own() {
    use meshcore::release::{board, fingerprint_hex, fingerprint, role};
    let root = LocalIdentity::from_seed(&[42; 32]);
    // the repeater's own update
    let own_data: Vec<u8> = (0..30_000u32).map(|i| (i * 13 % 251) as u8).collect();
    let own_base = esp_image(&own_data);
    let mut own_new = own_data.clone();
    own_new[100..140].fill(7);
    let own_file = delta_release_for(&root, board::HELTEC_V4, role::REPEATER, 0, 5, &own_base, &esp_image(&own_new));
    // the neighbour's (a T-Echo): a few kilobytes of new code, some minutes over LoRa
    let base = esp_image(&[1u8; 3000]);
    let mut x = 7u32;
    let code: Vec<u8> = (0..6000)
        .map(|_| {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (x >> 16) as u8
        })
        .collect();
    let their_file = delta_release(&root, 5, &base, &esp_image(&code));

    let own_slot: Slot = std::rc::Rc::new(std::cell::RefCell::new((vec![0u8; 64 * 1024], false)));
    let mut r = serving_repeater(their_file.clone());
    r.app.fw_store = Some(Box::new(TwoFiles(
        meshcore::fwshare::MemStore::new(own_file, 9),
        meshcore::fwshare::MemStore::new(their_file, 77),
    )));
    r.app.ota.target = Some(Box::new(RamSlots { running: own_base.clone(), slot: own_slot.clone() }));
    r.app.ota.build.board = board::HELTEC_V4;
    r.app.ota.build.counter = 4;
    let fp = fingerprint_hex(&fingerprint(root.pub_key()));
    r.app.handle_command(&mut r.ctx, 0, &format!("set fw.root {fp}"));
    // not by `set fw.auto`, which checks at once: the own update comes once the neighbour's has begun
    r.app.prefs.fw_mode = meshcore::fwstate::mode::INSTALL;
    let (mut c, _) = pulling_companion(&root, &base);
    let mut air = Air::new();
    air.link(0, 1);
    air.run(&mut [&mut r, &mut c], 20_000);
    let _ = c.app.fw_check_now(&c.ctx, false);
    air.run(&mut [&mut r, &mut c], 30_000);
    assert!(matches!(c.app.fw_state(), FwState::Downloading { .. }), "{:?}", c.app.fw_state());

    // the repeater's own update is ready meanwhile: it waits for the neighbour
    assert!(r.app.handle_command(&mut r.ctx, 0, "fw update").starts_with("OK"));
    air.run(&mut [&mut r, &mut c], 10_000);
    assert!(!r.app.take_effects().reboot, "no restart while a neighbour loads from it");
    assert_eq!(r.app.handle_command(&mut r.ctx, 0, "fw").lines().nth(1), Some("0.5.2 #5 ready: installs once a neighbour has its copy"));

    // the neighbour gets all of its update, then the repeater restarts into its own
    air.run(&mut [&mut r, &mut c], 600_000);
    assert!(c.app.fw_line(c.ctx.now).starts_with("0.5.2 #5 ready"));
    assert!(own_slot.borrow().1, "installed");
    assert!(r.app.handle_command(&mut r.ctx, 0, "fw").contains("installing 0.5.2 #5"));
}
