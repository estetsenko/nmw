//! Host-side NMW runtime, used by the desktop app (and later bots and gateways).
//!
//! A [`Mesh`] is something a host app can talk to: today the
//! [`VirtualMesh`] (this node plus a simulated mesh with bots), later a
//! serial/BLE link to a real radio. [`Runtime`] drives a mesh in real time
//! on a background thread and hands [`HostEvent`]s to a callback.

mod runtime;
mod virtual_mesh;

pub use runtime::Runtime;
pub use virtual_mesh::{VirtualMesh, VirtualMeshConfig, PUBLIC_CHANNEL};

use nmw_core::{Address, Channel, ChannelId};
use serde::{Deserialize, Serialize};

/// Something that happened, in the form the UI shows it.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostEvent {
    Contact {
        contact: ContactInfo,
    },
    Message {
        from: String,
        text: String,
        seq: u16,
        at_ms: u64,
    },
    Delivered {
        to: String,
        seq: u16,
    },
    Failed {
        to: String,
        seq: u16,
    },
    /// A message posted to a joined channel by someone else.
    GroupMessage {
        /// [`ChannelInfo::id`] of the channel.
        channel: String,
        /// Sender's short address (8 hex characters).
        from: String,
        /// Sender's full address when their signature checked out. `None`
        /// means unverified: anyone with the channel key could have sent it.
        from_addr: Option<String>,
        /// The verified sender's announced name; empty when unverified.
        name: String,
        text: String,
        at_ms: u64,
    },
    /// A frame this node sent or heard.
    Radio(RadioLogEntry),
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ContactInfo {
    /// 32 hex characters.
    pub addr: String,
    pub name: String,
    /// Repeaters on the known route (0 = direct neighbour), if a route is known.
    pub hops: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChannelInfo {
    /// On-air channel id, 8 hex characters.
    pub id: String,
    pub name: String,
    /// Public channels (`#name`) have a key derived from the name, so anyone
    /// who knows the name can read them. Private ones have a random key.
    pub public: bool,
    /// The channel key, 64 hex characters. Share it to invite someone.
    pub key: String,
}

impl ChannelInfo {
    pub fn channel(&self) -> Option<Channel> {
        parse_hex::<32>(&self.key).map(Channel::from_key)
    }
}

/// Work out the channel a user asked to join. A name starting with `#` and
/// no key is a public channel; otherwise the channel uses `key`, or a fresh
/// random key when none is given (a new private channel).
pub fn resolve_channel(name: &str, key: Option<&str>) -> Result<ChannelInfo, String> {
    let name = name.trim();
    if name.is_empty() || name.len() > 32 {
        return Err("a channel name is 1 to 32 characters".into());
    }
    let key = key.map(str::trim).filter(|k| !k.is_empty());
    let (channel, public) = match key {
        Some(k) => (
            Channel::from_key(parse_hex::<32>(k).ok_or("a channel key is 64 hex characters")?),
            false,
        ),
        None if name.starts_with('#') => (Channel::public(name), true),
        None => {
            let mut k = [0u8; 32];
            rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut k);
            (Channel::from_key(k), false)
        }
    };
    let name = if public {
        name.to_lowercase()
    } else {
        name.to_string()
    };
    Ok(ChannelInfo {
        id: channel_id_to_hex(&channel.id()),
        name,
        public,
        key: hex(&channel.key),
    })
}

pub fn channel_id_to_hex(id: &ChannelId) -> String {
    hex(&id.0)
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RadioLogEntry {
    pub at_ms: u64,
    /// "tx" or "rx"
    pub dir: &'static str,
    /// Name of the other side for "rx" (the transmitter); our own name for "tx".
    pub peer: String,
    /// Packet type, e.g. "DATA", "ACK", "ANNOUNCE".
    pub kind: String,
    pub len: usize,
    /// Repeater tags in the packet's path.
    pub path_len: usize,
    pub routed: bool,
    pub airtime_ms: Option<f64>,
    pub snr_db: Option<f32>,
    /// "ok", "collision" or "lost" for "rx".
    pub outcome: Option<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Status {
    pub addr: String,
    pub name: String,
    /// "virtual" for the simulated mesh.
    pub backend: &'static str,
    pub time_ms: u64,
    /// Share of time this node has spent transmitting.
    pub airtime_pct: f64,
    pub neighbours: usize,
    pub nodes: usize,
}

/// A mesh connection the host app drives.
pub trait Mesh: Send {
    /// Process everything up to `t` microseconds since the mesh started.
    fn advance_to(&mut self, t: u64);
    fn now(&self) -> u64;
    fn send_text(&mut self, to: &Address, text: &str) -> Result<u16, String>;
    fn announce(&mut self);
    fn contacts(&self) -> Vec<ContactInfo>;
    fn status(&self) -> Status;
    fn drain_events(&mut self) -> Vec<HostEvent>;
    /// Join a channel (see [`resolve_channel`]); joining twice is harmless.
    fn join_channel(&mut self, name: &str, key: Option<&str>) -> Result<ChannelInfo, String>;
    fn leave_channel(&mut self, id: &str);
    fn channels(&self) -> Vec<ChannelInfo>;
    fn send_group(&mut self, id: &str, text: &str) -> Result<(), String>;
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn parse_hex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let hex: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ':')
        .collect();
    if hex.len() != 2 * N || !hex.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

pub fn addr_to_hex(a: &Address) -> String {
    hex(&a.0)
}

/// Parse a 32-character hex address (spaces and colons are ignored).
pub fn parse_addr(s: &str) -> Option<Address> {
    parse_hex::<16>(s).map(Address)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_hex_roundtrip() {
        let a = Address([0xab; 16]);
        let h = addr_to_hex(&a);
        assert_eq!(h.len(), 32);
        assert_eq!(parse_addr(&h), Some(a));
        assert_eq!(parse_addr(&h.to_uppercase()), Some(a));
        assert_eq!(parse_addr("abab abab:abababababababababababab"), Some(a));
        assert_eq!(parse_addr("abc"), None);
        assert_eq!(parse_addr(&"zz".repeat(16)), None);
    }

    #[test]
    fn resolves_channels() {
        let a = resolve_channel("#Public", None).unwrap();
        assert!(a.public);
        assert_eq!(a.name, "#public");
        assert_eq!(a, resolve_channel(" #public", None).unwrap());
        assert_eq!(a.id.len(), 8);

        let fresh = resolve_channel("Team", None).unwrap();
        assert!(!fresh.public);
        assert_ne!(fresh.key, resolve_channel("Team", None).unwrap().key);
        let joined = resolve_channel("Our team", Some(&fresh.key)).unwrap();
        assert_eq!(joined.id, fresh.id);
        assert_eq!(joined.channel(), fresh.channel());

        assert!(resolve_channel("", None).is_err());
        assert!(resolve_channel("x", Some("abc")).is_err());
    }
}
