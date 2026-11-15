//! Signed network time (this firmware's extension): every node, connected to nothing, takes its
//! clock from the time server (`nw.ommesh.ru`), which signs its time with a key the firmware
//! carries ([`TIME_KEYS`]). Repeaters with an internet link (`set time.serve on`, gateways) fetch
//! the signed time and pass it on; they and the repeaters on the way are not trusted, the node
//! checks the signature itself.
//!
//! Two ways, both on upstream packet types, so upstream repeaters forward them:
//!
//! - The beacon: a gateway floods the server's token ([`Token`]) as GRP_DATA in channel
//!   [`CHANNEL_NAME`] a few times a day. It has no nonce, so it could be recorded and played
//!   again later: a node moves its clock forward by it only, never back. What a played beacon
//!   can do is leave a clock behind; a beacon from the future can't be made without the key. A
//!   beacon more than [`AHEAD_MS`] behind this node's clock makes it ask (below).
//! - The request: `TIME_REQ` (ANON_REQ kind [`ANON_REQ_TYPE_TIME`]) to a gateway, by the path
//!   its beacon or advert came by, with an 8-byte nonce; the gateway asks the server
//!   (`GET /time/<nonce>`) and returns the token over that nonce. The answer is fresh, so it may
//!   move the clock back too. The server signed its time somewhere between the request and the
//!   answer: the node sets `t + RTT/2`, off by RTT/2 at most, and takes no answer after
//!   [`MAX_RTT_MS`].
//!
//! Once the clock is right ([`Clock::Trusted`]) a token is taken only if it is within what the
//! clock may have drifted since ([`window_ms`]): a stolen server key can't move it far.
//!
//! The request's data, after upstream's `[tag u32][kind u8]`: `[reply path_len u8][reply path]
//! [nonce 8]`; the answer after the reflected tag: `[status u8]` and for [`status::READY`] the
//! token. The beacon's GRP_DATA: `[data type u16 = BEACON_DATA_TYPE][len u8][gateway key
//! prefix 6][token 85]`. The token: `[ver u8 = 1][unix_ms u64][nonce 8][key_id 4][Ed25519 sig 64]`,
//! signed over `"meshcore-time/1" | ver | unix_ms | nonce`; `key_id` is the start of the
//! SHA-256 of the server's public key.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::advert::AdvertData;
use crate::crypto::{self, Secret};
use crate::identity::{Identity, LocalIdentity};
use crate::mesh::{GroupChannel, MeshCtx};
use crate::packet::Packet;
use crate::util::{epoch_to_datetime, to_hex};
use crate::{MAX_PATH_SIZE, PUB_KEY_SIZE, SIGNATURE_SIZE};

/// ANON_REQ kind of a time request, next to [`crate::fwshare`]'s.
pub const ANON_REQ_TYPE_TIME: u8 = 0x1E;
/// Advert feat1 bit of a gateway (`time.serve on`), from the top like this firmware's others.
pub const FEAT1_TIME_SERVER: u16 = 0x4000;
/// GRP_DATA data type of a beacon.
pub const BEACON_DATA_TYPE: u16 = 0xFF01;
/// The channel beacons go in: a hashtag channel, its key the start of the SHA-256 of the name.
/// Its key is no secret, the server's signature is what counts.
pub const CHANNEL_NAME: &str = "#time";
pub const NONCE_LEN: usize = 8;
pub const TOKEN_LEN: usize = 85;
const TOKEN_VERSION: u8 = 1;
const SIG_CONTEXT: &[u8] = b"meshcore-time/1";
/// The start of the gateway's key in a beacon: the node finds its advert by it.
pub const GATEWAY_PREFIX: usize = 6;
const BEACON_LEN: usize = 3 + GATEWAY_PREFIX + TOKEN_LEN;

/// The time server's keys (nw.ommesh.ru), the current one first; a new one is added here a
/// release before the server signs with it. `set time.key` replaces them on one node.
pub const TIME_KEYS: [[u8; PUB_KEY_SIZE]; 1] = [[
    0x98, 0xe9, 0x6c, 0x7e, 0x4b, 0xa3, 0x76, 0xea, 0xa8, 0x19, 0xd9, 0xac, 0xc4, 0x6a, 0xc4, 0x30, 0x6a, 0xe0,
    0xf9, 0xdb, 0x9e, 0xbd, 0x7d, 0x7c, 0x01, 0x3d, 0xd7, 0xc2, 0xf1, 0xfe, 0x65, 0x80,
]];

