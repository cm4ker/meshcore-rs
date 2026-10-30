//! End-to-end behaviour of the repeater against simulated peers, exchanging real wire frames.

use meshcore::advert::{ADV_TYPE_CHAT, ADV_TYPE_REPEATER, AdvertData};
use meshcore::crypto::{self, Secret};
use meshcore::identity::{Identity, LocalIdentity};
use meshcore::mesh::{Mesh, MeshApp, MeshCtx};
use meshcore::packet::*;
use meshcore::prefs::NodePrefs;
use meshcore::radio::RadioParams;
use meshcore::repeater::{BoardInfo, Repeater};
use meshcore::{MAX_ADVERT_DATA_SIZE, MAX_PACKET_PAYLOAD};

const LATER: u64 = 60_000;

fn repeater(seed: u8) -> Mesh<Repeater> {
    let id = LocalIdentity::from_seed(&[seed; 32]);
    let ctx = MeshCtx::new(id, [seed; 16], RadioParams::default(), 1_760_000_000);
    let board = BoardInfo { manufacturer: "Test".into(), batt_mv: 4100, ..Default::default() };
    let mut m = Mesh::new(ctx, Repeater::new(NodePrefs::default(), board));
    m.begin(0);
    m
}

/// Collect every frame the node wants to transmit up to `now`.
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

/// Minimal client: knows one peer (the repeater) and records what it receives.
#[derive(Default)]
struct Client {
    peer: Option<(Identity, Secret)>,
    responses: Vec<Vec<u8>>,
    texts: Vec<Vec<u8>>,
    path_returns: Vec<(u8, Vec<u8>, u8, Vec<u8>)>,
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
        &mut self, _: &mut MeshCtx, _: &Packet, _: usize, _: &Secret, path_len: u8, path: &[u8], extra_type: u8,
        extra: &[u8],
    ) -> bool {
        self.path_returns.push((path_len, path.to_vec(), extra_type, extra.to_vec()));
        false
    }
}

fn client(seed: u8) -> Mesh<Client> {
    let id = LocalIdentity::from_seed(&[seed; 32]);
    let ctx = MeshCtx::new(id, [seed; 16], RadioParams::default(), 1_760_000_000);
    let mut m = Mesh::new(ctx, Client::default());
    m.begin(0);
    m
}

fn frame(pkt: &Packet) -> Vec<u8> {
    let mut buf = [0u8; 255];
    let n = pkt.write_to(&mut buf).unwrap();
    buf[..n].to_vec()
}

#[test]
fn boot_advert_is_valid_zero_hop() {
    let mut r = repeater(1);
    r.app.start(&mut r.ctx);
    assert!(drain_tx(&mut r, 1000).is_empty(), "boot advert waits 16s");
    let tx = drain_tx(&mut r, 17_000);
    assert_eq!(tx.len(), 1);
    let pkt = Packet::parse(&tx[0]).unwrap();
    assert_eq!(pkt.payload_type(), PAYLOAD_TYPE_ADVERT);
    assert_eq!(pkt.route_type(), ROUTE_TYPE_DIRECT);
    assert_eq!(pkt.path_hash_count(), 0);

    // signature checks out and app data says "repeater"
    let p = pkt.payload();
    let id = Identity::from_slice(&p[..32]).unwrap();
    assert_eq!(id, r.ctx.self_id.id);
    let sig: [u8; 64] = p[36..100].try_into().unwrap();
    let mut msg = p[..36].to_vec();
    msg.extend_from_slice(&p[100..]);
    assert!(id.verify(&sig, &msg));
    let adv = AdvertData::parse(&p[100..]).unwrap();
    assert_eq!(adv.adv_type, ADV_TYPE_REPEATER);
    assert_eq!(adv.name.as_deref(), Some("Rust Repeater"));
}

