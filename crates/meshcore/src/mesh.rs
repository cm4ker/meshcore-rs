//! The routing core: a port of upstream `Dispatcher` + `Mesh`.
//!
//! [`Mesh`] owns a [`MeshCtx`] (identity, clocks, queues, duplicate table, TX budget) and an
//! application ([`MeshApp`], e.g. the repeater) that receives decoded payloads through hooks.
//! Hooks get `&mut MeshCtx`, so they can build and queue replies while the core is
//! mid-dispatch.
//!
//! The firmware drives it without any I/O inside:
//! - [`Mesh::on_rx_frame`] for every received LoRa frame,
//! - [`Mesh::poll`] periodically (delayed inbound queue, app timers),
//! - [`Mesh::tx_pending`] / [`Mesh::tx_defer_busy`] / [`Mesh::tx_take`] / [`Mesh::tx_done`]
//!   to transmit.

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::crypto::{self, Secret};
use crate::identity::{Identity, LocalIdentity};
use crate::packet::*;
use crate::queue::PacketQueue;
use crate::radio::RadioParams;
use crate::rng::FastRng;
use crate::rtc::RtcClock;
use crate::tables::SeenTable;
use crate::{
    CIPHER_BLOCK_SIZE, CIPHER_MAC_SIZE, MAX_ADVERT_DATA_SIZE, MAX_PACKET_PAYLOAD, MAX_PATH_SIZE, MAX_TRANS_UNIT,
    PATH_HASH_SIZE, PUB_KEY_SIZE, SIGNATURE_SIZE,
};

pub const ERR_EVENT_FULL: u16 = 1 << 0;
pub const ERR_EVENT_CAD_TIMEOUT: u16 = 1 << 1;
pub const ERR_EVENT_STARTRX_TIMEOUT: u16 = 1 << 2;

const MAX_RX_DELAY_MILLIS: i32 = 32_000;
const MIN_TX_BUDGET_RESERVE_MS: u32 = 100;
const MIN_TX_BUDGET_AIRTIME_DIV: u32 = 2;
const DUTY_CYCLE_WINDOW_MS: u32 = 3_600_000;
const CAD_FAIL_MAX_DURATION_MS: u64 = 4000;
const MAX_COMBINED_PATH: usize = MAX_PACKET_PAYLOAD - 2 - CIPHER_BLOCK_SIZE;

/// Pool sizes (upstream uses one pool of 32 shared packets).
const OUTBOUND_CAPACITY: usize = 32;
const INBOUND_CAPACITY: usize = 16;

/// What to do with a received packet once the app has seen it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Release,
    Retransmit { priority: u8, delay_ms: u32 },
}

#[derive(Clone, Copy, Debug, Default)]
pub struct GroupChannel {
    pub hash: u8,
    pub secret: Secret,
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub n_sent_flood: u32,
    pub n_sent_direct: u32,
    pub n_recv_flood: u32,
    pub n_recv_direct: u32,
    pub n_packets_recv: u32,
    pub n_packets_sent: u32,
    pub n_recv_errors: u32,
    pub total_air_time_ms: u64,
    pub rx_air_time_ms: u64,
    pub err_flags: u16,
    pub last_rssi: f32,
    pub last_snr: f32,
    pub noise_floor: i16,
}

/// A frame ready to go on air.
pub struct TxFrame {
    pub data: [u8; MAX_TRANS_UNIT],
    pub len: usize,
    /// Give up waiting for TX-done after this long.
    pub timeout_ms: u32,
    /// Coding rate for this frame if it differs from the configured one (dynamic CR).
    pub cr: Option<u8>,
}

impl TxFrame {
    pub fn bytes(&self) -> &[u8] {
        &self.data[..self.len]
    }
}

struct InFlight {
    start: u64,
    flood: bool,
    /// The packet on the air, for the app's TX hook.
    pkt: Box<Packet>,
    len: usize,
}

/// Everything except the application: shared state the app hooks may use.
pub struct MeshCtx {
    pub self_id: LocalIdentity,
    /// Milliseconds since boot, updated at every entry point.
    pub now: u64,
    pub rtc: RtcClock,
    pub rng: FastRng,
    pub radio: RadioParams,
    pub tables: SeenTable,
    pub stats: Stats,
    outbound: PacketQueue,
    inbound: PacketQueue,
    tx_budget_ms: u32,
    last_budget_update: u64,
    next_tx_time: u64,
    cad_busy_start: Option<u64>,
    in_flight: Option<InFlight>,
    airtime_factor: f32,
}

/// Application layer hooks (the virtual methods of upstream `Mesh`).
#[allow(unused_variables)]
pub trait MeshApp {
    /// Called for every received packet before routing (upstream `MyMesh::onRecvPacket` prologue).
    fn on_recv_packet_pre(&mut self, ctx: &mut MeshCtx, pkt: &Packet) {}

    /// Whether this node may retransmit `pkt` at all.
    fn allow_packet_forward(&mut self, ctx: &MeshCtx, pkt: &Packet) -> bool {
        false
    }

    /// Return `true` to drop a flood packet before it is processed.
    fn filter_recv_flood_packet(&mut self, ctx: &mut MeshCtx, pkt: &Packet) -> bool {
        false
    }

    fn retransmit_delay(&mut self, ctx: &mut MeshCtx, pkt: &Packet) -> u32 {
        let t = (ctx.radio.est_airtime_ms(pkt.raw_length()) * 52 / 50) / 2;
        ctx.rng.next_int(0, 5) * t
    }

    fn direct_retransmit_delay(&mut self, ctx: &mut MeshCtx, pkt: &Packet) -> u32 {
        0
    }

    fn extra_ack_transmit_count(&self) -> u8 {
        0
    }

