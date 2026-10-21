//! A GPS position on [`port::POSITION`](crate::port::POSITION):
//! `lat_e7(4) | lon_e7(4) | alt_m(2)`, big-endian, degrees times 10^7 and
//! metres above sea level.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Position {
    pub lat: f64,
    pub lon: f64,
    pub alt_m: i16,
}

pub const LEN: usize = 10;

impl Position {
    pub fn encode(&self) -> [u8; LEN] {
        let mut b = [0u8; LEN];
        b[..4].copy_from_slice(&to_e7(self.lat, 90.0).to_be_bytes());
        b[4..8].copy_from_slice(&to_e7(self.lon, 180.0).to_be_bytes());
        b[8..].copy_from_slice(&self.alt_m.to_be_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Option<Position> {
        if b.len() < LEN {
            return None;
        }
        let lat = i32::from_be_bytes(b[..4].try_into().ok()?) as f64 / 1e7;
        let lon = i32::from_be_bytes(b[4..8].try_into().ok()?) as f64 / 1e7;
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            return None;
        }
        Some(Position {
            lat,
            lon,
            alt_m: i16::from_be_bytes([b[8], b[9]]),
        })
    }
}

fn to_e7(deg: f64, max: f64) -> i32 {
    let d = deg.clamp(-max, max) * 1e7;
    // `as` rounds towards zero; add half a unit for nearest (no_std has no round()).
    (if d >= 0.0 { d + 0.5 } else { d - 0.5 }) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_to_a_centimetre() {
        let p = Position {
            lat: 52.370_216_8,
            lon: -4.895_167_9,
            alt_m: -3,
        };
        let q = Position::decode(&p.encode()).unwrap();
        assert!((p.lat - q.lat).abs() < 1e-7 && (p.lon - q.lon).abs() < 1e-7);
        assert_eq!(q.alt_m, -3);
        assert_eq!(Position::decode(&[0; 9]), None);
        let mut bad = p.encode();
        bad[..4].copy_from_slice(&i32::MAX.to_be_bytes());
        assert_eq!(Position::decode(&bad), None);
    }
}
