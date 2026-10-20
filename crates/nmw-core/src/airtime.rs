//! LoRa time-on-air (Semtech AN1200.13), used for timeouts and rebroadcast delays.

/// LoRa modulation settings for one radio profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoraProfile {
    pub spreading_factor: u8,
    pub bandwidth_hz: u32,
    /// Coding rate denominator offset: 1 = 4/5 ... 4 = 4/8.
    pub coding_rate: u8,
    pub preamble_symbols: u16,
}

impl LoraProfile {
    /// SF11 / BW250: km-scale hops.
    pub const LONG: LoraProfile = LoraProfile {
        spreading_factor: 11,
        bandwidth_hz: 250_000,
        coding_rate: 1,
        preamble_symbols: 16,
    };
    /// SF9 / BW250: the v1 primary profile.
    pub const MEDIUM: LoraProfile = LoraProfile {
        spreading_factor: 9,
        bandwidth_hz: 250_000,
        coding_rate: 1,
        preamble_symbols: 16,
    };
    /// SF7 / BW500: short hops, live voice.
    pub const FAST: LoraProfile = LoraProfile {
        spreading_factor: 7,
        bandwidth_hz: 500_000,
        coding_rate: 1,
        preamble_symbols: 16,
    };

    /// Symbol duration in microseconds.
    pub fn symbol_us(&self) -> u64 {
        (1u64 << self.spreading_factor) * 1_000_000 / self.bandwidth_hz as u64
    }

    /// Time on air in microseconds for a `len`-byte payload
    /// (explicit header, CRC on, low-data-rate optimisation when symbols >= 16 ms).
    pub fn airtime_us(&self, len: usize) -> u64 {
        let sf = self.spreading_factor as i64;
        let tsym = self.symbol_us();
        let de = if tsym >= 16_000 { 1 } else { 0 };
        let num = 8 * len as i64 - 4 * sf + 28 + 16;
        let den = 4 * (sf - 2 * de);
        let blocks = if num > 0 { (num + den - 1) / den } else { 0 };
        let payload_symbols = 8 + (blocks * (self.coding_rate as i64 + 4)) as u64;
        let preamble_us = (self.preamble_symbols as u64 * 4 + 17) * tsym / 4;
        preamble_us + payload_symbols * tsym
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn medium_profile_airtime() {
        // SF9/BW250: 2.048 ms symbols; 50 bytes -> 68 payload symbols + 20.25 preamble.
        assert_eq!(LoraProfile::MEDIUM.symbol_us(), 2048);
        assert_eq!(LoraProfile::MEDIUM.airtime_us(50), 41_472 + 68 * 2048);
    }

    #[test]
    fn airtime_grows_with_length() {
        let p = LoraProfile::LONG;
        assert!(p.airtime_us(10) < p.airtime_us(100));
        assert!(p.airtime_us(100) < p.airtime_us(255));
    }
}
