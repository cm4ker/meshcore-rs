//! Pulling a firmware update from a neighbouring repeater (`fw.auto`, see [`crate::fwshare`]).
//! Connected to nothing, a companion that pins a root key asks the repeater it hears best directly
//! for a delta from the image it runs, fetches it a piece at a time into its update slot (through
//! the same session an app's update uses), and installs it when its user says so (`fw.auto
//! download`, then `fw install` or a button on the device) or by itself (`fw.auto install`).
//! Where it stands: `fw` and `fw_status` ([`crate::fwstate`]).

use alloc::string::ToString;

use super::*;
use crate::FIRMWARE_VERSION;
use crate::fwshare::{self, FW_PIECE, FwQuery, Loader, status};
use crate::fwstate::{self, FwState, mode};
use crate::ota::{OtaOutcome, REQ_TYPE_OTA_COMMIT};

/// The first check, a while after boot (the node has heard its neighbours by then), and the next.
const FIRST_CHECK_MS: u64 = 10 * 60_000;
const CHECK_EVERY_MS: u64 = 12 * 3_600_000;
/// After a failure or with no repeater to ask.
const RETRY_LATER_MS: u64 = 3_600_000;
/// A request and its answer take well under a second on air; past this it is asked again.
const ANSWER_WAIT_MS: u64 = 8_000;
const TRIES: u8 = 6;
/// Between pieces: an update leaves the channel to the others most of the time.
const PIECE_GAP_MS: u64 = 1_200;
/// Repeaters heard directly within this long are asked.
const HEARD_RECENTLY_SECS: u32 = 2 * 86_400;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Step {
    Sleep,
    Query,
    Get { offset: u32 },
    Ready,
}

/// A check asked for by hand: `fw check` follows the mode once it has the update, `fw update`
/// installs it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Hand {
    Check,
    Update,
}

pub struct FwPull {
    pub state: FwState,
    step: Step,
    /// When the step is due: the end of a sleep, or the deadline of an answer.
    at: u64,
    tag: u32,
    tries: u8,
    repeater: [u8; PUB_KEY_SIZE],
    file_id: u32,
    release: fwstate::Release,
    /// The update session being fed, while the file comes.
    loader: Option<Loader>,
    by_hand: Option<Hand>,
    /// The next query was asked for by hand: the repeater should not answer from what it knew.
    fresh: bool,
}

impl Default for FwPull {
    fn default() -> Self {
        Self {
            state: FwState::Waiting { at: 0 },
            step: Step::Sleep,
            at: FIRST_CHECK_MS,
            tag: 0,
            tries: 0,
            repeater: [0; PUB_KEY_SIZE],
            file_id: 0,
            release: fwstate::Release::default(),
            loader: None,
            by_hand: None,
            fresh: false,
        }
    }
}

/// Why an update setting or action was refused: a value it does not take, or not now.
pub(super) enum FwError {
    Value(String),
    State(String),
}

impl FwError {
    /// As a custom variable's error.
    pub(super) fn code(&self) -> u8 {
        match self {
            Self::Value(_) => ERR_CODE_ILLEGAL_ARG,
            Self::State(_) => ERR_CODE_BAD_STATE,
        }
    }

    pub(super) fn text(&self) -> &str {
        match self {
            Self::Value(t) | Self::State(t) => t,
        }
    }
}

impl Companion {
    /// Why the node can't check for updates now, if it can't: what a repeater hands on is only
    /// trusted for its signature, so a root key must be pinned, and an update needs a slot.
    pub(super) fn fw_blocked(&self) -> Option<FwState> {
        if self.ota.roots.is_empty() {
            Some(FwState::NoRoot)
        } else if self.ota.target.is_none() {
            Some(FwState::NoSlot)
        } else if self.prefs.fw_mode == mode::OFF && self.fwpull.by_hand.is_none() {
            Some(FwState::Off)
        } else {
            None
        }
    }

