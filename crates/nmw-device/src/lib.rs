//! Everything a standalone NMW radio needs that doesn't depend on the board:
//! the SX1262 driver, GPS parsing, the message inbox and the screen. The
//! firmware for each board (see `firmware/`) wires these to real pins; tests
//! run them on the host.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod app;
pub mod gps;
pub mod sx1262;
pub mod ui;
