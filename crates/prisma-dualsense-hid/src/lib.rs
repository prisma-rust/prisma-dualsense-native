//! Prisma's DualSense HID plugin.
//!
//! * [`report`] and [`input`] are pure protocol code (platform independent, unit
//!   tested): the 48/78-byte output reports, trigger encoding, Bluetooth CRC,
//!   and the 64-byte input report.
//! * [`win32`] is the raw Windows HID transport (enumeration, overlapped read
//!   thread, synchronous writes).
//! * [`device`] is the owner: a writer thread that coalesces semantic state,
//!   only writes what changed, rescans on loss, and keeps the pad off-safe.
//! * [`ffi`] is the C ABI the Unity client binds with `DllImport`.
#![cfg_attr(not(windows), allow(dead_code))]

pub mod input;
pub mod report;

#[cfg(windows)]
pub mod device;
#[cfg(windows)]
pub mod ffi;
#[cfg(windows)]
pub mod win32;

pub use input::InputState as PrismaInputState;

#[cfg(windows)]
pub use device::{Control, PadState, Rumble, Step, MAX_RUMBLE_STEPS};
#[cfg(windows)]
pub use ffi::*;
