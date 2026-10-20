//! Text longer than one packet, sent as up to [`MAX_PARTS`] messages on
//! [`port::TEXT_PART`](crate::port::TEXT_PART).
//!
//! Each part's payload is `msg_id(2) | index(1) | count(1) | bytes`, where the
//! bytes are a slice of the UTF-8 text cut at character boundaries. Parts may
//! arrive in any order; a message whose parts never all arrive is released
//! after [`PART_TIMEOUT`] with gaps marked.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::node::Micros;

pub const PART_HEADER: usize = 4;
pub const MAX_PARTS: usize = 16;
/// How long to wait for the rest of a message after its last part arrived.
pub const PART_TIMEOUT: Micros = 120_000_000;
/// Shown in place of a part that never arrived.
pub const MISSING: &str = " [part missing] ";

/// Longest text that fits in [`MAX_PARTS`] payloads of `max_payload` bytes.
pub const fn max_text_len(max_payload: usize) -> usize {
    MAX_PARTS * (max_payload - PART_HEADER)
}

/// Split `text` into part payloads of at most `max_payload` bytes each.
/// Returns `None` if it needs more than [`MAX_PARTS`] parts.
pub fn split(text: &str, max_payload: usize, msg_id: u16) -> Option<Vec<Vec<u8>>> {
    let room = max_payload - PART_HEADER;
    let mut chunks: Vec<&str> = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut end = rest.len().min(room);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        chunks.push(&rest[..end]);
        rest = &rest[end..];
    }
    if chunks.is_empty() || chunks.len() > MAX_PARTS {
        return None;
    }
    let count = chunks.len() as u8;
    Some(
        chunks
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mut p = Vec::with_capacity(PART_HEADER + c.len());
                p.extend_from_slice(&msg_id.to_be_bytes());
                p.push(i as u8);
                p.push(count);
                p.extend_from_slice(c.as_bytes());
                p
            })
            .collect(),
    )
}

struct Partial {
    parts: Vec<Option<Vec<u8>>>,
    deadline: Micros,
}

/// Collects parts per sender. `K` identifies the conversation and sender, so
/// parts from different senders never mix.
pub struct Reassembler<K: Ord> {
    partial: BTreeMap<(K, u16), Partial>,
}

impl<K: Ord + Clone> Default for Reassembler<K> {
    fn default() -> Self {
        Reassembler {
            partial: BTreeMap::new(),
        }
    }
}

impl<K: Ord + Clone> Reassembler<K> {
    /// Add a part. Returns the whole text once every part is in.
    pub fn push(&mut self, key: K, payload: &[u8], now: Micros) -> Option<String> {
        if payload.len() < PART_HEADER {
            return None;
        }
        let msg_id = u16::from_be_bytes([payload[0], payload[1]]);
        let (index, count) = (payload[2] as usize, payload[3] as usize);
        if count == 0 || count > MAX_PARTS || index >= count {
            return None;
        }
        let entry = self
            .partial
            .entry((key.clone(), msg_id))
            .or_insert_with(|| Partial {
                parts: alloc::vec![None; count],
                deadline: 0,
            });
        if entry.parts.len() != count {
            return None;
        }
        entry.parts[index] = Some(payload[PART_HEADER..].to_vec());
        entry.deadline = now + PART_TIMEOUT;
        if entry.parts.iter().all(Option::is_some) {
            let p = self.partial.remove(&(key, msg_id)).expect("just inserted");
            return Some(join(p.parts));
        }
        None
    }

    /// Release messages that stopped receiving parts, with gaps marked.
    pub fn expire(&mut self, now: Micros) -> Vec<(K, String)> {
        let done: Vec<(K, u16)> = self
            .partial
            .iter()
            .filter(|(_, p)| p.deadline <= now)
            .map(|(k, _)| k.clone())
            .collect();
        done.into_iter()
            .map(|k| {
                let p = self.partial.remove(&k).expect("key listed above");
                (k.0, join(p.parts))
            })
            .collect()
    }

    pub fn next_deadline(&self) -> Option<Micros> {
        self.partial.values().map(|p| p.deadline).min()
    }
}

fn join(parts: Vec<Option<Vec<u8>>>) -> String {
    let mut bytes = Vec::new();
    for p in parts {
        match p {
            Some(b) => bytes.extend_from_slice(&b),
            None => bytes.extend_from_slice(MISSING.as_bytes()),
        }
    }
    // Chunks are cut at character boundaries, so this only replaces garbage.
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_reassemble_out_of_order() {
        let text = "Привет, mesh! ".repeat(30); // multi-byte characters
        let parts = split(&text, 50, 7).unwrap();
        assert!(parts.len() > 1);
        assert!(parts.iter().all(|p| p.len() <= 50));
        let mut r = Reassembler::default();
        let mut out = None;
        for p in parts.iter().rev() {
            assert!(out.is_none());
            out = r.push("alice", p, 0);
        }
        assert_eq!(out.unwrap(), text);
        assert_eq!(r.next_deadline(), None);
    }

    #[test]
    fn limits_and_missing_parts() {
        let max = max_text_len(50);
        assert!(split(&"x".repeat(max), 50, 1).is_some());
        assert!(split(&"x".repeat(max + 1), 50, 1).is_none());
        assert!(split("", 50, 1).is_none());

        let parts = split(&"ab".repeat(60), 50, 3).unwrap();
        assert_eq!(parts.len(), 3);
        let mut r = Reassembler::default();
        assert_eq!(r.push(1, &parts[0], 10), None);
        assert_eq!(r.push(1, &parts[2], 20), None);
        // A different sender's parts with the same id stay separate.
        assert_eq!(r.push(2, &parts[1], 20), None);
        assert!(r.expire(20 + PART_TIMEOUT - 1).is_empty());
        let mut done = r.expire(20 + PART_TIMEOUT);
        done.sort();
        assert_eq!(done.len(), 2);
        assert!(done[0].1.contains(MISSING.trim()));
        assert_eq!(done[0].1.matches("part missing").count(), 1);
        assert_eq!(done[1].1.matches("part missing").count(), 2);
        assert_eq!(r.push(1, &[0, 1, 5, 2], 0), None, "index out of range");
    }
}
