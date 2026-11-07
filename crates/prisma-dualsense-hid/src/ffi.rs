//! C ABI. Everything the Unity client needs is semantic: it hands over desired
//! state and reads back a POD input struct, never report bytes.
//!
//! Every entry point takes a handle from [`prisma_hid_open`] and tolerates a
//! null or stale one by returning a failure code (the client also checks the
//! ABI version before calling anything), so the individual `# Safety` sections
//! would only repeat that one rule.

#![allow(clippy::missing_safety_doc)]

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::slice;

use crate::device::{Control, PadState, Step, MAX_RUMBLE_STEPS};
use crate::input::InputState;
use crate::report::Trigger;

/// The ABI version; the client rejects a mismatch instead of corrupting memory.
pub const ABI_VERSION: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PrismaTriggerState {
    /// 0: leave the trigger as it is.
    pub enabled: u8,
    /// `0x05` off, `0x21` feedback, `0x22` bow, `0x23` galloping, `0x25`
    /// weapon, `0x26` vibration, `0x27` machine.
    pub mode: u8,
    pub start_zone: u8,
    pub end_zone: u8,
    /// 1..8.
    pub strength: u8,
    /// Bow snap-back (1..8), galloping second foot (0..7), machine amplitude B
    /// (0..7).
    pub second_strength: u8,
    pub frequency: u8,
    pub period: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PrismaOutputState {
    pub lightbar_enabled: u8,
    pub lightbar: [u8; 3],
    pub player_led_enabled: u8,
    /// Raw LED pattern: five LEDs in bits 0..4, `1 << 5` switches instantly.
    pub player_leds: u8,
    pub mic_led_enabled: u8,
    /// 0 off, 1 on, 2 pulsing.
    pub mic_led_mode: u8,
    /// Left (heavy), right (light); 0..255. Held while no rumble shape plays.
    pub motors: [u8; 2],
    pub right_trigger: PrismaTriggerState,
    pub left_trigger: PrismaTriggerState,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PrismaPadStatus {
    pub open: u8,
    /// 0 none, 1 USB, 2 Bluetooth.
    pub transport: u8,
    /// 1 legacy, 2 vibration v2.
    pub vibration: u8,
    pub lost: u8,
    /// Firmware update version (feature report 0x20), 0 when unknown.
    pub update_version: u16,
    pub _reserved: u16,
    pub scans_left: u32,
    pub reports_sent: u32,
    pub write_failures: u32,
    pub reports_read: u32,
    pub last_report_len: u8,
    pub _reserved2: [u8; 3],
}

fn trigger(state: &PrismaTriggerState) -> Option<Trigger> {
    (state.enabled != 0).then_some(Trigger {
        mode: state.mode,
        start: state.start_zone,
        end: state.end_zone,
        strength: state.strength,
        second: state.second_strength,
        frequency: state.frequency,
        period: state.period,
    })
}

impl From<&PrismaOutputState> for PadState {
    fn from(value: &PrismaOutputState) -> PadState {
        PadState {
            lightbar: (value.lightbar_enabled != 0).then_some([
                value.lightbar[0],
                value.lightbar[1],
                value.lightbar[2],
            ]),
            player_leds: (value.player_led_enabled != 0).then_some(value.player_leds),
            mic_led: (value.mic_led_enabled != 0).then_some(value.mic_led_mode),
            left_trigger: trigger(&value.left_trigger),
            right_trigger: trigger(&value.right_trigger),
            motors: value.motors,
        }
    }
}

fn guard<T>(fallback: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(fallback)
}

unsafe fn control<'a>(handle: *mut Control) -> Option<&'a Control> {
    handle.as_ref()
}

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

#[no_mangle]
pub extern "system" fn prisma_hid_abi_version() -> u32 {
    ABI_VERSION
}

/// Starts the writer thread; the pad is found asynchronously.
#[no_mangle]
pub unsafe extern "system" fn prisma_hid_open(error: *mut u8, error_capacity: u32) -> *mut Control {
    guard(std::ptr::null_mut(), || {
        if error_capacity > 0 && !error.is_null() {
            *error = 0;
            write_text(error, error_capacity, "");
        }
        Box::into_raw(Box::new(Control::start()))
    })
}

#[no_mangle]
pub unsafe extern "system" fn prisma_hid_close(handle: *mut Control) {
    if !handle.is_null() {
        guard((), || drop(Box::from_raw(handle)));
    }
}

/// Queues the desired output state; returns immediately.
#[no_mangle]
pub unsafe extern "system" fn prisma_hid_set_state(
    handle: *mut Control,
    state: *const PrismaOutputState,
) -> i32 {
    guard(-1, || {
        let (Some(control), Some(state)) = (control(handle), state.as_ref()) else {
            return -1;
        };
        control.set_state(PadState::from(state));
        0
    })
}

/// Queues a vibration shape (up to eight steps), replacing the one playing.
#[no_mangle]
pub unsafe extern "system" fn prisma_hid_rumble(
    handle: *mut Control,
    steps: *const Step,
    count: u32,
) -> i32 {
    guard(-1, || {
        let Some(control) = control(handle) else {
            return -1;
        };
        if steps.is_null() || count == 0 {
            return -1;
        }
        let count = (count as usize).min(MAX_RUMBLE_STEPS);
        let input = slice::from_raw_parts(steps, count);
        let mut rumble = crate::device::Rumble {
            len: count,
            ..Default::default()
        };
        rumble.steps[..count].copy_from_slice(input);
        control.set_rumble(rumble);
        0
    })
}

/// Frees the triggers and stops the motors.
#[no_mangle]
pub unsafe extern "system" fn prisma_hid_release(handle: *mut Control) -> i32 {
    guard(-1, || {
        let Some(control) = control(handle) else {
            return -1;
        };
        control.release();
        0
    })
}

/// Switches the actuators to HD audio haptics (`enable`) or back to emulation.
#[no_mangle]
pub unsafe extern "system" fn prisma_hid_audio_haptics(handle: *mut Control, enable: u32) -> i32 {
    guard(-1, || {
        let Some(control) = control(handle) else {
            return -1;
        };
        control.audio_haptics(enable != 0);
        0
    })
}

/// Looks for the pad again (a controller was connected).
#[no_mangle]
pub unsafe extern "system" fn prisma_hid_rescan(handle: *mut Control) -> i32 {
    guard(-1, || {
        let Some(control) = control(handle) else {
            return -1;
        };
        control.rescan();
        0
    })
}

#[no_mangle]
pub unsafe extern "system" fn prisma_hid_status(
    handle: *mut Control,
    status: *mut PrismaPadStatus,
) -> i32 {
    guard(-1, || {
        let (Some(control), Some(status)) = (control(handle), status.as_mut()) else {
            return -1;
        };
        let snapshot = control.status();
        *status = PrismaPadStatus {
            open: u8::from(snapshot.open),
            transport: snapshot.transport,
            vibration: snapshot.vibration,
            lost: u8::from(snapshot.lost),
            update_version: snapshot.update_version,
            _reserved: 0,
            scans_left: snapshot.scans_left,
            reports_sent: snapshot.reports_sent,
            write_failures: snapshot.write_failures,
            reports_read: snapshot.reports_read,
            last_report_len: snapshot.last_report_len,
            _reserved2: [0; 3],
        };
        0
    })
}

/// Copies the last error message (NUL-terminated); returns its byte length.
#[no_mangle]
pub unsafe extern "system" fn prisma_hid_last_error(
    handle: *mut Control,
    buffer: *mut u8,
    capacity: u32,
) -> u32 {
    guard(0, || {
        let Some(control) = control(handle) else {
            return write_text(buffer, capacity, "invalid handle");
        };
        write_text(buffer, capacity, &control.last_error())
    })
}

/// Copies the last report actually written (NUL-terminated); returns its length.
#[no_mangle]
pub unsafe extern "system" fn prisma_hid_last_report(
    handle: *mut Control,
    buffer: *mut u8,
    capacity: u32,
) -> u32 {
    guard(0, || {
        let Some(control) = control(handle) else {
            return 0;
        };
        if buffer.is_null() || capacity < 2 {
            return 0;
        }
        let target = slice::from_raw_parts_mut(buffer, capacity as usize);
        let len = control.last_report(target);
        if len < target.len() {
            target[len] = 0;
        }
        len as u32
    })
}

/// 1: a new report was decoded, 0: none waiting, -1: unrecognised report.
#[no_mangle]
pub unsafe extern "system" fn prisma_hid_poll_input(
    handle: *mut Control,
    state: *mut InputState,
    raw: *mut u8,
    raw_capacity: u32,
    raw_length: *mut u32,
) -> i32 {
    guard(-1, || {
        let (Some(control), Some(state), Some(raw_length)) =
            (control(handle), state.as_mut(), raw_length.as_mut())
        else {
            return -1;
        };
        if raw.is_null() || raw_capacity == 0 {
            *raw_length = 0;
            return -1;
        }
        let mut length = 0u32;
        let target = slice::from_raw_parts_mut(raw, raw_capacity as usize);
        let result = control.poll_input(target, &mut length, state);
        *raw_length = length;
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    // The C# side declares the same structs with LayoutKind.Sequential; these
    // sizes are the contract. A change here needs the same change in
    // `DualSenseNativeHid.cs`.
    #[test]
    fn abi_layout_is_stable() {
        assert_eq!(size_of::<PrismaTriggerState>(), 8);
        assert_eq!(size_of::<PrismaOutputState>(), 26);
        assert_eq!(size_of::<PrismaPadStatus>(), 28);
        assert_eq!(size_of::<InputState>(), 58);
        assert_eq!(size_of::<Step>(), 4);
    }

    #[test]
    fn a_full_state_maps_to_the_pad_state() {
        let mut state = PrismaOutputState {
            lightbar_enabled: 1,
            lightbar: [1, 2, 3],
            player_led_enabled: 1,
            player_leds: 0x1b,
            mic_led_enabled: 1,
            mic_led_mode: 2,
            motors: [10, 20],
            ..Default::default()
        };
        state.right_trigger = PrismaTriggerState {
            enabled: 1,
            mode: 0x25,
            start_zone: 2,
            end_zone: 6,
            strength: 8,
            second_strength: 1,
            frequency: 40,
            period: 100,
        };

        let pad = PadState::from(&state);
        assert_eq!(pad.lightbar, Some([1, 2, 3]));
        assert_eq!(pad.player_leds, Some(0x1b));
        assert_eq!(pad.mic_led, Some(2));
        assert_eq!(pad.motors, [10, 20]);
        let trigger = pad.right_trigger.expect("enabled trigger");
        assert_eq!(trigger.mode, 0x25);
        assert_eq!((trigger.start, trigger.end), (2, 6));
        assert_eq!(trigger.strength, 8);
        // A disabled trigger is "leave it alone", not "off".
        assert_eq!(pad.left_trigger, None);
    }

    #[test]
    fn disabled_parts_are_left_alone() {
        let pad = PadState::from(&PrismaOutputState::default());
        assert_eq!(pad.lightbar, None);
        assert_eq!(pad.player_leds, None);
        assert_eq!(pad.mic_led, None);
        assert_eq!(pad.motors, [0, 0]);
        assert_eq!(pad.left_trigger, None);
        assert_eq!(pad.right_trigger, None);
    }

    #[test]
    fn handles_and_reports_round_trip() {
        unsafe {
            assert_eq!(prisma_hid_abi_version(), ABI_VERSION);
            assert!(prisma_hid_status(std::ptr::null_mut(), std::ptr::null_mut()) < 0);

            let control = prisma_hid_open(std::ptr::null_mut(), 0);
            assert!(!control.is_null());

            let mut status = PrismaPadStatus::default();
            assert_eq!(prisma_hid_status(control, &mut status), 0);

            let state = PrismaOutputState::default();
            assert_eq!(prisma_hid_set_state(control, &state), 0);
            assert_eq!(prisma_hid_set_state(control, std::ptr::null()), -1);
            assert_eq!(prisma_hid_release(control), 0);
            assert_eq!(prisma_hid_rescan(control), 0);

            let mut buffer = [0u8; 128];
            let length = prisma_hid_last_report(control, buffer.as_mut_ptr(), buffer.len() as u32);
            assert!(length <= buffer.len() as u32);

            let mut decoded = InputState::default();
            let mut raw = [0u8; 64];
            let mut raw_length = 0u32;
            let result = prisma_hid_poll_input(
                control,
                &mut decoded,
                raw.as_mut_ptr(),
                raw.len() as u32,
                &mut raw_length,
            );
            assert!(result == 0 || result == 1);
            assert_eq!(raw_length, 0);

            prisma_hid_close(control);
            prisma_hid_close(std::ptr::null_mut());
        }
    }
}
