//! Text for the screen: what the fonts can draw, numbers and times in Russian.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// What the fonts can draw (ASCII and Cyrillic; Ё as Е, the fonts lack it); the rest (emoji,
/// accented Latin) is dropped.
pub fn clean(s: &str) -> String {
    let kept: String = s
        .chars()
        .map(|c| match c {
            'Ё' => 'Е',
            'ё' => 'е',
            c if c.is_whitespace() => ' ',
            c => c,
        })
        .filter(|&c| matches!(c, ' '..='~' | 'А'..='я'))
        .collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cyrillic to where U8g2's crox fonts keep it (code page 1251, in the Latin-1 range).
pub fn cp1251(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'А'..='я' => char::from_u32(0xC0 + c as u32 - 'А' as u32).unwrap_or('?'),
            'Ё' => 'Е',
            'ё' => 'е',
            c => c,
        })
        .collect()
}

/// The Russian noun form for `n`: one (1, 21...), few (2-4, 22-24...), many (the rest).
pub fn plural(n: u32, one: &'static str, few: &'static str, many: &'static str) -> &'static str {
    match (n % 10, n % 100) {
        (1, r) if r != 11 => one,
        (2..=4, r) if !(12..=14).contains(&r) => few,
        _ => many,
    }
}

/// How long ago, from [`coarse_minutes`].
pub fn ago(min: u32) -> String {
    match min {
        0 => "сейчас".into(),
        1..60 => format!("{min} мин"),
        60..1440 => format!("{} ч", min / 60),
        _ => format!("{} д", min / 1440),
    }
}

pub fn duration(min: u32) -> String {
    match min {
        0..60 => format!("{min} мин"),
        60..1440 => format!("{} ч {} мин", min / 60, min % 60),
        _ => format!("{} д {} ч", min / 1440, min % 1440 / 60),
    }
}

/// x.y from a value multiplied by 4 (SNR).
pub fn quarter(v: i16) -> String {
    let tenths = v as i32 * 10 / 4;
    let sign = if tenths < 0 { "-" } else { "" };
    format!("{sign}{}.{}", tenths.abs() / 10, tenths.abs() % 10)
}

/// As [`quarter`], with a "+" on positive values.
pub fn quarter_signed(v: i16) -> String {
    if v > 0 { format!("+{}", quarter(v)) } else { quarter(v) }
}

/// `v` to at most `decimals` decimals (up to 8), trailing zeros dropped. By hand: core's float
/// formatting is 9 KB on the firmware.
pub fn trim_float(v: f32, decimals: usize) -> String {
    let unit = 10u64.pow(decimals as u32);
    // exact: an f32 times such a power of ten fits an f64
    let scaled = v.abs() as f64 * unit as f64;
    let mut n = scaled as u64;
    let rest = scaled - n as f64;
    if rest > 0.5 || (rest == 0.5 && n % 2 == 1) {
        n += 1;
    }
    let sign = if v < 0.0 { "-" } else { "" };
    let s = format!("{sign}{}.{:0decimals$}", n / unit, n % unit);
    s.trim_end_matches('0').trim_end_matches('.').into()
}

pub fn micro_deg(v: i32) -> String {
    let sign = if v < 0 { "-" } else { "" };
    format!("{sign}{}.{:05}", v.unsigned_abs() / 1_000_000, v.unsigned_abs() % 1_000_000 / 10)
}

/// Tenths of a percent as "1.2%".
pub fn permille(v: u16) -> String {
    format!("{}.{}%", v / 10, v % 10)
}

pub fn volts(mv: u16) -> String {
    format!("{}.{:02} В", mv / 1000, mv % 1000 / 10)
}

/// Minutes since something happened, coarse enough that it changes rarely: whole minutes in the
/// first hour, then whole hours, then whole days (in minutes).
pub fn coarse_minutes(secs: u32) -> u32 {
    let m = secs / 60;
    match m {
        0..60 => m,
        60..1440 => m / 60 * 60,
        _ => m / 1440 * 1440,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_and_words() {
        assert_eq!(plural(1, "a", "b", "c"), "a");
        assert_eq!(plural(12, "a", "b", "c"), "c");
        assert_eq!(plural(23, "a", "b", "c"), "b");
        assert_eq!(quarter_signed(33), "+8.2");
        assert_eq!(quarter_signed(-11), "-2.7");
        assert_eq!(permille(12), "1.2%");
        assert_eq!(trim_float(869.525, 3), "869.525");
        assert_eq!(trim_float(868.0, 3), "868");
        assert_eq!(trim_float(62.5, 1), "62.5");
        assert_eq!(trim_float(250.0, 1), "250");
        assert_eq!(trim_float(-0.26, 1), "-0.3");
        assert_eq!(trim_float(7.8125, 3), "7.812");
        assert_eq!(clean("PKIO🎃 Ёлка"), "PKIO Елка");
        assert_eq!(coarse_minutes(3 * 3600 + 59), 180);
    }
}
