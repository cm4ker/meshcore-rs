//! A repeater that updates itself (`set fw.auto download|install`, this firmware's extension, off
//! by default). With a working link to `fw.server` it asks its update store (see
//! [`crate::fwshare`]) for a release newer than the build it runs, for its board, role and channel.
//! Without one (no Wi-Fi, or the server out of reach), or when `fw.source` names a neighbour, it
//! asks a neighbouring repeater over LoRa instead, as a companion does: one that hands updates on
//! (`fw.serve on`) and so has the link this one lacks. That delta comes a piece at a time with
//! pauses between, which leave the channel to the others, and the node keeps repeating meanwhile.
//! Either way the delta is fed into its update session as an update over LoRa or BLE would be. In
//! `install` mode the node then restarts into it, on trial: an image that does not come up healthy
//! is rolled back. In `download` mode it waits for `fw install`. Only with a pinned root, since
//! neither the server nor the neighbour is trusted. `fw check` and `fw update` do one check now,
//! whatever the mode; `fw update` installs what it finds. Where it all stands: `fw`
//! ([`crate::fwstate`]).

use alloc::string::ToString;

use super::*;
use crate::fwshare::{self, FW_PIECE, FwQuery, Loader, MAX_LORA_FILE, Offer, status};
use crate::fwstate::{self, FwState, mode};
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
/// A loaded update waits for its restart while a neighbour is loading one from this node (the
/// restart would cut that transfer off): looked at again this often.
const INSTALL_WAIT_MS: u64 = 30_000;

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
    /// Loaded and verified, waiting for `fw install`.
    Ready,
    /// Loaded and verified, waiting for the neighbours this node hands an update on to.
    Install,
}

/// A check asked for by hand: `fw check` follows the mode once it has the update, `fw update`
/// installs it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hand {
    Check,
    Update,
}

pub(super) struct AutoUpdate {
    state: FwState,
    step: Step,
    at: u64,
    loader: Option<Loader>,
    release: fwstate::Release,
    by_hand: Option<Hand>,
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
            state: FwState::Waiting { at: 0 },
            step: Step::Sleep,
            at: FIRST_CHECK_MS,
            loader: None,
            release: fwstate::Release::default(),
            by_hand: None,
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

    /// Why it can't check for updates now, if it can't.
    pub(super) fn auto_blocked(&self) -> Option<FwState> {
        if self.ota.roots.is_empty() {
            Some(FwState::NoRoot)
        } else if self.ota.target.is_none() {
            Some(FwState::NoSlot)
        } else if self.prefs.fw_mode == mode::OFF && self.autoupdate.by_hand.is_none() {
            Some(FwState::Off)
        } else {
            None
        }
    }

    pub(super) fn auto_update_timer(&self) -> Option<u64> {
        let waiting = matches!(self.autoupdate.step, Step::Ready);
        (self.auto_blocked().is_none() && !waiting).then_some(self.autoupdate.at)
    }

    /// `fw`: the release that runs, the channel and mode; where its update stands; on a node that
    /// hands updates on, what its store holds.
    pub(super) fn fw_reply(&self, ctx: &MeshCtx) -> String {
        let mut s = fwstate::head(&self.board.build_date, self.prefs.fw_channel, self.prefs.fw_mode);
        s.push('\n');
        s.push_str(&self.fw_line(ctx));
        if self.prefs.fw_serve && self.fw_store.is_some() {
            s.push('\n');
            s.push_str(if self.board.fw_store.is_empty() { "serves nothing yet" } else { &self.board.fw_store });
        }
        s
    }

    /// Where the node's own update stands (the trial after an update aside, see `fw_line`).
    pub fn fw_state(&self) -> FwState {
        match (self.auto_blocked(), &self.autoupdate.state) {
            (Some(s @ (FwState::NoRoot | FwState::NoSlot)), _) => s,
            // off, and nothing asked by hand yet: what was last found stays in view otherwise
            (Some(FwState::Off), FwState::Waiting { .. }) => FwState::Off,
            (_, s) => s.clone(),
        }
    }

