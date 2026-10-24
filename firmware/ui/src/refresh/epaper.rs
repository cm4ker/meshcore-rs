//! E-paper: a page turn, a notice, a pairing PIN, Bluetooth, the home page or new messages change
//! the screen at once; anything else (counters, ages, the battery) at most every
//! [`MIN_REFRESH_MS`]: an e-paper keeps its image without power, and refreshes are slow and wear
//! it. Refreshes are partial, but a partial refresh drives only the pixels that change and the
//! rest slowly fades, so now and then the page is redrawn in full, which flashes the screen: once
//! nothing has changed for [`QUIET_MS`] after [`CLEAN_AFTER_PARTIALS`] partial refreshes (or
//! after any, if the last full one is [`CLEAN_EVERY_MS`] old), and at every [`FULL_EVERY`]th
//! refresh in a row.
//!
//! Times are in milliseconds since boot.

use crate::model::{Body, View};

pub const MIN_REFRESH_MS: u64 = 30_000;
pub const QUIET_MS: u64 = 20_000;
pub const CLEAN_AFTER_PARTIALS: u32 = 15;
pub const CLEAN_EVERY_MS: u64 = 60 * 60 * 1000;
pub const FULL_EVERY: u32 = 30;

/// A change worth a refresh right away (else it waits for [`MIN_REFRESH_MS`]).
pub fn urgent(old: &View, new: &View) -> bool {
    old.page != new.page
        || old.alert != new.alert
        || old.pin != new.pin
        || old.bar.ble != new.bar.ble
        || matches!(new.body, Body::Home { .. } | Body::Off { .. })
        || match (&old.body, &new.body) {
            // a new message while the page is open (not the ages ticking)
            (Body::Messages(a), Body::Messages(b)) => {
                a.len() != b.len() || a.iter().zip(b).any(|(a, b)| a.from != b.from || a.text != b.text)
            }
            // the update window opening, closing or a phone connecting (not the minutes left)
            (Body::Update(a), Body::Update(b)) => {
                a.window.as_ref().map(|w| (w.pin, w.connected)) != b.window.as_ref().map(|w| (w.pin, w.connected))
            }
            _ => false,
        }
}

/// What is on the screen, and how worn the image is.
#[derive(Default)]
pub struct Epaper {
    shown: Option<View>,
    /// The last refresh, and the last full one.
    last: u64,
    last_full: u64,
    /// Partial refreshes since the last full one.
    partials: u32,
}

impl Epaper {
    pub fn shown(&self) -> Option<&View> {
        self.shown.as_ref()
    }

    /// When to redraw the shown page in full if nothing else comes (restoring the contrast the
    /// partial refreshes wore off).
    pub fn clean_at(&self) -> Option<u64> {
        let quiet = self.last + QUIET_MS;
        match self.partials {
            0 => None,
            n if n >= CLEAN_AFTER_PARTIALS => Some(quiet),
            _ => Some(quiet.max(self.last_full + CLEAN_EVERY_MS)),
        }
    }

    /// The shown page was redrawn in full.
    pub fn cleaned(&mut self, now: u64) {
        self.partials = 0;
        self.last_full = now;
    }

    /// When `v` should go on the screen: none if it is there already, else the time (now or
    /// earlier for a change that cannot wait).
    pub fn due(&self, v: &View) -> Option<u64> {
        match &self.shown {
            Some(s) if s == v => None,
            Some(s) if !urgent(s, v) => Some(self.last + MIN_REFRESH_MS),
            _ => Some(0),
        }
    }

    /// Whether drawing `v` now should be a full refresh.
    pub fn full(&self, v: &View) -> bool {
        self.shown.is_none() || matches!(v.body, Body::Off { .. }) || self.partials + 1 >= FULL_EVERY
    }

    /// `v` went on the screen.
    pub fn drawn(&mut self, v: View, full: bool, now: u64) {
        self.partials = if full { 0 } else { self.partials + 1 };
        self.last = now;
        if full {
            self.last_full = now;
        }
        self.shown = Some(v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;

    #[test]
    fn counters_wait_page_turns_do_not() {
        let mut e = Epaper::default();
        let radio = samples::companion_view(crate::model::Page::Radio);
        assert_eq!(e.due(&radio), Some(0));
        assert!(e.full(&radio));
        e.drawn(radio.clone(), true, 1000);
        assert_eq!(e.due(&radio), None);
        let mut ticked = radio.clone();
        if let Body::Radio(r) = &mut ticked.body {
            r.rx += 1;
        }
        assert_eq!(e.due(&ticked), Some(1000 + MIN_REFRESH_MS));
        let home = samples::companion_view(crate::model::Page::Home);
        assert_eq!(e.due(&home), Some(0));
        assert!(!e.full(&home));
        e.drawn(home, false, 2000);
        assert_eq!(e.clean_at(), Some(1000 + CLEAN_EVERY_MS));
        for i in 0..CLEAN_AFTER_PARTIALS {
            e.drawn(ticked.clone(), false, 3000 + i as u64);
        }
        assert_eq!(e.clean_at(), Some(3000 + CLEAN_AFTER_PARTIALS as u64 - 1 + QUIET_MS));
        e.cleaned(100_000);
        assert_eq!(e.clean_at(), None);
    }
}
