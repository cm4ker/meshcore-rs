//! Pulling a firmware update from a neighbouring repeater (`fw_pull`, see [`crate::fwshare`]).
//! Connected to nothing, a companion that pins a root key asks the repeater it hears best directly
//! for a delta from the image it runs, fetches it a piece at a time into its update slot (through
//! the same session an app's update uses), and installs it when its user says so (`fw_pull on`,
//! a button on the device) or by itself (`fw_pull auto`).

use alloc::string::ToString;

use super::*;
use crate::FIRMWARE_VERSION;
use crate::fwshare::{self, FW_PIECE, FwQuery, Loader, status};
use crate::ota::{OtaOutcome, REQ_TYPE_OTA_COMMIT};

pub const FW_PULL_OFF: u8 = 0;
/// Fetch updates, install one when the user says so.
pub const FW_PULL_ON: u8 = 1;
/// Fetch and install updates by themselves.
pub const FW_PULL_AUTO: u8 = 2;

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

/// Where a pull stands, for the device's screen and `fw_status`.
#[derive(Clone, Debug, PartialEq)]
pub enum FwPullStatus {
    /// `fw_pull off`.
    Off,
    /// No root key is pinned, or there is no update slot: nothing is pulled, since anything
    /// would be installed.
    NotTrusted,
    /// Waiting for the next check; `up_to_date` once a repeater said there is nothing newer.
    Idle { up_to_date: bool },
    /// No repeater heard directly to ask.
    NoRepeater,
    Checking,
    /// The repeater is fetching the update from its server.
    RepeaterFetching,
    Loading { version: String, done: u32, total: u32 },
    /// Fetched and checked as far as it can be before installing.
    Ready { version: String },
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Step {
    Sleep,
    Query,
    Get { offset: u32 },
    Ready,
}

pub struct FwPull {
    pub status: FwPullStatus,
    step: Step,
    /// When the step is due: the end of a sleep, or the deadline of an answer.
    at: u64,
    tag: u32,
    tries: u8,
    repeater: [u8; PUB_KEY_SIZE],
    file_id: u32,
    version: String,
    /// The update session being fed, while the file comes.
    loader: Option<Loader>,
    /// `fw update`: one check that installs what it finds, whatever `fw_pull` says.
    once: bool,
}

impl Default for FwPull {
    fn default() -> Self {
        Self {
            status: FwPullStatus::Off,
            step: Step::Sleep,
            at: FIRST_CHECK_MS,
            tag: 0,
            tries: 0,
            repeater: [0; PUB_KEY_SIZE],
            file_id: 0,
            version: String::new(),
            loader: None,
            once: false,
        }
    }
}

impl Companion {
    /// The node may pull: it is asked to, and it pins a root key (what a repeater hands on is only
    /// trusted for its signature) and has an update slot.
    fn fw_pull_allowed(&self) -> Option<FwPullStatus> {
        if self.prefs.fw_pull == FW_PULL_OFF && !self.fwpull.once {
            Some(FwPullStatus::Off)
        } else if self.ota.roots.is_empty() || self.ota.target.is_none() {
            Some(FwPullStatus::NotTrusted)
        } else {
            None
        }
    }

    /// When [`Self::fw_pull_loop`] is next due.
    pub(super) fn fw_pull_timer(&self) -> Option<u64> {
        (self.fw_pull_allowed().is_none() && self.fwpull.step != Step::Ready).then_some(self.fwpull.at)
    }

    pub(super) fn fw_pull_loop(&mut self, ctx: &mut MeshCtx) {
        if let Some(status) = self.fw_pull_allowed() {
            if self.fwpull.step != Step::Sleep {
                self.fw_pull_reset();
            }
            self.fwpull.status = status;
            return;
        }
        if matches!(self.fwpull.status, FwPullStatus::Off | FwPullStatus::NotTrusted) {
            self.fwpull.status = FwPullStatus::Idle { up_to_date: false };
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
                self.fw_pull_fail(ctx, "no answer from the repeater");
            }
            Step::Query => self.fw_query(ctx),
            Step::Get { offset } => self.fw_get(ctx, offset),
            Step::Ready => {}
        }
    }

