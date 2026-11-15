//! A gateway's side of network time (`set time.serve on`, this firmware's extension, off by
//! default; see [`crate::timesync`]). The time server's token, fetched over the node's link
//! ([`Repeater::time_fetch`]), goes out as a beacon `time.beacon` times a day and as the answer to
//! each node that asks. The gateway signs nothing: the nodes check the server's signature. A beacon
//! another gateway sent within half a period stands for this one's, so a mesh with several gets
//! about as many beacons as one would send. Its own clock follows the tokens it fetches.

use super::*;
use crate::mesh::GroupChannel;
use crate::timesync::{self, NONCE_LEN, TOKEN_LEN, Token, status};

/// The first beacon, a while after boot (a power cut restarts the gateways too: their nodes
/// are waiting), plus up to a minute by the node's key.
const FIRST_BEACON_MS: u64 = 5 * 60_000;
/// After a failed fetch, or while the link is down.
const BEACON_RETRY_MS: u64 = 60_000;
/// An answer waits this long for its token; the asker gives up at `timesync::MAX_RTT_MS`.
const FETCH_WAIT_MS: u64 = 10_000;
/// Requests waiting for a token at once.
const MAX_WAITING: usize = 4;
/// What a busy gateway tells an asker to wait.
const BUSY_RETRY_SECS: u16 = 60;

/// A request waiting for its token.
struct Waiting {
    nonce: [u8; NONCE_LEN],
    to: Identity,
    secret: Secret,
    tag: u32,
    path_len: u8,
    path: [u8; MAX_PATH_SIZE],
    until: u64,
}

pub(super) struct TimeServe {
    waiting: Vec<Waiting>,
    /// The beacon's token being fetched: its nonce, and when it was asked for.
    beacon: Option<([u8; NONCE_LEN], u64)>,
    next_beacon: u64,
    /// When a beacon of another gateway was last heard.
    heard: Option<u64>,
    limiter: RateLimiter,
    /// While a token is awaited: when to look again.
    poll_at: Option<u64>,
}

impl TimeServe {
    pub(super) fn new() -> Self {
        Self { waiting: Vec::new(), beacon: None, next_beacon: 0, heard: None, limiter: RateLimiter::new(20, 60), poll_at: None }
    }
}

impl Repeater {
    fn beacon_period_ms(&self) -> u64 {
        24 * 3_600_000 / self.prefs.time_beacons.max(1) as u64
    }

    /// Up to a tenth of the period later, so gateways that started together drift apart.
    fn beacon_jitter_ms(&self, ctx: &mut MeshCtx) -> u64 {
        ctx.rng.next_u32() as u64 % (self.beacon_period_ms() / 10)
    }

    /// This node hands the server's time on (and can: it has a link).
    pub(super) fn time_serving(&self) -> bool {
        self.prefs.time_serve && self.time_fetch.is_some() && self.room.is_none()
    }

    pub(super) fn time_serve_start(&mut self, ctx: &MeshCtx) {
        let spread = u32::from_le_bytes(ctx.self_id.pub_key()[..4].try_into().unwrap()) as u64 % 60_000;
        self.timeserve.next_beacon = ctx.now + FIRST_BEACON_MS + spread;
    }

    /// A beacon heard; `signed` = by the server, from another gateway (this one's own come back
    /// as duplicates and are dropped before).
    pub(super) fn time_beacon_heard(&mut self, ctx: &mut MeshCtx) {
        self.timeserve.heard = Some(ctx.now);
        let half = self.beacon_period_ms() / 2;
        if self.timeserve.beacon.is_none() && self.timeserve.next_beacon < ctx.now + half {
            // it stands for this gateway's next one
            self.timeserve.next_beacon = ctx.now + self.beacon_period_ms() + self.beacon_jitter_ms(ctx);
        }
    }

    /// A TIME_REQ: `{reply-path-len}{reply-path}{nonce}`. Answered when the token comes (false
    /// here), or at once with a status if it can't be.
    pub(super) fn time_request(
        &mut self, ctx: &MeshCtx, tag: u32, sender: &Identity, secret: &Secret, data: &[u8], reply: &mut Vec<u8>,
    ) -> bool {
        if !self.time_serving() {
            return false;
        }
        let Some(&path_len) = data.first() else {
            return false;
        };
        if !Packet::is_valid_path_len(path_len) {
            return false;
        }
        let n = Packet::path_bytes_for(path_len);
        let Some(nonce) = data.get(1 + n..1 + n + NONCE_LEN) else {
            return false;
        };
        let mut path = [0u8; MAX_PATH_SIZE];
        path[..n].copy_from_slice(&data[1..1 + n]);
        self.reply_path = Some((path_len, path));
        reply.extend_from_slice(&tag.to_le_bytes());
        let nonce: [u8; NONCE_LEN] = nonce.try_into().unwrap();
        if !self.timeserve.limiter.allow(ctx.current_time()) || self.timeserve.waiting.len() >= MAX_WAITING {
            reply.push(status::BUSY);
            reply.extend_from_slice(&BUSY_RETRY_SECS.to_le_bytes());
            return true;
        }
        if !self.time_fetch.as_deref_mut().is_some_and(|f| f.start(nonce)) {
            reply.push(status::UNAVAILABLE);
            return true;
        }
        self.reply_path = None;
        reply.clear();
        self.timeserve.waiting.push(Waiting {
            nonce,
            to: *sender,
            secret: *secret,
            tag,
            path_len,
            path,
            until: ctx.now + FETCH_WAIT_MS,
        });
        false
    }