    fn airtime_budget_factor(&self) -> f32 {
        1.0
    }

    /// Delay before processing a received flood packet, from its reception score.
    fn calc_rx_delay(&self, score: f32, air_time: u32) -> i32 {
        ((libm::powf(10.0, 0.85 - score) - 1.0) * air_time as f32) as i32
    }

    /// Number of known peers whose hash matches; the app remembers their indexes.
    fn search_peers_by_hash(&mut self, ctx: &mut MeshCtx, hash: u8) -> usize {
        0
    }

    fn peer_shared_secret(&mut self, ctx: &mut MeshCtx, peer_idx: usize) -> Option<Secret> {
        None
    }

    /// Decrypted REQ / RESPONSE / TXT_MSG from a known peer.
    fn on_peer_data_recv(
        &mut self, ctx: &mut MeshCtx, pkt: &Packet, payload_type: u8, sender_idx: usize, secret: &Secret, data: &[u8],
    ) {
    }

    /// A path to a known peer was returned; return `true` to send a reciprocal path.
    fn on_peer_path_recv(
        &mut self, ctx: &mut MeshCtx, pkt: &Packet, sender_idx: usize, secret: &Secret, path_len: u8, path: &[u8],
        extra_type: u8, extra: &[u8],
    ) -> bool {
        false
    }

    fn on_trace_recv(
        &mut self, ctx: &mut MeshCtx, pkt: &Packet, tag: u32, auth_code: u32, flags: u8, path_snrs: &[u8],
        path_hashes: &[u8],
    ) {
    }

    fn on_advert_recv(&mut self, ctx: &mut MeshCtx, pkt: &Packet, id: &Identity, timestamp: u32, app_data: &[u8]) {}

    /// Decrypted ANON_REQ addressed to us.
    fn on_anon_data_recv(&mut self, ctx: &mut MeshCtx, pkt: &Packet, secret: &Secret, sender: &Identity, data: &[u8]) {
    }

    fn on_control_data_recv(&mut self, ctx: &mut MeshCtx, pkt: &Packet) {}

    fn on_raw_data_recv(&mut self, ctx: &mut MeshCtx, pkt: &Packet) {}

    fn search_channels_by_hash(&mut self, ctx: &mut MeshCtx, hash: u8) -> Vec<GroupChannel> {
        Vec::new()
    }

    fn on_group_data_recv(
        &mut self, ctx: &mut MeshCtx, pkt: &Packet, payload_type: u8, channel: &GroupChannel, data: &[u8],
    ) {
    }

    /// An ACK was heard; return `true` if it was for this node (it is then not forwarded).
    fn on_ack_recv(&mut self, ctx: &mut MeshCtx, pkt: &Packet, ack_crc: u32) -> bool {
        false
    }

    /// Periodic work (timers etc.).
    fn on_loop(&mut self, ctx: &mut MeshCtx) {}

    /// Earliest time the app needs `on_loop` again.
    fn next_timer(&self) -> Option<u64> {
        None
    }

    /// Every received frame, before it is parsed (`logRxRaw`).
    fn log_rx_raw(&mut self, ctx: &MeshCtx, raw: &[u8], snr: f32, rssi: f32) {}
    fn log_rx(&mut self, ctx: &MeshCtx, pkt: &Packet, len: usize, score: f32) {}
    /// A transmission finished: `len` bytes on air for `airtime_ms`.
    fn log_tx(&mut self, ctx: &MeshCtx, pkt: &Packet, len: usize, airtime_ms: usize) {}
    fn log_tx_fail(&mut self, ctx: &MeshCtx) {}

    /// Coding rate to use for this outgoing packet, if it should differ from the default.
    fn tx_coding_rate(&self, ctx: &MeshCtx, pkt: &Packet) -> Option<u8> {
        None
    }

    /// A packet to process as if it had just been received (never forwarded), e.g. an imported
    /// contact advert.
    fn take_loopback(&mut self) -> Option<Box<Packet>> {
        None
    }
}

fn read_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

impl MeshCtx {
    pub fn new(self_id: LocalIdentity, rng_seed: [u8; 16], radio: RadioParams, epoch: u32) -> Self {
        Self {
            self_id,
            now: 0,
            rtc: RtcClock::new(epoch),
            rng: FastRng::new(rng_seed),
            radio,
            tables: SeenTable::new(),
            stats: Stats::default(),
            outbound: PacketQueue::new(OUTBOUND_CAPACITY),
            inbound: PacketQueue::new(INBOUND_CAPACITY),
            tx_budget_ms: 0,
            last_budget_update: 0,
            next_tx_time: 0,
            cad_busy_start: None,
            in_flight: None,
            airtime_factor: 1.0,
        }
    }

    pub fn current_time(&self) -> u32 {
        self.rtc.current_time(self.now)
    }

    pub fn current_time_unique(&mut self) -> u32 {
        self.rtc.current_time_unique(self.now)
    }

    pub fn set_current_time(&mut self, epoch: u32) {
        self.rtc.set_current_time(self.now, epoch);
    }

    pub fn outbound_len(&self) -> usize {
        self.outbound.len()
    }

    pub fn outbound(&self) -> impl Iterator<Item = &Packet> {
        self.outbound.iter()
    }

    pub fn remaining_tx_budget_ms(&self) -> u32 {
        self.tx_budget_ms
    }

    pub fn reset_stats(&mut self) {
        let floor = self.stats.noise_floor;
        self.stats = Stats { noise_floor: floor, ..Stats::default() };
        self.tables.reset_stats();
    }

    fn duty_cycle(&self) -> f32 {
        1.0 / (1.0 + self.airtime_factor)
    }

    // ---------------------------------------------------------------- sending