#[test]
fn observer_queue_gets_rx_and_optionally_tx() {
    let mut r = repeater(1);
    r.app.uplink_enabled = true;
    r.app.start(&mut r.ctx);
    let advert = drain_tx(&mut r, 17_000);
    assert_eq!(advert.len(), 1);
    assert!(r.app.uplink.is_empty(), "mqtt.tx is off by default");

    r.app.prefs.mqtt_tx = true;
    let mut a = client(2);
    let b = LocalIdentity::from_seed(&[3; 32]);
    let secret = a.ctx.self_id.calc_shared_secret(b.pub_key()).unwrap();
    let mut flood = a.ctx.create_datagram(PAYLOAD_TYPE_TXT_MSG, &b.id, &secret, b"hello").unwrap();
    flood.set_route_type(ROUTE_TYPE_FLOOD);
    r.on_rx_frame(18_000, &frame(&flood), 5.0, -90.0);
    let fwd = drain_tx(&mut r, 18_000 + LATER);
    assert_eq!(fwd.len(), 1);
    let up = core::mem::take(&mut r.app.uplink);
    assert_eq!(up.len(), 2);
    assert!(!up[0].tx && up[0].score_duration.is_some());
    assert_eq!(up[0].raw, frame(&flood));
    assert!(up[1].tx && up[1].score_duration.is_none());
    assert_eq!(up[1].raw, fwd[0]);
}

#[test]
fn flood_is_forwarded_once_with_our_hash_appended() {
    let mut r = repeater(1);
    let mut a = client(2);
    let b = LocalIdentity::from_seed(&[3; 32]);
    let secret = a.ctx.self_id.calc_shared_secret(b.pub_key()).unwrap();
    let mut pkt = a.ctx.create_datagram(PAYLOAD_TYPE_TXT_MSG, &b.id, &secret, b"hello").unwrap();
    pkt.set_route_type(ROUTE_TYPE_FLOOD);
    pkt.set_path_hash_size_and_count(1, 1);
    pkt.path[0] = 0x42; // one hop already
    let raw = frame(&pkt);

    r.on_rx_frame(10, &raw, 8.0, -80.0);
    r.on_rx_frame(20, &raw, 8.0, -80.0); // duplicate
    let tx = drain_tx(&mut r, LATER);
    assert_eq!(tx.len(), 1, "forwarded exactly once");
    let fwd = Packet::parse(&tx[0]).unwrap();
    assert_eq!(fwd.path(), &[0x42, r.ctx.self_id.id.hash()]);
    assert_eq!(fwd.payload(), pkt.payload());
    assert_eq!(r.ctx.tables.flood_dups, 1);
}

#[test]
fn direct_is_forwarded_only_when_we_are_next_hop() {
    let mut r = repeater(1);
    let me = r.ctx.self_id.id.hash();
    let mut pkt = Packet::with_type(PAYLOAD_TYPE_TXT_MSG);
    pkt.set_route_type(ROUTE_TYPE_DIRECT);
    pkt.set_payload(&[0x10, 0x20, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18]);
    pkt.set_path(2, &[me, 0x77]);
    r.on_rx_frame(10, &frame(&pkt), 5.0, -90.0);
    let tx = drain_tx(&mut r, LATER);
    assert_eq!(tx.len(), 1);
    assert_eq!(Packet::parse(&tx[0]).unwrap().path(), &[0x77]);

    let mut other = pkt.clone();
    other.set_path(2, &[me.wrapping_add(1), 0x77]);
    other.payload[5] ^= 0xFF;
    r.on_rx_frame(LATER + 10, &frame(&other), 5.0, -90.0);
    assert!(drain_tx(&mut r, 2 * LATER).is_empty());
}

#[test]
fn repeat_off_and_hop_limits() {
    let mut r = repeater(1);
    let mut adv_pkt = {
        let mut other = repeater(9);
        let mut buf = [0u8; MAX_ADVERT_DATA_SIZE];
        let n = AdvertData::new(ADV_TYPE_CHAT, "phone").encode(&mut buf);
        other.ctx.create_advert(&buf[..n]).unwrap()
    };
    adv_pkt.set_route_type(ROUTE_TYPE_FLOOD);
    adv_pkt.set_path_hash_size_and_count(1, 8); // == flood.max.advert
    r.on_rx_frame(10, &frame(&adv_pkt), 5.0, -90.0);
    assert!(drain_tx(&mut r, LATER).is_empty(), "advert hop limit (8)");

    adv_pkt.set_path_hash_size_and_count(1, 2);
    let reply = r.app.handle_command(&mut r.ctx, 0, "set repeat off");
    assert_eq!(reply, "OK - repeat is now OFF");
    r.on_rx_frame(LATER, &frame(&adv_pkt), 5.0, -90.0);
    assert!(drain_tx(&mut r, 2 * LATER).is_empty(), "repeat off");
}

