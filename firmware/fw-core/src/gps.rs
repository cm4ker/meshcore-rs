//! GPS receivers that report NMEA 0183 sentences over a serial line (the T-Echo's L76K, most
//! modules on other boards): the sentences' parser, and the latest reading, which the board's
//! serial task publishes and the companion reads.

use core::cell::Cell;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::Instant;

/// A reading older than this is stale: the receiver stopped talking.
const FRESH_MS: u64 = 5000;
/// Longest sentence kept (upstream's MicroNMEA buffer); NMEA's own limit is 82 characters.
const LINE_MAX: usize = 100;

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Fix {
    /// The receiver has a position (the latest GGA or RMC said so).
    pub valid: bool,
    /// Millionths of a degree.
    pub lat: i32,
    pub lon: i32,
    /// Millimetres above sea level.
    pub alt_mm: i32,
    /// Used for the position.
    pub satellites: u8,
    /// Seen, of all constellations.
    pub in_view: u8,
    /// UTC in seconds since 1970, once a sentence has carried the date.
    pub time: Option<u32>,
}

/// Turns the receiver's bytes into readings, from the sentences that carry the position and
/// time (GGA, RMC) and the satellites in view (GSV), of any constellation (`$GP`, `$GN`, `$BD`...).
pub struct Nmea {
    line: [u8; LINE_MAX],
    len: usize,
    fix: Fix,
    /// Days since 1970, from the latest RMC.
    days: Option<u32>,
    /// Seconds since midnight, from the latest GGA or RMC.
    day_secs: Option<u32>,
    /// Satellites in view by constellation (talker), from the latest GSV of each.
    in_view: [([u8; 2], u8); 6],
}

impl Default for Nmea {
    fn default() -> Self {
        Self::new()
    }
}

impl Nmea {
    pub const fn new() -> Self {
        Self {
            line: [0; LINE_MAX],
            len: 0,
            fix: Fix { valid: false, lat: 0, lon: 0, alt_mm: 0, satellites: 0, in_view: 0, time: None },
            days: None,
            day_secs: None,
            in_view: [([0; 2], 0); 6],
        }
    }

    /// One byte from the receiver; an intact GGA, RMC or GSV sentence ending with it updates the
    /// reading, and returns it.
    pub fn push(&mut self, b: u8) -> Option<Fix> {
        match b {
            b'$' => {
                self.line[0] = b;
                self.len = 1;
            }
            b'\r' | b'\n' => {
                let len = core::mem::take(&mut self.len);
                if len > 0 && self.sentence(len) {
                    return Some(self.fix);
                }
            }
            _ if self.len > 0 && self.len < LINE_MAX => {
                self.line[self.len] = b;
                self.len += 1;
            }
            _ => self.len = 0, // too long: drop it
        }
        None
    }

    /// `$<talker><type>,<fields>*<checksum>` in `line[..len]`: true if it updated the reading.
    fn sentence(&mut self, len: usize) -> bool {
        let copy = self.line;
        let line = &copy[..len];
        let Some(star) = line.iter().rposition(|&c| c == b'*') else {
            return false;
        };
        let body = &line[1..star];
        let sum = body.iter().fold(0u8, |a, c| a ^ c);
        if hex_byte(&line[star + 1..]) != Some(sum) || body.len() < 5 {
            return false;
        }
        let mut fields = body.split(|&c| c == b',');
        let id = fields.next().unwrap_or_default();
        let (talker, kind) = id.split_at(id.len().saturating_sub(3));
        let mut f: [&[u8]; 12] = [&[]; 12];
        for (slot, field) in f.iter_mut().zip(fields) {
            *slot = field;
        }
        match kind {
            // time, lat, N/S, lon, E/W, quality, satellites, HDOP, altitude, M, ...
            b"GGA" => {
                self.day_secs = time_of_day(f[0]).or(self.day_secs);
                self.fix.valid = matches!(f[5], [b'1'..=b'5', ..]);
                self.position(f[1], f[2], f[3], f[4]);
                if let Some(n) = number(f[6], 0) {
                    self.fix.satellites = n.clamp(0, 255) as u8;
                }
                if let Some(mm) = number(f[8], 3) {
                    self.fix.alt_mm = mm.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
                }
            }
            // time, status, lat, N/S, lon, E/W, speed, course, date, ...
            b"RMC" => {
                self.day_secs = time_of_day(f[0]).or(self.day_secs);
                self.fix.valid = f[1] == b"A";
                self.position(f[2], f[3], f[4], f[5]);
                self.days = date(f[8]).or(self.days);
            }
            // messages, this message, satellites in view, ...
            b"GSV" => {
                let (Ok(talker), Some(n)) = (<[u8; 2]>::try_from(talker), number(f[2], 0)) else {
                    return false;
                };
                let n = n.clamp(0, 255) as u8;
                match self.in_view.iter_mut().find(|(t, _)| *t == talker) {
                    Some(slot) => slot.1 = n,
                    None => {
                        if let Some(slot) = self.in_view.iter_mut().find(|(t, _)| *t == [0; 2]) {
                            *slot = (talker, n);
                        }
                    }
                }
                let total: u32 = self.in_view.iter().map(|(_, n)| *n as u32).sum();
                self.fix.in_view = total.min(255) as u8;
            }
            _ => return false,
        }
        self.fix.time = match (self.days, self.day_secs) {
            (Some(d), Some(s)) => Some(d * 86_400 + s),
            _ => None,
        };
        true
    }

