# prisma-dualsense-native

Windows native plugins behind Prisma's DualSense support: HID I/O and the
controller's USB audio endpoint (HD haptics / speaker).

This repository was split out of `F:\repos\PrismaFramework` (previously
`Client/Plugins/NativePlugins/Windows`). The Unity client keeps only the built
DLLs under `Client/Assets/Plugins/NativePlugins/Windows`; the sources live here.

## Crates

| Crate | DLL | Role |
| --- | --- | --- |
| `prisma-dualsense-hid` | `prisma_dualsense_hid.dll` | HID enumeration, output-report writer thread, input-report reader thread, protocol encoding/decoding |
| `prisma-dualsense-audio` | `prisma_dualsense_audio.dll` | Persistent WASAPI four-channel stream for the DualSense speaker/haptics channels |

## Design

The HID crate owns the device, not just the bytes:

- **Writer thread.** `set_state` only stores the latest semantic state; a
  dedicated thread coalesces it, writes only what changed, and never blocks
  the caller (a Bluetooth write can take milliseconds).
- **Reconnect/backoff.** A failed write or read marks the pad lost and the
  thread re-scans (five tries, one second apart).
- **Protocol correctness.** Every report carries the motors (a report without
  the rumble flags re-arms the audio haptics), the first report clears another
  host's motor/trigger attenuation, the lights are released once before the
  first colour, and the vibration emulation (legacy vs. v2) is picked from the
  firmware version (feature report `0x20`).
- **USB and Bluetooth.** USB reports `0x02` (48 B); Bluetooth reports `0x31`
  (78 B, sequence nibble + tag `0x10` + CRC-32 seeded with `0xA2`). Bluetooth
  output is opt-in (`PRISMA_DUALSENSE_BT=1`) because any output report switches
  the pad to its full input report and can blind other readers.
- **No steady-state allocation.** Fixed-size state/rumble slots, a fixed
  mailbox for input reports, stack report buffers.
- **Off-safe.** Closing the handle releases the triggers, stops the motors and
  restores a host's default lightbar/player LEDs.

Input is decoded here as well (touch points, gyro/accelerometer, battery,
charging and jack status) and returned as a `#[repr(C)]` POD struct.

## Build

```powershell
cargo build --workspace --release
cargo test --workspace
```

Outputs land in `target/release/`. Copy the DLLs (and PDBs for debugging) to
`PrismaFramework/Client/Assets/Plugins/NativePlugins/Windows`.

## Smoke tests / examples

```powershell
cargo run -p prisma-dualsense-hid --bin hid-smoke --release
cargo run -p prisma-dualsense-audio --bin audio-smoke --release
cargo run -p prisma-dualsense-hid --example hid-dump -- 5
cargo run -p prisma-dualsense-hid --example dualsense -- all
```

## C ABI

Both DLLs expose a small semantic C ABI (`extern "system"`), versioned with
`*_abi_version() -> u32`. The Unity client refuses a mismatch instead of
touching memory it does not understand. Every entry point is panic-guarded.

`prisma_dualsense_hid` — described by `DualSenseNativeHid.cs`:

```
prisma_hid_abi_version() -> u32
prisma_hid_open(error, cap) -> handle          starts the writer thread
prisma_hid_close(handle)                       off-safe release + join
prisma_hid_set_state(handle, const state*)     queues desired output state
prisma_hid_rumble(handle, const steps*, count) queues a vibration shape (<= 8 steps)
prisma_hid_release(handle)                     triggers off, motors off
prisma_hid_audio_haptics(handle, enable)       HD-audio mode init / hand-back
prisma_hid_rescan(handle)                      look for the pad again
prisma_hid_status(handle, status*)
prisma_hid_last_error(handle, buf, cap) -> len
prisma_hid_last_report(handle, buf, cap) -> len
prisma_hid_poll_input(handle, state*, raw, cap, len*) -> 1 new / 0 none / -1 unknown
```

Struct sizes are the contract and are asserted by `abi_layout_is_stable`:

| struct | bytes |
| --- | --- |
| `PrismaTriggerState` | 8 |
| `PrismaOutputState` | 26 |
| `PrismaPadStatus` | 28 |
| `PrismaInputState` | 58 |
| `Step` / `RumbleStep` | 4 |

`prisma_dualsense_audio` — described by `PrismaNativeAudio.cs`:

```
prisma_audio_abi_version() -> u32
prisma_audio_open(error, cap) -> handle
prisma_audio_close(handle)
prisma_audio_start(handle, error, cap) / prisma_audio_stop(handle)
prisma_audio_status(handle, status*)
prisma_audio_submit(handle, const f32* quad, frames, rate) -> frames
prisma_audio_submit_haptics(handle, const f32* stereo, frames, rate) -> frames
prisma_audio_rumble(handle, const steps*, count) -> frames
prisma_audio_name(handle, buf, cap) -> len
prisma_audio_last_error(handle, buf, cap) -> len
```

`submit` takes interleaved `[speaker L, speaker R, left actuator, right
actuator]` frames; rates other than the endpoint's are linearly resampled. The
ring is one second deep, drops the oldest frames on overflow, and
`status.queued_frames` is what the client uses as backpressure.