    /// When [`Self::fw_pull_loop`] is next due.
    pub(super) fn fw_pull_timer(&self) -> Option<u64> {
        (self.fw_blocked().is_none() && self.fwpull.step != Step::Ready).then_some(self.fwpull.at)
    }

    pub(super) fn fw_pull_loop(&mut self, ctx: &mut MeshCtx) {
        if self.fw_blocked().is_some() {
            if !matches!(self.fwpull.step, Step::Sleep | Step::Ready) {
                self.fw_pull_reset();
            }
            return;
        }
        if ctx.now < self.fwpull.at {
            return;
        }
        match self.fwpull.step {
            Step::Sleep => {
                self.fwpull.tries = 0;
                self.fw_query(ctx);
            }
            Step::Query | Step::Get { .. } if self.fwpull.tries >= TRIES => {
                let name = self.fw_repeater_name();
                self.fw_pull_fail(ctx, alloc::format!("no answer from {name}"));
            }
            Step::Query => self.fw_query(ctx),
            Step::Get { offset } => self.fw_get(ctx, offset),
            Step::Ready => {}
        }
    }

    /// Where the update stands (the trial after an update aside, see [`Self::fw_line`]).
    pub fn fw_state(&self) -> FwState {
        match (self.fw_blocked(), &self.fwpull.state) {
            (Some(s @ (FwState::NoRoot | FwState::NoSlot)), _) => s,
            // off, and nothing asked by hand yet: what was last found stays in view otherwise
            (Some(FwState::Off), FwState::Waiting { .. }) => FwState::Off,
            (_, s) => s.clone(),
        }
    }

    /// The second line of `fw`, and `fw_status`.
    pub fn fw_line(&self, now: u64) -> String {
        if let Some(until) = self.board.fw_trial_until.filter(|&t| t > now) {
            return fwstate::trial_line((until - now) / 1000, &self.board.fw_prev);
        }
        self.fw_state().line()
    }

    /// `fw`: the release that runs, the channel and mode, and where its update stands.
    pub fn fw_reply(&self, now: u64) -> String {
        alloc::format!(
            "{}\n{}",
            fwstate::head(&self.board.build_date, self.prefs.fw_channel, self.prefs.fw_mode),
            self.fw_line(now)
        )
    }

    /// Check now (`fw check`, `fw update`, the device's button): the reply says whom it asks. An
    /// update already downloaded is installed at once by `fw update`.
    pub fn fw_check_now(&mut self, ctx: &MeshCtx, update: bool) -> Result<String, String> {
        if let Some(s @ (FwState::NoRoot | FwState::NoSlot)) = self.fw_blocked() {
            return Err(s.line());
        }
        match self.fwpull.step {
            Step::Ready if update => return self.fw_install(),
            Step::Ready => return Ok(self.fw_line(ctx.now)),
            Step::Sleep => {}
            _ => {
                if update {
                    self.fwpull.by_hand = Some(Hand::Update);
                }
                return Ok(self.fw_line(ctx.now));
            }
        }
        self.fwpull.by_hand = Some(if update { Hand::Update } else { Hand::Check });
        self.fwpull.at = ctx.now;
        self.fwpull.fresh = true; // asked by hand
        let whom = match self.fw_repeater(ctx) {
            Some(idx) => self.contacts[idx].name_lossy(),
            None => "the repeaters heard".into(),
        };
        Ok(match update {
            true => alloc::format!("asking {whom}; if there is an update the device installs it and restarts"),
            false => alloc::format!("asking {whom}, see fw"),
        })
    }

    /// Install the downloaded update: the device restarts into it.
    pub fn fw_install(&mut self) -> Result<String, String> {
        if self.fwpull.step != Step::Ready {
            return Err(match &self.fwpull.state {
                FwState::Downloading { .. } => alloc::format!("not downloaded yet: {}", self.fwpull.state.line()),
                _ => "nothing downloaded: fw check first".into(),
            });
        }
        let release = self.fwpull.release.clone();
        let (reply, outcome) = self.ota.handle(&[REQ_TYPE_OTA_COMMIT], FIRMWARE_VERSION);
        let why = match outcome {
            OtaOutcome::Committed => {
                self.effects.reboot = true;
                self.fwpull.state = FwState::Installing { release: release.clone() };
                return Ok(alloc::format!("installing {release}: restarting"));
            }
            OtaOutcome::CommitFailed(line) => line,
            OtaOutcome::None => fwshare::refusal(reply.and_then(|r| r.first().copied()).unwrap_or(0)).to_string(),
        };
        self.fw_pull_reset();
        self.fwpull.state = FwState::Failed { why: why.clone(), retry: 0 };
        Err(why)
    }