    fn position(&mut self, lat: &[u8], ns: &[u8], lon: &[u8], ew: &[u8]) {
        if let (Some(lat), Some(lon)) = (degrees(lat), degrees(lon)) {
            self.fix.lat = if ns == b"S" { -lat } else { lat };
            self.fix.lon = if ew == b"W" { -lon } else { lon };
        }
    }
}

fn hex_byte(s: &[u8]) -> Option<u8> {
    let digit = |c: u8| (c as char).to_digit(16);
    match s {
        [h, l, ..] => Some((digit(*h)? * 16 + digit(*l)?) as u8),
        _ => None,
    }
}

/// A decimal number with `scale` digits kept after the point, as an integer: "120.5" with
/// scale 3 is 120500.
fn number(s: &[u8], scale: u32) -> Option<i64> {
    let (neg, s) = match s {
        [b'-', rest @ ..] => (true, rest),
        _ => (false, s),
    };
    let (int, frac) = match s.iter().position(|&c| c == b'.') {
        Some(p) => (&s[..p], &s[p + 1..]),
        None => (s, &[][..]),
    };
    if int.is_empty() || !int.iter().chain(frac).all(u8::is_ascii_digit) || int.len() > 12 {
        return None;
    }
    let mut v = int.iter().fold(0i64, |a, c| a * 10 + (c - b'0') as i64);
    for k in 0..scale as usize {
        v = v * 10 + frac.get(k).map_or(0, |c| (c - b'0') as i64);
    }
    Some(if neg { -v } else { v })
}

/// `ddmm.mmmm` or `dddmm.mmmm` in millionths of a degree.
fn degrees(s: &[u8]) -> Option<i32> {
    let v = number(s, 6)?; // minutes x 10^6, with the degrees x 100 above them
    let (deg, min) = (v / 100_000_000, v % 100_000_000);
    (deg <= 180 && min < 60_000_000).then(|| (deg * 1_000_000 + min / 60) as i32)
}

/// `hhmmss.ss` in seconds since midnight.
fn time_of_day(s: &[u8]) -> Option<u32> {
    let v = number(s, 0)? as u32;
    let (h, m, sec) = (v / 10_000, v / 100 % 100, v % 100);
    (s.len() >= 6 && h < 24 && m < 60 && sec < 61).then_some(h * 3600 + m * 60 + sec)
}