    /// [`FwPullStatus`] in a few words, for `fw_status` (no commas: the custom variables are a
    /// comma-separated list).
    pub fn fw_status_text(&self) -> String {
        match &self.fwpull.status {
            FwPullStatus::Off => "off".into(),
            FwPullStatus::NotTrusted => "needs a pinned root key (ota_root)".into(),
            FwPullStatus::Idle { up_to_date: true } => "up to date".into(),
            FwPullStatus::Idle { up_to_date: false } => "waiting".into(),
            FwPullStatus::NoRepeater => "no repeater heard directly".into(),
            FwPullStatus::Checking => "checking".into(),
            FwPullStatus::RepeaterFetching => "the repeater is fetching it".into(),
            FwPullStatus::Loading { version, done, total } => alloc::format!("loading {version}: {done} of {total} bytes"),
            FwPullStatus::Ready { version } => alloc::format!("{version} ready to install"),
            FwPullStatus::Failed(why) => alloc::format!("failed: {}", why.replace(',', ";")),
        }
    }

    /// `fw update`: ask the repeater now, and install what comes as soon as it is in (the device
    /// restarts), whatever `fw_pull` says. An update already fetched is installed at once.
    pub fn fw_update_now(&mut self) -> Result<String, String> {
        if self.ota.roots.is_empty() {
            return Err("pin the release root key first (ota_root)".into());
        }
        if self.ota.target.is_none() {
            return Err("no update slot on this device".into());
        }
        if self.fwpull.step == Step::Ready {
            self.fw_install()?;
            return Ok("installing, the device restarts".into());
        }
        self.fwpull.once = true;
        if self.fwpull.step == Step::Sleep {
            self.fwpull.at = 0;
        }
        Ok("asking the repeater; the update installs itself when it is in (fw status)".into())
    }

    /// Check for an update now (the device's button, or the app).
    pub fn fw_check_now(&mut self, ctx: &MeshCtx) {
        if self.fwpull.step == Step::Sleep {
            self.fwpull.at = ctx.now;
        }
    }

    /// Install the fetched update: the device reboots into it.
    pub fn fw_install(&mut self) -> Result<(), String> {
        if self.fwpull.step != Step::Ready {
            return Err("no update fetched".into());
        }
        let (reply, outcome) = self.ota.handle(&[REQ_TYPE_OTA_COMMIT], FIRMWARE_VERSION);
        match outcome {
            OtaOutcome::Committed => {
                self.effects.reboot = true;
                Ok(())
            }
            OtaOutcome::CommitFailed(line) => {
                self.fw_pull_reset();
                self.fwpull.status = FwPullStatus::Failed(line.clone());
                Err(line)
            }
            OtaOutcome::None => {
                let why = fwshare::refusal(reply.and_then(|r| r.first().copied()).unwrap_or(0)).to_string();
                self.fw_pull_reset();
                self.fwpull.status = FwPullStatus::Failed(why.clone());
                Err(why)
            }
        }
    }

    fn fw_pull_reset(&mut self) {
        let status = core::mem::replace(&mut self.fwpull.status, FwPullStatus::Off);
        self.fwpull = FwPull { status, at: 0, ..FwPull::default() };
    }

    fn fw_sleep(&mut self, ctx: &MeshCtx, ms: u64, status: FwPullStatus) {
        self.fw_pull_reset();
        self.fwpull.status = status;
        self.fwpull.at = ctx.now + ms;
    }

    fn fw_pull_fail(&mut self, ctx: &MeshCtx, why: &str) {
        self.fw_sleep(ctx, RETRY_LATER_MS, FwPullStatus::Failed(why.into()));
    }