    fn time_answer(&mut self, ctx: &mut MeshCtx, w: &Waiting, token: Option<&[u8; TOKEN_LEN]>) {
        let mut reply = w.tag.to_le_bytes().to_vec();
        match token {
            Some(t) => {
                reply.push(status::READY);
                reply.extend_from_slice(t);
            }
            None => reply.push(status::UNAVAILABLE),
        }
        if let Some(resp) = ctx.create_datagram(PAYLOAD_TYPE_RESPONSE, &w.to, &w.secret, &reply) {
            ctx.send_direct(resp, w.path_len, &w.path, 0);
        }
    }

    fn send_beacon(&mut self, ctx: &mut MeshCtx, token: &Token) {
        let data = timesync::beacon(&ctx.self_id.id, token);
        let channel: GroupChannel = timesync::channel();
        if let Some(pkt) = ctx.create_group_datagram(PAYLOAD_TYPE_GRP_DATA, &channel, &data) {
            let scope = self.default_scope();
            self.send_flood_scoped(ctx, pkt, scope, 0, self.prefs.path_hash_mode as usize + 1);
        }
    }

    pub(super) fn time_serve_loop(&mut self, ctx: &mut MeshCtx) {
        while let Some((nonce, token)) = self.time_fetch.as_deref_mut().and_then(|f| f.take()) {
            if let Some((beacon_nonce, asked)) = self.timeserve.beacon
                && beacon_nonce == nonce
            {
                self.timeserve.beacon = None;
                match token.as_ref().and_then(|t| Token::parse(t)) {
                    Some(t) if t.nonce == nonce && t.verify(&self.time_keys()) => {
                        // its own clock first: the beacon carries the time as fetched
                        self.timesync.on_server_token(ctx, &t, ctx.now - asked);
                        self.send_beacon(ctx, &t);
                        self.timeserve.next_beacon = ctx.now + self.beacon_period_ms() + self.beacon_jitter_ms(ctx);
                    }
                    _ => self.timeserve.next_beacon = ctx.now + BEACON_RETRY_MS,
                }
                continue;
            }
            if let Some(i) = self.timeserve.waiting.iter().position(|w| w.nonce == nonce) {
                let w = self.timeserve.waiting.remove(i);
                self.time_answer(ctx, &w, token.as_ref());
            }
        }
        // a token that never came
        while let Some(i) = self.timeserve.waiting.iter().position(|w| ctx.now >= w.until) {
            let w = self.timeserve.waiting.remove(i);
            self.time_answer(ctx, &w, None);
        }
        if self.timeserve.beacon.is_some_and(|(_, asked)| ctx.now >= asked + FETCH_WAIT_MS) {
            self.timeserve.beacon = None;
            self.timeserve.next_beacon = ctx.now + BEACON_RETRY_MS;
        }
        if self.time_serving()
            && self.prefs.time_beacons > 0
            && self.timeserve.beacon.is_none()
            && ctx.now >= self.timeserve.next_beacon
        {
            let mut nonce = [0u8; NONCE_LEN];
            ctx.rng.fill(&mut nonce);
            if self.time_fetch.as_deref_mut().is_some_and(|f| f.start(nonce)) {
                self.timeserve.beacon = Some((nonce, ctx.now));
            } else {
                self.timeserve.next_beacon = ctx.now + BEACON_RETRY_MS;
            }
        }
        let awaited = !self.timeserve.waiting.is_empty() || self.timeserve.beacon.is_some();
        self.timeserve.poll_at = awaited.then_some(ctx.now + 200);
    }

    pub(super) fn time_serve_timer(&self) -> Option<u64> {
        if self.timeserve.poll_at.is_some() {
            return self.timeserve.poll_at;
        }
        (self.time_serving() && self.prefs.time_beacons > 0).then_some(self.timeserve.next_beacon)
    }

    /// The server keys this node checks against.
    pub(super) fn time_keys(&self) -> Vec<[u8; PUB_KEY_SIZE]> {
        if self.prefs.time_keys.is_empty() { timesync::TIME_KEYS.to_vec() } else { self.prefs.time_keys.clone() }
    }