/// Answer statuses.
pub mod status {
    /// The token follows.
    pub const READY: u8 = 0;
    /// The gateway can't reach the server now, or the server refused.
    pub const UNAVAILABLE: u8 = 1;
    /// The gateway has too many requests at once: `retry_secs u16` follows.
    pub const BUSY: u8 = 2;
}

/// An answer coming later than this is not taken: it is off by up to half of it.
pub const MAX_RTT_MS: u64 = 30_000;
/// An answer slower than this, and another gateway is asked too: the quicker one counts.
const SECOND_GATEWAY_RTT_MS: u64 = 10_000;
/// A beacon this far behind the clock: the clock may run ahead, a request settles it.
pub const AHEAD_MS: u64 = 60_000;
/// ... at most this often (played beacons must not make every node ask).
const AHEAD_EVERY_MS: u64 = 6 * 3600 * 1000;
/// After a boot a node listens this long for a beacon before it asks, plus 1..10 minutes by its
/// key, so after a power cut in town the nodes don't ask all at once.
const BOOT_WAIT_MS: u64 = 15 * 60 * 1000;
/// A failed request is tried again after these.
const RETRY_MS: [u64; 3] = [5 * 60 * 1000, 15 * 60 * 1000, 60 * 60 * 1000];
/// A gateway that failed is left alone this long (unless asked by hand).
const GATEWAY_REST_MS: u64 = 60 * 60 * 1000;
/// A beacon moves the clock only when it is at least this much ahead.
const MIN_STEP_MS: u64 = 250;
/// A beacon's lag is corrected by the air time of this many transmissions at most: the path is
/// not signed, longer forged ones move the clock no further.
const MAX_CORRECTED_HOPS: usize = 8;
const MAX_GATEWAYS: usize = 4;

/// How far a trusted clock may have drifted `elapsed_ms` after it was set: 30 s, plus 0.5 %
/// (a clock in light sleep runs on an RC oscillator and drifts more than a crystal's).
pub fn window_ms(elapsed_ms: u64) -> u64 {
    30_000 + elapsed_ms / 200
}

/// The start of the SHA-256 of a server key: which key signed a token.
pub fn key_id(key: &[u8; PUB_KEY_SIZE]) -> [u8; 4] {
    let mut id = [0u8; 4];
    crypto::sha256(&mut id, &[key]);
    id
}

/// The server's signed time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    pub unix_ms: u64,
    pub nonce: [u8; NONCE_LEN],
    pub key_id: [u8; 4],
    pub sig: [u8; SIGNATURE_SIZE],
}

impl Token {
    pub fn parse(b: &[u8]) -> Option<Self> {
        let b = b.get(..TOKEN_LEN)?;
        (b[0] == TOKEN_VERSION).then(|| Self {
            unix_ms: u64::from_le_bytes(b[1..9].try_into().unwrap()),
            nonce: b[9..17].try_into().unwrap(),
            key_id: b[17..21].try_into().unwrap(),
            sig: b[21..85].try_into().unwrap(),
        })
    }

    pub fn to_bytes(&self) -> [u8; TOKEN_LEN] {
        let mut b = [0u8; TOKEN_LEN];
        b[0] = TOKEN_VERSION;
        b[1..9].copy_from_slice(&self.unix_ms.to_le_bytes());
        b[9..17].copy_from_slice(&self.nonce);
        b[17..21].copy_from_slice(&self.key_id);
        b[21..].copy_from_slice(&self.sig);
        b
    }

    fn message(unix_ms: u64, nonce: &[u8; NONCE_LEN]) -> [u8; SIG_CONTEXT.len() + 1 + 8 + NONCE_LEN] {
        let mut m = [0u8; SIG_CONTEXT.len() + 1 + 8 + NONCE_LEN];
        let n = SIG_CONTEXT.len();
        m[..n].copy_from_slice(SIG_CONTEXT);
        m[n] = TOKEN_VERSION;
        m[n + 1..n + 9].copy_from_slice(&unix_ms.to_le_bytes());
        m[n + 9..].copy_from_slice(nonce);
        m
    }

    /// What the server makes (tests and the simulator stand in for it).
    pub fn signed(key: &LocalIdentity, unix_ms: u64, nonce: [u8; NONCE_LEN]) -> Self {
        let sig = key.sign(&Self::message(unix_ms, &nonce));
        Self { unix_ms, nonce, key_id: key_id(key.pub_key()), sig }
    }

    /// Signed by one of `keys`.
    pub fn verify(&self, keys: &[[u8; PUB_KEY_SIZE]]) -> bool {
        let m = Self::message(self.unix_ms, &self.nonce);
        keys.iter().any(|k| key_id(k) == self.key_id && Identity::new(*k).verify(&self.sig, &m))
    }
}

