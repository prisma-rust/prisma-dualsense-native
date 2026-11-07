//! The DualSense input report (USB `0x01`, Bluetooth `0x31`) decoded into a
//! plain C-layout struct.
//!
//! Why this exists at all: Unity's Input System support squeezes the report
//! into a nine-byte synthetic state (sticks, triggers, three button bytes) and
//! parses **none** of the touch points; its minimal-report branch even masks
//! the microphone bit out of `buttons2`. Touch coordinates, the gyro and the
//! battery/jack status only exist in the raw report.
//!
//! Offsets below are relative to the payload (report id excluded). The
//! Bluetooth report is the same layout shifted by one byte (a flags byte follows
//! the report id), so decoding just picks a base offset.
//!
//! ### Layout (USB, absolute indices include the report id)
//!
//! ```text
//! 0x00 report id (USB 0x01 / Bluetooth 0x31)
//! 0x01..0x04 sticks XY                   0x05 L2  0x06 R2  0x07 sequence
//! 0x08 buttons0: low nibble dpad (0 up .. 8 released), high nibble square/cross/circle/triangle
//! 0x09 buttons1: L1 R1 L2 R2 Create Options L3 R3
//! 0x0A buttons2: PS, touchpad click, mute
//! 0x10..0x15 gyro XYZ (le16 x3)          0x16..0x1B accelerometer XYZ (le16 x3)
//! 0x1C..0x1F motion timestamp (0.33 us units, not decoded)
//! 0x21..0x24 / 0x25..0x28 touch 1 / touch 2
//! 0x35..0x37 status[0..2]: battery/charging, jack, reserved
//! ```
//!
//! The gyro/accelerometer split is not the DualShock 4's (`0x0D` / `0x13`):
//! a real pad reports a stable near-zero gyro and gravity on accel Y only with
//! `0x10` / `0x16` (the kernel's `dualsense_input_report`).
//!
//! ### Touch point (4 bytes)
//!
//! `[0] bit7 = 1 means no finger, bit0..6 = contact id`;
//! `[1] x low 8`; `[2] low nibble = x high 4, high nibble = y low 4`; `[3] y high 8`.
//! The pad is 1920x1080.
//!
//! ### status[0]
//!
//! Low nibble: battery level 0..10 (10% buckets; `0xF` = error).
//! High nibble: `0` discharging, `1` charging, `2` charged, `0xA` voltage or
//! temperature out of range, `0xB` temperature error, `0xF` charging error.
//!
//! ### status[1]
//!
//! bit0 headset plugged, bit1 microphone plugged, bit2 microphone muted, **active
//! high**. The kernel reads it inverted; on this firmware a four-pole headset
//! goes `0x18 -> 0x1B`, i.e. the bits go high when plugged in.

pub const USB_REPORT_ID: u8 = 0x01;
pub const BLUETOOTH_REPORT_ID: u8 = 0x31;
pub const USB_REPORT_SIZE: usize = 64;
pub const TOUCHPAD_WIDTH: u16 = 1920;
pub const TOUCHPAD_HEIGHT: u16 = 1080;

// Offsets relative to the payload (report id excluded).
const STICK_LEFT_X: usize = 1;
const STICK_LEFT_Y: usize = 2;
const STICK_RIGHT_X: usize = 3;
const STICK_RIGHT_Y: usize = 4;
const TRIGGER_LEFT: usize = 5;
const TRIGGER_RIGHT: usize = 6;
const BUTTONS0: usize = 8;
const BUTTONS1: usize = 9;
const BUTTONS2: usize = 10;
const GYRO_X: usize = 16;
const ACCEL_X: usize = 22;
const TOUCH0: usize = 33;
const TOUCH1: usize = 37;
const STATUS0: usize = 53;

pub const CHARGING_DISCHARGING: u8 = 0x0;
pub const CHARGING_CHARGING: u8 = 0x1;
pub const CHARGING_CHARGED: u8 = 0x2;
pub const CHARGING_VOLTAGE_OR_TEMPERATURE: u8 = 0xA;
pub const CHARGING_TEMPERATURE_ERROR: u8 = 0xB;
pub const CHARGING_ERROR: u8 = 0xF;
pub const CHARGING_UNKNOWN: u8 = 0xFF;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TouchPoint {
    /// 1 when a finger is on the pad.
    pub active: u8,
    /// Contact id 0..127, stable while one finger moves.
    pub id: u8,
    pub x: u16,
    pub y: u16,
}