    /// Queue a fully-built packet for transmission (`Dispatcher::sendPacket`).
    pub fn send_packet(&mut self, pkt: Box<Packet>, priority: u8, delay_ms: u32) {
        if !Packet::is_valid_path_len(pkt.path_len) || pkt.payload_len > MAX_PACKET_PAYLOAD {
            return;
        }
        if self.outbound.add(pkt, priority, self.now + delay_ms as u64).is_err() {
            self.stats.err_flags |= ERR_EVENT_FULL;
        }
    }

    fn flood_priority(pkt: &Packet) -> u8 {
        match pkt.payload_type() {
            PAYLOAD_TYPE_PATH => 2,
            PAYLOAD_TYPE_ADVERT => 3, // de-prioritise
            _ => 1,
        }
    }

    pub fn send_flood(&mut self, mut pkt: Box<Packet>, delay_ms: u32, path_hash_size: usize) {
        if pkt.payload_type() == PAYLOAD_TYPE_TRACE || !(1..=3).contains(&path_hash_size) {
            return;
        }
        pkt.set_route_type(ROUTE_TYPE_FLOOD);
        pkt.set_path_hash_size_and_count(path_hash_size, 0);
        self.tables.mark_seen(&pkt);
        let pri = Self::flood_priority(&pkt);
        self.send_packet(pkt, pri, delay_ms);
    }

    pub fn send_flood_scoped(&mut self, mut pkt: Box<Packet>, codes: [u16; 2], delay_ms: u32, path_hash_size: usize) {
        if pkt.payload_type() == PAYLOAD_TYPE_TRACE || !(1..=3).contains(&path_hash_size) {
            return;
        }
        pkt.set_route_type(ROUTE_TYPE_TRANSPORT_FLOOD);
        pkt.transport_codes = codes;
        pkt.set_path_hash_size_and_count(path_hash_size, 0);
        self.tables.mark_seen(&pkt);
        let pri = Self::flood_priority(&pkt);
        self.send_packet(pkt, pri, delay_ms);
    }

    pub fn send_direct(&mut self, mut pkt: Box<Packet>, path_len: u8, path: &[u8], delay_ms: u32) {
        pkt.set_route_type(ROUTE_TYPE_DIRECT);
        
        let pri = if pkt.payload_type() == PAYLOAD_TYPE_TRACE {
            // TRACE: path goes at the end of the payload, `path` collects SNRs
            let n = (path_len as usize).min(path.len()).min(MAX_PACKET_PAYLOAD - pkt.payload_len);
            let start = pkt.payload_len;
            pkt.payload[start..start + n].copy_from_slice(&path[..n]);
            pkt.payload_len += n;
            pkt.path_len = 0;
            5
        } else {
            pkt.set_path(path_len, path);
            if pkt.payload_type() == PAYLOAD_TYPE_PATH { 1 } else { 0 }
        };
        self.tables.mark_seen(&pkt);
        self.send_packet(pkt, pri, delay_ms);
    }

    pub fn send_zero_hop(&mut self, mut pkt: Box<Packet>, delay_ms: u32) {
        pkt.set_route_type(ROUTE_TYPE_DIRECT);
        pkt.path_len = 0;
        self.tables.mark_seen(&pkt);
        self.send_packet(pkt, 0, delay_ms);
    }

    pub fn send_zero_hop_scoped(&mut self, mut pkt: Box<Packet>, codes: [u16; 2], delay_ms: u32) {
        pkt.set_route_type(ROUTE_TYPE_TRANSPORT_DIRECT);
        pkt.transport_codes = codes;
        pkt.path_len = 0;
        self.tables.mark_seen(&pkt);
        self.send_packet(pkt, 0, delay_ms);
    }

    // --------------------------------------------------------------- builders

    /// Signed self-advertisement (`Mesh::createAdvert`).
    pub fn create_advert(&mut self, app_data: &[u8]) -> Option<Box<Packet>> {
        if app_data.len() > MAX_ADVERT_DATA_SIZE {
            return None;
        }
        let mut pkt = Box::new(Packet::with_type(PAYLOAD_TYPE_ADVERT));
        let ts = self.current_time();
        let pub_key = *self.self_id.pub_key();

        let mut msg = [0u8; PUB_KEY_SIZE + 4 + MAX_ADVERT_DATA_SIZE];
        msg[..PUB_KEY_SIZE].copy_from_slice(&pub_key);
        msg[PUB_KEY_SIZE..PUB_KEY_SIZE + 4].copy_from_slice(&ts.to_le_bytes());
        msg[PUB_KEY_SIZE + 4..PUB_KEY_SIZE + 4 + app_data.len()].copy_from_slice(app_data);
        let sig = self.self_id.sign(&msg[..PUB_KEY_SIZE + 4 + app_data.len()]);

        let p = &mut pkt.payload;
        p[..32].copy_from_slice(&pub_key);
        p[32..36].copy_from_slice(&ts.to_le_bytes());
        p[36..100].copy_from_slice(&sig);
        p[100..100 + app_data.len()].copy_from_slice(app_data);
        pkt.payload_len = 100 + app_data.len();
        Some(pkt)
    }

    /// Encrypted REQ / RESPONSE / TXT_MSG to a known peer.
    pub fn create_datagram(
        &mut self, payload_type: u8, dest: &Identity, secret: &Secret, data: &[u8],
    ) -> Option<Box<Packet>> {
        if !matches!(payload_type, PAYLOAD_TYPE_TXT_MSG | PAYLOAD_TYPE_REQ | PAYLOAD_TYPE_RESPONSE) {
            return None;
        }
        if data.len() + CIPHER_MAC_SIZE + CIPHER_BLOCK_SIZE - 1 > MAX_PACKET_PAYLOAD {
            return None;
        }
        let mut pkt = Box::new(Packet::with_type(payload_type));
        pkt.payload[0] = dest.hash();
        pkt.payload[1] = self.self_id.id.hash();
        let n = crypto::encrypt_then_mac(secret, &mut pkt.payload[2..], data);
        pkt.payload_len = 2 + n;
        Some(pkt)
    }