/// `ddmmyy` (21st century) in days since 1970.
fn date(s: &[u8]) -> Option<u32> {
    if s.len() != 6 {
        return None;
    }
    let v = number(s, 0)? as u32;
    let (d, m, y) = (v / 10_000, v / 100 % 100, 2000 + v % 100);
    if !(1..=31).contains(&d) || !(1..=12).contains(&m) {
        return None;
    }
    // days from the civil date (H. Hinnant's algorithm), years counted from March
    let y = if m <= 2 { y - 1 } else { y };
    let (era, yoe) = (y / 400, y % 400);
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// The latest reading and when it came.
static LATEST: Mutex<CriticalSectionRawMutex, Cell<Option<(Fix, u64)>>> = Mutex::new(Cell::new(None));

/// From the board's serial task: a new reading.
pub fn publish(fix: Fix) {
    let now = Instant::now().as_millis();
    LATEST.lock(|l| l.set(Some((fix, now))));
}

/// The receiver went off: forget its readings.
pub fn clear() {
    LATEST.lock(|l| l.set(None));
}

/// The latest reading, if the receiver is talking.
pub fn latest() -> Option<Fix> {
    let now = Instant::now().as_millis();
    LATEST.lock(|l| l.get()).filter(|(_, at)| now.saturating_sub(*at) < FRESH_MS).map(|(f, _)| f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(nmea: &mut Nmea, text: &str) -> Option<Fix> {
        text.bytes().fold(None, |last, b| nmea.push(b).or(last))
    }

    #[test]
    fn position_time_and_satellites() {
        let mut n = Nmea::new();
        let rmc = "$GNRMC,123519.00,A,5501.2345,N,07324.5678,E,0.02,,231026,,,A*5A\r\n";
        let fix = feed(&mut n, rmc).unwrap();
        assert!(fix.valid);
        assert_eq!((fix.lat, fix.lon), (55_020_575, 73_409_463));
        assert_eq!(fix.time, Some(1_792_758_919));
        let gga = "$GNGGA,123519.00,5501.2345,N,07324.5678,E,1,08,0.9,120.5,M,46.9,M,,*7C\r\n";
        let fix = feed(&mut n, gga).unwrap();
        assert_eq!((fix.satellites, fix.alt_mm), (8, 120_500));

        let south_west = "$GNRMC,000001.00,A,3330.0000,S,05830.0000,W,0.0,,010125,,,A*6A\r\n";
        let fix = feed(&mut n, south_west).unwrap();
        assert_eq!((fix.lat, fix.lon), (-33_500_000, -58_500_000));
        assert_eq!(fix.time, Some(1_735_689_601));
    }

    #[test]
    fn satellites_in_view_add_up_over_constellations() {
        let mut n = Nmea::new();
        let gps = "$GPGSV,3,1,11,10,63,137,17,07,61,098,15,05,59,290,20,08,54,157,30*70
";
        let beidou = "$BDGSV,1,1,03,01,45,120,30,02,30,200,25,03,10,300,20*60
";
        feed(&mut n, &with_checksum(gps));
        assert_eq!(feed(&mut n, &with_checksum(beidou)).map(|f| f.in_view), Some(14));
        // a newer count for GPS replaces the old one
        let gps = "$GPGSV,1,1,04,10,63,137,17,07,61,098,15,05,59,290,20,08,54,157,30*70
";
        assert_eq!(feed(&mut n, &with_checksum(gps)).map(|f| f.in_view), Some(7));
    }

    /// The sentence with its checksum recomputed.
    fn with_checksum(s: &str) -> String {
        let body = &s[1..s.find('*').unwrap()];
        let sum = body.bytes().fold(0u8, |a, c| a ^ c);
        format!("${body}*{sum:02X}
")
    }

    #[test]
    fn no_fix_bad_checksum_and_noise() {
        let mut n = Nmea::new();
        let fix = feed(&mut n, "$GPGGA,,,,,,0,00,99.99,,,,,,*48\r\n").unwrap();
        assert!(!fix.valid);
        assert_eq!(fix.time, None);
        assert_eq!(feed(&mut n, "$GNGGA,123519.00,5501.2345,N,07324.5678,E,1,08,0.9,120.5,M,46.9,M,,*7D\r\n"), None);
        assert_eq!(feed(&mut n, "$PCAS04,7*1E\r\n"), None);
        assert_eq!(feed(&mut n, "garbage\r\n$GNRMC,12"), None);
        // a sentence cut short by a new one
        let rmc = "$GNRMC,123519.00,A,5501.2345,N,07324.5678,E,0.02,,231026,,,A*5A\r\n";
        assert!(feed(&mut n, rmc).is_some_and(|f| f.valid));
    }
}
