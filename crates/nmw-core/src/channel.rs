//! Group channels: a symmetric key shared by every member.
//!
//! A GROUP packet is flooded with `dst` set to the channel id (a hash of the
//! key) and is not acknowledged. Anyone holding the key can read it and post,
//! so `src` and the sender name inside are only as trustworthy as the members.

use sha2::{Digest, Sha256};

/// On-air channel id, carried in the packet's `dst` field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChannelId(pub [u8; 4]);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Channel {
    pub key: [u8; 32],
}

impl Channel {
    /// A private channel; share the key by QR code or link.
    pub fn from_key(key: [u8; 32]) -> Self {
        Channel { key }
    }

    /// A public channel anyone can join by name, like `#public`. The key is
    /// derived from the name (case-insensitive), so it hides nothing from
    /// someone who knows or guesses the name.
    pub fn public(name: &str) -> Self {
        let mut h = Sha256::new();
        h.update(b"nmw-v0 public channel\0");
        for c in name.trim().chars().flat_map(char::to_lowercase) {
            let mut b = [0u8; 4];
            h.update(c.encode_utf8(&mut b).as_bytes());
        }
        Channel {
            key: h.finalize().into(),
        }
    }

    pub fn id(&self) -> ChannelId {
        let mut h = Sha256::new();
        h.update(b"nmw-v0 channel id\0");
        h.update(self.key);
        let d = h.finalize();
        ChannelId([d[0], d[1], d[2], d[3]])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_names_are_case_insensitive() {
        assert_eq!(Channel::public("#Public"), Channel::public(" #public "));
        assert_ne!(Channel::public("#public"), Channel::public("#other"));
        assert_ne!(
            Channel::public("#public").id(),
            Channel::public("#other").id()
        );
    }
}