#[test]
fn neighbour_table_from_zero_hop_repeater_advert() {
    let mut r = repeater(1);
    let mut other = repeater(5);
    let mut buf = [0u8; MAX_ADVERT_DATA_SIZE];
    let n = AdvertData::new(ADV_TYPE_REPEATER, "far rptr").encode(&mut buf);
    let mut pkt = other.ctx.create_advert(&buf[..n]).unwrap();
    pkt.set_route_type(ROUTE_TYPE_DIRECT);
    pkt.path_len = 0;
    r.on_rx_frame(10, &frame(&pkt), 6.25, -70.0);
    assert_eq!(r.app.neighbours().len(), 1);
    assert_eq!(r.app.neighbours()[0].snr, 25);
    let list = r.app.handle_command(&mut r.ctx, 0, "neighbors");
    assert!(list.starts_with(&meshcore::util::to_hex(&other.ctx.self_id.pub_key()[..4])), "{list}");
}

/// Admin logs in with the password over flood, then runs a CLI command via DIRECT.
#[test]
fn admin_login_and_remote_cli() {
    let mut r = repeater(1);
    let mut c = client(2);
    let r_id = r.ctx.self_id.id;
    let secret = c.ctx.self_id.calc_shared_secret(&r_id.pub_key).unwrap();
    c.app.peer = Some((r_id, secret));

    // ANON_REQ: timestamp + password
    let mut data = 1_760_000_100u32.to_le_bytes().to_vec();
    data.extend_from_slice(b"password");
    let mut req = c.ctx.create_anon_datagram(&r_id, &secret, &data).unwrap();
    req.set_route_type(ROUTE_TYPE_FLOOD);
    req.set_path_hash_size_and_count(1, 0);
    r.on_rx_frame(10, &frame(&req), 7.0, -60.0);

    let tx = drain_tx(&mut r, LATER);
    assert_eq!(tx.len(), 1, "login reply (the request itself is not re-flooded)");
    let resp = Packet::parse(&tx[0]).unwrap();
    assert_eq!(resp.payload_type(), PAYLOAD_TYPE_PATH);
    assert!(resp.is_route_flood());

    // client decodes the PATH return carrying the RESPONSE
    c.on_rx_frame(LATER + 10, &tx[0], 7.0, -60.0);
    assert_eq!(c.app.path_returns.len(), 1);
    let (_, _, extra_type, extra) = &c.app.path_returns[0];
    assert_eq!(*extra_type, PAYLOAD_TYPE_RESPONSE);
    assert_eq!(extra[4], 0, "RESP_SERVER_LOGIN_OK");
    assert_eq!(extra[6], 1, "is admin");
    assert_eq!(extra[12], 2, "firmware level");
    assert_eq!(r.app.clients().len(), 1);
    assert!(r.app.clients()[0].is_admin());

    // CLI command as TXT_MSG (flags = CLI_COMMAND << 2), sent zero-hop DIRECT
    let mut cmd = 1_760_000_200u32.to_le_bytes().to_vec();
    cmd.push(3 << 2);
    cmd.extend_from_slice(b"get name");
    let mut msg = c.ctx.create_datagram(PAYLOAD_TYPE_TXT_MSG, &r_id, &secret, &cmd).unwrap();
    msg.set_route_type(ROUTE_TYPE_DIRECT);
    msg.path_len = 0;
    r.on_rx_frame(2 * LATER, &frame(&msg), 7.0, -60.0);
    let tx = drain_tx(&mut r, 3 * LATER);
    assert_eq!(tx.len(), 1);
    c.on_rx_frame(3 * LATER + 10, &tx[0], 7.0, -60.0);
    assert_eq!(c.app.texts.len(), 1);
    let text = &c.app.texts[0];
    assert_eq!(text[4], 1 << 2, "TXT_TYPE_CLI_DATA");
    let reply_end = text[5..].iter().position(|&b| b == 0).map(|p| p + 5).unwrap_or(text.len());
    assert_eq!(&text[5..reply_end], b"> Rust Repeater");

    // status request (REQ type 1) returns the 56-byte stats block
    let mut q = 1_760_000_300u32.to_le_bytes().to_vec();
    q.extend_from_slice(&[0x01, 0, 0, 0, 0]);
    let mut reqp = c.ctx.create_datagram(PAYLOAD_TYPE_REQ, &r_id, &secret, &q).unwrap();
    reqp.set_route_type(ROUTE_TYPE_DIRECT);
    reqp.path_len = 0;
    r.on_rx_frame(4 * LATER, &frame(&reqp), 7.0, -60.0);
    let tx = drain_tx(&mut r, 5 * LATER);
    c.on_rx_frame(5 * LATER + 10, &tx[0], 7.0, -60.0);
    let stats = &c.app.responses[0];
    assert_eq!(u32::from_le_bytes(stats[..4].try_into().unwrap()), 1_760_000_300);
    assert_eq!(u16::from_le_bytes(stats[4..6].try_into().unwrap()), 4100, "battery mV");
}