impl TouchPoint {
    /// Coordinates normalised to 0..1 (the panel draws them).
    pub fn normalized(&self) -> (f32, f32) {
        (
            (f32::from(self.x) / f32::from(TOUCHPAD_WIDTH - 1)).clamp(0.0, 1.0),
            (f32::from(self.y) / f32::from(TOUCHPAD_HEIGHT - 1)).clamp(0.0, 1.0),
        )
    }
}

/// One decoded report. `#[repr(C)]` because the C# side reads it as-is.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputState {
    pub valid: u8,
    pub report_id: u8,
    pub left_x: u8,
    pub left_y: u8,
    pub right_x: u8,
    pub right_y: u8,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub buttons0: u8,
    pub buttons1: u8,
    pub buttons2: u8,
    pub dpad_up: u8,
    pub dpad_right: u8,
    pub dpad_down: u8,
    pub dpad_left: u8,
    pub square: u8,
    pub cross: u8,
    pub circle: u8,
    pub triangle: u8,
    pub l1: u8,
    pub r1: u8,
    pub l2: u8,
    pub r2: u8,
    pub create: u8,
    pub options: u8,
    pub l3: u8,
    pub r3: u8,
    pub playstation: u8,
    pub touchpad: u8,
    pub mute: u8,
    pub gyro: [i16; 3],
    pub accel: [i16; 3],
    pub status0: u8,
    pub status1: u8,
    pub status2: u8,
    pub status_valid: u8,
    pub touch0: TouchPoint,
    pub touch1: TouchPoint,
}

impl InputState {
    pub fn battery_level(&self) -> u8 {
        if self.status_valid == 0 {
            0
        } else {
            self.status0 & 0x0f
        }
    }

    /// Charging state; [`CHARGING_UNKNOWN`] when the report has no status block.
    pub fn charging(&self) -> u8 {
        if self.status_valid == 0 {
            CHARGING_UNKNOWN
        } else {
            decode_charging(self.status0 >> 4)
        }
    }

    /// Battery percent, bucket midpoints like the Linux driver
    /// (`min(level * 10 + 5, 100)`); charged is 100, and an error state is 0
    /// — that is "unreadable", not "empty".
    pub fn battery_percent(&self) -> u8 {
        if self.status_valid == 0 {
            return 0;
        }
        match self.charging() {
            CHARGING_CHARGED => 100,
            CHARGING_VOLTAGE_OR_TEMPERATURE
            | CHARGING_TEMPERATURE_ERROR
            | CHARGING_ERROR
            | CHARGING_UNKNOWN => 0,
            _ => (self.battery_level() * 10 + 5).min(100),
        }
    }

    pub fn is_charging(&self) -> bool {
        self.charging() == CHARGING_CHARGING
    }

    pub fn is_fully_charged(&self) -> bool {
        self.charging() == CHARGING_CHARGED
    }

    pub fn headphone_plugged(&self) -> bool {
        self.status_valid != 0 && self.status1 & 0x01 != 0
    }

    pub fn microphone_plugged(&self) -> bool {
        self.status_valid != 0 && self.status1 & 0x02 != 0
    }

    pub fn microphone_muted(&self) -> bool {
        self.status_valid != 0 && self.status1 & 0x04 != 0
    }

    pub fn any_touch(&self) -> bool {
        self.touch0.active != 0 || self.touch1.active != 0
    }

    /// Touch 0 if pressed, else touch 1, else an inactive point.
    pub fn primary_touch(&self) -> TouchPoint {
        if self.touch0.active != 0 {
            self.touch0
        } else if self.touch1.active != 0 {
            self.touch1
        } else {
            TouchPoint::default()
        }
    }
}

pub fn decode_charging(value: u8) -> u8 {
    match value {
        CHARGING_DISCHARGING
        | CHARGING_CHARGING
        | CHARGING_CHARGED
        | CHARGING_VOLTAGE_OR_TEMPERATURE
        | CHARGING_TEMPERATURE_ERROR
        | CHARGING_ERROR => value,
        _ => CHARGING_UNKNOWN,
    }
}