/// The channel beacons go in.
pub fn channel() -> GroupChannel {
    let mut secret = [0u8; PUB_KEY_SIZE];
    crypto::sha256(&mut secret[..16], &[CHANNEL_NAME.as_bytes()]);
    let mut hash = [0u8; 1];
    crypto::sha256(&mut hash, &[&secret[..16]]);
    GroupChannel { hash: hash[0], secret }
}

/// A beacon's GRP_DATA data.
pub fn beacon(gateway: &Identity, token: &Token) -> Vec<u8> {
    let mut b = Vec::with_capacity(BEACON_LEN);
    b.extend_from_slice(&BEACON_DATA_TYPE.to_le_bytes());
    b.push((GATEWAY_PREFIX + TOKEN_LEN) as u8);
    b.extend_from_slice(&gateway.pub_key[..GATEWAY_PREFIX]);
    b.extend_from_slice(&token.to_bytes());
    b
}

/// The gateway's key prefix and the token of a beacon (unchecked).
pub fn parse_beacon(data: &[u8]) -> Option<([u8; GATEWAY_PREFIX], Token)> {
    let data = data.get(..BEACON_LEN)?;
    if u16::from_le_bytes([data[0], data[1]]) != BEACON_DATA_TYPE || data[2] as usize != GATEWAY_PREFIX + TOKEN_LEN {
        return None;
    }
    Some((data[3..3 + GATEWAY_PREFIX].try_into().unwrap(), Token::parse(&data[3 + GATEWAY_PREFIX..])?))
}

/// What a node does about network time (`time.sync`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    /// Nothing: the clock is set by hand, by the phone, SNTP or GPS only.
    Off,
    /// Takes beacons; sends nothing.
    #[default]
    Listen,
    /// Takes beacons and asks gateways when they don't settle it.
    Auto,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Listen => "listen",
            Mode::Auto => "auto",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "off" | "0" => Some(Mode::Off),
            "listen" | "on" | "1" => Some(Mode::Listen),
            "auto" | "2" => Some(Mode::Auto),
            _ => None,
        }
    }

    pub fn code(self) -> u8 {
        self as u8
    }

    pub fn from_code(v: u8) -> Self {
        match v {
            0 => Mode::Off,
            2 => Mode::Auto,
            _ => Mode::Listen,
        }
    }
}

/// How far the clock is to be believed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clock {
    /// Nothing set it since boot (or only what was saved before it).
    Unknown,
    /// Set by hand, by the phone or by SNTP: any signed time may correct it.
    Approx,
    /// Signed time or GPS.
    Trusted,
}

/// Where a trusted clock came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// A gateway's beacon, `hops` repeaters away.
    Beacon { gateway: [u8; GATEWAY_PREFIX], hops: u8 },
    /// A gateway's answer to this node's request.
    Answer { gateway: [u8; GATEWAY_PREFIX], hops: u8, rtt_ms: u32 },
    /// The time server over this node's own link (a gateway's own clock).
    Server { rtt_ms: u32 },
    Gps,
}

/// The pseudo peer index of the gateway asked (its answer is decrypted like a known peer's).
pub const TIME_PEER: usize = usize::MAX - 1;

#[derive(Clone, Debug)]
struct Gateway {
    prefix: [u8; GATEWAY_PREFIX],
    /// From its advert: needed to ask it.
    id: Option<Identity>,
    /// How its last beacon or advert came here: path_len and the hashes, from it to this node.
    route: Option<(u8, [u8; MAX_PATH_SIZE])>,
    heard: u64,
    /// When it last failed to answer.
    failed: Option<u64>,
}