    pub fn create_anon_datagram(&mut self, dest: &Identity, secret: &Secret, data: &[u8]) -> Option<Box<Packet>> {
        if data.len() + 1 + PUB_KEY_SIZE + CIPHER_BLOCK_SIZE - 1 > MAX_PACKET_PAYLOAD {
            return None;
        }
        let mut pkt = Box::new(Packet::with_type(PAYLOAD_TYPE_ANON_REQ));
        pkt.payload[0] = dest.hash();
        pkt.payload[1..1 + PUB_KEY_SIZE].copy_from_slice(self.self_id.pub_key());
        let n = crypto::encrypt_then_mac(secret, &mut pkt.payload[1 + PUB_KEY_SIZE..], data);
        pkt.payload_len = 1 + PUB_KEY_SIZE + n;
        Some(pkt)
    }

    pub fn create_group_datagram(
        &mut self, payload_type: u8, channel: &GroupChannel, data: &[u8],
    ) -> Option<Box<Packet>> {
        if !matches!(payload_type, PAYLOAD_TYPE_GRP_TXT | PAYLOAD_TYPE_GRP_DATA) {
            return None;
        }
        if data.len() + 1 + CIPHER_BLOCK_SIZE - 1 > MAX_PACKET_PAYLOAD {
            return None;
        }
        let mut pkt = Box::new(Packet::with_type(payload_type));
        pkt.payload[0] = channel.hash;
        let n = crypto::encrypt_then_mac(&channel.secret, &mut pkt.payload[1..], data);
        pkt.payload_len = 1 + n;
        Some(pkt)
    }

    /// PATH payload returning `path` to `dest_hash`, optionally carrying an extra payload.
    pub fn create_path_return(
        &mut self, dest_hash: u8, secret: &Secret, path_len: u8, path: &[u8], extra_type: u8, extra: &[u8],
    ) -> Option<Box<Packet>> {
        let path_bytes = Packet::path_bytes_for(path_len);
        if path_bytes + extra.len() + 5 > MAX_COMBINED_PATH || path_bytes > path.len() {
            return None;
        }
        let mut pkt = Box::new(Packet::with_type(PAYLOAD_TYPE_PATH));
        pkt.payload[0] = dest_hash;
        pkt.payload[1] = self.self_id.id.hash();

        let mut data = [0u8; MAX_PACKET_PAYLOAD];
        let mut n = 0;
        data[n] = path_len;
        n += 1;
        data[n..n + path_bytes].copy_from_slice(&path[..path_bytes]);
        n += path_bytes;
        if !extra.is_empty() {
            data[n] = extra_type;
            n += 1;
            data[n..n + extra.len()].copy_from_slice(extra);
            n += extra.len();
        } else {
            // dummy type + random blob, to make the packet hash unique
            data[n] = 0xFF;
            n += 1;
            self.rng.fill(&mut data[n..n + 4]);
            n += 4;
        }
        let len = crypto::encrypt_then_mac(secret, &mut pkt.payload[2..], &data[..n]);
        pkt.payload_len = 2 + len;
        Some(pkt)
    }

    pub fn create_ack(&mut self, ack: &[u8]) -> Box<Packet> {
        let mut pkt = Box::new(Packet::with_type(PAYLOAD_TYPE_ACK));
        pkt.set_payload(ack);
        pkt
    }

    pub fn create_multi_ack(&mut self, ack: &[u8], remaining: u8) -> Box<Packet> {
        let mut pkt = Box::new(Packet::with_type(PAYLOAD_TYPE_MULTIPART));
        pkt.payload[0] = (remaining << 4) | PAYLOAD_TYPE_ACK;
        let n = ack.len().min(MAX_PACKET_PAYLOAD - 1);
        pkt.payload[1..1 + n].copy_from_slice(&ack[..n]);
        pkt.payload_len = 1 + n;
        pkt
    }

    pub fn create_raw_data(&mut self, data: &[u8]) -> Option<Box<Packet>> {
        let mut pkt = Box::new(Packet::with_type(PAYLOAD_TYPE_RAW_CUSTOM));
        pkt.set_payload(data).then_some(pkt)
    }

    pub fn create_trace(&mut self, tag: u32, auth_code: u32, flags: u8) -> Box<Packet> {
        let mut pkt = Box::new(Packet::with_type(PAYLOAD_TYPE_TRACE));
        pkt.payload[..4].copy_from_slice(&tag.to_le_bytes());
        pkt.payload[4..8].copy_from_slice(&auth_code.to_le_bytes());
        pkt.payload[8] = flags;
        pkt.payload_len = 9;
        pkt
    }

    pub fn create_control_data(&mut self, data: &[u8]) -> Option<Box<Packet>> {
        let mut pkt = Box::new(Packet::with_type(PAYLOAD_TYPE_CONTROL));
        pkt.set_payload(data).then_some(pkt)
    }

    // ------------------------------------------------------------ TX budget

    fn update_tx_budget(&mut self) {
        let elapsed = self.now.saturating_sub(self.last_budget_update);
        let dc = self.duty_cycle();
        let max_budget = (DUTY_CYCLE_WINDOW_MS as f32 * dc) as u32;
        let refill = (elapsed as f32 * dc) as u32;
        if refill > 0 {
            self.tx_budget_ms = self.tx_budget_ms.saturating_add(refill).min(max_budget);
            self.last_budget_update = self.now;
        }
    }
}

