# prisma-dualsense-native — repository rules

Windows-only native plugins behind Prisma's DualSense support. The Unity client
(`F:/repos/PrismaFramework`) consumes the built DLLs from
`Client/Assets/Plugins/NativePlugins/Windows`; it contains no Rust.

## Boundaries

- **The platform boundary is the transport.** `transport.rs` is hidapi
  (`hid.dll` / `hidraw` / IOKit); everything above it is platform-independent.
  Do not add platform `#[cfg]`s to `device.rs`, `ffi.rs` or `report.rs`: the
  only desktop gate is the `cfg(any(windows, linux, macos))` in `lib.rs` and
  the matching dependency block in `Cargo.toml`.
- **The C ABI is the product.** The Unity side (`DualSenseNativeHid.cs`,
  `PrismaNativeAudio.cs`) binds POD structs and semantic calls, never report
  bytes. Changing a struct, a field order or an export requires the same change
  there plus a bump of `ABI_VERSION` in both crates and both C# constants. The
  client refuses a mismatch at load time instead of corrupting memory.
- **`abi_layout_is_stable`** (hid crate) and **`PrismaAudioStatus`** are the
  size contract. Update the test together with the struct, never after.
- **The caller thread never writes to the device.** `set_state`/`set_rumble`
  only queue a `Copy` value into a fixed 8-slot action queue and wake the
  writer. Do not add blocking work to those paths: a Bluetooth write takes
  milliseconds.
- **No steady-state allocation.** Fixed state slots, a fixed input mailbox, a
  one-second ring, stack report buffers. `Vec` growth is only allowed for the
  audio ring's resample scratch and the synthesised waveform.
- **Motors in every report.** A report without the rumble flags re-arms the
  audio haptics and stops a vibration. `flush` always fills them.
- **USB by default.** Bluetooth output switches the pad to its full input report
  and can blind other readers (Steam Input, DS4Windows, DSX, reWAD, HidHide);
  it stays behind `PRISMA_DUALSENSE_BT=1`.
- **Off-safe on close.** Triggers free, motors stopped, default lightbar/player
  LEDs restored, otherwise the pad keeps the effect until power-off.

## Protocol facts

`input.rs` and `report.rs` document the byte facts measured on hardware
(gyro at 0x10 not 0x0D, status[1] active high, trigger block stride 11, CRC-32
seeded 0xA2 for Bluetooth). Keep the comments next to the offsets; the Unity
repo's `docs/dualsense-protocol-notes.md` is the long-form version.

## Verification

```bash
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all -- --check
```

Hardware smoke tests (require a pad on USB):

```bash
cargo run -p prisma-dualsense-hid --bin hid-smoke --release
cargo run -p prisma-dualsense-audio --bin audio-smoke --release
cargo run -p prisma-dualsense-hid --example hid-dump -- 5
```

After a change, rebuild `--release` and copy
`prisma_dualsense_hid.dll` / `prisma_dualsense_hid.pdb` /
`prisma_dualsense_audio.dll` into the Unity project; on Linux/macOS copy
`libprisma_dualsense_hid.so` / `.dylib` into
`Client/Assets/Plugins/NativePlugins/{Linux,macOS}`. CI
(`.github/workflows/ci.yml`) builds all three desktop targets — use its
artifacts rather than trusting a single-platform local build.