    /// `fw rollback`: the firmware in the other slot, after a restart.
    pub fn fw_rollback(&mut self) -> Result<String, String> {
        if !self.board.fw_rollback {
            return Err("no previous firmware on this device".into());
        }
        self.effects.fw_rollback = true;
        Ok(match self.board.fw_prev.as_str() {
            "" => "back to the previous firmware: restarting".into(),
            prev => alloc::format!("back to {prev}: restarting"),
        })
    }

    /// Switched on: check now rather than at the next due time.
    pub(super) fn fw_check_soon(&mut self, ctx: &MeshCtx) {
        if self.fwpull.step == Step::Sleep {
            self.fwpull.at = ctx.now;
        }
    }

    fn fw_pull_reset(&mut self) {
        let state = core::mem::replace(&mut self.fwpull.state, FwState::Off);
        self.fwpull = FwPull { state, at: 0, ..FwPull::default() };
    }

    fn fw_sleep(&mut self, ctx: &MeshCtx, ms: u64, state: FwState) {
        self.fw_pull_reset();
        self.fwpull.state = state;
        self.fwpull.at = ctx.now + ms;
    }

    /// When a check `ms` from now happens, by the wall clock; 0 if the node does not check by itself.
    fn fw_next_check(&self, ctx: &MeshCtx, ms: u64) -> u32 {
        if self.prefs.fw_mode == mode::OFF { 0 } else { fwstate::wall(ctx.current_time(), ctx.now, ctx.now + ms) }
    }

    fn fw_pull_fail(&mut self, ctx: &MeshCtx, why: String) {
        let retry = self.fw_next_check(ctx, RETRY_LATER_MS);
        self.fw_sleep(ctx, RETRY_LATER_MS, FwState::Failed { why, retry });
    }

    /// The update's delta is larger than this node takes over LoRa: until the next check (a later
    /// release may have a small delta from this image), it goes in over USB or BLE.
    fn fw_too_large(&mut self, ctx: &MeshCtx, len: u32) {
        let (kb, max) = (len.div_ceil(1024), self.fw_max_file() / 1024);
        let why = alloc::format!("the update is {kb} KB, more than this node takes over LoRa ({max} KB): install it over USB or BLE");
        let retry = self.fw_next_check(ctx, CHECK_EVERY_MS);
        self.fw_sleep(ctx, CHECK_EVERY_MS, FwState::Failed { why, retry });
    }

    /// The largest update this node takes over LoRa: what its slot holds, at most the ceiling.
    fn fw_max_file(&mut self) -> u32 {
        self.ota.delta_room().min(fwshare::MAX_LORA_FILE)
    }

    /// The repeater to ask: the one set (`fw.source`), else the best heard directly lately.
    fn fw_repeater(&self, ctx: &MeshCtx) -> Option<usize> {
        if !self.prefs.fw_source.is_empty() {
            return self.lookup_contact(&self.prefs.fw_source);
        }
        let now = ctx.current_time();
        self.heard
            .iter()
            .filter(|h| h.adv_type == ADV_TYPE_REPEATER && h.hops == 0 && now.saturating_sub(h.heard) < HEARD_RECENTLY_SECS)
            .filter_map(|h| Some((h.snr, self.lookup_contact(&h.key_prefix)?)))
            .max_by_key(|&(snr, _)| snr)
            .map(|(_, idx)| idx)
    }