    /// `get time.serve`'s detail.
    pub(super) fn time_serve_status(&self, ctx: &MeshCtx) -> String {
        if !self.prefs.time_serve {
            return "off".into();
        }
        if self.time_fetch.is_none() {
            return "on, but this board has no internet link".into();
        }
        let mut s = format!("on, {} beacons a day from {}", self.prefs.time_beacons, self.prefs.time_server);
        if self.prefs.time_beacons > 0 {
            let mins = self.timeserve.next_beacon.saturating_sub(ctx.now).div_ceil(60_000);
            s.push_str(&format!(", next in {mins} min"));
        }
        if let Some(t) = self.timeserve.heard {
            s.push_str(&format!(", another gateway's beacon {} min ago", (ctx.now - t) / 60_000));
        }
        s
    }
}

impl Repeater {
    /// `get time.*`.
    pub(super) fn handle_get_time(&self, ctx: &MeshCtx, cfg: &str) -> Option<String> {
        let p = &self.prefs;
        Some(if cfg.starts_with("time.status") {
            format!("> {}", self.timesync.status(ctx))
        } else if cfg.starts_with("time.sync") {
            format!("> {}", timesync::Mode::from_code(p.time_sync).name())
        } else if cfg.starts_with("time.server") {
            format!("> {}", p.time_server)
        } else if cfg.starts_with("time.serve") {
            format!("> {}", self.time_serve_status(ctx))
        } else if cfg.starts_with("time.beacon") {
            format!("> {}", p.time_beacons)
        } else if cfg.starts_with("time.key") {
            let ids: Vec<String> = self.time_keys().iter().map(|k| to_hex(&timesync::key_id(k))).collect();
            let whose = if p.time_keys.is_empty() { "built in (nw.ommesh.ru)" } else { "set by hand" };
            format!("> {} ({whose})", ids.join(", "))
        } else {
            return None;
        })
    }

    /// `set time.*`.
    pub(super) fn handle_set_time(&mut self, ctx: &mut MeshCtx, serial: bool, cfg: &str) -> Option<String> {
        if let Some(v) = cfg.strip_prefix("time.sync ") {
            let Some(mode) = timesync::Mode::parse(v.trim()) else {
                return Some("Error: off, listen or auto".into());
            };
            self.prefs.time_sync = mode.code();
            self.save_prefs();
            if self.room.is_none() {
                self.timesync.set_mode(ctx, mode);
            }
            return Some(
                match mode {
                    timesync::Mode::Off => "OK - the clock is set by hand, by SNTP or GPS only",
                    timesync::Mode::Listen => "OK - the clock follows the time server's beacons, nothing is sent",
                    timesync::Mode::Auto => "OK - the clock follows the beacons and asks a gateway when they don't settle it",
                }
                .into(),
            );
        }
        if let Some(v) = cfg.strip_prefix("time.server ") {
            let v = v.trim();
            if v.is_empty() || v.len() > 63 || v.contains(char::is_whitespace) {
                return Some("Error: a host name, or host:port".into());
            }
            self.prefs.time_server = v.into();
            self.save_prefs();
            return Some(format!("OK - time server {v}"));
        }
        if let Some(v) = cfg.strip_prefix("time.serve ") {
            let on = match v.trim() {
                "on" => true,
                "off" => false,
                _ => return Some("Error: on or off".into()),
            };
            let was = core::mem::replace(&mut self.prefs.time_serve, on);
            self.save_prefs();
            if on && !was {
                self.time_serve_start(ctx);
            }
            let no_wifi = if self.prefs.wifi_ssid.is_empty() {
                Some("not set up (set wifi.ssid)")
            } else if !self.prefs.wifi_enabled {
                Some("off (set wifi.enabled on)")
            } else {
                None
            };
            return Some(match (on, self.time_fetch.is_some(), no_wifi) {
                (true, false, _) => "OK - but this board has no internet link".into(),
                (true, true, Some(why)) => format!("OK - but Wi-Fi is {why}: no time to pass on yet"),
                (true, true, None) => format!("OK - the time of {} passed on: beacons, and answers to requests", self.prefs.time_server),
                (false, _, _) => "OK - the time server's time not passed on".into(),
            });
        }
        if let Some(v) = cfg.strip_prefix("time.beacon ") {
            let n = atoi(v.trim());
            if n > 24 {
                return Some("Error: beacons a day, 0 to 24".into());
            }
            self.prefs.time_beacons = n as u8;
            self.save_prefs();
            return Some(if n == 0 { "OK - no beacons, answers to requests only".into() } else { format!("OK - {n} beacons a day") });
        }
        if let Some(v) = cfg.strip_prefix("time.key ") {
            if !serial {
                return Some("Error: the time server's key is changed on the serial console only".into());
            }
            let keys = match v.trim() {
                "default" => Vec::new(),
                v => match crate::prefs::parse_keys(v) {
                    Some(k) if !k.is_empty() => k,
                    _ => return Some("Error: default, or public keys (64 hex digits each, comma-separated)".into()),
                },
            };
            self.prefs.time_keys = keys;
            self.timesync.set_keys(&self.prefs.time_keys);
            self.save_prefs();
            return Some(if self.prefs.time_keys.is_empty() { "OK - the built-in key".into() } else { "OK - time server key set".into() });
        }
        None
    }
}