/// The routing node: core context plus application.
pub struct Mesh<A: MeshApp> {
    pub ctx: MeshCtx,
    pub app: A,
}

impl<A: MeshApp> Mesh<A> {
    pub fn new(ctx: MeshCtx, app: A) -> Self {
        Self { ctx, app }
    }

    pub fn begin(&mut self, now: u64) {
        self.ctx.now = now;
        self.ctx.airtime_factor = self.app.airtime_budget_factor();
        self.ctx.tx_budget_ms = (DUTY_CYCLE_WINDOW_MS as f32 * self.ctx.duty_cycle()) as u32;
        self.ctx.last_budget_update = now;
        self.ctx.next_tx_time = now;
    }

    fn enter(&mut self, now: u64) {
        self.ctx.now = now;
        self.ctx.airtime_factor = self.app.airtime_budget_factor();
    }

    // -------------------------------------------------------------- receive

    /// Feed a raw received frame (`Dispatcher::checkRecv`).
    pub fn on_rx_frame(&mut self, now: u64, raw: &[u8], snr: f32, rssi: f32) {
        self.enter(now);
        self.ctx.stats.n_packets_recv += 1;
        self.ctx.stats.last_snr = snr;
        self.ctx.stats.last_rssi = rssi;
        self.app.log_rx_raw(&self.ctx, raw, snr, rssi);
        let Some(mut pkt) = Packet::parse(raw) else {
            return;
        };
        pkt.snr = (snr * 4.0) as i8;
        let score = self.ctx.radio.packet_score(snr, raw.len());
        let air_time = self.ctx.radio.est_airtime_ms(raw.len());
        self.ctx.stats.rx_air_time_ms += air_time as u64;
        self.app.log_rx(&self.ctx, &pkt, pkt.raw_length(), score);

        if pkt.is_route_flood() {
            self.ctx.stats.n_recv_flood += 1;
            let delay = self.app.calc_rx_delay(score, air_time);
            if delay < 50 {
                self.process_recv_packet(Box::new(pkt));
            } else {
                let delay = delay.min(MAX_RX_DELAY_MILLIS) as u64;
                let _ = self.ctx.inbound.add(Box::new(pkt), 0, now + delay);
            }
        } else {
            self.ctx.stats.n_recv_direct += 1;
            self.process_recv_packet(Box::new(pkt));
        }
    }

    /// A packet that came in over a bridge: processed like one heard on LoRa, `delay_ms` from now,
    /// without the RX log, counters and score delay (upstream `BridgeBase::handleReceivedPacket`
    /// queues it as an inbound packet).
    pub fn queue_bridged(&mut self, now: u64, pkt: Packet, delay_ms: u64) {
        self.enter(now);
        if self.ctx.inbound.add(Box::new(pkt), 0, now + delay_ms).is_err() {
            self.ctx.stats.err_flags |= ERR_EVENT_FULL;
        }
    }

    /// Periodic processing: delayed inbound packets and app timers.
    pub fn poll(&mut self, now: u64) {
        self.enter(now);
        while let Some(pkt) = self.ctx.inbound.get(now) {
            self.process_recv_packet(pkt);
        }
        self.app.on_loop(&mut self.ctx);
        while let Some(mut pkt) = self.app.take_loopback() {
            let _ = self.on_recv_packet(&mut pkt); // the action is ignored, like upstream
        }
    }

    /// Earliest time `poll` or the TX path needs attention.
    pub fn next_wakeup(&self) -> u64 {
        self.next_deadline().unwrap_or(u64::MAX).min(self.ctx.now + 1000)
    }

    /// The earliest scheduled event (a queued packet, an app timer), without the once-a-second
    /// poll of [`Self::next_wakeup`]: how long a sleeping board may stay asleep.
    pub fn next_deadline(&self) -> Option<u64> {
        let outbound = self.ctx.outbound.next_due().map(|d| d.max(self.ctx.next_tx_time));
        [self.ctx.inbound.next_due(), outbound, self.app.next_timer()].into_iter().flatten().min()
    }

    fn process_recv_packet(&mut self, mut pkt: Box<Packet>) {
        match self.on_recv_packet(&mut pkt) {
            Action::Release => {}
            Action::Retransmit { priority, delay_ms } => {
                let at = self.ctx.now + delay_ms as u64;
                if self.ctx.outbound.add(pkt, priority, at).is_err() {
                    self.ctx.stats.err_flags |= ERR_EVENT_FULL;
                }
            }
        }
    }

