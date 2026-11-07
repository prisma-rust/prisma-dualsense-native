//! Prisma's DualSense HID plugin.
//!
//! * [`report`] and [`input`] are pure protocol code (platform independent, unit
//!   tested): the 48/78-byte output reports, trigger encoding, Bluetooth CRC,
//!   and the 64-byte input report.
//! * [`transport`] is the HID transport: [hidapi](https://crates.io/crates/hidapi),
//!   so Windows (`hid.dll`), Linux (`hidraw`) and macOS (IOKit, opened shared)
//!   differ only in how bytes reach the device.
//! * [`device`] is the owner: a writer thread that coalesces semantic state,
//!   only writes what changed, rescans on loss, and keeps the pad off-safe.
//! * [`ffi`] is the C ABI the Unity client binds with `DllImport`.
#![cfg_attr(not(windows), allow(dead_code))]

pub mod input;
pub mod report;

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
pub mod device;
#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
pub mod ffi;
#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
pub mod transport;

pub use input::InputState as PrismaInputState;

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
pub use device::{Control, PadState, Rumble, Step, MAX_RUMBLE_STEPS};
#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
pub use ffi::*;