#[test]
fn wrong_password_is_ignored() {
    let mut r = repeater(1);
    let c = client(2);
    let r_id = r.ctx.self_id.id;
    let secret = c.ctx.self_id.calc_shared_secret(&r_id.pub_key).unwrap();
    let mut data = 1_760_000_100u32.to_le_bytes().to_vec();
    data.extend_from_slice(b"nope");
    let mut c = c;
    let mut req = c.ctx.create_anon_datagram(&r_id, &secret, &data).unwrap();
    req.set_route_type(ROUTE_TYPE_DIRECT);
    req.path_len = 0;
    r.on_rx_frame(10, &frame(&req), 7.0, -60.0);
    assert!(drain_tx(&mut r, LATER).is_empty());
    assert!(r.app.clients().is_empty());
}

#[test]
fn trace_appends_snr() {
    let mut r = repeater(1);
    let me = r.ctx.self_id.id.hash();
    let mut t = r.ctx.create_trace(0xDEADBEEF, 7, 0);
    let mut c = client(2);
    let path = [me, 0x55];
    c.ctx.send_direct(Box::new((*t).clone()), 2, &path, 0);
    let tx = drain_tx(&mut c, 10);
    *t = Packet::parse(&tx[0]).unwrap();
    assert_eq!(t.payload_len, 9 + 2);
    r.on_rx_frame(20, &tx[0], -2.5, -100.0);
    let out = drain_tx(&mut r, LATER);
    assert_eq!(out.len(), 1);
    let fwd = Packet::parse(&out[0]).unwrap();
    assert_eq!(fwd.path_len, 1);
    assert_eq!(fwd.path[0] as i8, -10, "SNR x4");
}

#[test]
fn payload_crypto_roundtrip_through_builders() {
    let a = LocalIdentity::from_seed(&[7; 32]);
    let b = LocalIdentity::from_seed(&[8; 32]);
    let s = a.calc_shared_secret(b.pub_key()).unwrap();
    let mut out = [0u8; MAX_PACKET_PAYLOAD];
    let mut enc = [0u8; MAX_PACKET_PAYLOAD];
    let n = crypto::encrypt_then_mac(&s, &mut enc, b"abc");
    let m = crypto::mac_then_decrypt(&b.calc_shared_secret(a.pub_key()).unwrap(), &mut out, &enc[..n]).unwrap();
    assert_eq!(&out[..3], b"abc");
    assert_eq!(m, 16);
}