    /// The second line of `fw`.
    fn fw_line(&self, ctx: &MeshCtx) -> String {
        if let Some(until) = self.board.fw_trial_until.filter(|&t| t > ctx.now) {
            return fwstate::trial_line((until - ctx.now) / 1000, &self.board.fw_prev);
        }
        match self.fw_state() {
            FwState::Off | FwState::Waiting { .. } if self.board.fw_prev_failed => fwstate::failed_line(&self.board.fw_prev),
            s => s.line(),
        }
    }

    /// Start a check now (`fw check`, `fw update`): the reply says where it asks.
    pub(super) fn fw_check_now(&mut self, ctx: &mut MeshCtx, update: bool) -> String {
        if let Some(s @ (FwState::NoRoot | FwState::NoSlot)) = self.auto_blocked() {
            return format!("Error: {}", s.line());
        }
        match self.autoupdate.step {
            Step::Ready | Step::Install if update => return self.fw_install_now(ctx),
            Step::Ready | Step::Install => return format!("OK - {}", self.fw_line(ctx)),
            Step::Sleep => {}
            // a check under way: from now on it installs what it finds
            _ => {
                if update {
                    self.autoupdate.by_hand = Some(Hand::Update);
                }
                return format!("OK - {}", self.fw_line(ctx));
            }
        }
        self.autoupdate.by_hand = Some(if update { Hand::Update } else { Hand::Check });
        self.autoupdate.at = ctx.now;
        // asked by hand: not what the store (or the neighbour's) last heard from the server
        self.autoupdate.fresh = true;
        if let Some(q) = self.own_query()
            && let Some(store) = self.fw_store.as_deref_mut()
        {
            store.refresh(&q);
        }
        let whom = if self.prefs.fw_source.is_empty() && self.fw_store.is_some() {
            self.prefs.fw_server.clone()
        } else if self.prefs.fw_source.is_empty() {
            "the neighbours".into()
        } else {
            self.source_name()
        };
        if update {
            format!("OK - asking {whom}; if there is an update the node installs it and is silent for 1-2 min")
        } else {
            format!("OK - asking {whom}, see fw")
        }
    }

    /// Switched on: check now rather than at the next due time.
    pub(super) fn fw_check_soon(&mut self, ctx: &MeshCtx) {
        if self.autoupdate.step == Step::Sleep {
            self.autoupdate.at = ctx.now;
        }
    }

    /// `fw install`: the update downloaded, now.
    pub(super) fn fw_install_now(&mut self, ctx: &mut MeshCtx) -> String {
        match self.autoupdate.step {
            Step::Ready | Step::Install => {
                let release = self.autoupdate.release.to_string();
                self.auto_install(ctx, true);
                match &self.autoupdate.state {
                    FwState::Installing { .. } => format!("OK - installing {release}: restarting, silent for 1-2 min"),
                    s => format!("Error: {}", s.line()),
                }
            }
            Step::Load { .. } | Step::Get { .. } => format!("Error: not downloaded yet: {}", self.fw_line(ctx)),
            _ => "Error: nothing downloaded: fw check first".into(),
        }
    }

    /// The check is over until `ms` from now (nothing to do, or it failed), saying `state`.
    fn auto_sleep(&mut self, ctx: &MeshCtx, ms: u64, state: FwState) {
        let a = &mut self.autoupdate;
        a.step = Step::Sleep;
        a.loader = None;
        a.by_hand = None;
        a.peer = None;
        a.others.clear();
        a.tag = 0;
        a.at = ctx.now + ms;
        a.state = state;
    }

    /// Not now: tried again in an hour, if the node checks by itself.
    fn auto_fail(&mut self, ctx: &MeshCtx, why: String) {
        let retry = self.next_check(ctx, RETRY_LATER_MS);
        self.auto_sleep(ctx, RETRY_LATER_MS, FwState::Failed { why, retry });
    }

    /// Nothing newer: the next check in twelve hours or so.
    fn auto_done(&mut self, ctx: &MeshCtx, state: FwState) {
        let next = CHECK_EVERY_MS + Self::auto_jitter(ctx);
        self.autoupdate.fresh = false;
        self.auto_sleep(ctx, next, state);
    }