    /// The repeater to ask: the one set (`fw_source`), else the best heard directly lately.
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
                return self.fw_sleep(ctx, RETRY_LATER_MS, FwPullStatus::NoRepeater);
            };
            self.fwpull.repeater = self.contacts[idx].id.pub_key;
        }
        let Some((base_len, base_sha)) = self.ota.running() else {
            return self.fw_pull_fail(ctx, "can't read this firmware's image");
        };
        let q = FwQuery {
            board: self.ota.build.board,
            role: self.ota.build.role,
            counter: self.ota.build.counter,
            base_len,
            base_sha8: base_sha[..8].try_into().unwrap(),
            channel: self.prefs.fw_channel,
        };
        self.fwpull.step = Step::Query;
        if !matches!(self.fwpull.status, FwPullStatus::RepeaterFetching) {
            self.fwpull.status = FwPullStatus::Checking;
        }
        if !self.fw_send(ctx, fwshare::ANON_REQ_TYPE_FW_QUERY, &q.to_bytes()) {
            self.fw_pull_fail(ctx, "the repeater is not a contact");
        }
    }

    fn fw_get(&mut self, ctx: &mut MeshCtx, offset: u32) {
        self.fwpull.step = Step::Get { offset };
        let len = self.fwpull.loader.as_ref().map_or(FW_PIECE, |l| l.want(offset)) as u8;
        let mut req = self.fwpull.file_id.to_le_bytes().to_vec();
        req.extend_from_slice(&offset.to_le_bytes());
        req.push(len);
        if !self.fw_send(ctx, fwshare::ANON_REQ_TYPE_FW_GET, &req) {
            self.fw_pull_fail(ctx, "the repeater is not a contact");
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
        match (self.fwpull.step, data[4]) {
            (Step::Query, status::READY) if body.len() >= 13 => {
                let len = read_u32(body, 0);
                let vlen = (body[12] as usize).min(body.len() - 13);
                self.fwpull.file_id = read_u32(body, 4);
                self.fwpull.version = String::from_utf8_lossy(&body[13..13 + vlen]).into_owned();
                self.fwpull.loader = Some(Loader::new(len, self.prefs.fw_channel));
                self.fwpull.status = FwPullStatus::Loading { version: self.fwpull.version.clone(), done: 0, total: len };
                self.fw_get(ctx, 0);
            }
            (Step::Query, status::FETCHING) => {
                let secs = body.get(..2).map_or(60, |b| u16::from_le_bytes([b[0], b[1]])).clamp(10, 900);
                self.fwpull.step = Step::Sleep;
                self.fwpull.status = FwPullStatus::RepeaterFetching;
                self.fwpull.at = ctx.now + secs as u64 * 1000;
            }
            (Step::Query, status::NONE) => self.fw_sleep(ctx, CHECK_EVERY_MS, FwPullStatus::Idle { up_to_date: true }),
            (Step::Query, _) => self.fw_pull_fail(ctx, "the repeater has no updates now"),
            (Step::Get { offset }, status::READY) if read_u32(body, 0) == offset && body.len() > 4 => {
                if let Err(why) = self.fw_piece(ctx, offset, &body[4..]) {
                    self.fw_pull_fail(ctx, why);
                }
            }
            (Step::Get { .. }, status::CHANGED) => {
                // the repeater holds another file now: start over
                self.fwpull.step = Step::Query;
                self.fwpull.status = FwPullStatus::Checking;
                self.fw_query(ctx);
            }
            (Step::Get { .. }, _) => self.fw_pull_fail(ctx, "the repeater could not send the update"),
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
        if let FwPullStatus::Loading { done, .. } = &mut self.fwpull.status {
            *done = next.unwrap_or(loader.file_len());
        }
        if let Some(next) = next {
            self.fwpull.step = Step::Get { offset: next };
            self.fwpull.at = ctx.now + PIECE_GAP_MS;
            return Ok(());
        }
        // the whole patch is in the session: wait for the word to install (or install now)
        self.fwpull.step = Step::Ready;
        self.fwpull.status = FwPullStatus::Ready { version: self.fwpull.version.clone() };
        if self.prefs.fw_pull == FW_PULL_AUTO || self.fwpull.once {
            let _ = self.fw_install();
        }
        Ok(())
    }
}