/// `setperm` with a mistyped key that is no curve point: refused, as it would give no secret
/// (kept with zeros, it would match packets anyone can make).
/// Wrong passwords are counted, whoever sends them: past ten in ten minutes none is compared, not
/// even the right one, until the window ends.
#[test]
fn password_guesses_are_limited() {
    let mut r = repeater(1);
    let mut c = client(2);
    let r_id = r.ctx.self_id.id;
    let secret = c.ctx.self_id.calc_shared_secret(&r_id.pub_key).unwrap();
    let mut login = |r: &mut Mesh<Repeater>, ts: u32, password: &[u8], now: u64| {
        let mut data = ts.to_le_bytes().to_vec();
        data.extend_from_slice(password);
        let mut req = c.ctx.create_anon_datagram(&r_id, &secret, &data).unwrap();
        req.set_route_type(ROUTE_TYPE_DIRECT);
        req.path_len = 0;
        r.on_rx_frame(now, &frame(&req), 7.0, -60.0);
        !drain_tx(r, now + 10_000).is_empty()
    };
    let t = 1_760_000_100;
    assert!(login(&mut r, t, b"", 0), "the guest's (empty) password: no guess, not counted");
    for k in 1..=10 {
        assert!(!login(&mut r, t + k, b"guess", k as u64 * 20_000));
    }
    assert!(!login(&mut r, t + 20, b"password", 300_000), "locked for the rest of the ten minutes");
    assert!(login(&mut r, t + 30, b"password", 620_000));
}

/// The admins' last timestamps reach flash before a restart: a recorded `reboot` played again
/// after it is ignored (upstream keeps them in RAM, and would restart again, and again).
#[test]
fn a_recorded_command_does_not_outlive_a_restart() {
    let mut r = repeater(1);
    let mut c = client(2);
    let r_id = r.ctx.self_id.id;
    let secret = c.ctx.self_id.calc_shared_secret(&r_id.pub_key).unwrap();
    let mut data = 1_760_000_100u32.to_le_bytes().to_vec();
    data.extend_from_slice(b"password");
    let mut req = c.ctx.create_anon_datagram(&r_id, &secret, &data).unwrap();
    req.set_route_type(ROUTE_TYPE_DIRECT);
    req.path_len = 0;
    r.on_rx_frame(10, &frame(&req), 7.0, -60.0);
    assert_eq!(drain_tx(&mut r, LATER).len(), 1, "logged in");
    assert!(r.app.take_effects().save_acl);

    let mut cmd = 1_760_000_200u32.to_le_bytes().to_vec();
    cmd.push(3 << 2);
    cmd.extend_from_slice(b"reboot");
    let mut msg = c.ctx.create_datagram(PAYLOAD_TYPE_TXT_MSG, &r_id, &secret, &cmd).unwrap();
    msg.set_route_type(ROUTE_TYPE_DIRECT);
    msg.path_len = 0;
    let recorded = frame(&msg);
    r.on_rx_frame(2 * LATER, &recorded, 7.0, -60.0);
    drain_tx(&mut r, 2 * LATER + 100);
    let fx = r.app.take_effects();
    assert!(fx.reboot && fx.save_acl, "saved at once, not a few seconds later");

    let mut again = repeater(1);
    let self_id = again.ctx.self_id.clone();
    again.app.acl_load(&r.app.acl_serialize(), &self_id);
    assert_eq!(again.app.clients()[0].last_timestamp, 1_760_000_200);
    again.on_rx_frame(10, &recorded, 7.0, -60.0);
    drain_tx(&mut again, LATER);
    assert!(!again.app.take_effects().reboot, "the replay is ignored");

    // an ACL saved by older firmware has no timestamps
    let old = &r.app.acl_serialize()[..136];
    again.app.acl_load(old, &self_id);
    assert_eq!(again.app.clients()[0].last_timestamp, 0);
    assert!(again.app.clients()[0].is_admin());
}

#[test]
fn setperm_refuses_a_key_that_gives_no_secret() {
    let mut r = repeater(1);
    let me = r.ctx.self_id.clone();
    let hex = |k: &[u8]| k.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let bad = (2u8..).map(|k| [k; 32]).find(|k| me.calc_shared_secret(k).is_none()).unwrap();
    assert_eq!(r.app.handle_command(&mut r.ctx, 0, &format!("setperm {} 3", hex(&bad))), "Err - invalid params");
    let good = LocalIdentity::from_seed(&[9; 32]);
    assert_eq!(r.app.handle_command(&mut r.ctx, 0, &format!("setperm {} 3", hex(good.pub_key()))), "OK");
}