    /// The repeater asked, for people.
    fn fw_repeater_name(&self) -> String {
        match self.lookup_contact(&self.fwpull.repeater) {
            Some(idx) => self.contacts[idx].name_lossy(),
            None => "the repeater".into(),
        }
    }

    /// Send FW request `kind` to the repeater, zero-hop (it is a neighbour), its answer due back
    /// the same way.
    fn fw_send(&mut self, ctx: &mut MeshCtx, kind: u8, payload: &[u8]) -> bool {
        let Some(idx) = self.lookup_contact(&self.fwpull.repeater) else {
            return false;
        };
        let tag = ctx.current_time_unique();
        let mut data = tag.to_le_bytes().to_vec();
        data.extend_from_slice(&[kind, 0]); // the reply path: none, zero-hop
        data.extend_from_slice(payload);
        let secret = self.secret_of(ctx, idx);
        let id = self.contacts[idx].id;
        let Some(pkt) = ctx.create_anon_datagram(&id, &secret, &data) else {
            return false;
        };
        ctx.send_zero_hop(pkt, 0);
        self.fwpull.tag = tag;
        self.fwpull.tries += 1;
        self.fwpull.at = ctx.now + ANSWER_WAIT_MS;
        true
    }

    fn fw_query(&mut self, ctx: &mut MeshCtx) {
        if self.fwpull.tries == 0 {
            let Some(idx) = self.fw_repeater(ctx) else {
                let why = match self.prefs.fw_source.is_empty() {
                    true => "no repeater heard directly; one with fw.serve on is needed".into(),
                    false => alloc::format!("{} is not a contact", crate::util::to_hex(&self.prefs.fw_source)),
                };
                return self.fw_pull_fail(ctx, why);
            };
            self.fwpull.repeater = self.contacts[idx].id.pub_key;
        }
        let Some((base_len, base_sha)) = self.ota.running() else {
            return self.fw_pull_fail(ctx, "can't read this firmware's image".into());
        };
        let q = FwQuery {
            board: self.ota.build.board,
            role: self.ota.build.role,
            counter: self.ota.build.counter,
            base_len,
            base_sha8: base_sha[..8].try_into().unwrap(),
            channel: self.prefs.fw_channel,
            fresh: core::mem::take(&mut self.fwpull.fresh),
            owner8: FwQuery::owner_of(&self.ota.owners),
            max_file: self.fw_max_file(),
        };
        self.fwpull.step = Step::Query;
        if !matches!(self.fwpull.state, FwState::Fetching { .. }) {
            self.fwpull.state = FwState::Asking { whom: self.fw_repeater_name() };
        }
        if !self.fw_send(ctx, fwshare::ANON_REQ_TYPE_FW_QUERY, &q.to_bytes()) {
            self.fw_pull_fail(ctx, "the repeater is not a contact".into());
        }
    }

    fn fw_get(&mut self, ctx: &mut MeshCtx, offset: u32) {
        self.fwpull.step = Step::Get { offset };
        let len = self.fwpull.loader.as_ref().map_or(FW_PIECE, |l| l.want(offset)) as u8;
        let mut req = self.fwpull.file_id.to_le_bytes().to_vec();
        req.extend_from_slice(&offset.to_le_bytes());
        req.push(len);
        if !self.fw_send(ctx, fwshare::ANON_REQ_TYPE_FW_GET, &req) {
            self.fw_pull_fail(ctx, "the repeater is not a contact".into());
        }
    }