/// Decodes one raw report. Returns false for an unknown report id or a report
/// too short to hold the button bytes.
pub fn decode(report: &[u8], size: usize, out: &mut InputState) -> bool {
    *out = InputState::default();
    let size = size.min(report.len());
    if size == 0 {
        return false;
    }

    let base = match report[0] {
        USB_REPORT_ID => 0,
        BLUETOOTH_REPORT_ID => 1,
        _ => return false,
    };
    if size < base + BUTTONS2 + 1 {
        return false;
    }

    out.valid = 1;
    out.report_id = report[0];
    out.left_x = report[base + STICK_LEFT_X];
    out.left_y = report[base + STICK_LEFT_Y];
    out.right_x = report[base + STICK_RIGHT_X];
    out.right_y = report[base + STICK_RIGHT_Y];
    out.left_trigger = report[base + TRIGGER_LEFT];
    out.right_trigger = report[base + TRIGGER_RIGHT];

    let b0 = report[base + BUTTONS0];
    let b1 = report[base + BUTTONS1];
    let b2 = report[base + BUTTONS2];
    out.buttons0 = b0;
    out.buttons1 = b1;
    out.buttons2 = b2;

    let dpad = b0 & 0x0f;
    out.dpad_up = u8::from(matches!(dpad, 0 | 1 | 7));
    out.dpad_right = u8::from((1..=3).contains(&dpad));
    out.dpad_down = u8::from((3..=5).contains(&dpad));
    out.dpad_left = u8::from((5..=7).contains(&dpad));

    out.square = u8::from(b0 & 0x10 != 0);
    out.cross = u8::from(b0 & 0x20 != 0);
    out.circle = u8::from(b0 & 0x40 != 0);
    out.triangle = u8::from(b0 & 0x80 != 0);

    out.l1 = u8::from(b1 & 0x01 != 0);
    out.r1 = u8::from(b1 & 0x02 != 0);
    out.l2 = u8::from(b1 & 0x04 != 0);
    out.r2 = u8::from(b1 & 0x08 != 0);
    out.create = u8::from(b1 & 0x10 != 0);
    out.options = u8::from(b1 & 0x20 != 0);
    out.l3 = u8::from(b1 & 0x40 != 0);
    out.r3 = u8::from(b1 & 0x80 != 0);

    out.playstation = u8::from(b2 & 0x01 != 0);
    out.touchpad = u8::from(b2 & 0x02 != 0);
    out.mute = u8::from(b2 & 0x04 != 0);

    if size >= base + ACCEL_X + 6 {
        out.gyro = [
            read_i16(report, base + GYRO_X),
            read_i16(report, base + GYRO_X + 2),
            read_i16(report, base + GYRO_X + 4),
        ];
        out.accel = [
            read_i16(report, base + ACCEL_X),
            read_i16(report, base + ACCEL_X + 2),
            read_i16(report, base + ACCEL_X + 4),
        ];
    }

    out.touch0 = decode_touch(report, size, base + TOUCH0);
    out.touch1 = decode_touch(report, size, base + TOUCH1);

    if size >= base + STATUS0 + 2 {
        out.status0 = report[base + STATUS0];
        out.status1 = report[base + STATUS0 + 1];
        out.status_valid = 1;
        if size >= base + STATUS0 + 3 {
            out.status2 = report[base + STATUS0 + 2];
        }
    }

    true
}

fn read_i16(report: &[u8], offset: usize) -> i16 {
    i16::from_le_bytes([report[offset], report[offset + 1]])
}

