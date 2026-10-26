//! A repeater that updates itself (`set fw.auto on`, this firmware's extension, off by default).
//! With a working link to `fw.server` it asks its update store (see [`crate::fwshare`]) for a
//! release newer than the build it runs, for its board, role and channel. Without one (no Wi-Fi, or
//! the server out of reach), or when `fw.source` names a neighbour, it asks a neighbouring repeater
//! over LoRa instead, as a companion does (`fw_pull`): one that hands updates on (`fw.serve on`)
//! and so has the link this one lacks. That delta comes a piece at a time with pauses between, which
//! leave the channel to the others, and the node keeps repeating meanwhile. Either way the delta is
//! fed into its update session as an update over LoRa or BLE would be, and the node restarts into
//! it, on trial: an image that does not come up healthy is rolled back. Only with a pinned root,
//! since neither the server nor the neighbour is trusted. `fw update` does one such check now,
//! whatever `fw.auto` says.

use core::cmp::Reverse;

use super::*;
use crate::fwshare::{FW_PIECE, FwQuery, Loader, Offer, status};
use crate::ota::REQ_TYPE_OTA_COMMIT;

/// The first check, a while after boot (the node has heard its neighbours by then), then twice a
/// day; each node a little later than the next (by its key), so the repeaters of a mesh do not
/// restart all at once.
const FIRST_CHECK_MS: u64 = 10 * 60_000;
const CHECK_EVERY_MS: u64 = 12 * 3_600_000;
const RETRY_LATER_MS: u64 = 3_600_000;
/// While an update from someone else is under way.
const BUSY_RETRY_MS: u64 = 10 * 60_000;
/// Pieces fed per pass of the main loop: it keeps serving the radio meanwhile.
const PIECES_PER_PASS: usize = 8;
/// Over LoRa: a request and its answer take well under a second on air; past this it is asked again.
const ANSWER_WAIT_MS: u64 = 8_000;
/// Queries to one neighbour before the next is asked: most repeaters do not hand updates on.
const QUERY_TRIES: u8 = 2;
/// Requests for one piece before the pull gives up.
const PIECE_TRIES: u8 = 6;
/// Between pieces: a repeater's delta is tens of kilobytes, and the channel is everyone's. With the
/// request and its answer (about 1.1 s at SF7/62.5 kHz) a piece takes some 4 s, the pull a quarter
/// of the air near the two nodes: a 40 KB delta in about 20 minutes.
const PIECE_GAP_MS: u64 = 3_000;
/// Neighbours heard within this long are asked, the best heard first, at most this many per check.
const HEARD_RECENTLY_SECS: u32 = 2 * 86_400;
const MAX_ASKED: usize = 4;

/// [`Repeater::matching_peers`]' entry for the neighbour an update is pulled from (not a client).
pub(super) const FW_PEER: usize = usize::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Step {
    Sleep,
    /// From the store, the piece at `offset` of its file `id` next.
    Load { id: u32, offset: u32 },
    /// Over LoRa: waiting for the neighbour's answer to a query, or to the request for a piece.
    Ask,
    Get { offset: u32 },
}

pub(super) struct AutoUpdate {
    status: String,
    step: Step,
    at: u64,
    loader: Option<Loader>,
    version: String,
    /// `fw update`: one check that installs what it finds, whatever `fw.auto` says.
    once: bool,
    /// The neighbour asked over LoRa and the secret shared with it; the ones to ask if it does not
    /// answer (the best last); how many were asked this check.
    peer: Option<(Identity, Secret)>,
    others: Vec<Identity>,
    asked: u8,
    /// The store said the server can't be reached: that is why a neighbour is asked.
    server_down: bool,
    tag: u32,
    tries: u8,
    file_id: u32,
    /// Asked by hand: the neighbour should ask its server again rather than answer from what it knew.
    fresh: bool,
}

impl AutoUpdate {
    pub(super) fn new() -> Self {
        Self {
            status: String::new(),
            step: Step::Sleep,
            at: FIRST_CHECK_MS,
            loader: None,
            version: String::new(),
            once: false,
            peer: None,
            others: Vec::new(),
            asked: 0,
            server_down: false,
            tag: 0,
            tries: 0,
            file_id: 0,
            fresh: false,
        }
    }