    /// `Mesh::onRecvPacket`: decide to discard, consume or forward.
    fn on_recv_packet(&mut self, pkt: &mut Packet) -> Action {
        let ctx = &mut self.ctx;
        let app = &mut self.app;
        app.on_recv_packet_pre(ctx, pkt);

        if pkt.is_route_direct() && pkt.payload_type() == PAYLOAD_TYPE_TRACE {
            if (pkt.path_len as usize) < MAX_PATH_SIZE && pkt.payload_len >= 9 {
                let tag = read_u32(&pkt.payload, 0);
                let auth = read_u32(&pkt.payload, 4);
                let flags = pkt.payload[8];
                let path_sz = flags & 0x03;
                let len = pkt.payload_len - 9;
                let offset = (pkt.path_len as usize) << path_sz;
                if offset >= len {
                    let snrs: Vec<u8> = pkt.path[..pkt.path_len as usize].to_vec();
                    let hashes: Vec<u8> = pkt.payload[9..pkt.payload_len].to_vec();
                    app.on_trace_recv(ctx, pkt, tag, auth, flags, &snrs, &hashes);
                } else {
                    let hash_len = 1usize << path_sz;
                    let start = 9 + offset;
                    let matches = start + hash_len <= pkt.payload_len
                        && ctx.self_id.id.is_hash_match(&pkt.payload[start..start + hash_len]);
                    if matches && app.allow_packet_forward(ctx, pkt) && !ctx.tables.was_seen(pkt) {
                        ctx.tables.mark_seen(pkt);
                        pkt.path[pkt.path_len as usize] = pkt.snr as u8; // append SNR, not hash
                        pkt.path_len += 1;
                        let d = app.direct_retransmit_delay(ctx, pkt);
                        return Action::Retransmit { priority: 5, delay_ms: d };
                    }
                }
            }
            return Action::Release;
        }

        if pkt.is_route_direct()
            && pkt.payload_type() == PAYLOAD_TYPE_CONTROL
            && pkt.payload_len > 0
            && pkt.payload[0] & 0x80 != 0
        {
            if pkt.path_hash_count() == 0 {
                app.on_control_data_recv(ctx, pkt);
            }
            return Action::Release; // only zero-hop allowed for this subset
        }

        if pkt.is_route_direct() && pkt.path_hash_count() > 0 {
            // 'early received' ACK
            if pkt.payload_type() == PAYLOAD_TYPE_ACK && pkt.payload_len >= 4 {
                let crc = read_u32(&pkt.payload, 0);
                if app.on_ack_recv(ctx, pkt, crc) {
                    pkt.mark_do_not_retransmit();
                }
            }
            let hs = pkt.path_hash_size();
            if ctx.self_id.id.is_hash_match(&pkt.path[..hs]) && app.allow_packet_forward(ctx, pkt) {
                let d = app.direct_retransmit_delay(ctx, pkt);
                if pkt.payload_type() == PAYLOAD_TYPE_MULTIPART {
                    Self::forward_multipart_direct(ctx, app, pkt, d);
                    return Action::Release;
                } else if pkt.payload_type() == PAYLOAD_TYPE_ACK {
                    if !ctx.tables.was_seen(pkt) {
                        ctx.tables.mark_seen(pkt);
                        remove_self_from_path(pkt);
                        Self::route_direct_recv_acks(ctx, app, pkt, d);
                    }
                    return Action::Release;
                }
                if !ctx.tables.was_seen(pkt) {
                    ctx.tables.mark_seen(pkt);
                    remove_self_from_path(pkt);
                    return Action::Retransmit { priority: 0, delay_ms: d }; // routed traffic is highest priority
                }
            }
            return Action::Release; // not the next hop, or already forwarded
        }

        if pkt.is_route_flood() && app.filter_recv_flood_packet(ctx, pkt) {
            return Action::Release;
        }

        let mut action = Action::Release;
        match pkt.payload_type() {
            PAYLOAD_TYPE_ACK => {
                if pkt.payload_len >= 4 && !ctx.tables.was_seen(pkt) {
                    ctx.tables.mark_seen(pkt);
                    let crc = read_u32(&pkt.payload, 0);
                    if app.on_ack_recv(ctx, pkt, crc) {
                        pkt.mark_do_not_retransmit();
                    }
                    action = Self::route_recv_packet(ctx, app, pkt);
                }
            }
            PAYLOAD_TYPE_PATH | PAYLOAD_TYPE_REQ | PAYLOAD_TYPE_RESPONSE | PAYLOAD_TYPE_TXT_MSG => {
                if 2 + CIPHER_MAC_SIZE < pkt.payload_len && !ctx.tables.was_seen(pkt) {
                    ctx.tables.mark_seen(pkt);
                    let dest_hash = pkt.payload[0];
                    let src_hash = pkt.payload[1];
                    if ctx.self_id.id.is_hash_match_v1(dest_hash) {
                        let found = Self::handle_peer_datagram(ctx, app, pkt, src_hash);
                        if found {
                            pkt.mark_do_not_retransmit(); // it was for us
                        }
                    }
                    action = Self::route_recv_packet(ctx, app, pkt);
                }
            }
            PAYLOAD_TYPE_ANON_REQ => {
                let i = 1 + PUB_KEY_SIZE;
                if i + 2 < pkt.payload_len && !ctx.tables.was_seen(pkt) {
                    ctx.tables.mark_seen(pkt);
                    if ctx.self_id.id.is_hash_match_v1(pkt.payload[0]) {
                        let sender = Identity::from_slice(&pkt.payload[1..i]).unwrap();
                        if let Some(secret) = ctx.self_id.calc_shared_secret(&sender.pub_key) {
                            let mut data = [0u8; MAX_PACKET_PAYLOAD];
                            if let Some(len) =
                                crypto::mac_then_decrypt(&secret, &mut data, &pkt.payload[i..pkt.payload_len])
                            {
                                app.on_anon_data_recv(ctx, pkt, &secret, &sender, &data[..len]);
                                pkt.mark_do_not_retransmit();
                            }
                        }
                    }
                    action = Self::route_recv_packet(ctx, app, pkt);
                }
            }
            PAYLOAD_TYPE_GRP_DATA | PAYLOAD_TYPE_GRP_TXT => {
                if 1 + 2 < pkt.payload_len && !ctx.tables.was_seen(pkt) {
                    ctx.tables.mark_seen(pkt);
                    let channels = app.search_channels_by_hash(ctx, pkt.payload[0]);
                    for ch in channels.iter().take(4) {
                        let mut data = [0u8; MAX_PACKET_PAYLOAD];
                        if let Some(len) = crypto::mac_then_decrypt(&ch.secret, &mut data, &pkt.payload[1..pkt.payload_len])
                        {
                            app.on_group_data_recv(ctx, pkt, pkt.payload_type(), ch, &data[..len]);
                            break;
                        }
                    }
                    action = Self::route_recv_packet(ctx, app, pkt);
                }
            }
            PAYLOAD_TYPE_ADVERT => {
                let i = PUB_KEY_SIZE + 4 + SIGNATURE_SIZE;
                if pkt.payload_len >= i {
                    let id = Identity::from_slice(&pkt.payload[..PUB_KEY_SIZE]).unwrap();
                    if id.pub_key == *ctx.self_id.pub_key() {
                        // our own advert echoed back
                    } else if !ctx.tables.was_seen(pkt) {
                        ctx.tables.mark_seen(pkt);
                        let ts = read_u32(&pkt.payload, PUB_KEY_SIZE);
                        let sig: [u8; SIGNATURE_SIZE] = pkt.payload[PUB_KEY_SIZE + 4..i].try_into().unwrap();
                        let app_len = (pkt.payload_len - i).min(MAX_ADVERT_DATA_SIZE);
                        let app_data: Vec<u8> = pkt.payload[i..i + app_len].to_vec();

                        let mut msg = [0u8; PUB_KEY_SIZE + 4 + MAX_ADVERT_DATA_SIZE];
                        msg[..PUB_KEY_SIZE].copy_from_slice(&id.pub_key);
                        msg[PUB_KEY_SIZE..PUB_KEY_SIZE + 4].copy_from_slice(&ts.to_le_bytes());
                        msg[PUB_KEY_SIZE + 4..PUB_KEY_SIZE + 4 + app_len].copy_from_slice(&app_data);
                        if id.verify(&sig, &msg[..PUB_KEY_SIZE + 4 + app_len]) {
                            app.on_advert_recv(ctx, pkt, &id, ts, &app_data);
                            action = Self::route_recv_packet(ctx, app, pkt);
                        }
                    }
                }
            }
            PAYLOAD_TYPE_RAW_CUSTOM => {
                if pkt.is_route_direct() && !ctx.tables.was_seen(pkt) {
                    ctx.tables.mark_seen(pkt);
                    app.on_raw_data_recv(ctx, pkt); // not flood-routed (yet)
                }
            }
            PAYLOAD_TYPE_MULTIPART
                if pkt.payload_len >= 5 && pkt.payload[0] & 0x0F == PAYLOAD_TYPE_ACK => {
                    let tmp = multipart_inner(pkt);
                    if !ctx.tables.was_seen(&tmp) {
                        ctx.tables.mark_seen(&tmp);
                        let crc = read_u32(&tmp.payload, 0);
                        app.on_ack_recv(ctx, &tmp, crc);
                    }
                }
            _ => {} // unknown types are never flood-routed
        }
        action
    }

