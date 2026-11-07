//! Prisma's DualSense audio plugin: the pad's four-channel USB endpoint.
//!
//! * [`session`] owns one persistent WASAPI stream and a fixed ring buffer.
//! * [`synth`] turns rumble shapes into actuator waveforms.
//! * [`pcm`] has the conversion helpers used by the smoke tooling and tests.
//!
//! The Unity client submits interleaved frames `[speaker L, speaker R, left
//! actuator, right actuator]`; nothing is allocated per submission in the steady
//! state.
//!
//! Every entry point takes a handle from [`prisma_audio_open`] and tolerates a
//! null or stale one by returning a failure code (the client also checks the ABI
//! version first), so a per-function `# Safety` section would only repeat that.

#![allow(clippy::missing_safety_doc)]

pub mod pcm;
pub mod synth;

#[cfg(windows)]
pub mod session;

#[cfg(windows)]
use std::panic::{catch_unwind, AssertUnwindSafe};
#[cfg(windows)]
use std::slice;

#[cfg(windows)]
use session::Session;
#[cfg(windows)]
use synth::RumbleStep;

/// The ABI version; the client rejects a mismatch instead of corrupting memory.
pub const ABI_VERSION: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PrismaAudioStatus {
    pub open: u8,
    pub running: u8,
    pub channels: u8,
    pub _reserved: u8,
    pub sample_rate: u32,
    pub buffer_frames: u32,
    pub queued_frames: u32,
    pub underruns: u32,
}

#[cfg(windows)]
fn guard<T>(fallback: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(fallback)
}

#[cfg(windows)]
unsafe fn session<'a>(handle: *mut Session) -> Option<&'a Session> {
    handle.as_ref()
}

#[cfg(windows)]
unsafe fn write_text(buffer: *mut u8, capacity: u32, text: &str) -> u32 {
    if buffer.is_null() || capacity == 0 {
        return 0;
    }
    let bytes = text.as_bytes();
    let copy = bytes.len().min(capacity as usize - 1);
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, copy);
    *buffer.add(copy) = 0;
    copy as u32
}

#[cfg(windows)]
#[no_mangle]
pub extern "system" fn prisma_audio_abi_version() -> u32 {
    ABI_VERSION
}

/// Finds the DualSense audio endpoint and opens the persistent stream.
#[cfg(windows)]
#[no_mangle]
pub unsafe extern "system" fn prisma_audio_open(
    error: *mut u8,
    error_capacity: u32,
) -> *mut Session {
    guard(std::ptr::null_mut(), || match Session::open() {
        Ok(session) => Box::into_raw(Box::new(session)),
        Err(problem) => {
            write_text(error, error_capacity, &problem.to_string());
            std::ptr::null_mut()
        }
    })
}

#[cfg(windows)]
#[no_mangle]
pub unsafe extern "system" fn prisma_audio_close(handle: *mut Session) {
    if !handle.is_null() {
        guard((), || drop(Box::from_raw(handle)));
    }
}

#[cfg(windows)]
#[no_mangle]
pub unsafe extern "system" fn prisma_audio_start(
    handle: *mut Session,
    error: *mut u8,
    error_capacity: u32,
) -> i32 {
    guard(-1, || {
        let Some(session) = session(handle) else {
            write_text(error, error_capacity, "invalid audio handle");
            return -1;
        };
        match session.start() {
            Ok(()) => 0,
            Err(problem) => {
                write_text(error, error_capacity, &problem.to_string());
                -1
            }
        }
    })
}

#[cfg(windows)]
#[no_mangle]
pub unsafe extern "system" fn prisma_audio_stop(handle: *mut Session) -> i32 {
    guard(-1, || {
        let Some(session) = session(handle) else {
            return -1;
        };
        session.stop();
        0
    })
}

#[cfg(windows)]
#[no_mangle]
pub unsafe extern "system" fn prisma_audio_status(
    handle: *mut Session,
    status: *mut PrismaAudioStatus,
) -> i32 {
    guard(-1, || {
        let (Some(session), Some(status)) = (session(handle), status.as_mut()) else {
            return -1;
        };
        *status = PrismaAudioStatus {
            open: u8::from(session.is_open()),
            running: u8::from(session.is_running()),
            channels: 4,
            _reserved: 0,
            sample_rate: session.rate,
            buffer_frames: session.buffer_frames,
            queued_frames: session.queued_frames(),
            underruns: session.underruns(),
        };
        0
    })
}

/// Pushes interleaved four-channel PCM; returns the frames accepted, or -1.
#[cfg(windows)]
#[no_mangle]
pub unsafe extern "system" fn prisma_audio_submit(
    handle: *mut Session,
    samples: *const f32,
    frames: u32,
    sample_rate: u32,
) -> i32 {
    guard(-1, || {
        let Some(session) = session(handle) else {
            return -1;
        };
        if samples.is_null() || frames == 0 {
            return -1;
        }
        let source = slice::from_raw_parts(samples, frames as usize * 4);
        session.submit_quad(source, sample_rate) as i32
    })
}

/// Pushes interleaved stereo haptics onto the actuator channels.
#[cfg(windows)]
#[no_mangle]
pub unsafe extern "system" fn prisma_audio_submit_haptics(
    handle: *mut Session,
    samples: *const f32,
    frames: u32,
    sample_rate: u32,
) -> i32 {
    guard(-1, || {
        let Some(session) = session(handle) else {
            return -1;
        };
        if samples.is_null() || frames == 0 {
            return -1;
        }
        let source = slice::from_raw_parts(samples, frames as usize * 2);
        session.submit_haptics(source, sample_rate) as i32
    })
}

/// Synthesises and plays a rumble shape (up to eight steps).
#[cfg(windows)]
#[no_mangle]
pub unsafe extern "system" fn prisma_audio_rumble(
    handle: *mut Session,
    steps: *const RumbleStep,
    count: u32,
) -> i32 {
    guard(-1, || {
        let Some(session) = session(handle) else {
            return -1;
        };
        if steps.is_null() || count == 0 {
            return -1;
        }
        let steps = slice::from_raw_parts(steps, count as usize);
        session.play_rumble(steps) as i32
    })
}

#[cfg(windows)]
#[no_mangle]
pub unsafe extern "system" fn prisma_audio_name(
    handle: *mut Session,
    buffer: *mut u8,
    capacity: u32,
) -> u32 {
    guard(0, || {
        let Some(session) = session(handle) else {
            return write_text(buffer, capacity, "invalid audio handle");
        };
        write_text(buffer, capacity, &session.name)
    })
}

#[cfg(windows)]
#[no_mangle]
pub unsafe extern "system" fn prisma_audio_last_error(
    handle: *mut Session,
    buffer: *mut u8,
    capacity: u32,
) -> u32 {
    guard(0, || {
        let Some(session) = session(handle) else {
            return write_text(buffer, capacity, "invalid audio handle");
        };
        write_text(buffer, capacity, &session.last_error())
    })
}
