//! Files and photos sent to a contact on [`port::RESOURCE`](crate::port::RESOURCE).
//!
//! The sender first sends an offer (name, type, size, chunk count and hash),
//! then the file in chunks of up to [`chunk_room`] bytes, a few at a time.
//! Every message is an ordinary reliable DATA message, so each one is
//! acknowledged and retried by the node. The receiver collects chunks in any
//! order and checks the hash once all are in.
//!
//! ```text
//! offer:  0 | id(2) | size(4) | count(2) | sha256[..16] | name_len(1) | name | mime_len(1) | mime
//! chunk:  1 | id(2) | index(2) | bytes
//! cancel: 2 | id(2)
//! ```

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use sha2::{Digest, Sha256};

use crate::node::Micros;

/// Largest file a transfer carries. At the default radio profile this is
/// about 100 s of airtime per hop.
pub const MAX_FILE: usize = 32 * 1024;
pub const CHUNK_HEADER: usize = 5;
pub const HASH_LEN: usize = 16;
pub const MAX_FILE_NAME: usize = 64;
pub const MAX_MIME: usize = 48;
/// A transfer that gets no chunk for this long is dropped by the receiver.
pub const RECEIVE_TIMEOUT: Micros = 300_000_000;

const OFFER: u8 = 0;
const CHUNK: u8 = 1;
const CANCEL: u8 = 2;

/// Bytes of file data in one chunk when a message carries `max_payload` bytes.
pub const fn chunk_room(max_payload: usize) -> usize {
    max_payload - CHUNK_HEADER
}

