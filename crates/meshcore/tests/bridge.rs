//! Two repeaters joined by a bridge (upstream's ESP-NOW bridge framing), exchanging real frames.

use meshcore::identity::LocalIdentity;
use meshcore::mesh::{Mesh, MeshCtx};
use meshcore::packet::{PAYLOAD_TYPE_ADVERT, Packet};
use meshcore::prefs::NodePrefs;
use meshcore::radio::RadioParams;
use meshcore::repeater::{BoardInfo, Repeater};

fn bridged(seed: u8, source_rx: bool) -> Mesh<Repeater> {
    let id = LocalIdentity::from_seed(&[seed; 32]);
    let ctx = MeshCtx::new(id, [seed; 16], RadioParams::default(), 1_760_000_000);
    let board = BoardInfo { manufacturer: "Test".into(), bridge: Some("espnow"), powersaving_supported: true, ..Default::default() };
    let prefs = NodePrefs { bridge_enabled: true, bridge_source_rx: source_rx, ..NodePrefs::default() };
    let mut m = Mesh::new(ctx, Repeater::new(prefs, board));
    m.begin(0);
    m
}

/// Every frame the node transmits up to `now`.
fn drain_tx(m: &mut Mesh<Repeater>, now: u64) -> Vec<Vec<u8>> {
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
fn a_flood_crosses_the_bridge_once() {
    let mut a = bridged(1, false);
    let mut b = bridged(2, false);
    let _ = a.app.handle_command(&mut a.ctx, 0, "advert");
    let sent = drain_tx(&mut a, 2_000);
    assert_eq!(sent.len(), 1);
    let frames = a.app.take_bridge_out();
    assert_eq!(frames.len(), 1, "what a transmits goes over the bridge");

    let (pkt, delay) = b.app.bridge_receive(&frames[0]).unwrap();
    assert_eq!(pkt.payload_type(), PAYLOAD_TYPE_ADVERT);
    assert_eq!(delay, 500, "bridge.delay");
    b.queue_bridged(3_000, pkt, delay);
    assert!(drain_tx(&mut b, 3_400).is_empty(), "held for bridge.delay");
    // taken in once the delay is up, then sent after the usual retransmit delay
    let mut relayed = drain_tx(&mut b, 3_500);
    relayed.extend(drain_tx(&mut b, 20_000));
    assert_eq!(relayed.len(), 1, "flooded on b's LoRa");
    assert!(b.app.take_bridge_out().is_empty(), "and not sent back over the bridge");
    assert!(b.app.bridge_receive(&frames[0]).is_none(), "a repeat of the frame is dropped");
    assert_eq!(Packet::parse(&relayed[0]).unwrap().path_hash_count(), 1, "b added its hash");
}

#[test]
fn source_rx_bridges_what_the_node_hears() {
    let mut a = bridged(1, false);
    let mut b = bridged(2, true);
    let _ = a.app.handle_command(&mut a.ctx, 0, "advert");
    let heard = drain_tx(&mut a, 2_000);
    a.app.take_bridge_out();
    b.on_rx_frame(2_100, &heard[0], 8.0, -60.0);
    assert_eq!(b.app.take_bridge_out().len(), 1, "received packets go over the bridge");
    drain_tx(&mut b, 20_000);
    assert!(b.app.take_bridge_out().is_empty(), "its own retransmission does not");
}

#[test]
fn bridge_settings_from_the_cli() {
    let mut a = bridged(1, false);
    let mut cmd = |c: &str| a.app.handle_command(&mut a.ctx, 0, c);
    assert_eq!(cmd("get bridge.type"), "> espnow");
    assert_eq!(cmd("get bridge.source"), "> logTx");
    assert_eq!(cmd("set bridge.source rx"), "OK");
    assert_eq!(cmd("get bridge.source"), "> logRx");
    assert_eq!(cmd("set bridge.channel 15"), "Error: channel must be between 1-14");
    assert_eq!(cmd("set bridge.channel 6"), "OK - rebooting to apply");
    assert_eq!(cmd("set bridge.delay 20000"), "Error: delay must be between 0-10000 ms");
    assert_eq!(cmd("set bridge.secret abc"), "OK");
    assert_eq!(cmd("get bridge.secret"), "> abc");
    assert_eq!(cmd("powersaving on"), "Bridge not supported");
    assert!(a.app.take_effects().bridge_changed);
}