impl Gateway {
    fn hops(&self) -> usize {
        self.route.map_or(usize::MAX, |(len, _)| (len & 63) as usize)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Why {
    Boot,
    Ahead,
    Hand,
}

struct Pending {
    gateway: Identity,
    secret: Secret,
    tag: u32,
    nonce: [u8; NONCE_LEN],
    sent: u64,
    hops: u8,
    why: Why,
}

/// A node's network time: what it believes, what it heard, what it asked.
pub struct TimeSync {
    pub mode: Mode,
    keys: Vec<[u8; PUB_KEY_SIZE]>,
    clock: Clock,
    /// When (uptime, ms) the clock was last trusted.
    trusted_at: u64,
    /// The last trusted setting: where from, when (uptime), how far off it may be (ms).
    last: Option<(Source, u64, u32)>,
    /// The last time the node refused or failed, for `time.status`.
    note: String,
    gateways: Vec<Gateway>,
    pending: Option<Pending>,
    /// When to ask, and why.
    ask_at: Option<(u64, Why)>,
    /// Failed rounds in a row.
    retries: usize,
    /// Gateways asked in this round.
    tried: Vec<[u8; GATEWAY_PREFIX]>,
    /// The quickest answer of this round.
    best_rtt: Option<u64>,
    ahead_asked: Option<u64>,
    beacon_heard: Option<u64>,
    /// How far this clock was ahead of the last beacon's time (ms; behind if negative).
    beacon_ahead_ms: i64,
    /// A gateway answered that [`Self::take_remembered`] has not handed out yet.
    remember_changed: bool,
}

/// The time server, as a gateway's firmware reaches it.
pub trait TimeFetch {
    /// Ask the server for its token over `nonce`; false if it can't now (no link, busy).
    fn start(&mut self, nonce: [u8; NONCE_LEN]) -> bool;
    /// A fetch that ended: its nonce and the token, none if it failed.
    fn take(&mut self) -> Option<([u8; NONCE_LEN], Option<[u8; TOKEN_LEN]>)>;
}

impl TimeSync {
    pub fn new(mode: Mode, keys: &[[u8; PUB_KEY_SIZE]]) -> Self {
        let mut t = Self {
            mode,
            keys: Vec::new(),
            clock: Clock::Unknown,
            trusted_at: 0,
            last: None,
            note: String::new(),
            gateways: Vec::new(),
            pending: None,
            ask_at: None,
            retries: 0,
            tried: Vec::new(),
            best_rtt: None,
            ahead_asked: None,
            beacon_heard: None,
            beacon_ahead_ms: 0,
            remember_changed: false,
        };
        t.set_keys(keys);
        t
    }

    /// The server keys to check against: `keys`, or the built-in ones if empty.
    pub fn set_keys(&mut self, keys: &[[u8; PUB_KEY_SIZE]]) {
        self.keys = if keys.is_empty() { TIME_KEYS.to_vec() } else { keys.to_vec() };
    }

    /// After boot, or when the mode changes: a node that asks plans its first request.
    pub fn start(&mut self, ctx: &MeshCtx) {
        self.pending = None;
        self.ask_at = None;
        if self.mode == Mode::Auto && self.clock != Clock::Trusted {
            // a gateway kept from before is asked without waiting for a beacon
            let wait = if self.gateways.iter().any(|g| g.id.is_some()) { 0 } else { BOOT_WAIT_MS };
            let spread = u32::from_le_bytes(ctx.self_id.pub_key()[..4].try_into().unwrap()) as u64 % (9 * 60 * 1000);
            self.begin_round(ctx.now + wait + 60 * 1000 + spread, Why::Boot);
        }
    }

    pub fn set_mode(&mut self, ctx: &MeshCtx, mode: Mode) {
        self.mode = mode;
        self.start(ctx);
    }

    pub fn clock(&self) -> Clock {
        self.clock
    }

    /// Something else set the clock: by hand, the phone or SNTP ([`Clock::Approx`]), or GPS.
    pub fn note_set(&mut self, ctx: &MeshCtx, clock: Clock, source: Option<Source>) {
        self.clock = clock;
        if clock == Clock::Trusted {
            self.trusted_at = ctx.now;
            self.last = source.map(|s| (s, ctx.now, 1000));
            if matches!(self.ask_at, Some((_, Why::Boot))) {
                self.ask_at = None;
            }
        }
    }

    /// Set the clock to `est_ms` from a trusted source, if the clock may have drifted that far.
    fn apply(&mut self, ctx: &mut MeshCtx, est_ms: u64, source: Source, err_ms: u32) -> bool {
        let delta = est_ms as i64 - ctx.current_time_ms() as i64;
        if self.clock == Clock::Trusted {
            let window = window_ms(ctx.now - self.trusted_at);
            if delta.unsigned_abs() > window {
                self.note = format!(
                    "refused {} ({} s off; this clock may have drifted {} s)",
                    source_name(&source),
                    delta / 1000,
                    window / 1000
                );
                return false;
            }
        }
        ctx.set_current_time_ms(est_ms);
        self.clock = Clock::Trusted;
        self.trusted_at = ctx.now;
        self.last = Some((source, ctx.now, err_ms));
        self.note.clear();
        self.retries = 0;
        if matches!(self.ask_at, Some((_, Why::Boot))) {
            self.ask_at = None;
        }
        true
    }

    /// A gateway's own time, fetched from the server over its link `rtt_ms` long.
    pub fn on_server_token(&mut self, ctx: &mut MeshCtx, token: &Token, rtt_ms: u64) -> bool {
        token.verify(&self.keys)
            && self.apply(ctx, token.unix_ms + rtt_ms / 2, Source::Server { rtt_ms: rtt_ms as u32 }, (rtt_ms / 2) as u32)
    }

    fn gateway_mut(&mut self, prefix: &[u8; GATEWAY_PREFIX], now: u64) -> &mut Gateway {
        if let Some(i) = self.gateways.iter().position(|g| g.prefix == *prefix) {
            return &mut self.gateways[i];
        }
        if self.gateways.len() >= MAX_GATEWAYS {
            // the one heard least lately makes room
            let i = (0..self.gateways.len()).min_by_key(|&i| self.gateways[i].heard).unwrap();
            self.gateways.remove(i);
        }
        self.gateways.push(Gateway { prefix: *prefix, id: None, route: None, heard: now, failed: None });
        self.gateways.last_mut().unwrap()
    }

    fn note_route(&mut self, ctx: &MeshCtx, prefix: &[u8; GATEWAY_PREFIX], pkt: &Packet) {
        let mut path = [0u8; MAX_PATH_SIZE];
        let route = if pkt.is_route_flood() {
            let p = pkt.path();
            path[..p.len()].copy_from_slice(p);
            (pkt.path_len, path)
        } else {
            (0, path) // zero-hop: a neighbour
        };
        let g = self.gateway_mut(prefix, ctx.now);
        g.route = Some(route);
        g.heard = ctx.now;
    }

    /// Every advert heard: a gateway's tells its key and a way to it.
    pub fn on_advert(&mut self, ctx: &MeshCtx, pkt: &Packet, id: &Identity, app_data: &[u8]) {
        if self.mode == Mode::Off || AdvertData::parse(app_data).is_none_or(|a| a.feat1 & FEAT1_TIME_SERVER == 0) {
            return;
        }
        let prefix: [u8; GATEWAY_PREFIX] = id.pub_key[..GATEWAY_PREFIX].try_into().unwrap();
        self.note_route(ctx, &prefix, pkt);
        self.gateway_mut(&prefix, ctx.now).id = Some(*id);
    }

    /// A GRP_DATA in [`channel`]: the clock moves forward by a beacon, never back. Returns
    /// whether it was a beacon signed by the server (a gateway puts off its own then).
    pub fn on_beacon(&mut self, ctx: &mut MeshCtx, pkt: &Packet, data: &[u8]) -> bool {
        let Some((prefix, token)) = parse_beacon(data) else {
            return false;
        };
        if !token.verify(&self.keys) {
            return false;
        }
        self.beacon_heard = Some(ctx.now);
        if self.mode == Mode::Off {
            return true;
        }
        self.note_route(ctx, &prefix, pkt);
        // the lag of each transmission's air time; the repeaters' wait before each is not known
        let hops = pkt.path_hash_count();
        let air = ctx.radio.est_airtime_ms(pkt.raw_length()) as u64;
        let est = token.unix_ms + air * (hops + 1).min(MAX_CORRECTED_HOPS) as u64;
        let local = ctx.current_time_ms();
        self.beacon_ahead_ms = local as i64 - est as i64;
        if est >= local + MIN_STEP_MS {
            self.apply(ctx, est, Source::Beacon { gateway: prefix, hops: hops as u8 }, 0);
        } else if local > est + AHEAD_MS {
            if self.mode == Mode::Auto
                && self.pending.is_none()
                && self.ahead_asked.is_none_or(|t| ctx.now - t >= AHEAD_EVERY_MS)
            {
                self.ahead_asked = Some(ctx.now);
                self.note = format!("a beacon is {} s behind this clock: asking a gateway", (local - est) / 1000);
                self.begin_round(ctx.now, Why::Ahead);
            }
        } else if self.clock == Clock::Unknown {
            // at most a minute ahead of the server's time: right enough to call approximate
            self.clock = Clock::Approx;
        }
        true
    }

    fn begin_round(&mut self, now: u64, why: Why) {
        self.tried.clear();
        self.best_rtt = None;
        self.ask_at = Some((now, why));
    }

    /// The gateway asked, while its answer is awaited: its RESPONSE is decrypted as a peer's.
    pub fn peer(&self) -> Option<(&Identity, &Secret)> {
        self.pending.as_ref().map(|p| (&p.gateway, &p.secret))
    }

    /// `time sync`: ask now.
    pub fn ask_now(&mut self, ctx: &mut MeshCtx) -> String {
        if let Some(p) = &self.pending {
            return format!("asking {} already", to_hex(&p.gateway.pub_key[..4]));
        }
        self.begin_round(ctx.now, Why::Hand);
        self.ask_at = None;
        match self.ask(ctx, Why::Hand) {
            Ok(text) => text,
            Err(e) => {
                self.note = e.clone();
                format!("Error: {e}")
            }
        }
    }

    /// The best gateway not asked in this round: fewest hops, then heard last.
    fn pick(&self, now: u64, why: Why) -> Option<usize> {
        (0..self.gateways.len())
            .filter(|&i| {
                let g = &self.gateways[i];
                g.id.is_some()
                    && g.route.is_some()
                    && !self.tried.contains(&g.prefix)
                    && (why == Why::Hand || g.failed.is_none_or(|t| now - t >= GATEWAY_REST_MS))
            })
            .min_by_key(|&i| (self.gateways[i].hops(), u64::MAX - self.gateways[i].heard))
    }

    fn ask(&mut self, ctx: &mut MeshCtx, why: Why) -> Result<String, String> {
        let i = self.pick(ctx.now, why).ok_or_else(|| String::from("no gateway heard yet (time.serve on, with internet)"))?;
        let g = &self.gateways[i];
        let id = g.id.unwrap();
        let (path_len, route) = g.route.unwrap();
        let secret = ctx.self_id.calc_shared_secret(&id.pub_key).ok_or_else(|| String::from("bad gateway key"))?;
        let tag = ctx.rng.next_u32();
        let mut nonce = [0u8; NONCE_LEN];
        ctx.rng.fill(&mut nonce);

        // the answer comes back the way the beacon or advert came; the request goes the other way
        let n = Packet::path_bytes_for(path_len).min(MAX_PATH_SIZE);
        let mut data = tag.to_le_bytes().to_vec();
        data.extend_from_slice(&[ANON_REQ_TYPE_TIME, path_len]);
        data.extend_from_slice(&route[..n]);
        data.extend_from_slice(&nonce);
        let pkt = ctx.create_anon_datagram(&id, &secret, &data).ok_or_else(|| String::from("request too long"))?;
        let hops = (path_len & 63) as usize;
        if hops == 0 {
            ctx.send_zero_hop(pkt, 0);
        } else {
            let size = ((path_len >> 6) + 1) as usize;
            let mut out = [0u8; MAX_PATH_SIZE];
            for (k, hash) in route[..n].chunks_exact(size).rev().enumerate() {
                out[k * size..(k + 1) * size].copy_from_slice(hash);
            }
            ctx.send_direct(pkt, path_len, &out[..n], 0);
        }
        self.tried.push(g.prefix);
        self.pending = Some(Pending { gateway: id, secret, tag, nonce, sent: ctx.now, hops: hops as u8, why });
        Ok(format!("asking {} ({hops} hops)", to_hex(&id.pub_key[..4])))
    }

    /// The asked gateway's RESPONSE; false if it is not the awaited one.
    pub fn on_answer(&mut self, ctx: &mut MeshCtx, data: &[u8]) -> bool {
        let Some(p) = &self.pending else {
            return false;
        };
        if data.len() < 5 || u32::from_le_bytes(data[..4].try_into().unwrap()) != p.tag {
            return false;
        }
        let p = self.pending.take().unwrap();
        let prefix: [u8; GATEWAY_PREFIX] = p.gateway.pub_key[..GATEWAY_PREFIX].try_into().unwrap();
        let name = to_hex(&prefix[..4]);
        let rtt = ctx.now - p.sent;
        let token = match data[4] {
            status::READY => Token::parse(&data[5..]),
            status::BUSY => {
                self.failed(ctx, &p, format!("{name} is busy"));
                return true;
            }
            _ => {
                self.failed(ctx, &p, format!("{name} can't reach the time server"));
                return true;
            }
        };
        let Some(token) = token.filter(|t| t.nonce == p.nonce && t.verify(&self.keys)) else {
            self.failed(ctx, &p, format!("{name} sent a token the server did not sign for this request"));
            return true;
        };
        if rtt > MAX_RTT_MS {
            self.failed(ctx, &p, format!("{name} answered after {} s", rtt / 1000));
            return true;
        }
        if self.best_rtt.is_some_and(|best| best <= rtt) {
            return true; // the first gateway's answer was quicker
        }
        let source = Source::Answer { gateway: prefix, hops: p.hops, rtt_ms: rtt as u32 };
        if self.apply(ctx, token.unix_ms + rtt / 2, source, (rtt / 2) as u32) {
            self.best_rtt = Some(rtt);
            self.remember_changed = true;
            if rtt > SECOND_GATEWAY_RTT_MS && self.pick(ctx.now, p.why).is_some() {
                self.ask_at = Some((ctx.now, p.why));
            }
        }
        true
    }

    fn failed(&mut self, ctx: &MeshCtx, p: &Pending, why_text: String) {
        if let Some(g) = self.gateways.iter_mut().find(|g| g.id == Some(p.gateway)) {
            g.failed = Some(ctx.now);
        }
        self.note = why_text;
        if self.best_rtt.is_some() {
            return; // one answer of this round came
        }
        if self.pick(ctx.now, p.why).is_some() {
            self.ask_at = Some((ctx.now, p.why)); // the next gateway
        } else {
            self.retry_later(ctx.now, p.why);
        }
    }

    /// A round without an answer: another later, a few times (one asked by hand is not).
    fn retry_later(&mut self, now: u64, why: Why) {
        if why != Why::Hand && self.retries < RETRY_MS.len() {
            self.ask_at = Some((now + RETRY_MS[self.retries], why));
            self.retries += 1;
            self.tried.clear();
        }
    }

    /// Timers: an answer that never came, a request due.
    pub fn poll(&mut self, ctx: &mut MeshCtx) {
        if let Some(p) = self.pending.take_if(|p| ctx.now >= p.sent + MAX_RTT_MS) {
            self.failed(ctx, &p, format!("no answer from {}", to_hex(&p.gateway.pub_key[..4])));
        }
        if self.pending.is_none()
            && let Some((at, why)) = self.ask_at
            && ctx.now >= at
        {
            self.ask_at = None;
            // planned when it was needed; maybe it is not any more
            if why != Why::Hand && (self.mode != Mode::Auto || (why == Why::Boot && self.clock == Clock::Trusted)) {
                return;
            }
            if let Err(e) = self.ask(ctx, why) {
                self.note = e;
                if self.best_rtt.is_none() {
                    self.retry_later(ctx.now, why);
                }
            }
        }
    }

    /// The gateway that last answered, to keep across a reboot (`<key>/<path_len>/<path>`, hex),
    /// when it changed since last taken.
    pub fn take_remembered(&mut self) -> Option<String> {
        if !self.remember_changed {
            return None;
        }
        self.remember_changed = false;
        let (Source::Answer { gateway, .. }, _, _) = self.last? else {
            return None;
        };
        let g = self.gateways.iter().find(|g| g.prefix == gateway)?;
        let (path_len, path) = g.route?;
        let n = Packet::path_bytes_for(path_len).min(MAX_PATH_SIZE);
        Some(format!("{}/{}/{}", to_hex(&g.id?.pub_key), to_hex(&[path_len]), to_hex(&path[..n])))
    }

    /// A gateway kept from before the reboot ([`Self::take_remembered`]): asked first after it.
    pub fn remember(&mut self, text: &str, now: u64) {
        let mut parts = text.trim().split('/');
        let (Some(key), Some(len), path) = (parts.next(), parts.next(), parts.next().unwrap_or("")) else {
            return;
        };
        let (mut k, mut l) = ([0u8; PUB_KEY_SIZE], [0u8; 1]);
        if !crate::util::from_hex(&mut k, key) || !crate::util::from_hex(&mut l, len) || !Packet::is_valid_path_len(l[0]) {
            return;
        }
        let mut route = [0u8; MAX_PATH_SIZE];
        let n = Packet::path_bytes_for(l[0]);
        if n > MAX_PATH_SIZE || path.len() != n * 2 || !crate::util::from_hex(&mut route[..n], path) {
            return;
        }
        let id = Identity::new(k);
        let g = self.gateway_mut(&k[..GATEWAY_PREFIX].try_into().unwrap(), now);
        g.id = Some(id);
        g.route = Some((l[0], route));
    }

    /// A key of a gateway heard of by its prefix only (a companion finds it in its contacts).
    pub fn learn(&mut self, id: &Identity) {
        if let Some(g) = self.gateways.iter_mut().find(|g| g.id.is_none() && id.pub_key.starts_with(&g.prefix)) {
            g.id = Some(*id);
        }
    }

    /// Gateways heard of by their prefix only.
    pub fn unknown_gateways(&self) -> impl Iterator<Item = &[u8; GATEWAY_PREFIX]> {
        self.gateways.iter().filter(|g| g.id.is_none()).map(|g| &g.prefix)
    }

    pub fn timer(&self) -> Option<u64> {
        [self.pending.as_ref().map(|p| p.sent + MAX_RTT_MS), self.ask_at.map(|(at, _)| at)].into_iter().flatten().min()
    }

    /// `get time.status`.
    pub fn status(&self, ctx: &MeshCtx) -> String {
        let (y, mo, d, h, mi, s) = epoch_to_datetime(ctx.current_time());
        let now = format!("{y}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02} UTC");
        let mut text = match (self.clock, &self.last) {
            (Clock::Trusted, Some((source, at, err))) => {
                let ago = (ctx.now - at) / 60_000;
                let err = if *err >= 100 { format!(" ±{}.{} s", err / 1000, err % 1000 / 100) } else { String::new() };
                format!("trusted {now}, {}{err}, {ago} min ago", source_name(source))
            }
            (Clock::Trusted, None) => format!("trusted {now}"),
            (Clock::Approx, _) => format!("approximate {now} (set by hand, by the phone or SNTP)"),
            (Clock::Unknown, _) => format!("unknown {now} (not set since boot)"),
        };
        text.push_str(&format!("; time.sync {}", self.mode.name()));
        if let Some(p) = &self.pending {
            text.push_str(&format!("; asking {}", to_hex(&p.gateway.pub_key[..4])));
        } else if let Some((at, _)) = self.ask_at {
            text.push_str(&format!("; asks in {} min", at.saturating_sub(ctx.now).div_ceil(60_000)));
        }
        let known = self.gateways.iter().filter(|g| g.id.is_some()).count();
        if self.mode != Mode::Off {
            text.push_str(&format!("; {known} gateway{}", if known == 1 { "" } else { "s" }));
        }
        if let Some(t) = self.beacon_heard {
            let a = self.beacon_ahead_ms;
            let side = if a >= 0 { "ahead of" } else { "behind" };
            text.push_str(&format!(
                "; last beacon {} min ago, this clock {}.{} s {side} it",
                (ctx.now - t) / 60_000,
                a.unsigned_abs() / 1000,
                a.unsigned_abs() % 1000 / 100
            ));
        }
        if !self.note.is_empty() {
            text.push_str("; ");
            text.push_str(&self.note);
        }
        text
    }
}

fn source_name(s: &Source) -> String {
    match s {
        Source::Beacon { gateway, hops } => format!("beacon from {} {hops} hops", to_hex(&gateway[..4])),
        Source::Answer { gateway, hops, rtt_ms } => {
            format!("answer from {} {hops} hops rtt {}.{} s", to_hex(&gateway[..4]), rtt_ms / 1000, rtt_ms % 1000 / 100)
        }
        Source::Server { rtt_ms } => format!("time server rtt {rtt_ms} ms"),
        Source::Gps => "GPS".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> LocalIdentity {
        LocalIdentity::from_seed(&[7; 32])
    }

    #[test]
    fn token_round_trip_and_signature() {
        let key = server();
        let t = Token::signed(&key, 1_763_200_000_123, [1, 2, 3, 4, 5, 6, 7, 8]);
        let b = t.to_bytes();
        assert_eq!(b.len(), TOKEN_LEN);
        assert_eq!(b[0], 1);
        assert_eq!(Token::parse(&b), Some(t));
        assert!(t.verify(&[*key.pub_key()]));
        assert!(!t.verify(&TIME_KEYS), "another key");
        let mut forged = t;
        forged.unix_ms += 1;
        assert!(!forged.verify(&[*key.pub_key()]));
    }

    /// The same token as meshmap's `time.test.ts` makes: the server and the firmware agree.
    #[test]
    fn token_matches_the_servers_test_vector() {
        let key = LocalIdentity::from_seed(&[7; 32]);
        assert_eq!(to_hex(key.pub_key()).to_ascii_lowercase(), "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c");
        let t = Token::signed(&key, 1_792_800_000_000, [0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(
            to_hex(&t.to_bytes()).to_ascii_lowercase(),
            "010008356ba10100000001020304050607fe812c1256bf46c8ced2ca0f7364f921ef62023a056e6117392c6e860165ad3657a921d5cdf85c4866b3324a079a5a927907fccd217801cbefe09d5a1cd5300a4503000a"
        );
        assert!(t.verify(&[*key.pub_key()]));
    }

    #[test]
    fn key_id_is_the_fingerprint_start() {
        assert_eq!(key_id(&TIME_KEYS[0]), [0xbe, 0x02, 0x83, 0x3c]);
    }

    #[test]
    fn beacon_round_trip() {
        let key = server();
        let t = Token::signed(&key, 5, [9; 8]);
        let gw = Identity::new([0xAB; 32]);
        let b = beacon(&gw, &t);
        assert_eq!(b.len(), 94);
        assert_eq!(parse_beacon(&b), Some(([0xAB; 6], t)));
        let mut other = b.clone();
        other[0] ^= 1;
        assert_eq!(parse_beacon(&other), None, "another data type");
    }

    #[test]
    fn window_grows_with_time() {
        assert_eq!(window_ms(0), 30_000);
        assert_eq!(window_ms(24 * 3600 * 1000), 30_000 + 432_000);
    }
}
