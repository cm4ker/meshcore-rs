//! Pulling a firmware update from a neighbouring repeater (`fw_pull`, see [`crate::fwshare`]).
//! Connected to nothing, a companion that pins a root key asks the repeater it hears best directly
//! for a delta from the image it runs, fetches it a piece at a time into its update slot (through
//! the same session an app's update uses), and installs it when its user says so (`fw_pull on`,
//! a button on the device) or by itself (`fw_pull auto`).

use alloc::string::ToString;

use super::*;
use crate::FIRMWARE_VERSION;
use crate::fwshare::{self, DeltaHeader, FW_PIECE, FwQuery, HeaderSize, status};
use crate::ota::{
    OtaError, OtaOutcome, REQ_TYPE_OTA_BEGIN, REQ_TYPE_OTA_COMMIT, REQ_TYPE_OTA_DATA, REQ_TYPE_OTA_MANIFEST,
    REQ_TYPE_OTA_STATUS,
};

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
/// Envelope pieces fed to the update session.
const ENVELOPE_PIECE: usize = 128;

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
    file_len: u32,
    version: String,
    /// The start of the file, until its header is whole.
    head: Vec<u8>,
    header: Option<DeltaHeader>,
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
            file_len: 0,
            version: String::new(),
            head: Vec::new(),
            header: None,
        }
    }
}

/// Why a device turns a release down, for people.
fn refusal(code: u8) -> &'static str {
    match code {
        c if c == OtaError::Untrusted as u8 => "not signed by the pinned root key",
        c if c == OtaError::WrongBoard as u8 => "made for another board",
        c if c == OtaError::Downgrade as u8 => "older than this firmware",
        c if c == OtaError::BaseMismatch as u8 => "made for another firmware than this one",
        c if c == OtaError::BadEnvelope as u8 => "unreadable",
        c if c == OtaError::FlashError as u8 => "the update slot can't be written",
        _ => "refused",
    }
}

impl Companion {
    /// The node may pull: it is asked to, and it pins a root key (what a repeater hands on is only
    /// trusted for its signature) and has an update slot.
    fn fw_pull_allowed(&self) -> Option<FwPullStatus> {
        if self.prefs.fw_pull == FW_PULL_OFF {
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
                let why = refusal(reply.and_then(|r| r.first().copied()).unwrap_or(0)).to_string();
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
        let len = (self.fwpull.file_len - offset).min(FW_PIECE as u32) as u8;
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
                self.fwpull.file_len = len;
                self.fwpull.version = String::from_utf8_lossy(&body[13..13 + vlen]).into_owned();
                self.fwpull.head.clear();
                self.fwpull.header = None;
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

    /// A piece of the file at `offset`: the header first, then the patch, chunk by chunk.
    fn fw_piece(&mut self, ctx: &mut MeshCtx, offset: u32, piece: &[u8]) -> Result<(), &'static str> {
        // the datagram is zero-padded to the cipher block: cut to what was asked for
        let want = (self.fwpull.file_len - offset).min(FW_PIECE as u32) as usize;
        let Some(piece) = piece.get(..want) else {
            self.fwpull.at = ctx.now + PIECE_GAP_MS; // cut short: ask for it again
            return Ok(());
        };
        let mut next = offset + piece.len() as u32;
        match &self.fwpull.header {
            None => {
                self.fwpull.head.extend_from_slice(piece);
                match DeltaHeader::size(&self.fwpull.head) {
                    HeaderSize::NotDelta => return Err("the repeater sent no update file"),
                    HeaderSize::NeedMore(_) => {}
                    HeaderSize::Known(n) if self.fwpull.head.len() < n => {}
                    HeaderSize::Known(_) => next = self.fw_begin()?,
                }
            }
            Some(h) => {
                let index = (offset as usize - h.patch_offset()) / FW_PIECE;
                let mut req = alloc::vec![REQ_TYPE_OTA_DATA];
                req.extend_from_slice(&(index as u16).to_le_bytes());
                req.extend_from_slice(piece);
                self.ota.handle(&req, FIRMWARE_VERSION);
            }
        }
        if let FwPullStatus::Loading { done, .. } = &mut self.fwpull.status {
            *done = next;
        }
        if next < self.fwpull.file_len {
            self.fwpull.step = Step::Get { offset: next };
            self.fwpull.at = ctx.now + PIECE_GAP_MS;
            return Ok(());
        }
        self.fw_finished()
    }

    /// The header is whole: hand the release's envelope to the update session and begin it.
    /// Returns where the patch starts.
    fn fw_begin(&mut self) -> Result<u32, &'static str> {
        let h = DeltaHeader::parse(&self.fwpull.head).ok_or("the update file can't be read")?;
        let (run_len, run_sha) = self.ota.running().ok_or("can't read this firmware's image")?;
        if (h.base_len, h.base_sha256) != (run_len, run_sha) {
            return Err("made for another firmware than this one");
        }
        let envelope = self.fwpull.head[..h.envelope_len].to_vec();
        for (i, piece) in envelope.chunks(ENVELOPE_PIECE).enumerate() {
            let mut req = alloc::vec![REQ_TYPE_OTA_MANIFEST];
            req.extend_from_slice(&((i * ENVELOPE_PIECE) as u16).to_le_bytes());
            req.extend_from_slice(&(envelope.len() as u16).to_le_bytes());
            req.push(piece.len() as u8);
            req.extend_from_slice(piece);
            let (reply, _) = self.ota.handle(&req, FIRMWARE_VERSION);
            let code = reply.and_then(|r| r.first().copied()).unwrap_or(OtaError::BadEnvelope as u8);
            if code != 0 {
                return Err(refusal(code));
            }
        }
        let m = &h.envelope.manifest;
        let patch_len = self.fwpull.file_len.checked_sub(h.patch_offset() as u32).ok_or("the update file is cut short")?;
        let mut req = alloc::vec![REQ_TYPE_OTA_BEGIN];
        req.extend_from_slice(&patch_len.to_le_bytes());
        req.extend_from_slice(&m.image_sha256);
        req.extend_from_slice(&(FW_PIECE as u16).to_le_bytes());
        req.push(0b011); // zlib, delta
        req.extend_from_slice(&m.image_len.to_le_bytes());
        req.extend_from_slice(&h.base_len.to_le_bytes());
        req.extend_from_slice(&h.base_sha256);
        let (reply, _) = self.ota.handle(&req, FIRMWARE_VERSION);
        let code = reply.and_then(|r| r.first().copied()).unwrap_or(OtaError::BadParams as u8);
        if code != 0 {
            return Err(refusal(code));
        }
        let at = h.patch_offset() as u32;
        self.fwpull.header = Some(h);
        Ok(at)
    }

    /// The whole patch is in the session: check it holds every chunk, then wait for the word to
    /// install (or install now, with `fw_pull auto`).
    fn fw_finished(&mut self) -> Result<(), &'static str> {
        let (reply, _) = self.ota.handle(&[REQ_TYPE_OTA_STATUS, 0, 0], FIRMWARE_VERSION);
        let r = reply.unwrap_or_default();
        let complete = r.len() >= 5 && r[0] == 0 && r[1..3] == r[3..5];
        if !complete {
            return Err("pieces of the update are missing");
        }
        self.fwpull.step = Step::Ready;
        self.fwpull.status = FwPullStatus::Ready { version: self.fwpull.version.clone() };
        if self.prefs.fw_pull == FW_PULL_AUTO {
            let _ = self.fw_install();
        }
        Ok(())
    }
}