pub fn hash(data: &[u8]) -> [u8; HASH_LEN] {
    let d = Sha256::digest(data);
    let mut h = [0u8; HASH_LEN];
    h.copy_from_slice(&d[..HASH_LEN]);
    h
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub id: u16,
    pub size: u32,
    pub count: u16,
    pub hash: [u8; HASH_LEN],
    pub name: String,
    pub mime: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Msg<'a> {
    Offer(Offer),
    Chunk { id: u16, index: u16, data: &'a [u8] },
    Cancel { id: u16 },
}

impl Offer {
    /// Describe `data` for sending in chunks of `room` bytes. `None` if the
    /// file is empty or larger than [`MAX_FILE`].
    pub fn new(id: u16, name: &str, mime: &str, data: &[u8], room: usize) -> Option<Offer> {
        if data.is_empty() || data.len() > MAX_FILE {
            return None;
        }
        Some(Offer {
            id,
            size: data.len() as u32,
            count: data.len().div_ceil(room) as u16,
            hash: hash(data),
            name: truncate(name, MAX_FILE_NAME).into(),
            mime: truncate(mime, MAX_MIME).into(),
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(27 + self.name.len() + self.mime.len());
        b.push(OFFER);
        b.extend_from_slice(&self.id.to_be_bytes());
        b.extend_from_slice(&self.size.to_be_bytes());
        b.extend_from_slice(&self.count.to_be_bytes());
        b.extend_from_slice(&self.hash);
        b.push(self.name.len() as u8);
        b.extend_from_slice(self.name.as_bytes());
        b.push(self.mime.len() as u8);
        b.extend_from_slice(self.mime.as_bytes());
        b
    }
}

/// Chunk `index` of `data`, split into pieces of `room` bytes.
pub fn encode_chunk(id: u16, index: u16, data: &[u8], room: usize) -> Vec<u8> {
    let start = index as usize * room;
    let piece = &data[start..(start + room).min(data.len())];
    let mut b = Vec::with_capacity(CHUNK_HEADER + piece.len());
    b.push(CHUNK);
    b.extend_from_slice(&id.to_be_bytes());
    b.extend_from_slice(&index.to_be_bytes());
    b.extend_from_slice(piece);
    b
}

pub fn encode_cancel(id: u16) -> Vec<u8> {
    let mut b = alloc::vec![CANCEL];
    b.extend_from_slice(&id.to_be_bytes());
    b
}

pub fn decode(payload: &[u8]) -> Option<Msg<'_>> {
    let (&kind, rest) = payload.split_first()?;
    let id = u16::from_be_bytes([*rest.first()?, *rest.get(1)?]);
    let rest = &rest[2..];
    match kind {
        OFFER => {
            let size = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?);
            let count = u16::from_be_bytes(rest.get(4..6)?.try_into().ok()?);
            let hash: [u8; HASH_LEN] = rest.get(6..6 + HASH_LEN)?.try_into().ok()?;
            let rest = &rest[6 + HASH_LEN..];
            let (name, rest) = take_str(rest)?;
            let (mime, _) = take_str(rest)?;
            if size == 0 || size as usize > MAX_FILE || count == 0 {
                return None;
            }
            Some(Msg::Offer(Offer {
                id,
                size,
                count,
                hash,
                name,
                mime,
            }))
        }
        CHUNK => {
            let index = u16::from_be_bytes(rest.get(..2)?.try_into().ok()?);
            Some(Msg::Chunk {
                id,
                index,
                data: &rest[2..],
            })
        }
        CANCEL => Some(Msg::Cancel { id }),
        _ => None,
    }
}

fn take_str(b: &[u8]) -> Option<(String, &[u8])> {
    let (&len, rest) = b.split_first()?;
    let s = rest.get(..len as usize)?;
    Some((
        String::from_utf8_lossy(s).into_owned(),
        &rest[len as usize..],
    ))
}

fn truncate(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

struct Partial {
    offer: Offer,
    chunks: Vec<Option<Vec<u8>>>,
    received: u16,
    deadline: Micros,
}

/// What [`Receiver::push`] made of a message.
#[derive(Debug, PartialEq, Eq)]
pub enum Received {
    /// A new transfer was offered.
    Offered(Offer),
    /// One more chunk arrived: (chunks received, chunk count).
    Progress { id: u16, received: u16, count: u16 },
    /// Every chunk is in and the hash matches.
    Complete { offer: Offer, data: Vec<u8> },
    /// The sender cancelled, or the reassembled file failed its hash check.
    Failed { id: u16 },
    /// Nothing to report (a duplicate, or a chunk for an unknown transfer).
    Nothing,
}

/// Collects incoming transfers. `K` identifies the sender, so transfers from
/// different senders never mix.
pub struct Receiver<K: Ord> {
    partial: BTreeMap<(K, u16), Partial>,
}

impl<K: Ord> Default for Receiver<K> {
    fn default() -> Self {
        Receiver {
            partial: BTreeMap::new(),
        }
    }
}

/// At most this many transfers are collected at once; more offers are ignored.
pub const MAX_INCOMING: usize = 4;

impl<K: Ord + Clone> Receiver<K> {
    pub fn push(&mut self, from: K, payload: &[u8], now: Micros) -> Received {
        match decode(payload) {
            Some(Msg::Offer(offer)) => {
                let key = (from, offer.id);
                if self.partial.contains_key(&key) || self.partial.len() >= MAX_INCOMING {
                    return Received::Nothing;
                }
                self.partial.insert(
                    key,
                    Partial {
                        chunks: alloc::vec![None; offer.count as usize],
                        offer: offer.clone(),
                        received: 0,
                        deadline: now + RECEIVE_TIMEOUT,
                    },
                );
                Received::Offered(offer)
            }
            Some(Msg::Chunk { id, index, data }) => {
                let key = (from, id);
                let Some(p) = self.partial.get_mut(&key) else {
                    return Received::Nothing;
                };
                let Some(slot) = p.chunks.get_mut(index as usize) else {
                    return Received::Nothing;
                };
                if slot.is_some() {
                    return Received::Nothing;
                }
                *slot = Some(data.to_vec());
                p.received += 1;
                p.deadline = now + RECEIVE_TIMEOUT;
                if p.received < p.offer.count {
                    return Received::Progress {
                        id,
                        received: p.received,
                        count: p.offer.count,
                    };
                }
                let p = self.partial.remove(&key).expect("present above");
                let data: Vec<u8> = p.chunks.into_iter().flatten().flatten().collect();
                if data.len() != p.offer.size as usize || hash(&data) != p.offer.hash {
                    return Received::Failed { id };
                }
                Received::Complete {
                    offer: p.offer,
                    data,
                }
            }
            Some(Msg::Cancel { id }) => match self.partial.remove(&(from, id)) {
                Some(_) => Received::Failed { id },
                None => Received::Nothing,
            },
            None => Received::Nothing,
        }
    }

    /// Transfers that stopped receiving chunks: (sender, id).
    pub fn expire(&mut self, now: Micros) -> Vec<(K, u16)> {
        let gone: Vec<(K, u16)> = self
            .partial
            .iter()
            .filter(|(_, p)| p.deadline <= now)
            .map(|(k, _)| k.clone())
            .collect();
        for k in &gone {
            self.partial.remove(k);
        }
        gone
    }

    pub fn next_deadline(&self) -> Option<Micros> {
        self.partial.values().map(|p| p.deadline).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 % 251) as u8).collect()
    }

    #[test]
    fn offer_roundtrip_and_limits() {
        let data = file(1000);
        let offer = Offer::new(9, "photo.jpg", "image/jpeg", &data, 100).unwrap();
        assert_eq!(offer.count, 10);
        assert_eq!(decode(&offer.encode()), Some(Msg::Offer(offer.clone())));
        assert!(Offer::new(1, "x", "", &[], 100).is_none());
        assert!(Offer::new(1, "x", "", &file(MAX_FILE + 1), 100).is_none());
        let long = "й".repeat(100);
        let o = Offer::new(1, &long, "", &data, 100).unwrap();
        assert!(o.name.len() <= MAX_FILE_NAME && long.starts_with(&o.name));
        assert_eq!(decode(&[1, 0]), None);
        assert_eq!(decode(&[7, 0, 1]), None);
    }

    #[test]
    fn chunks_reassemble_in_any_order() {
        let data = file(1050);
        let room = 100;
        let offer = Offer::new(3, "a.bin", "", &data, room).unwrap();
        let mut r = Receiver::default();
        assert_eq!(
            r.push("bob", &offer.encode(), 0),
            Received::Offered(offer.clone())
        );
        let mut last = Received::Nothing;
        for i in (0..offer.count).rev() {
            let c = encode_chunk(3, i, &data, room);
            assert!(c.len() <= CHUNK_HEADER + room);
            last = r.push("bob", &c, 1);
            // Duplicates and other senders' chunks change nothing.
            assert_eq!(r.push("eve", &c, 1), Received::Nothing);
            if i > 0 {
                assert_eq!(r.push("bob", &c, 1), Received::Nothing);
            }
        }
        assert_eq!(last, Received::Complete { offer, data });
        assert_eq!(r.next_deadline(), None);
    }

    #[test]
    fn bad_hash_cancel_and_timeout() {
        let data = file(250);
        let offer = Offer::new(1, "a", "", &data, 100).unwrap();
        let mut r = Receiver::default();
        r.push(1, &offer.encode(), 0);
        let mut bad = encode_chunk(1, 0, &data, 100);
        bad[CHUNK_HEADER] ^= 1;
        r.push(1, &bad, 0);
        r.push(1, &encode_chunk(1, 1, &data, 100), 0);
        assert_eq!(
            r.push(1, &encode_chunk(1, 2, &data, 100), 0),
            Received::Failed { id: 1 }
        );

        r.push(2, &offer.encode(), 0);
        assert_eq!(r.push(2, &encode_cancel(1), 0), Received::Failed { id: 1 });

        r.push(3, &offer.encode(), 10);
        r.push(3, &encode_chunk(1, 0, &data, 100), 20);
        assert!(r.expire(20 + RECEIVE_TIMEOUT - 1).is_empty());
        assert_eq!(r.expire(20 + RECEIVE_TIMEOUT), vec![(3, 1)]);
    }
}
