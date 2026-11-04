//! How a node's own firmware update stands, told the same way by a repeater and a companion: the
//! `fw` command's reply, a companion's `fw_status`, and the screens (which word the same values in
//! Russian).
//!
//! `fw` answers in two lines, three on a repeater that hands updates on: [`head`] (the release that
//! runs, the channel, the mode), then [`FwState::line`] or [`trial_line`] (what happens now and
//! what comes next), then what the update store holds for the neighbours. The whole reply fits in
//! one answer over LoRa (159 bytes).

use alloc::format;
use alloc::string::String;
use core::fmt;

use crate::release;
use crate::util::epoch_to_datetime;

/// `fw.auto`: what a node does about updates by itself.
pub mod mode {
    /// Nothing; `fw check` and `fw update` still work.
    pub const OFF: u8 = 0;
    /// Checks and downloads; `fw install` installs.
    pub const DOWNLOAD: u8 = 1;
    /// Checks, downloads and installs.
    pub const INSTALL: u8 = 2;
}

pub fn mode_name(m: u8) -> &'static str {
    match m {
        mode::DOWNLOAD => "download",
        mode::INSTALL => "install",
        _ => "off",
    }
}

pub fn mode_by_name(name: &str) -> Option<u8> {
    match name {
        "off" => Some(mode::OFF),
        "download" => Some(mode::DOWNLOAD),
        "install" => Some(mode::INSTALL),
        _ => None,
    }
}

/// A release as people know it: its version (empty when the build did not say) and its number.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub counter: u32,
}

impl Release {
    pub fn new(version: &str, counter: u32) -> Self {
        Self { version: version.into(), counter }
    }
}

impl fmt::Display for Release {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.version.is_empty(), self.counter) {
            (false, 0) => write!(f, "{}", self.version),
            (false, n) => write!(f, "{} #{n}", self.version),
            (true, n) => write!(f, "#{n}"),
        }
    }
}

/// Where a node's own update stands.
#[derive(Clone, Debug, PartialEq)]
pub enum FwState {
    /// `fw.auto off`, and nothing was asked by hand since the start.
    Off,
    /// No root key is pinned: what comes would be installed unchecked, so nothing is fetched.
    NoRoot,
    /// No second slot to write an update into.
    NoSlot,
    /// The next check is due at `at` (wall clock; 0 = the first, a while after the start).
    Waiting { at: u32 },
    /// Asking the release server or a neighbour (by name).
    Asking { whom: String },
    /// Nothing newer, as `whom` said at `at`.
    UpToDate { whom: String, at: u32 },
    /// `whom` is fetching the update from its server; asked again shortly.
    Fetching { whom: String },
    Downloading { release: Release, from: String, done: u32, total: u32 },
    /// Downloaded and checked: `fw install` installs it.
    Ready { release: Release, len: u32 },
    /// Downloaded; installs once the neighbour loading an update from this node has its copy.
    Held { release: Release },
    /// Rolled back from before: not installed by itself again, a newer release is.
    Skipped { release: Release },
    Installing { release: Release },
    /// Why, in a few words; tried again at `retry` (wall clock; 0 = not by itself).
    Failed { why: String, retry: u32 },
}

impl FwState {
    /// The second line of `fw`.
    pub fn line(&self) -> String {
        match self {
            Self::Off => "auto off; check by hand: fw check".into(),
            Self::NoRoot => "needs a release key: set fw.root".into(),
            Self::NoSlot => "no update slot: update over USB".into(),
            Self::Waiting { at: 0 } => "first check 10 min after the start".into(),
            Self::Waiting { at } => format!("next check {}", hm(*at)),
            Self::Asking { whom } => format!("asking {whom}"),
            Self::UpToDate { whom, at } => format!("up to date, asked {whom} at {}", hm(*at)),
            Self::Fetching { whom } => format!("{whom} is fetching the update"),
            Self::Downloading { release, from, done, total } => {
                format!("downloading {release} from {from}: {} of {} KB", done / 1024, total.div_ceil(1024))
            }
            Self::Ready { release, len } => format!("{release} ready ({} KB): fw install", len.div_ceil(1024)),
            Self::Held { release } => format!("{release} ready: installs once a neighbour has its copy"),
            Self::Skipped { release } => format!("{release} was rolled back: waiting for a newer release"),
            Self::Installing { release } => format!("installing {release}: restarting, silent for 1-2 min"),
            Self::Failed { why, retry: 0 } => format!("failed: {why}"),
            Self::Failed { why, retry } => format!("failed: {why}; next try {}", hm(*retry)),
        }
    }
}

/// The first line of `fw`: the release that runs, the channel followed, the mode.
pub fn head(build_date: &str, channel: u8, mode: u8) -> String {
    format!("{} {}, auto {}", release::running_label(build_date), release::channel_name(channel), mode_name(mode))
}

/// The second line of `fw` after an update, until the new firmware is kept (in `kept_in_secs`):
/// `prev` is what `fw rollback` goes back to, if known.
pub fn trial_line(kept_in_secs: u64, prev: &str) -> String {
    let mins = kept_in_secs.div_ceil(60).max(1);
    if prev.is_empty() {
        format!("on trial, kept in {mins} min (fw rollback goes back)")
    } else {
        format!("on trial, kept in {mins} min (fw rollback: back to {prev})")
    }
}

/// The second line of `fw` after the node went back by itself, while nothing newer is known:
/// `prev` (the firmware left) failed its trial boots.
pub fn failed_line(prev: &str) -> String {
    let prev = if prev.is_empty() { "the update" } else { prev };
    format!("rolled back: {prev} did not start")
}

/// Wall-clock hours and minutes, UTC.
pub fn hm(t: u32) -> String {
    let (_, _, _, h, mi, _) = epoch_to_datetime(t);
    format!("{h:02}:{mi:02}")
}

/// The wall-clock time `ms` of uptime from `now` (also uptime), given the clock reads `clock` now.
pub fn wall(clock: u32, now: u64, ms: u64) -> u32 {
    clock.saturating_add((ms.saturating_sub(now) / 1000) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn lines_say_what_happens_and_what_next() {
        let r = Release::new("0.8.4", 47);
        // 2026-11-02 16:13:00 UTC
        let t = 1_793_635_980;
        assert_eq!(FwState::UpToDate { whom: "nw.ommesh.ru".into(), at: t }.line(), "up to date, asked nw.ommesh.ru at 16:13");
        assert_eq!(
            FwState::Downloading { release: r.clone(), from: "PKIO".into(), done: 41_000, total: 65_578 }.line(),
            "downloading 0.8.4 #47 from PKIO: 40 of 65 KB"
        );
        assert_eq!(FwState::Ready { release: r.clone(), len: 65_578 }.line(), "0.8.4 #47 ready (65 KB): fw install");
        assert_eq!(FwState::Failed { why: "PKIO stopped answering".into(), retry: t }.line(), "failed: PKIO stopped answering; next try 16:13");
        assert_eq!(FwState::Failed { why: "x".into(), retry: 0 }.line(), "failed: x");
        assert_eq!(trial_line(30, "0.8.3-dev.1 #45"), "on trial, kept in 1 min (fw rollback: back to 0.8.3-dev.1 #45)");
        assert_eq!(Release::new("", 45).to_string(), "#45");
    }

    #[test]
    fn modes_by_name() {
        for m in [mode::OFF, mode::DOWNLOAD, mode::INSTALL] {
            assert_eq!(mode_by_name(mode_name(m)), Some(m));
        }
        assert_eq!(mode_by_name("on"), None);
    }
}