    /// When a check `ms` from now happens, by the wall clock; 0 if the node does not check by itself.
    fn next_check(&self, ctx: &MeshCtx, ms: u64) -> u32 {
        if self.prefs.fw_mode == mode::OFF { 0 } else { fwstate::wall(ctx.current_time(), ctx.now, ctx.now + ms) }
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
                self.auto_fail(ctx, format!("{peer} stopped answering"));
            }
            Step::Get { offset } => self.lora_get(ctx, offset),
            Step::Ready => {}
            Step::Install => self.auto_install(ctx, false),
        }
    }

    /// The release a check found was rolled back on this node before: only a check by hand takes it.
    fn skipped(&self, counter: u32) -> bool {
        counter == self.board.fw_skip && self.autoupdate.by_hand.is_none()
    }

    fn auto_check(&mut self, ctx: &mut MeshCtx) {
        if self.ota.busy() {
            self.autoupdate.at = ctx.now + BUSY_RETRY_MS;
            return;
        }
        let Some(q) = self.own_query() else {
            return self.auto_fail(ctx, "can't read this firmware's image".into());
        };
        self.autoupdate.server_down = false;
        if self.prefs.fw_source.is_empty()
            && let Some(store) = self.fw_store.as_deref_mut()
        {
            let server = self.prefs.fw_server.clone();
            match store.offer(&q) {
                Offer::Ready { counter, version, .. } if counter > q.counter && self.skipped(counter) => {
                    let release = fwstate::Release::new(&version, counter);
                    return self.auto_done(ctx, FwState::Skipped { release });
                }
                Offer::Ready { len, id, counter, version } if counter > q.counter => {
                    let a = &mut self.autoupdate;
                    a.loader = Some(Loader::new(len, self.prefs.fw_channel));
                    a.step = Step::Load { id, offset: 0 };
                    a.release = fwstate::Release::new(&version, counter);
                    a.state = FwState::Downloading { release: a.release.clone(), from: server, done: 0, total: len };
                    a.at = ctx.now;
                    a.fresh = false;
                    return;
                }
                Offer::Fetching { retry_secs } => {
                    self.autoupdate.state = FwState::Asking { whom: server };
                    self.autoupdate.at = ctx.now + retry_secs as u64 * 1000;
                    return;
                }
                Offer::Ready { .. } | Offer::NoUpdate => {
                    return self.auto_done(ctx, FwState::UpToDate { whom: server, at: ctx.current_time() });
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
            max_file: self.lora_max_file(),
        })
    }

    /// The largest update this node takes over LoRa: what its slot holds, by itself at most the
    /// ceiling (`fw update` takes more, and holds the channel longer).
    fn lora_max_file(&mut self) -> u32 {
        let room = self.ota.delta_room();
        if self.autoupdate.by_hand == Some(Hand::Update) { room } else { room.min(MAX_LORA_FILE) }
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
                Ok(None) => return self.auto_loaded(ctx),
                Err(why) => return self.auto_fail(ctx, why.into()),
            }
        }
        self.autoupdate.step = Step::Load { id, offset };
        self.autoupdate.at = ctx.now;
        if let FwState::Downloading { done, .. } = &mut self.autoupdate.state {
            *done = offset;
        }
    }

    /// The whole update is in the session and checked: installed now, or when told.
    fn auto_loaded(&mut self, ctx: &mut MeshCtx) {
        if self.prefs.fw_mode == mode::INSTALL || self.autoupdate.by_hand == Some(Hand::Update) {
            return self.auto_install(ctx, false);
        }
        let len = self.autoupdate.loader.as_ref().map_or(0, |l| l.file_len());
        let a = &mut self.autoupdate;
        a.step = Step::Ready;
        a.by_hand = None;
        a.peer = None;
        a.state = FwState::Ready { release: a.release.clone(), len };
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
        sort_neighbours(&mut heard, |n| -(n.snr as i64));
        let a = &mut self.autoupdate;
        a.others = heard.iter().take(MAX_ASKED).rev().map(|n| n.id).collect();
        a.asked = 0;
        self.lora_next(ctx);
    }

    /// The update's delta is larger than this node takes over LoRa: no neighbour can hand it on,
    /// so the check is over until the next one (a later release may have a small delta from this
    /// image). One over the ceiling only comes with `fw update`.
    fn lora_too_large(&mut self, ctx: &mut MeshCtx, len: u32) {
        let (kb, room) = (len.div_ceil(1024), self.ota.delta_room());
        let how = if self.board.ble_ota_name.is_some() { "fw ble or USB" } else { "USB" };
        let why = if len > room {
            format!("the update is {kb} KB, more than this node takes over LoRa ({} KB): install it with {how}", room / 1024)
        } else if self.autoupdate.by_hand != Some(Hand::Update) {
            format!("the update is {kb} KB, over the {} KB a node takes over LoRa by itself: fw update takes it", MAX_LORA_FILE / 1024)
        } else {
            // asked by hand: the neighbour runs firmware that hands on no more than the ceiling
            format!("the update is {kb} KB, more than {} hands on over LoRa: install it with {how}", self.peer_name())
        };
        let retry = self.next_check(ctx, CHECK_EVERY_MS);
        self.auto_done(ctx, FwState::Failed { why, retry });
    }

    /// The next neighbour to ask, or the check is over.
    fn lora_next(&mut self, ctx: &mut MeshCtx) {
        let Some(id) = self.autoupdate.others.pop() else {
            let server =
                if self.autoupdate.server_down { format!("{} not reachable, ", self.prefs.fw_server) } else { String::new() };
            let why = match (self.autoupdate.asked, self.prefs.fw_source.is_empty()) {
                (0, true) => format!("{server}no repeater heard directly to ask"),
                (0, false) => format!("{server}{} not heard lately", self.source_name()),
                _ => format!("{server}no neighbour hands updates on (fw.serve)"),
            };
            return self.auto_fail(ctx, why);
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

    /// The name a neighbour whose key starts with `key` advertised, if it is heard.
    fn neighbour_named(&self, key: &[u8]) -> Option<String> {
        self.neighbours.iter().find(|n| n.id.pub_key.starts_with(key) && !n.name.is_empty()).map(|n| n.name.clone())
    }

    /// A neighbour, for people: its name if it advertised one, else the start of its key.
    fn neighbour_name(&self, key: &[u8]) -> String {
        self.neighbour_named(key).unwrap_or_else(|| to_hex(&key[..key.len().min(4)]))
    }

    /// `fw.source`, for people: `PKIO (CE5B9348)`, or the start of the key alone.
    pub(super) fn fw_source_label(&self) -> String {
        let key = &self.prefs.fw_source;
        let hex = to_hex(&key[..key.len().min(4)]);
        self.neighbour_named(key).map_or_else(|| hex.clone(), |n| format!("{n} ({hex})"))
    }

    /// `fw`, `fw check`, `fw update`, `fw install`, `fw rollback`.
    pub(super) fn handle_fw_command(&mut self, ctx: &mut MeshCtx, cmd: &str) -> Option<String> {
        Some(match cmd {
            "fw" => self.fw_reply(ctx),
            "fw check" => self.fw_check_now(ctx, false),
            "fw update" => self.fw_check_now(ctx, true),
            "fw install" => self.fw_install_now(ctx),
            "fw rollback" if !self.board.fw_rollback => "Error: no previous firmware on this node".into(),
            "fw rollback" => {
                self.effects.fw_rollback = true;
                match self.board.fw_prev.as_str() {
                    "" => "OK - back to the previous firmware: restarting".into(),
                    prev => format!("OK - back to {prev}: restarting"),
                }
            }
            _ => return None,
        })
    }

    /// The neighbour asked.
    fn peer_name(&self) -> String {
        self.autoupdate.peer.as_ref().map_or_else(|| "the neighbour".into(), |(id, _)| self.neighbour_name(&id.pub_key))
    }

    /// The neighbour `fw.source` names.
    fn source_name(&self) -> String {
        self.neighbour_name(&self.prefs.fw_source)
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
            return self.auto_fail(ctx, "can't read this firmware's image".into());
        };
        q.fresh = self.autoupdate.fresh;
        self.autoupdate.step = Step::Ask;
        if self.autoupdate.tries == 0 && !matches!(self.autoupdate.state, FwState::Fetching { .. }) {
            self.autoupdate.state = FwState::Asking { whom: self.peer_name() };
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
            self.auto_fail(ctx, "can't reach the neighbour".into());
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
            // a neighbour from before the limit offers it anyway: not loaded either
            (Step::Ask, status::TOO_LARGE) if body.len() >= 4 => self.lora_too_large(ctx, read_u32(body, 0)),
            (Step::Ask, status::READY) if body.len() >= 13 && read_u32(body, 0) > self.lora_max_file() => {
                self.lora_too_large(ctx, read_u32(body, 0))
            }
            (Step::Ask, status::READY) if body.len() >= 13 => {
                let len = read_u32(body, 0);
                let counter = read_u32(body, 8);
                let vlen = (body[12] as usize).min(body.len() - 13);
                let release = fwstate::Release::new(&String::from_utf8_lossy(&body[13..13 + vlen]), counter);
                if self.skipped(counter) {
                    self.auto_done(ctx, FwState::Skipped { release });
                    return true;
                }
                let a = &mut self.autoupdate;
                a.fresh = false;
                a.file_id = read_u32(body, 4);
                a.loader = Some(Loader::new(len, self.prefs.fw_channel));
                a.state = FwState::Downloading { release: release.clone(), from: peer, done: 0, total: len };
                a.release = release;
                self.lora_get(ctx, 0);
            }
            (Step::Ask, status::FETCHING) => {
                // asked again when it should have it; it asks its server already, so not by hand
                // again (or each question would send it to the server anew)
                let secs = body.get(..2).map_or(60, |b| u16::from_le_bytes([b[0], b[1]])).clamp(10, 900);
                self.autoupdate.fresh = false;
                self.autoupdate.state = FwState::Fetching { whom: peer };
                self.autoupdate.at = ctx.now + secs as u64 * 1000;
            }
            (Step::Ask, status::NONE) => self.auto_done(ctx, FwState::UpToDate { whom: peer, at: ctx.current_time() }),
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
            (Step::Get { .. }, _) => self.auto_fail(ctx, format!("{peer} could not send the update")),
            _ => {}
        }
        true
    }

    /// A piece of the file at `offset`, fed to the update session; the next one is asked for after
    /// a pause, or the update is in.
    fn lora_piece(&mut self, ctx: &mut MeshCtx, offset: u32, piece: &[u8]) {
        let Some(loader) = self.autoupdate.loader.as_mut() else {
            return;
        };
        if piece.len() < loader.want(offset) {
            self.autoupdate.at = ctx.now + PIECE_GAP_MS; // cut short: asked for again
            return;
        }
        match loader.feed(&mut self.ota, offset, piece) {
            Ok(Some(next)) => {
                let a = &mut self.autoupdate;
                a.step = Step::Get { offset: next };
                a.at = ctx.now + PIECE_GAP_MS;
                if let FwState::Downloading { done, .. } = &mut a.state {
                    *done = next;
                }
            }
            Ok(None) => self.auto_loaded(ctx),
            Err(why) => self.auto_fail(ctx, why.into()),
        }
    }

    /// Restart into the update, unless a neighbour is loading one from this node (`now`: asked by
    /// hand, it does not wait for that).
    fn auto_install(&mut self, ctx: &mut MeshCtx, now: bool) {
        let release = self.autoupdate.release.clone();
        // a neighbour loading an update from this node would lose it to the restart
        let serving_until = self.fw_served_at.map_or(0, |t| t + fwshare::STORE_IDLE_SECS * 1000);
        if ctx.now < serving_until && !now {
            let a = &mut self.autoupdate;
            a.step = Step::Install;
            a.at = ctx.now + INSTALL_WAIT_MS;
            a.state = FwState::Held { release };
            return;
        }
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
            self.console(format!("fw: installing {release} from {source}"));
            self.autoupdate.state = FwState::Installing { release };
            self.ota_outcome(outcome);
            return;
        }
        let why = match outcome {
            OtaOutcome::CommitFailed(line) => line,
            _ => String::from(crate::fwshare::refusal(reply.and_then(|r| r.first().copied()).unwrap_or(0))),
        };
        self.auto_fail(ctx, why);
    }
}
