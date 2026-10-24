//! Input as intents: next page, previous page, act on the page. Boards turn their buttons into
//! intents (a joystick maps its directions straight to them); the interface sees intents only.

/// Two clicks closer together than this are a double click (upstream `MULTI_CLICK_WINDOW_MS`).
const MULTI_CLICK_MS: u64 = 280;
/// A press this long is a long press (upstream's T-Echo button).
const LONG_PRESS_MS: u64 = 1000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Intent {
    Next,
    Back,
    Act,
}

/// One button, as upstream drives it: a click goes to the next page, a double click back, a
/// long press acts (reported while the button is still held, once it has been down long enough).
#[derive(Default)]
pub struct Button {
    down_at: Option<u64>,
    long_sent: bool,
    clicks: u8,
    last_up: u64,
}

impl Button {
    /// The button's level, sampled every loop pass.
    pub fn poll(&mut self, down: bool, now: u64) -> Option<Intent> {
        match (down, self.down_at) {
            (true, None) => {
                self.down_at = Some(now);
                None
            }
            (true, Some(t)) if !self.long_sent && now - t >= LONG_PRESS_MS => {
                self.long_sent = true;
                self.clicks = 0;
                Some(Intent::Act)
            }
            (false, Some(_)) => {
                self.down_at = None;
                if core::mem::take(&mut self.long_sent) {
                    return None;
                }
                self.clicks += 1;
                self.last_up = now;
                if self.clicks >= 2 {
                    self.clicks = 0;
                    return Some(Intent::Back);
                }
                None
            }
            (false, None) if self.clicks == 1 && now - self.last_up > MULTI_CLICK_MS => {
                self.clicks = 0;
                Some(Intent::Next)
            }
            _ => None,
        }
    }

    /// The button is down or a click may still become a double click: poll it often.
    pub fn busy(&self) -> bool {
        self.down_at.is_some() || self.clicks > 0
    }

    /// Down right now (wakes a dark screen at once, before the click is decided).
    pub fn is_down(&self) -> bool {
        self.down_at.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(b: &mut Button, levels: &[(bool, u64)]) -> alloc::vec::Vec<Intent> {
        levels.iter().filter_map(|&(down, t)| b.poll(down, t)).collect()
    }

    #[test]
    fn click_double_and_long_press() {
        let mut b = Button::default();
        assert_eq!(run(&mut b, &[(true, 0), (false, 100), (false, 300), (false, 400)]), [Intent::Next]);
        let mut b = Button::default();
        assert_eq!(run(&mut b, &[(true, 0), (false, 100), (true, 200), (false, 250), (false, 900)]), [Intent::Back]);
        let mut b = Button::default();
        assert_eq!(run(&mut b, &[(true, 0), (true, 999), (true, 1000), (true, 1500), (false, 1600), (false, 2500)]), [Intent::Act]);
    }
}
