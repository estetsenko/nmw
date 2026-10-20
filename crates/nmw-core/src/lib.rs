//! NMW mesh protocol core.
//!
//! Sans-IO implementation of the protocol described in `docs/architecture.md`:
//! the caller feeds received frames and the current time into a [`Node`] and
//! drains frames to transmit and application events. The same code runs in
//! firmware, the simulator and host apps.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod airtime;
pub mod crypto;
pub mod identity;
pub mod node;
pub mod packet;
mod rng;

pub use airtime::LoraProfile;
pub use identity::{Address, Identity, PublicIdentity, ShortAddr};
pub use node::{Event, Node, NodeConfig, Role, SendError, MAX_DATA};
pub use packet::{Packet, PacketType};

/// Well-known service ports.
pub mod port {
    pub const TEXT: u16 = 1;
    pub const RECEIPT: u16 = 2;
    pub const VOICE_NOTE: u16 = 3;
    pub const LIVE_VOICE: u16 = 4;
    pub const RESOURCE: u16 = 5;
    pub const POSITION: u16 = 6;
    pub const TELEMETRY: u16 = 7;
    pub const BOT: u16 = 8;
    pub const GATEWAY: u16 = 9;
}
