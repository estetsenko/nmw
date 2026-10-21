//! NMEA 0183 reader for the GPS module (an L76K on the Heltec V4): feeds
//! bytes in, keeps the latest fix from `RMC` and `GGA` sentences of any
//! talker (GP, GN, BD, GL...).

use nmw_core::position::Position;

const MAX_LINE: usize = 100;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Fix {
    pub lat: f64,
    pub lon: f64,
    pub alt_m: f32,
    pub satellites: u8,
    /// UTC time of day as hh, mm, ss.
    pub utc: (u8, u8, u8),
}

impl Fix {
    pub fn position(&self) -> Position {
        Position {
            lat: self.lat,
            lon: self.lon,
            alt_m: self.alt_m.clamp(i16::MIN as f32, i16::MAX as f32) as i16,
        }
    }
}

pub struct Gps {
    line: [u8; MAX_LINE],
    len: usize,
    overflow: bool,
    /// The latest valid fix, if the receiver has one now.
    fix: Option<Fix>,
    /// Satellites in use from the last GGA, also when there is no fix.
    satellites: u8,
    alt_m: f32,
    /// Sentences with a good checksum, to tell "no GPS" from "no fix".
    pub sentences: u32,
}

impl Default for Gps {
    fn default() -> Self {
        Gps {
            line: [0; MAX_LINE],
            len: 0,
            overflow: false,
            fix: None,
            satellites: 0,
            alt_m: 0.0,
            sentences: 0,
        }
    }
}

impl Gps {
    pub fn fix(&self) -> Option<Fix> {
        self.fix
    }

    pub fn satellites(&self) -> u8 {
        self.satellites
    }

    /// Feed bytes from the UART. Returns true when the fix changed.
    pub fn feed(&mut self, bytes: &[u8]) -> bool {
        let mut changed = false;
        for &b in bytes {
            match b {
                b'$' => {
                    self.len = 0;
                    self.overflow = false;
                    self.push(b);
                }
                b'\r' | b'\n' => {
                    if self.len > 0 && !self.overflow {
                        let line = self.line;
                        changed |= self.sentence(&line[..self.len]);
                    }
                    self.len = 0;
                }
                _ => self.push(b),
            }
        }
        changed
    }

    fn push(&mut self, b: u8) {
        if self.len < MAX_LINE {
            self.line[self.len] = b;
            self.len += 1;
        } else {
            self.overflow = true;
        }
    }

    fn sentence(&mut self, line: &[u8]) -> bool {
        let Some(body) = checked_body(line) else {
            return false;
        };
        self.sentences = self.sentences.wrapping_add(1);
        let mut f = body.split(|&c| c == b',');
        let Some(kind) = f.next().filter(|k| k.len() == 5) else {
            return false;
        };
        let fields: [&[u8]; 12] = core::array::from_fn(|_| f.next().unwrap_or(b""));
        match &kind[2..] {
            b"RMC" => {
                // time, status, lat, N/S, lon, E/W, ...
                let before = self.fix;
                self.fix = if fields[1] == b"A" {
                    match (coord(fields[2], fields[3]), coord(fields[4], fields[5])) {
                        (Some(lat), Some(lon)) => Some(Fix {
                            lat,
                            lon,
                            alt_m: self.alt_m,
                            satellites: self.satellites,
                            utc: utc(fields[0]),
                        }),
                        _ => None,
                    }
                } else {
                    None
                };
                self.fix != before
            }
            b"GGA" => {
                // time, lat, N/S, lon, E/W, quality, satellites, hdop, altitude
                self.satellites = number(fields[6]).unwrap_or(0.0) as u8;
                if let Some(alt) = number(fields[8]) {
                    self.alt_m = alt as f32;
                }
                if let Some(fix) = self.fix.as_mut() {
                    fix.satellites = self.satellites;
                    fix.alt_m = self.alt_m;
                }
                false
            }
            _ => false,
        }
    }
}