/// Full LoRa firmware update through admin requests, as the host tool drives it.
#[test]
fn lora_ota_update() {
    use meshcore::ota::{FirmwareTarget, LORA_CHUNK_SIZE, OtaError, REQ_TYPE_OTA_MANIFEST};
    use meshcore::release::{Envelope, Manifest, board, role};
    use meshcore::repeater::*;
    use sha2::{Digest, Sha256};
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    struct Flash {
        mem: Vec<u8>,
        activated: bool,
    }
    struct Target(Rc<RefCell<Flash>>);
    impl FirmwareTarget for Target {
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
    }

    let flash = Rc::new(RefCell::new(Flash { mem: vec![0; 32 * 1024], activated: false }));
    let mut r = repeater(1);
    r.app.ota.target = Some(Box::new(Target(flash.clone())));
    let mut c = client(2);
    let r_id = r.ctx.self_id.id;
    let secret = c.ctx.self_id.calc_shared_secret(&r_id.pub_key).unwrap();
    c.app.peer = Some((r_id, secret));

    let mut now = 10u64;
    let mut ts = 1_760_000_100u32;
    // login (zero-hop direct, so replies come back as plain RESPONSE datagrams)
    let mut login = ts.to_le_bytes().to_vec();
    login.extend_from_slice(b"password");
    let mut pkt = c.ctx.create_anon_datagram(&r_id, &secret, &login).unwrap();
    pkt.set_route_type(ROUTE_TYPE_DIRECT);
    pkt.path_len = 0;
    r.on_rx_frame(now, &frame(&pkt), 7.0, -60.0);
    for f in drain_tx(&mut r, now + LATER) {
        c.on_rx_frame(now + LATER, &f, 7.0, -60.0);
    }
    now += 2 * LATER;

    // one request -> optional response body (after the 4-byte tag)
    let mut req = |body: &[u8], r: &mut Mesh<Repeater>, c: &mut Mesh<Client>| -> Option<Vec<u8>> {
        ts += 1;
        let mut d = ts.to_le_bytes().to_vec();
        d.extend_from_slice(body);
        let mut p = c.ctx.create_datagram(PAYLOAD_TYPE_REQ, &r_id, &secret, &d).unwrap();
        p.set_route_type(ROUTE_TYPE_DIRECT);
        p.path_len = 0;
        let before = c.app.responses.len();
        r.on_rx_frame(now, &frame(&p), 7.0, -60.0);
        now += LATER;
        for f in drain_tx(r, now) {
            c.on_rx_frame(now, &f, 7.0, -60.0);
        }
        now += 1000;
        (c.app.responses.len() > before).then(|| c.app.responses.last().unwrap()[4..].to_vec())
    };

    let mut img: Vec<u8> = (0..5000).map(|i| (i % 253) as u8).collect();
    img[0] = 0xE9;
    img[12..14].copy_from_slice(&9u16.to_le_bytes());
    img[23] = 1; // hash appended, like espflash images
    let h: [u8; 32] = Sha256::digest(&img[..5000 - 32]).into();
    img[5000 - 32..].copy_from_slice(&h);
    let sha: [u8; 32] = Sha256::digest(&img).into();

    let info = req(&[REQ_TYPE_OTA_INFO], &mut r, &mut c).unwrap();
    assert_eq!(info[0], 1, "has target");

    let mut begin = vec![REQ_TYPE_OTA_BEGIN];
    begin.extend_from_slice(&(img.len() as u32).to_le_bytes());
    begin.extend_from_slice(&sha);
    begin.extend_from_slice(&LORA_CHUNK_SIZE.to_le_bytes());
    // the admin password alone installs nothing over LoRa: only a release a pinned root signed
    assert_eq!(req(&begin, &mut r, &mut c).unwrap()[..3], [OtaError::NoRoot as u8, 0, 0]);

    let root = LocalIdentity::from_seed(&[0x5A; 32]);
    r.app.ota.roots = vec![meshcore::release::fingerprint(root.pub_key())];
    r.app.ota.build.board = board::HELTEC_V4;
    r.app.ota.build.counter = 4; // whatever MCFW_COUNTER this test was built with
    let manifest = Manifest {
        board: board::HELTEC_V4,
        role: role::REPEATER,
        flags: 0,
        counter: 5,
        image_len: img.len() as u32,
        image_sha256: sha,
        issued: 0,
        version: "test".into(),
    };
    let mut env = Envelope { manifest, signatures: Vec::new() };
    env.add_signature(*root.pub_key(), root.sign(&Envelope::signed_message(&env.manifest)));
    let env = env.to_bytes();
    for (k, piece) in env.chunks(100).enumerate() {
        let mut m = vec![REQ_TYPE_OTA_MANIFEST];
        m.extend_from_slice(&((k * 100) as u16).to_le_bytes());
        m.extend_from_slice(&(env.len() as u16).to_le_bytes());
        m.push(piece.len() as u8);
        m.extend_from_slice(piece);
        assert_eq!(req(&m, &mut r, &mut c).unwrap()[0], 0);
    }

    let b = req(&begin, &mut r, &mut c).unwrap();
    assert_eq!(b[0], 0);
    let total = u16::from_le_bytes([b[1], b[2]]) as usize;
    let cs = LORA_CHUNK_SIZE as usize;
    assert_eq!(total, img.len().div_ceil(cs));

    let chunk = |i: usize| {
        let mut d = vec![REQ_TYPE_OTA_DATA];
        d.extend_from_slice(&(i as u16).to_le_bytes());
        d.extend_from_slice(&img[i * cs..((i + 1) * cs).min(img.len())]);
        // a companion forwards at most 143 bytes of request (176-byte serial frames)
        assert!(d.len() <= 143);
        d
    };
    // "lose" every 5th chunk on the first pass; DATA gets no response
    for i in (0..total).filter(|i| i % 5 != 0) {
        assert!(req(&chunk(i), &mut r, &mut c).is_none());
    }
    let st = req(&[REQ_TYPE_OTA_STATUS, 0, 0], &mut r, &mut c).unwrap();
    let n = st[5] as usize;
    let missing: Vec<usize> = (0..n).map(|k| u16::from_le_bytes([st[6 + 2 * k], st[7 + 2 * k]]) as usize).collect();
    assert_eq!(missing, (0..total).filter(|i| i % 5 == 0).collect::<Vec<_>>());
    for i in missing {
        req(&chunk(i), &mut r, &mut c);
    }
    let commit = req(&[REQ_TYPE_OTA_COMMIT], &mut r, &mut c).unwrap();
    assert_eq!(commit[0], 0, "commit ok");
    assert!(flash.borrow().activated);
    assert_eq!(&flash.borrow().mem[..img.len()], &img[..]);
    assert!(r.app.take_effects().reboot);
}