    /// The neighbour whose answer is awaited, and the secret shared with it.
    pub(super) fn peer(&self) -> Option<&(Identity, Secret)> {
        self.peer.as_ref().filter(|_| matches!(self.step, Step::Ask | Step::Get { .. }))
    }
}

impl Repeater {
    /// Spread over an hour by the node's key.
    fn auto_jitter(ctx: &MeshCtx) -> u64 {
        let k = ctx.self_id.pub_key();
        u32::from_le_bytes([k[0], k[1], k[2], k[3]]) as u64 % 3_600_000
    }

    /// Why it can't update itself, if it can't.
    fn auto_blocked(&self) -> Option<&'static str> {
        if !self.prefs.fw_auto && !self.autoupdate.once {
            Some("off")
        } else if self.ota.roots.is_empty() {
            Some("needs a pinned root key (set ota.root)")
        } else if self.ota.target.is_none() {
            Some("no update slot")
        } else {
            None
        }
    }

    pub(super) fn auto_update_timer(&self) -> Option<u64> {
        self.auto_blocked().is_none().then_some(self.autoupdate.at)
    }

    /// `get fw.auto`.
    pub(super) fn auto_update_status(&self) -> String {
        let mode = if self.prefs.fw_auto { "on" } else { "off" };
        match (self.auto_blocked(), self.autoupdate.status.as_str()) {
            (Some(why), _) if self.prefs.fw_auto => format!("> on, but {why}"),
            (Some(_), _) => "> off".into(),
            (None, "") => format!("> {mode}, channel {}, waiting for the first check", release::channel_name(self.prefs.fw_channel)),
            (None, s) => format!("> {mode}, channel {}: {s}", release::channel_name(self.prefs.fw_channel)),
        }
    }

    /// `fw update`.
    pub(super) fn auto_update_now(&mut self, ctx: &MeshCtx) -> String {
        self.autoupdate.once = true;
        if let Some(why) = self.auto_blocked() {
            self.autoupdate.once = false;
            return format!("Error: {why}");
        }
        if self.autoupdate.step == Step::Sleep {
            self.autoupdate.at = ctx.now;
            // asked by hand: not what the store (or the neighbour's) last heard from the server
            self.autoupdate.fresh = true;
            if let Some(q) = self.own_query()
                && let Some(store) = self.fw_store.as_deref_mut()
            {
                store.refresh(&q);
            }
        }
        "OK - checking for an update; it installs itself and restarts (get fw.auto)".into()
    }

    fn auto_sleep(&mut self, ctx: &MeshCtx, ms: u64, status: String) {
        let a = &mut self.autoupdate;
        a.step = Step::Sleep;
        a.loader = None;
        a.once = false;
        a.peer = None;
        a.others.clear();
        a.tag = 0;
        a.status = status;
        a.at = ctx.now + ms;
    }

    pub(super) fn auto_update_loop(&mut self, ctx: &mut MeshCtx) {
        if self.auto_blocked().is_some() || ctx.now < self.autoupdate.at {
            return;
        }
        match self.autoupdate.step {
            Step::Sleep => self.auto_check(ctx),
            Step::Load { id, offset } => self.auto_load(ctx, id, offset),
            Step::Ask if self.autoupdate.tries >= QUERY_TRIES => self.lora_next(ctx),
            Step::Ask => self.lora_query(ctx),
            Step::Get { .. } if self.autoupdate.tries >= PIECE_TRIES => {
                let peer = self.peer_name();
                self.auto_sleep(ctx, RETRY_LATER_MS, format!("failed: {peer} stopped answering"));
            }
            Step::Get { offset } => self.lora_get(ctx, offset),
        }
    }

    fn auto_check(&mut self, ctx: &mut MeshCtx) {
        if self.ota.busy() {
            self.autoupdate.at = ctx.now + BUSY_RETRY_MS;
            return;
        }
        let Some(q) = self.own_query() else {
            return self.auto_sleep(ctx, RETRY_LATER_MS, "can't read this firmware's image".into());
        };
        self.autoupdate.server_down = false;
        if self.prefs.fw_source.is_empty()
            && let Some(store) = self.fw_store.as_deref_mut()
        {
            match store.offer(&q) {
                Offer::Ready { len, id, counter, version } if counter > q.counter => {
                    self.autoupdate.loader = Some(Loader::new(len, self.prefs.fw_channel));
                    self.autoupdate.step = Step::Load { id, offset: 0 };
                    self.autoupdate.status = format!("loading {version} #{counter}");
                    self.autoupdate.version = version;
                    self.autoupdate.at = ctx.now;
                    self.autoupdate.fresh = false;
                    return;
                }
                Offer::Fetching { retry_secs } => {
                    self.autoupdate.status = "the update is being fetched".into();
                    self.autoupdate.at = ctx.now + retry_secs as u64 * 1000;
                    return;
                }
                Offer::Ready { .. } | Offer::NoUpdate => {
                    self.autoupdate.fresh = false;
                    let next = CHECK_EVERY_MS + Self::auto_jitter(ctx);
                    return self.auto_sleep(ctx, next, format!("up to date (checked {})", fmt_time(ctx.current_time())));
                }
                // no link to the server: a neighbour may have one
                Offer::Unavailable => self.autoupdate.server_down = true,
            }
        }
        self.lora_start(ctx);
    }

    /// What this node asks for: an update from the image it runs.
    fn own_query(&mut self) -> Option<FwQuery> {
        let (base_len, base_sha) = self.ota.running()?;
        Some(FwQuery {
            board: self.ota.build.board,
            role: self.ota.build.role,
            counter: self.ota.build.counter,
            base_len,
            base_sha8: base_sha[..8].try_into().unwrap(),
            channel: self.prefs.fw_channel,
            fresh: false,
            owner8: FwQuery::owner_of(&self.ota.owners),
        })
    }

    fn auto_load(&mut self, ctx: &mut MeshCtx, id: u32, mut offset: u32) {
        let mut buf = [0u8; FW_PIECE];
        for _ in 0..PIECES_PER_PASS {
            let (Some(store), Some(loader)) = (self.fw_store.as_deref_mut(), self.autoupdate.loader.as_mut()) else {
                return;
            };
            let want = loader.want(offset);
            let Some(n) = store.read(id, offset, &mut buf[..want]) else {
                // the store holds another file now: ask again
                self.autoupdate.step = Step::Sleep;
                self.autoupdate.loader = None;
                self.autoupdate.at = ctx.now + 60_000;
                return;
            };
            match loader.feed(&mut self.ota, offset, &buf[..n]) {
                Ok(Some(next)) => offset = next,
                Ok(None) => return self.auto_install(ctx),
                Err(why) => return self.auto_sleep(ctx, RETRY_LATER_MS, format!("failed: {why}")),
            }
        }
        self.autoupdate.step = Step::Load { id, offset };
        self.autoupdate.at = ctx.now;
    }

    // ---------------------------------------------------- from a neighbour, over LoRa

    /// Ask the neighbours: the one `fw.source` names, else the best heard directly lately.
    fn lora_start(&mut self, ctx: &mut MeshCtx) {
        let now = ctx.current_time();
        let source = &self.prefs.fw_source;
        let mut heard: Vec<&Neighbour> = self
            .neighbours
            .iter()
            .filter(|n| n.heard_timestamp > 0 && now.saturating_sub(n.heard_timestamp) < HEARD_RECENTLY_SECS)
            .filter(|n| n.id.pub_key.starts_with(source))
            .collect();
        heard.sort_by_key(|n| Reverse(n.snr));
        let a = &mut self.autoupdate;
        a.others = heard.iter().take(MAX_ASKED).rev().map(|n| n.id).collect();
        a.asked = 0;
        self.lora_next(ctx);
    }

    /// The next neighbour to ask, or the check is over.
    fn lora_next(&mut self, ctx: &mut MeshCtx) {
        let Some(id) = self.autoupdate.others.pop() else {
            let server = if self.autoupdate.server_down { "the release server can't be reached, and " } else { "" };
            let why = match (self.autoupdate.asked, self.prefs.fw_source.is_empty()) {
                (0, true) => format!("{server}no repeater heard directly to ask"),
                (0, false) => format!("{server}{} not heard lately", to_hex(&self.prefs.fw_source)),
                _ => format!("{server}no neighbour hands updates on (fw.serve)"),
            };
            return self.auto_sleep(ctx, RETRY_LATER_MS, why);
        };
        let Some(secret) = ctx.self_id.calc_shared_secret(&id.pub_key) else {
            return self.lora_next(ctx);
        };
        let a = &mut self.autoupdate;
        a.peer = Some((id, secret));
        a.asked += 1;
        a.tries = 0;
        self.lora_query(ctx);
    }

    /// The neighbour asked, for people: the start of its key.
    fn peer_name(&self) -> String {
        self.autoupdate.peer.as_ref().map_or_else(|| "the neighbour".into(), |(id, _)| to_hex(&id.pub_key[..4]))
    }

    /// Send FW request `kind` to the neighbour, zero-hop, its answer due back the same way.
    fn lora_send(&mut self, ctx: &mut MeshCtx, kind: u8, payload: &[u8]) -> bool {
        let Some((id, secret)) = self.autoupdate.peer else {
            return false;
        };
        let tag = ctx.current_time_unique();
        let mut data = tag.to_le_bytes().to_vec();
        data.extend_from_slice(&[kind, 0]); // the reply path: none, zero-hop
        data.extend_from_slice(payload);
        let Some(pkt) = ctx.create_anon_datagram(&id, &secret, &data) else {
            return false;
        };
        ctx.send_zero_hop(pkt, 0);
        let a = &mut self.autoupdate;
        a.tag = tag;
        a.tries += 1;
        a.at = ctx.now + ANSWER_WAIT_MS;
        true
    }

    fn lora_query(&mut self, ctx: &mut MeshCtx) {
        let Some(mut q) = self.own_query() else {
            return self.auto_sleep(ctx, RETRY_LATER_MS, "can't read this firmware's image".into());
        };
        q.fresh = self.autoupdate.fresh;
        self.autoupdate.step = Step::Ask;
        if self.autoupdate.tries == 0 {
            self.autoupdate.status = format!("asking {}", self.peer_name());
        }
        if !self.lora_send(ctx, crate::fwshare::ANON_REQ_TYPE_FW_QUERY, &q.to_bytes()) {
            self.lora_next(ctx);
        }
    }

    fn lora_get(&mut self, ctx: &mut MeshCtx, offset: u32) {
        self.autoupdate.step = Step::Get { offset };
        let len = self.autoupdate.loader.as_ref().map_or(FW_PIECE, |l| l.want(offset)) as u8;
        let mut req = self.autoupdate.file_id.to_le_bytes().to_vec();
        req.extend_from_slice(&offset.to_le_bytes());
        req.push(len);
        if !self.lora_send(ctx, crate::fwshare::ANON_REQ_TYPE_FW_GET, &req) {
            self.auto_sleep(ctx, RETRY_LATER_MS, "failed: can't reach the neighbour".into());
        }
    }

    /// The neighbour's answer (a RESPONSE from [`FW_PEER`]); false if it is not one awaited.
    pub(super) fn auto_update_answer(&mut self, ctx: &mut MeshCtx, data: &[u8]) -> bool {
        if self.autoupdate.peer().is_none() || data.len() < 5 || read_u32(data, 0) != self.autoupdate.tag {
            return false;
        }
        self.autoupdate.tag = 0;
        self.autoupdate.tries = 0;
        let body = &data[5..];
        let peer = self.peer_name();
        match (self.autoupdate.step, data[4]) {
            (Step::Ask, status::READY) if body.len() >= 13 => {
                let len = read_u32(body, 0);
                let counter = read_u32(body, 8);
                let vlen = (body[12] as usize).min(body.len() - 13);
                let a = &mut self.autoupdate;
                a.fresh = false;
                a.file_id = read_u32(body, 4);
                a.version = String::from_utf8_lossy(&body[13..13 + vlen]).into_owned();
                a.loader = Some(Loader::new(len, self.prefs.fw_channel));
                a.status = format!("loading {} #{counter} from {peer}: 0 of {len} bytes", a.version);
                self.lora_get(ctx, 0);
            }
            (Step::Ask, status::FETCHING) => {
                // asked again when it should have it
                let secs = body.get(..2).map_or(60, |b| u16::from_le_bytes([b[0], b[1]])).clamp(10, 900);
                self.autoupdate.status = format!("{peer} is fetching the update");
                self.autoupdate.at = ctx.now + secs as u64 * 1000;
            }
            (Step::Ask, status::NONE) => {
                self.autoupdate.fresh = false;
                let next = CHECK_EVERY_MS + Self::auto_jitter(ctx);
                self.auto_sleep(ctx, next, format!("up to date (checked {} with {peer})", fmt_time(ctx.current_time())));
            }
            // it can't get updates now: the next neighbour
            (Step::Ask, _) => self.lora_next(ctx),
            (Step::Get { offset }, status::READY) if body.len() > 4 && read_u32(body, 0) == offset => {
                self.lora_piece(ctx, offset, &body[4..]);
            }
            (Step::Get { .. }, status::CHANGED) => {
                // the neighbour holds another file now: start over with it
                self.autoupdate.loader = None;
                self.lora_query(ctx);
            }
            (Step::Get { .. }, _) => self.auto_sleep(ctx, RETRY_LATER_MS, format!("failed: {peer} could not send the update")),
            _ => {}
        }
        true
    }

    /// A piece of the file at `offset`, fed to the update session; the next one is asked for after
    /// a pause, or the update is installed.
    fn lora_piece(&mut self, ctx: &mut MeshCtx, offset: u32, piece: &[u8]) {
        let Some(loader) = self.autoupdate.loader.as_mut() else {
            return;
        };
        if piece.len() < loader.want(offset) {
            self.autoupdate.at = ctx.now + PIECE_GAP_MS; // cut short: asked for again
            return;
        }
        let total = loader.file_len();
        match loader.feed(&mut self.ota, offset, piece) {
            Ok(Some(next)) => {
                let a = &mut self.autoupdate;
                a.step = Step::Get { offset: next };
                a.at = ctx.now + PIECE_GAP_MS;
                if let Some(at) = a.status.rfind(": ") {
                    a.status.truncate(at);
                }
                a.status = format!("{}: {next} of {total} bytes", a.status);
            }
            Ok(None) => self.auto_install(ctx),
            Err(why) => self.auto_sleep(ctx, RETRY_LATER_MS, format!("failed: {why}")),
        }
    }

    fn auto_install(&mut self, ctx: &mut MeshCtx) {
        let version = self.autoupdate.version.clone();
        let source = match &self.autoupdate.peer {
            Some(_) => format!("neighbour {}", self.peer_name()),
            None => self.prefs.fw_server.clone(),
        };
        let (reply, outcome) = self.ota.handle(&[REQ_TYPE_OTA_COMMIT], FIRMWARE_VERSION);
        if outcome == OtaOutcome::Committed {
            // nothing more to do: the node restarts into it
            let a = &mut self.autoupdate;
            a.step = Step::Sleep;
            a.loader = None;
            a.peer = None;
            a.at = u64::MAX;
            a.status = format!("installing {version} from {source}");
            self.console(format!("fw: installing {version} from {source}"));
            self.ota_outcome(outcome);
            return;
        }
        let why = match outcome {
            OtaOutcome::CommitFailed(line) => line,
            _ => String::from(crate::fwshare::refusal(reply.and_then(|r| r.first().copied()).unwrap_or(0))),
        };
        self.auto_sleep(ctx, RETRY_LATER_MS, format!("failed: {why}"));
    }
}