    /// Try every peer matching `src_hash`; returns `true` if one decrypted the payload.
    fn handle_peer_datagram(ctx: &mut MeshCtx, app: &mut A, pkt: &Packet, src_hash: u8) -> bool {
        let num = app.search_peers_by_hash(ctx, src_hash);
        for j in 0..num {
            let Some(secret) = app.peer_shared_secret(ctx, j) else {
                continue;
            };
            let mut data = [0u8; MAX_PACKET_PAYLOAD];
            let Some(len) = crypto::mac_then_decrypt(&secret, &mut data, &pkt.payload[2..pkt.payload_len]) else {
                continue;
            };
            if pkt.payload_type() == PAYLOAD_TYPE_PATH {
                let path_len = data[0];
                if !Packet::is_valid_path_len(path_len) {
                    return false; // reject bad encoding
                }
                let path_bytes = Packet::path_bytes_for(path_len);
                let mut k = 1 + path_bytes;
                if k >= len {
                    return false;
                }
                let path = data[1..k].to_vec();
                let extra_type = data[k] & 0x0F; // upper 4 bits reserved
                k += 1;
                let extra = data[k..len].to_vec(); // may be zero-padded
                if app.on_peer_path_recv(ctx, pkt, j, &secret, path_len, &path, extra_type, &extra)
                    && pkt.is_route_flood()
                    && let Some(rpath) =
                        ctx.create_path_return(src_hash, &secret, pkt.path_len, &pkt.path, 0, &[])
                {
                    // reciprocal return path, sent DIRECT
                    ctx.send_direct(rpath, path_len, &path, 500);
                }
            } else {
                app.on_peer_data_recv(ctx, pkt, pkt.payload_type(), j, &secret, &data[..len]);
            }
            return true;
        }
        false
    }

    /// `Mesh::routeRecvPacket`: append our hash and re-flood.
    fn route_recv_packet(ctx: &mut MeshCtx, app: &mut A, pkt: &mut Packet) -> Action {
        let n = pkt.path_hash_count();
        let size = pkt.path_hash_size();
        if pkt.is_route_flood()
            && !pkt.is_marked_do_not_retransmit()
            && (n + 1) * size <= MAX_PATH_SIZE
            && app.allow_packet_forward(ctx, pkt)
        {
            let hash = &ctx.self_id.pub_key()[..size];
            pkt.path[n * size..(n + 1) * size].copy_from_slice(hash);
            pkt.set_path_hash_count(n + 1);
            let d = app.retransmit_delay(ctx, pkt);
            // closer sources get priority over far-away ones
            return Action::Retransmit { priority: pkt.path_hash_count() as u8, delay_ms: d };
        }
        Action::Release
    }

    fn forward_multipart_direct(ctx: &mut MeshCtx, app: &mut A, pkt: &Packet, delay_ms: u32) {
        let remaining = pkt.payload[0] >> 4;
        if pkt.payload[0] & 0x0F == PAYLOAD_TYPE_ACK && pkt.payload_len >= 5 {
            let mut tmp = multipart_inner(pkt);
            if !ctx.tables.was_seen(&tmp) {
                ctx.tables.mark_seen(&tmp);
                remove_self_from_path(&mut tmp);
                // multipart ACKs are expected ~300ms apart
                Self::route_direct_recv_acks(ctx, app, &tmp, delay_ms + (remaining as u32 + 1) * 300);
            }
        }
    }

