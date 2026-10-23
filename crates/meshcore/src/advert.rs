//! Advertisement `app_data` encoding: `flags(1) [lat(4) lon(4)] [feat1(2)] [feat2(2)] [name]`.

use alloc::string::String;

use crate::MAX_ADVERT_DATA_SIZE;
use crate::util::valid_utf8_prefix_len;

pub const ADV_TYPE_NONE: u8 = 0;
pub const ADV_TYPE_CHAT: u8 = 1;
pub const ADV_TYPE_REPEATER: u8 = 2;
pub const ADV_TYPE_ROOM: u8 = 3;
pub const ADV_TYPE_SENSOR: u8 = 4;

pub const ADV_LATLON_MASK: u8 = 0x10;
pub const ADV_FEAT1_MASK: u8 = 0x20;
pub const ADV_FEAT2_MASK: u8 = 0x40;
pub const ADV_NAME_MASK: u8 = 0x80;

#[derive(Clone, Debug, Default)]
pub struct AdvertData {
    pub adv_type: u8,
    pub name: Option<String>,
    /// Latitude/longitude in micro-degrees.
    pub location: Option<(i32, i32)>,
    pub feat1: u16,
    pub feat2: u16,
}

impl AdvertData {
    pub fn new(adv_type: u8, name: &str) -> Self {
        Self { adv_type, name: Some(name.into()), ..Default::default() }
    }

    pub fn with_location(mut self, lat: f64, lon: f64) -> Self {
        self.location = Some(((lat * 1e6) as i32, (lon * 1e6) as i32));
        self
    }

    /// Encode into `out` (`AdvertDataBuilder::encodeTo`). Returns the encoded length.
    pub fn encode(&self, out: &mut [u8; MAX_ADVERT_DATA_SIZE]) -> usize {
        out[0] = self.adv_type;
        let mut i = 1;
        if let Some((lat, lon)) = self.location {
            out[0] |= ADV_LATLON_MASK;
            out[i..i + 4].copy_from_slice(&lat.to_le_bytes());
            out[i + 4..i + 8].copy_from_slice(&lon.to_le_bytes());
            i += 8;
        }
        if self.feat1 != 0 {
            out[0] |= ADV_FEAT1_MASK;
            out[i..i + 2].copy_from_slice(&self.feat1.to_le_bytes());
            i += 2;
        }
        if self.feat2 != 0 {
            out[0] |= ADV_FEAT2_MASK;
            out[i..i + 2].copy_from_slice(&self.feat2.to_le_bytes());
            i += 2;
        }
        if let Some(name) = self.name.as_deref() {
            let n = valid_utf8_prefix_len(name.as_bytes(), MAX_ADVERT_DATA_SIZE - i);
            if n > 0 {
                out[0] |= ADV_NAME_MASK;
                out[i..i + n].copy_from_slice(&name.as_bytes()[..n]);
                i += n;
            }
        }
        i
    }

    /// Decode (`AdvertDataParser`). Returns `None` when the data is truncated.
    pub fn parse(data: &[u8]) -> Option<Self> {
        let flags = *data.first()?;
        let mut i = 1;
        let mut adv = AdvertData { adv_type: flags & 0x0F, ..Default::default() };
        if flags & ADV_LATLON_MASK != 0 {
            let lat = i32::from_le_bytes(data.get(i..i + 4)?.try_into().ok()?);
            let lon = i32::from_le_bytes(data.get(i + 4..i + 8)?.try_into().ok()?);
            adv.location = Some((lat, lon));
            i += 8;
        }
        if flags & ADV_FEAT1_MASK != 0 {
            adv.feat1 = u16::from_le_bytes(data.get(i..i + 2)?.try_into().ok()?);
            i += 2;
        }
        if flags & ADV_FEAT2_MASK != 0 {
            adv.feat2 = u16::from_le_bytes(data.get(i..i + 2)?.try_into().ok()?);
            i += 2;
        }
        if data.len() < i {
            return None;
        }
        if flags & ADV_NAME_MASK != 0 && data.len() > i {
            adv.name = Some(String::from_utf8_lossy(&data[i..]).into_owned());
        }
        Some(adv)
    }
}

/// Characters rejected in node names (`isValidName`).
pub fn is_valid_name(name: &str) -> bool {
    !name.chars().any(|c| matches!(c, '[' | ']' | '\\' | ':' | ',' | '?' | '*'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeater_advert_layout() {
        let adv = AdvertData::new(ADV_TYPE_REPEATER, "Rust Rptr").with_location(55.75, 37.6175);
        let mut buf = [0u8; MAX_ADVERT_DATA_SIZE];
        let n = adv.encode(&mut buf);
        assert_eq!(buf[0], ADV_TYPE_REPEATER | ADV_LATLON_MASK | ADV_NAME_MASK);
        assert_eq!(i32::from_le_bytes(buf[1..5].try_into().unwrap()), 55_750_000);
        assert_eq!(&buf[9..n], b"Rust Rptr");
        let back = AdvertData::parse(&buf[..n]).unwrap();
        assert_eq!(back.adv_type, ADV_TYPE_REPEATER);
        assert_eq!(back.name.as_deref(), Some("Rust Rptr"));
        assert_eq!(back.location, Some((55_750_000, 37_617_500)));
    }

    #[test]
    fn long_name_is_cut_on_char_boundary() {
        let adv = AdvertData::new(ADV_TYPE_REPEATER, "Ретранслятор-на-крыше-дома-12345");
        let mut buf = [0u8; MAX_ADVERT_DATA_SIZE];
        let n = adv.encode(&mut buf);
        assert!(n <= MAX_ADVERT_DATA_SIZE);
        assert!(core::str::from_utf8(&buf[1..n]).is_ok());
    }
}