/// The part between `$` and `*` if the checksum matches.
fn checked_body(line: &[u8]) -> Option<&[u8]> {
    let line = line.strip_prefix(b"$")?;
    let star = line.iter().position(|&c| c == b'*')?;
    let (body, sum) = (&line[..star], &line[star + 1..]);
    let want = u8::from_str_radix(core::str::from_utf8(sum.get(..2)?).ok()?, 16).ok()?;
    let got = body.iter().fold(0u8, |a, &c| a ^ c);
    (got == want).then_some(body)
}

/// `ddmm.mmmm` or `dddmm.mmmm` plus hemisphere to signed degrees.
fn coord(v: &[u8], hemi: &[u8]) -> Option<f64> {
    let x = number(v)?;
    let deg = libm::floor(x / 100.0);
    let d = deg + (x - deg * 100.0) / 60.0;
    match hemi {
        b"N" | b"E" => Some(d),
        b"S" | b"W" => Some(-d),
        _ => None,
    }
}

fn utc(v: &[u8]) -> (u8, u8, u8) {
    let two = |i: usize| -> u8 {
        v.get(i..i + 2)
            .and_then(|s| core::str::from_utf8(s).ok())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    };
    (two(0), two(2), two(4))
}

fn number(v: &[u8]) -> Option<f64> {
    core::str::from_utf8(v).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GGA: &[u8] = b"$GNGGA,123519.00,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*77\r\n";
    const RMC: &[u8] =
        b"$GNRMC,123519.00,A,4807.038,N,01131.000,E,022.4,084.4,230394,003.1,W*5A\r\n";

    fn with_sum(body: &str) -> alloc::string::String {
        let sum = body.bytes().fold(0u8, |a, c| a ^ c);
        alloc::format!("${body}*{sum:02X}\r\n")
    }

    #[test]
    fn checksums_of_test_sentences() {
        assert!(
            checked_body(GGA.trim_ascii_end()).is_some(),
            "{}",
            with_sum("GNGGA,123519.00,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,")
        );
        assert!(
            checked_body(RMC.trim_ascii_end()).is_some(),
            "{}",
            with_sum("GNRMC,123519.00,A,4807.038,N,01131.000,E,022.4,084.4,230394,003.1,W")
        );
    }

    #[test]
    fn fix_from_gga_and_rmc_split_across_reads() {
        let mut g = Gps::default();
        assert!(!g.feed(GGA));
        assert_eq!(g.satellites(), 8);
        assert_eq!(g.fix(), None);
        let (a, b) = RMC.split_at(20);
        assert!(!g.feed(a));
        assert!(g.feed(b));
        let f = g.fix().unwrap();
        assert!((f.lat - 48.1173).abs() < 1e-6, "{f:?}");
        assert!((f.lon - 11.516_666).abs() < 1e-5, "{f:?}");
        assert_eq!((f.satellites, f.alt_m, f.utc), (8, 545.4, (12, 35, 19)));
        assert_eq!(g.sentences, 2);
        assert_eq!(f.position().alt_m, 545);
    }

    #[test]
    fn lost_fix_bad_checksum_and_south_west() {
        let mut g = Gps::default();
        g.feed(RMC);
        assert!(g.feed(with_sum("GPRMC,000000.00,V,,,,,,,,,,N").as_bytes()));
        assert_eq!(g.fix(), None);
        let mut bad = RMC.to_vec();
        bad[10] = b'9';
        assert!(!g.feed(&bad));
        assert_eq!(g.fix(), None);
        g.feed(with_sum("GPRMC,010203,A,3351.000,S,15112.600,W,0,0,010125,,").as_bytes());
        let f = g.fix().unwrap();
        assert!(
            (f.lat + 33.85).abs() < 1e-9 && (f.lon + 151.21).abs() < 1e-9,
            "{f:?}"
        );
        // Junk and overlong lines are ignored.
        assert!(!g.feed(&[b'$'; 300]));
        assert!(!g.feed(b"garbage\r\n"));
    }
}