fn decode_touch(report: &[u8], size: usize, offset: usize) -> TouchPoint {
    if offset + 4 > size {
        return TouchPoint::default();
    }
    let contact = report[offset];
    let packed = report[offset + 2];
    TouchPoint {
        active: u8::from(contact & 0x80 == 0),
        id: contact & 0x7f,
        x: u16::from(report[offset + 1]) | (u16::from(packed & 0x0f) << 8),
        y: u16::from(packed >> 4) | (u16::from(report[offset + 3]) << 4),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usb_report() -> [u8; 64] {
        let mut r = [0u8; 64];
        r[0] = USB_REPORT_ID;
        r[1..5].copy_from_slice(&[10, 20, 30, 40]);
        r[5] = 50;
        r[6] = 60;
        // dpad right, cross
        r[8] = 0x02 | 0x20;
        // L1, Options, R3
        r[9] = 0x01 | 0x20 | 0x80;
        // PS, mute
        r[10] = 0x01 | 0x04;
        // gyro/accel: signed little-endian, one negative
        r[16..22].copy_from_slice(&[0x10, 0x00, 0xff, 0xff, 0x00, 0x01]);
        r[22..28].copy_from_slice(&[0x00, 0x20, 0x00, 0xe0, 0x00, 0x00]);
        // touch 0: active, id 7, x = 0x123, y = 0x456
        r[33] = 7;
        r[34] = 0x23;
        r[35] = 0x61; // x high 1, y low 6
        r[36] = 0x45;
        // touch 1: no finger
        r[37] = 0x80 | 3;
        // status
        r[53] = 0x16; // level 6, charging
        r[54] = 0x1b;
        r[55] = 0x01;
        r
    }

    #[test]
    fn decodes_a_usb_report() {
        let r = usb_report();
        let mut s = InputState::default();
        assert!(decode(&r, 64, &mut s));
        assert_eq!(s.report_id, 0x01);
        assert_eq!((s.left_x, s.left_y, s.right_x, s.right_y), (10, 20, 30, 40));
        assert_eq!((s.left_trigger, s.right_trigger), (50, 60));
        assert!(s.dpad_right != 0 && s.dpad_up == 0 && s.dpad_down == 0 && s.dpad_left == 0);
        assert!(s.cross != 0 && s.square == 0);
        assert!(s.l1 != 0 && s.options != 0 && s.r3 != 0);
        assert!(s.playstation != 0 && s.mute != 0 && s.touchpad == 0);
        assert_eq!(s.gyro, [16, -1, 256]);
        assert_eq!(s.accel, [8192, -8192, 0]);
        assert_eq!(
            s.touch0,
            TouchPoint {
                active: 1,
                id: 7,
                x: 0x123,
                y: 0x456
            }
        );
        assert_eq!(s.touch1.active, 0);
        assert!(s.any_touch());
        assert_eq!(s.status_valid, 1);
        assert_eq!(s.battery_level(), 6);
        assert_eq!(s.battery_percent(), 65);
        assert!(s.is_charging());
        assert!(s.headphone_plugged() && s.microphone_plugged() && !s.microphone_muted());
    }

    #[test]
    fn bluetooth_reports_are_shifted_by_one() {
        let mut r = [0u8; 78];
        r[0] = BLUETOOTH_REPORT_ID;
        // payload starts at 2
        r[2] = 11;
        r[3] = 22;
        r[4] = 33;
        r[5] = 44;
        r[6] = 55;
        r[7] = 66;
        r[9] = 0x20; // cross, dpad released
                     // touch 0 at payload 33 -> absolute 34
        r[34] = 1;
        r[35] = 0x10;
        // status at payload 53 -> absolute 54: level 0, charged (high nibble 2).
        r[54] = 0x20;
        r[55] = 0x00;
        let mut s = InputState::default();
        assert!(decode(&r, 78, &mut s));
        assert_eq!((s.left_x, s.left_y, s.right_x, s.right_y), (11, 22, 33, 44));
        assert_eq!((s.left_trigger, s.right_trigger), (55, 66));
        assert!(s.cross != 0);
        assert_eq!(s.touch0.x, 0x10);
        assert_eq!(s.status_valid, 1);
        assert_eq!(s.charging(), CHARGING_CHARGED);
        assert_eq!(s.battery_percent(), 100);
    }

    #[test]
    fn rejects_short_and_unknown_reports() {
        let mut s = InputState::default();
        assert!(!decode(&[0x01, 0, 0], 3, &mut s));
        assert!(!decode(&[0x02, 0, 0, 0], 4, &mut s));
        let mut short = usb_report();
        short[0] = 0x09;
        assert!(!decode(&short, 64, &mut s));
        // A report that stops before the button bytes.
        let r = usb_report();
        assert!(!decode(&r, 5, &mut s));
    }

    #[test]
    fn charging_and_jack_semantics() {
        let mut s = InputState {
            status_valid: 1,
            // high nibble = charging state, low nibble = battery level
            status0: 0xf0,
            status1: 0x00,
            ..InputState::default()
        };
        assert_eq!(s.charging(), CHARGING_ERROR);
        assert_eq!(s.battery_percent(), 0);
        s.status0 = 0xa0;
        assert_eq!(s.charging(), CHARGING_VOLTAGE_OR_TEMPERATURE);
        assert_eq!(s.battery_percent(), 0);
        s.status0 = 0xb0;
        assert_eq!(s.charging(), CHARGING_TEMPERATURE_ERROR);
        s.status0 = 0x00;
        s.status1 = 0x04;
        assert_eq!(s.charging(), CHARGING_DISCHARGING);
        assert_eq!(s.battery_percent(), 5);
        assert!(s.microphone_muted() && !s.headphone_plugged());
        s.status_valid = 0;
        assert_eq!(s.battery_percent(), 0);
        assert_eq!(s.charging(), CHARGING_UNKNOWN);
    }
}
