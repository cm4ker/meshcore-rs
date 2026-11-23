//! The speaker on a Pro Micro: a passive buzzer or a small loudspeaker on one pin, driven by the
//! chip's PWM (upstream's `PIN_MSG_TONE`, Arduino pin 9, which is P1.06). Short tones mark a new
//! message and a boot; each note is a few tens to a couple of hundred milliseconds of a square
//! wave.
//!
//! The sound is played by its own task, from [`SOUND`], so the loop that hears a message is not
//! held up by a melody. Whether the board sounds at all is the caller's decision
//! (`prefs.buzzer_quiet`, `set buzzer`); this module only plays what it is given.

use embassy_nrf::Peri;
use embassy_nrf::peripherals;
use embassy_nrf::pwm::{DutyCycle, Prescaler, SimpleConfig, SimplePwm};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::Timer;

/// The tone output: Arduino pin 9 on upstream's Pro Micro, P1.06 on the chip.
pub const TONE_PIN: usize = 6;

/// What to play: `(frequency in Hz, length in ms)`, a zero frequency resting.
pub enum Sound {
    /// Two rising notes as the firmware starts.
    Startup,
    /// A new message: two short pips.
    Message,
    /// A short pip for something done.
    Ack,
}

impl Sound {
    fn notes(self) -> &'static [(u32, u16)] {
        match self {
            Sound::Startup => &[(1568, 60), (2093, 90)],
            Sound::Message => &[(2794, 70), (0, 50), (2794, 70)],
            Sound::Ack => &[(2093, 40)],
        }
    }
}

/// The sound to play next; one queued before the last has ended follows it.
pub static SOUND: Signal<CriticalSectionRawMutex, Sound> = Signal::new();

/// The PWM on the tone pin, at the full 16 MHz clock: the lowest note is 488 Hz (the counter's
/// 32767 tops out there), and the few kHz a piezo is loudest at takes a duty cycle in the hundreds.
pub fn new(pwm: Peri<'static, peripherals::PWM0>, pin: Peri<'static, peripherals::P1_06>) -> SimplePwm<'static> {
    let mut config = SimpleConfig::default();
    config.prescaler = Prescaler::Div1;
    let pwm = SimplePwm::new_1ch(pwm, pin, &config);
    // `SimplePwm::new_1ch` enables PWM0 on creation; keep it off until a tone actually plays
    // (boards with `buzzer_quiet = true` never signal `SOUND`).
    pwm.disable();
    pwm
}

#[embassy_executor::task]
pub async fn task(mut pwm: SimplePwm<'static>) {
    loop {
        let sound = SOUND.wait().await;
        for &(freq, ms) in sound.notes() {
            match freq {
                0 => pwm.disable(),
                freq => {
                    pwm.set_period(freq);
                    pwm.enable();
                    // half the period high: the loudest a square wave gets
                    pwm.set_duty(0, DutyCycle::normal(pwm.max_duty() / 2));
                }
            }
            Timer::after_millis(ms as u64).await;
        }
        pwm.disable();
    }
}