/// Transport-code zoning: only scopes this repeater knows and allows are re-flooded.
#[test]
fn regions_filter_scoped_floods() {
    use meshcore::region::TransportKey;

    let mut r = repeater(1);
    let mut a = client(2);
    let b = LocalIdentity::from_seed(&[3; 32]);
    let secret = a.ctx.self_id.calc_shared_secret(b.pub_key()).unwrap();
    let mut n = 0u8;
    let mut scoped = |key: Option<&str>| {
        n += 1;
        let mut pkt = a.ctx.create_datagram(PAYLOAD_TYPE_TXT_MSG, &b.id, &secret, &[n; 9]).unwrap();
        match key {
            Some(name) => {
                pkt.set_route_type(ROUTE_TYPE_TRANSPORT_FLOOD);
                pkt.transport_codes = [TransportKey::for_hashtag(name).transport_code(&pkt), 0];
            }
            None => pkt.set_route_type(ROUTE_TYPE_FLOOD),
        }
        frame(&pkt)
    };
    let mut t = 0;
    let mut forwarded = |r: &mut Mesh<Repeater>, raw: Vec<u8>| {
        t += 2 * LATER;
        r.on_rx_frame(t, &raw, 5.0, -90.0);
        !drain_tx(r, t + LATER).is_empty()
    };

    assert!(!forwarded(&mut r, scoped(Some("#omsk"))), "unknown scope");
    assert!(forwarded(&mut r, scoped(None)), "un-scoped allowed by default");

    assert_eq!(r.app.handle_command(&mut r.ctx, 0, "region put omsk"), "OK - (flood allowed)");
    assert!(forwarded(&mut r, scoped(Some("#omsk"))), "known scope");
    assert!(!forwarded(&mut r, scoped(Some("#tomsk"))));
    assert_eq!(r.app.handle_command(&mut r.ctx, 0, "region denyf omsk"), "OK");
    assert!(!forwarded(&mut r, scoped(Some("#omsk"))), "scope denied");
    assert_eq!(r.app.handle_command(&mut r.ctx, 0, "region denyf *"), "OK");
    assert!(!forwarded(&mut r, scoped(None)), "un-scoped denied");

    // own floods use the default scope
    assert_eq!(r.app.handle_command(&mut r.ctx, 0, "region default omsk"), " default scope is now omsk");
    r.app.handle_command(&mut r.ctx, 0, "advert");
    let tx = drain_tx(&mut r, t + 10 * LATER);
    let adv = Packet::parse(tx.last().unwrap()).unwrap();
    assert_eq!(adv.route_type(), ROUTE_TYPE_TRANSPORT_FLOOD);
    assert_eq!(adv.transport_codes[0], TransportKey::for_hashtag("#omsk").transport_code(&adv));

    // multi-line `region load`
    assert_eq!(r.app.handle_command(&mut r.ctx, 0, "region load"), "");
    r.app.handle_command(&mut r.ctx, 0, " ru");
    r.app.handle_command(&mut r.ctx, 0, "  omsk F");
    r.app.handle_command(&mut r.ctx, 0, "  novosibirsk F");
    assert_eq!(r.app.handle_command(&mut r.ctx, 0, ""), "OK - loaded 3 regions");
    let tree = r.app.handle_command(&mut r.ctx, 0, "region");
    assert!(tree.contains(" ru\n  omsk F\n  novosibirsk F\n"), "{tree}"); // omsk keeps its flags (default scope)
    assert!(r.app.take_effects().save_regions);
}