    fn route_direct_recv_acks(ctx: &mut MeshCtx, app: &mut A, pkt: &Packet, mut delay_ms: u32) {
        if pkt.is_marked_do_not_retransmit() {
            return;
        }
        let mut extra = app.extra_ack_transmit_count();
        while extra > 0 {
            let mut a1 = ctx.create_multi_ack(pkt.payload(), extra);
            a1.set_path(pkt.path_len, &pkt.path);
            a1.set_route_type(ROUTE_TYPE_DIRECT);
            ctx.send_packet(a1, 0, delay_ms);
            delay_ms += 300;
            extra -= 1;
        }
        let mut a2 = ctx.create_ack(pkt.payload());
        a2.set_path(pkt.path_len, &pkt.path);
        a2.set_route_type(ROUTE_TYPE_DIRECT);
        ctx.send_packet(a2, 0, delay_ms);
    }

    // ------------------------------------------------------------- transmit

    /// Is a packet due, with enough TX budget and the inter-packet gap respected?
    pub fn tx_pending(&mut self, now: u64) -> bool {
        self.enter(now);
        let ctx = &mut self.ctx;
        if ctx.in_flight.is_some() || ctx.outbound.count_due(now) == 0 {
            return false;
        }
        ctx.update_tx_budget();
        let est = ctx.radio.est_airtime_ms(MAX_TRANS_UNIT);
        if ctx.tx_budget_ms < est / MIN_TX_BUDGET_AIRTIME_DIV {
            let needed = est / MIN_TX_BUDGET_AIRTIME_DIV - ctx.tx_budget_ms;
            ctx.next_tx_time = now + (needed as f32 / ctx.duty_cycle()) as u64;
            return false;
        }
        now >= ctx.next_tx_time
    }

    /// The radio reports channel activity. Returns `true` to defer, `false` to transmit anyway
    /// (channel busy for too long, radio is probably stuck).
    pub fn tx_defer_busy(&mut self, now: u64) -> bool {
        self.enter(now);
        let ctx = &mut self.ctx;
        let start = *ctx.cad_busy_start.get_or_insert(now);
        if now - start > CAD_FAIL_MAX_DURATION_MS {
            ctx.stats.err_flags |= ERR_EVENT_CAD_TIMEOUT;
            false
        } else {
            ctx.next_tx_time = now + ctx.rng.next_int(1, 4) as u64 * 120;
            true
        }
    }

    /// Pop the next packet and serialize it; the caller must transmit it and then call
    /// [`Self::tx_done`] or [`Self::tx_failed`].
    pub fn tx_take(&mut self, now: u64) -> Option<TxFrame> {
        self.enter(now);
        let ctx = &mut self.ctx;
        ctx.cad_busy_start = None;
        loop {
            let pkt = ctx.outbound.get(now)?;
            let mut frame = TxFrame { data: [0; MAX_TRANS_UNIT], len: 0, timeout_ms: 0, cr: None };
            let Some(len) = pkt.write_to(&mut frame.data) else {
                continue; // invalid packet queued, drop it
            };
            frame.len = len;
            frame.cr = self.app.tx_coding_rate(ctx, &pkt).filter(|&cr| cr != ctx.radio.cr);
            let params = frame.cr.map_or(ctx.radio, |cr| ctx.radio.with_cr(cr));
            frame.timeout_ms = params.est_airtime_ms(len) * 3 / 2 + 100;
            ctx.in_flight = Some(InFlight { start: now, flood: pkt.is_route_flood(), pkt, len });
            return Some(frame);
        }
    }

    pub fn tx_done(&mut self, now: u64) {
        self.enter(now);
        let ctx = &mut self.ctx;
        let Some(f) = ctx.in_flight.take() else {
            return;
        };
        let t = (now - f.start) as u32;
        ctx.stats.total_air_time_ms += t as u64;
        ctx.update_tx_budget();
        ctx.tx_budget_ms = ctx.tx_budget_ms.saturating_sub(t);
        if ctx.tx_budget_ms < MIN_TX_BUDGET_RESERVE_MS {
            let needed = MIN_TX_BUDGET_RESERVE_MS - ctx.tx_budget_ms;
            ctx.next_tx_time = now + (needed as f32 / ctx.duty_cycle()) as u64;
        } else {
            ctx.next_tx_time = now;
        }
        ctx.stats.n_packets_sent += 1;
        if f.flood {
            ctx.stats.n_sent_flood += 1;
        } else {
            ctx.stats.n_sent_direct += 1;
        }
        self.app.log_tx(&self.ctx, &f.pkt, f.len, t as usize);
    }

    pub fn tx_failed(&mut self, now: u64) {
        self.enter(now);
        self.ctx.in_flight = None;
        self.app.log_tx_fail(&self.ctx);
    }

    pub fn tx_in_flight(&self) -> bool {
        self.ctx.in_flight.is_some()
    }
}

fn remove_self_from_path(pkt: &mut Packet) {
    let count = pkt.path_hash_count();
    if count == 0 {
        return;
    }
    let sz = pkt.path_hash_size();
    pkt.set_path_hash_count(count - 1);
    pkt.path.copy_within(sz..count * sz, 0);
}

/// The ACK packet carried inside a multipart ACK (same header/path, payload minus first byte).
fn multipart_inner(pkt: &Packet) -> Packet {
    let mut tmp = Packet::new();
    tmp.header = pkt.header;
    tmp.set_path(pkt.path_len, &pkt.path);
    tmp.set_payload(&pkt.payload[1..pkt.payload_len]);
    tmp
}

const _: () = assert!(PATH_HASH_SIZE == 1);
