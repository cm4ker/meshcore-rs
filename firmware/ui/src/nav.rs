//! Which page is up, notices, and whether the screen is lit: the interface's state, driven by
//! intents ([`crate::input`]).

use crate::input::Intent;
use crate::model::{Action, Alert, Page};
use crate::text::battery_percent;

/// Without a press for this long, the interface returns to the home page.
const HOME_AFTER_MS: u64 = 60_000;
/// How long a notice ("advert sent") stays over the page.
const ALERT_MS: u64 = 2500;

pub struct Ui {
    pages: &'static [Page],
    idx: usize,
    last_press: u64,
    alert: Option<(Alert, u64)>,
    battery_pct: Option<u8>,
    /// A screen that goes dark (OLED, TFT): how long it stays lit after a press, and until when.
    lit_ms: Option<u64>,
    lit_until: u64,
    /// The press under way began on a dark screen: it only lights it.
    waking: bool,
}

impl Ui {
    /// `pages` in the order [`Intent::Next`] goes through them, the home page first; none = the
    /// board has no interface of its own.
    pub fn new(pages: &'static [Page]) -> Self {
        Self { pages, idx: 0, last_press: 0, alert: None, battery_pct: None, lit_ms: None, lit_until: 0, waking: false }
    }

    /// The screen goes dark `ms` after the last press (lit from `now` on); a press on a dark
    /// screen only lights it.
    pub fn with_screen_timeout(mut self, ms: u64, now: u64) -> Self {
        self.lit_ms = Some(ms);
        self.lit_until = now + ms;
        self
    }

    pub fn enabled(&self) -> bool {
        !self.pages.is_empty()
    }

    pub fn page(&self) -> Page {
        self.pages.get(self.idx).copied().unwrap_or(Page::Home)
    }

    /// The button went down: lights a dark screen at once (the press is decided later).
    pub fn button_down(&mut self, now: u64) {
        if !self.screen_on(now) {
            self.waking = true;
        }
        self.wake(now);
    }

    pub fn press(&mut self, intent: Intent, now: u64) -> Option<Action> {
        self.last_press = now;
        self.alert = None;
        self.wake(now);
        if core::mem::take(&mut self.waking) {
            return None;
        }
        let n = self.pages.len().max(1);
        match intent {
            Intent::Next => self.idx = (self.idx + 1) % n,
            Intent::Back => self.idx = (self.idx + n - 1) % n,
            Intent::Act => return self.page().action(),
        }
        None
    }

    /// Something worth seeing happened (a message, a pairing): light the screen.
    pub fn wake(&mut self, now: u64) {
        if let Some(ms) = self.lit_ms {
            self.lit_until = self.lit_until.max(now + ms);
        }
    }

    pub fn screen_on(&self, now: u64) -> bool {
        self.lit_ms.is_none() || now < self.lit_until
    }

    pub fn alert(&mut self, alert: Alert, now: u64) {
        self.alert = Some((alert, now + ALERT_MS));
        self.wake(now);
    }

    /// Time passes: notices expire, an idle interface goes home.
    pub fn tick(&mut self, now: u64) {
        if self.alert.is_some_and(|(_, until)| now >= until) {
            self.alert = None;
        }
        if self.idx != 0 && now - self.last_press >= HOME_AFTER_MS {
            self.idx = 0;
        }
    }

    /// Battery level to show, in steps of 5 % that move only once the reading has clearly
    /// crossed into the next step (a reading near a step's edge would flip it back and forth).
    pub fn battery_pct(&mut self, mv: u16) -> u8 {
        let pct = battery_percent(mv);
        let shown = match self.battery_pct {
            Some(s) if pct.abs_diff(s) < 5 + 2 => s,
            _ => (pct + 2) / 5 * 5,
        };
        self.battery_pct = Some(shown.min(100));
        shown.min(100)
    }

    /// The page's position and the notice, for the [`crate::View`].
    pub fn view_frame(&self) -> (u8, u8, Option<Alert>) {
        (self.idx as u8, self.pages.len() as u8, self.alert.map(|(a, _)| a))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGES: &[Page] = &[Page::Home, Page::Neighbours, Page::Update];

    #[test]
    fn a_press_on_a_dark_screen_only_lights_it() {
        let mut ui = Ui::new(PAGES).with_screen_timeout(60_000, 0);
        assert!(ui.screen_on(59_999) && !ui.screen_on(60_000));
        ui.button_down(70_000);
        assert!(ui.screen_on(70_000));
        assert_eq!(ui.press(Intent::Next, 70_300), None);
        assert_eq!(ui.page(), Page::Home);
        ui.button_down(71_000);
        assert_eq!(ui.press(Intent::Back, 71_300), None);
        assert_eq!(ui.page(), Page::Update);
        assert_eq!(ui.press(Intent::Act, 72_000), Some(Action::ToggleUpdate));
        // idle: dark, and back home
        ui.tick(200_000);
        assert!(!ui.screen_on(200_000));
        assert_eq!(ui.page(), Page::Home);
    }

    #[test]
    fn a_screen_that_keeps_its_image_never_goes_dark() {
        let mut ui = Ui::new(PAGES);
        assert!(ui.screen_on(10_000_000));
        ui.button_down(10_000_000);
        assert_eq!(ui.press(Intent::Next, 10_000_300), None);
        assert_eq!(ui.page(), Page::Neighbours);
    }
}
