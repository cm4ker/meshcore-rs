//! A repeater that updates itself (`set fw.auto on`, this firmware's extension, off by default).
//! With a Wi-Fi link it asks its update store (see [`crate::fwshare`], fetched from `fw.server`)
//! for a release newer than the build it runs, for its board, role and channel; feeds the delta
//! into its update session as an update over LoRa or BLE would; and restarts into it, on trial: an
//! image that does not come up healthy is rolled back. Only with a pinned root, since the server
//! is not trusted. `fw update` does one such check now, whatever `fw.auto` says.

use super::*;
use crate::fwshare::{FW_PIECE, FwQuery, Loader, Offer};
use crate::ota::REQ_TYPE_OTA_COMMIT;

/// The first check, a while after boot, then twice a day; each node a little later than the
/// next (by its key), so the repeaters of a mesh do not restart all at once.
const FIRST_CHECK_MS: u64 = 10 * 60_000;
const CHECK_EVERY_MS: u64 = 12 * 3_600_000;
const RETRY_LATER_MS: u64 = 3_600_000;
/// While an update from someone else is under way.
const BUSY_RETRY_MS: u64 = 10 * 60_000;
/// Pieces fed per pass of the main loop: it keeps serving the radio meanwhile.
const PIECES_PER_PASS: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Step {
    Sleep,
    Load { id: u32, offset: u32 },
}

pub(super) struct AutoUpdate {
    status: String,
    step: Step,
    at: u64,
    loader: Option<Loader>,
    version: String,
    /// `fw update`: one check that installs what it finds, whatever `fw.auto` says.
    once: bool,
}

impl AutoUpdate {
    pub(super) fn new() -> Self {
        Self { status: String::new(), step: Step::Sleep, at: FIRST_CHECK_MS, loader: None, version: String::new(), once: false }
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
        } else if self.fw_store.is_none() {
            Some("no update store on this board")
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
        }
        "OK - checking for an update; it installs itself and restarts (get fw.auto)".into()
    }

    fn auto_sleep(&mut self, ctx: &MeshCtx, ms: u64, status: String) {
        let a = &mut self.autoupdate;
        a.step = Step::Sleep;
        a.loader = None;
        a.once = false;
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
        }
    }

    fn auto_check(&mut self, ctx: &mut MeshCtx) {
        if self.ota.busy() {
            self.autoupdate.at = ctx.now + BUSY_RETRY_MS;
            return;
        }
        let Some((base_len, base_sha)) = self.ota.running() else {
            return self.auto_sleep(ctx, RETRY_LATER_MS, "can't read this firmware's image".into());
        };
        let q = FwQuery {
            board: self.ota.build.board,
            role: self.ota.build.role,
            counter: self.ota.build.counter,
            base_len,
            base_sha8: base_sha[..8].try_into().unwrap(),
            channel: self.prefs.fw_channel,
        };
        let Some(store) = self.fw_store.as_deref_mut() else { return };
        match store.offer(&q) {
            Offer::Ready { len, id, counter, version } if counter > q.counter => {
                self.autoupdate.loader = Some(Loader::new(len, self.prefs.fw_channel));
                self.autoupdate.step = Step::Load { id, offset: 0 };
                self.autoupdate.status = format!("loading {version} #{counter}");
                self.autoupdate.version = version;
                self.autoupdate.at = ctx.now;
            }
            Offer::Fetching { retry_secs } => {
                self.autoupdate.status = "the update is being fetched".into();
                self.autoupdate.at = ctx.now + retry_secs as u64 * 1000;
            }
            Offer::Ready { .. } | Offer::NoUpdate => {
                let next = CHECK_EVERY_MS + Self::auto_jitter(ctx);
                self.auto_sleep(ctx, next, format!("up to date (checked {})", fmt_time(ctx.current_time())));
            }
            Offer::Unavailable => self.auto_sleep(ctx, RETRY_LATER_MS, "the release server can't be reached".into()),
        }
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

    fn auto_install(&mut self, ctx: &mut MeshCtx) {
        let version = self.autoupdate.version.clone();
        let (reply, outcome) = self.ota.handle(&[REQ_TYPE_OTA_COMMIT], FIRMWARE_VERSION);
        if outcome == OtaOutcome::Committed {
            // nothing more to do: the node restarts into it
            self.autoupdate.step = Step::Sleep;
            self.autoupdate.loader = None;
            self.autoupdate.at = u64::MAX;
            self.autoupdate.status = format!("installing {version}");
            self.console(format!("fw: installing {version} from {}", self.prefs.fw_server));
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