/// Roadmap "dynamic CR": direct hops to strong neighbours go out at 4/5, weak ones at 4/8.
#[test]
fn dynamic_coding_rate_per_hop() {
    let mut r = repeater(1);
    r.ctx.radio.cr = 7; // like PKIO
    assert_eq!(r.app.handle_command(&mut r.ctx, 0, "set dyncr on"), "OK - dynamic coding rate on");
    let me = r.ctx.self_id.id.hash();
    // two neighbour repeaters heard zero-hop: one strong (SNR 10), one weak (SNR -4)
    let mut hashes = Vec::new();
    for (seed, snr) in [(5u8, 10.0f32), (6, -4.0)] {
        let mut other = repeater(seed);
        let mut buf = [0u8; MAX_ADVERT_DATA_SIZE];
        let n = AdvertData::new(ADV_TYPE_REPEATER, "n").encode(&mut buf);
        let mut adv = other.ctx.create_advert(&buf[..n]).unwrap();
        adv.set_route_type(ROUTE_TYPE_DIRECT);
        adv.path_len = 0;
        r.on_rx_frame(10, &frame(&adv), snr, -80.0);
        hashes.push(other.ctx.self_id.id.hash());
    }
    let mut t = LATER;
    for (next, expected_cr) in [(hashes[0], Some(5u8)), (hashes[1], Some(8u8)), (0x77, None)] {
        let mut pkt = Packet::with_type(PAYLOAD_TYPE_TXT_MSG);
        pkt.set_route_type(ROUTE_TYPE_DIRECT);
        pkt.set_payload(&[next, t as u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18]);
        pkt.set_path(2, &[me, next]);
        r.on_rx_frame(t, &frame(&pkt), 5.0, -90.0);
        t += LATER;
        r.poll(t);
        assert!(r.tx_pending(t));
        let f = r.tx_take(t).unwrap();
        r.tx_done(t);
        assert_eq!(f.cr, expected_cr, "next hop {next:02X}");
    }
}
