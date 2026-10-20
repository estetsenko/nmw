//! On-air packet format.
//!
//! ```text
//! byte 0     ver(2) | type(3) | routed(1) | has_path(1) | ack_req(1)
//! byte 1     hop_limit(4) | hop_count(4)
//! bytes 2-5  packet id (u32, big endian; unique per source, used in the nonce)
//! bytes 6-9  dst (short address)
//! 10-13      src (short address)
//! [14]       path_len, then path_len x 1-byte repeater tags (if has_path)
//! ...        payload
//! ```

use crate::identity::ShortAddr;
use alloc::vec::Vec;

pub const VERSION: u8 = 0;
pub const HEADER_LEN: usize = 14;
pub const MAX_FRAME: usize = 255;
pub const MAX_PATH: usize = 15;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketType {
    Data = 0,
    Ack = 1,
    Announce = 2,
    Group = 3,
    Ctrl = 4,
    /// DATA prefixed with the sender's 32-byte public key, used until the
    /// destination has proven it knows the sender (so it can decrypt without
    /// having heard the sender's announce).
    DataKey = 5,
}

impl PacketType {
    pub fn is_data(self) -> bool {
        matches!(self, PacketType::Data | PacketType::DataKey)
    }
}

impl PacketType {
    fn from_bits(v: u8) -> Option<Self> {
        Some(match v {
            0 => PacketType::Data,
            1 => PacketType::Ack,
            2 => PacketType::Announce,
            3 => PacketType::Group,
            4 => PacketType::Ctrl,
            5 => PacketType::DataKey,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub ptype: PacketType,
    /// Source-routed along `path` (otherwise flooded, and repeaters append to `path`).
    pub routed: bool,
    pub ack_req: bool,
    pub hop_limit: u8,
    pub hop_count: u8,
    pub id: u32,
    pub dst: ShortAddr,
    pub src: ShortAddr,
    pub path: Vec<u8>,
    pub payload: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    TooShort,
    TooLong,
    BadVersion,
    BadType,
    BadPath,
}

impl Packet {
    fn byte0(&self) -> u8 {
        (VERSION << 6)
            | ((self.ptype as u8) << 3)
            | ((self.routed as u8) << 2)
            | ((!self.path.is_empty() as u8) << 1)
            | self.ack_req as u8
    }

    pub fn encoded_len(&self) -> usize {
        HEADER_LEN
            + if self.path.is_empty() {
                0
            } else {
                1 + self.path.len()
            }
            + self.payload.len()
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        out.push(self.byte0());
        out.push((self.hop_limit << 4) | (self.hop_count & 0x0f));
        out.extend_from_slice(&self.id.to_be_bytes());
        out.extend_from_slice(&self.dst.0);
        out.extend_from_slice(&self.src.0);
        if !self.path.is_empty() {
            out.push(self.path.len() as u8);
            out.extend_from_slice(&self.path);
        }
        out.extend_from_slice(&self.payload);
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Packet, DecodeError> {
        if buf.len() > MAX_FRAME {
            return Err(DecodeError::TooLong);
        }
        if buf.len() < HEADER_LEN {
            return Err(DecodeError::TooShort);
        }
        let b0 = buf[0];
        if b0 >> 6 != VERSION {
            return Err(DecodeError::BadVersion);
        }
        let ptype = PacketType::from_bits((b0 >> 3) & 0x07).ok_or(DecodeError::BadType)?;
        let mut pos = HEADER_LEN;
        let mut path = Vec::new();
        if b0 & 0b10 != 0 {
            let len = *buf.get(pos).ok_or(DecodeError::BadPath)? as usize;
            if len == 0 || len > MAX_PATH || buf.len() < pos + 1 + len {
                return Err(DecodeError::BadPath);
            }
            path.extend_from_slice(&buf[pos + 1..pos + 1 + len]);
            pos += 1 + len;
        }
        Ok(Packet {
            ptype,
            routed: b0 & 0b100 != 0,
            ack_req: b0 & 1 != 0,
            hop_limit: buf[1] >> 4,
            hop_count: buf[1] & 0x0f,
            id: u32::from_be_bytes([buf[2], buf[3], buf[4], buf[5]]),
            dst: ShortAddr([buf[6], buf[7], buf[8], buf[9]]),
            src: ShortAddr([buf[10], buf[11], buf[12], buf[13]]),
            path,
            payload: buf[pos..].to_vec(),
        })
    }

    /// Authenticated header fields: everything repeaters never change
    /// (version, type, routed, ack_req, id, dst, src). Hop counters and the path are excluded.
    pub fn aad(&self) -> [u8; 13] {
        let mut a = [0u8; 13];
        a[0] = self.byte0() & !0b10;
        a[1..5].copy_from_slice(&self.id.to_be_bytes());
        a[5..9].copy_from_slice(&self.dst.0);
        a[9..13].copy_from_slice(&self.src.0);
        a
    }

    /// AEAD nonce: unique per (source, packet id).
    pub fn nonce(&self) -> [u8; 12] {
        let mut n = [0u8; 12];
        n[..4].copy_from_slice(&self.src.0);
        n[4..8].copy_from_slice(&self.id.to_be_bytes());
        n[8] = self.ptype as u8;
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn sample() -> Packet {
        Packet {
            ptype: PacketType::Data,
            routed: true,
            ack_req: true,
            hop_limit: 4,
            hop_count: 1,
            id: 0xdead_beef,
            dst: ShortAddr([1, 2, 3, 4]),
            src: ShortAddr([5, 6, 7, 8]),
            path: vec![0xaa, 0xbb],
            payload: vec![9; 20],
        }
    }

    #[test]
    fn roundtrip() {
        let p = sample();
        let bytes = p.encode();
        assert_eq!(bytes.len(), HEADER_LEN + 3 + 20);
        assert_eq!(Packet::decode(&bytes).unwrap(), p);

        let mut q = sample();
        q.path.clear();
        q.routed = false;
        assert_eq!(Packet::decode(&q.encode()).unwrap(), q);
    }

    #[test]
    fn aad_ignores_mutable_fields() {
        let p = sample();
        let mut q = sample();
        q.hop_count = 3;
        q.path.push(0xcc);
        assert_eq!(p.aad(), q.aad());
        q.id += 1;
        assert_ne!(p.aad(), q.aad());
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(Packet::decode(&[0; 5]), Err(DecodeError::TooShort));
        let mut b = sample().encode();
        b[0] |= 0xc0;
        assert_eq!(Packet::decode(&b), Err(DecodeError::BadVersion));
        let mut b = sample().encode();
        b[HEADER_LEN] = 200;
        assert_eq!(Packet::decode(&b), Err(DecodeError::BadPath));
    }
}
