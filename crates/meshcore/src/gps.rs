//! A GPS position reading the firmware hands to the core, kept free of any hardware or driver
//! type (the NMEA parser and the receiver live in the firmware). The firmware fills
//! [`crate::companion::Board::gps`] / [`crate::repeater::BoardInfo::gps`] from its receiver, and
//! the `gps` CLI reads it back. Mirrors the firmware's `gps::Fix` field for field.

/// The latest GPS fix, as the firmware's receiver reported it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GpsReading {
    /// The receiver has a position.
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
    /// In view and heard: the receiver reports a signal-to-noise ratio for them.
    pub heard: u8,
    /// The strongest of those signals, in dB-Hz (0: none heard).
    pub best_snr: u8,
    /// UTC in seconds since 1970, once a sentence has carried the date.
    pub time: Option<u32>,
}