    /// An answer from contact `idx`; false if it is not one this node waits for.
    pub(super) fn fw_pull_answer(&mut self, ctx: &mut MeshCtx, idx: usize, data: &[u8]) -> bool {
        let waiting = matches!(self.fwpull.step, Step::Query | Step::Get { .. });
        if !waiting || data.len() < 5 || read_u32(data, 0) != self.fwpull.tag {
            return false;
        }
        if self.contacts[idx].id.pub_key != self.fwpull.repeater {
            return false;
        }
        self.fwpull.tag = 0;
        self.fwpull.tries = 0;
        let body = &data[5..];
        let name = self.contacts[idx].name_lossy();
        match (self.fwpull.step, data[4]) {
            // a repeater from before the limit offers it anyway: not loaded either
            (Step::Query, status::TOO_LARGE) if body.len() >= 4 => self.fw_too_large(ctx, read_u32(body, 0)),
            (Step::Query, status::READY) if body.len() >= 13 && read_u32(body, 0) > self.fw_max_file() => {
                self.fw_too_large(ctx, read_u32(body, 0))
            }
            (Step::Query, status::READY) if body.len() >= 13 => {
                let len = read_u32(body, 0);
                let counter = read_u32(body, 8);
                let vlen = (body[12] as usize).min(body.len() - 13);
                let release = fwstate::Release::new(&String::from_utf8_lossy(&body[13..13 + vlen]), counter);
                // rolled back from before: only a check by hand takes it
                if counter == self.board.fw_skip && self.fwpull.by_hand.is_none() {
                    self.fw_sleep(ctx, CHECK_EVERY_MS, FwState::Skipped { release });
                    return true;
                }
                self.fwpull.file_id = read_u32(body, 4);
                self.fwpull.loader = Some(Loader::new(len, self.prefs.fw_channel));
                self.fwpull.state = FwState::Downloading { release: release.clone(), from: name, done: 0, total: len };
                self.fwpull.release = release;
                self.fw_get(ctx, 0);
            }
            (Step::Query, status::FETCHING) => {
                let secs = body.get(..2).map_or(60, |b| u16::from_le_bytes([b[0], b[1]])).clamp(10, 900);
                self.fwpull.step = Step::Sleep;
                self.fwpull.state = FwState::Fetching { whom: name };
                self.fwpull.at = ctx.now + secs as u64 * 1000;
            }
            (Step::Query, status::NONE) => {
                let at = ctx.current_time();
                self.fw_sleep(ctx, CHECK_EVERY_MS, FwState::UpToDate { whom: name, at })
            }
            (Step::Query, _) => self.fw_pull_fail(ctx, alloc::format!("{name} has no updates now")),
            (Step::Get { offset }, status::READY) if read_u32(body, 0) == offset && body.len() > 4 => {
                if let Err(why) = self.fw_piece(ctx, offset, &body[4..]) {
                    self.fw_pull_fail(ctx, why.into());
                }
            }
            (Step::Get { .. }, status::CHANGED) => {
                // the repeater holds another file now: start over
                self.fwpull.step = Step::Query;
                self.fw_query(ctx);
            }
            (Step::Get { .. }, _) => self.fw_pull_fail(ctx, alloc::format!("{name} could not send the update")),
            _ => {}
        }
        true
    }

    /// A piece of the file at `offset`, fed to the update session; the next one is asked for after
    /// a pause, or the update is ready.
    fn fw_piece(&mut self, ctx: &mut MeshCtx, offset: u32, piece: &[u8]) -> Result<(), &'static str> {
        let Some(loader) = self.fwpull.loader.as_mut() else {
            return Err("no update file");
        };
        if piece.len() < loader.want(offset) {
            self.fwpull.at = ctx.now + PIECE_GAP_MS; // cut short: ask for it again
            return Ok(());
        }
        let next = loader.feed(&mut self.ota, offset, piece)?;
        let len = loader.file_len();
        if let FwState::Downloading { done, .. } = &mut self.fwpull.state {
            *done = next.unwrap_or(len);
        }
        if let Some(next) = next {
            self.fwpull.step = Step::Get { offset: next };
            self.fwpull.at = ctx.now + PIECE_GAP_MS;
            return Ok(());
        }
        // the whole patch is in the session: wait for the word to install (or install now)
        let install = self.prefs.fw_mode == mode::INSTALL || self.fwpull.by_hand == Some(Hand::Update);
        self.fwpull.step = Step::Ready;
        self.fwpull.by_hand = None;
        self.fwpull.state = FwState::Ready { release: self.fwpull.release.clone(), len };
        if install {
            let _ = self.fw_install();
        }
        Ok(())
    }
}
