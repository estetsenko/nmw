//! Host-side NMW runtime, used by the desktop app (and later bots and gateways).
//!
//! A [`Mesh`] is something a host app can talk to: today the
//! [`VirtualMesh`] (this node plus a simulated mesh with bots), later a
//! serial/BLE link to a real radio. [`Runtime`] drives a mesh in real time
//! on a background thread and hands [`HostEvent`]s to a callback.

mod runtime;
mod virtual_mesh;

pub use runtime::Runtime;
pub use virtual_mesh::{VirtualMesh, VirtualMeshConfig};

use nmw_core::Address;
use serde::Serialize;

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
}

pub fn addr_to_hex(a: &Address) -> String {
    a.0.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parse a 32-character hex address (spaces and colons are ignored).
pub fn parse_addr(s: &str) -> Option<Address> {
    let hex: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ':')
        .collect();
    if hex.len() != 32 || !hex.is_ascii() {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(Address(out))
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
}
