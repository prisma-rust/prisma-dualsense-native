//! The DualSense output report, built by hand.
//!
//! One 47-byte block carries everything: the three valid-flag bytes, the two
//! compatible-vibration motors, both adaptive triggers, the lightbar and the
//! player LEDs. Over USB it is report `0x02` (48 bytes with the report id); over
//! Bluetooth report `0x31` (78 bytes), led by a sequence number and the tag
//! `0x10`, closed by a CRC-32 seeded with `0xA2`.
//!
//! Offsets here are relative to the common block (report byte 1 on USB), which
//! is how the Linux `hid-playstation` driver and SDL's `SDL_hidapi_ps5.c` define
//! them.
//!
//! Report semantics that are easy to get wrong (and are pinned by tests):
//!
//! * **Every report carries the motors.** A report *without* the rumble flags
//!   ends the motors' emulation and hands the actuators back to the audio
//!   haptics, so a trigger-only report must still carry the current rumble or a
//!   running vibration is cut. Stopping is `rumble = [0, 0]` (no flags).
//! * **The first report after opening clears attenuation** another host (a PS5's
//!   intensity settings) may have left in the pad (`flags1` bit 6, byte 36 = 0).
//! * **The lights are released once, alone, before the first colour**
//!   (`flags1` bit 3 + `lightbar_setup` "light out").
//! * **Trigger effects need the `0x20` prefix** (`0x21` feedback, `0x25` weapon,
//!   `0x26` vibration, ...). The Microsoft GameInput helper header's `0x01` /
//!   `0x02` / `0x06` values do nothing on a real pad.

/// Sony's vendor id and the DualSense's product ids (the plain pad and the Edge).
pub const SONY: u16 = 0x054c;
pub const DUALSENSE: u16 = 0x0ce6;
pub const DUALSENSE_EDGE: u16 = 0x0df2;

/// Whether a USB vendor / product pair is a DualSense.
pub fn is_dualsense(vendor: u16, product: u16) -> bool {
    vendor == SONY && matches!(product, DUALSENSE | DUALSENSE_EDGE)
}

pub const USB_REPORT_ID: u8 = 0x02;
pub const USB_REPORT_LEN: usize = 1 + COMMON_LEN;
pub const BT_REPORT_ID: u8 = 0x31;
pub const BT_REPORT_LEN: usize = 78;
const BT_TAG: u8 = 0x10;
const BT_CRC_SEED: u8 = 0xa2;
pub const COMMON_LEN: usize = 47;

// Offsets inside the common block (report byte 1 on USB).
const FLAGS0: usize = 0;
const FLAGS1: usize = 1;
const MOTOR_RIGHT: usize = 2;
const MOTOR_LEFT: usize = 3;
const MIC_LED: usize = 8;
const POWER_SAVE: usize = 9;
const RIGHT_TRIGGER: usize = 10;
const LEFT_TRIGGER: usize = 21;
/// `reduce_motor_power` (dualsensectl): motor attenuation in bits 0..2, trigger
/// attenuation in bits 4..6, 0 (none) to 7.
const ATTENUATION: usize = 36;
const FLAGS2: usize = 38;
const LIGHTBAR_SETUP: usize = 41;
const LED_BRIGHTNESS: usize = 42;
const PLAYER_LEDS: usize = 43;
const LIGHTBAR: usize = 44;

// Which parts of the block the pad applies.
const FLAG0_COMPATIBLE_VIBRATION: u8 = 1 << 0;
const FLAG0_HAPTICS_SELECT: u8 = 1 << 1;
const FLAG0_RIGHT_TRIGGER: u8 = 1 << 2;
const FLAG0_LEFT_TRIGGER: u8 = 1 << 3;
const FLAG1_MIC_LED: u8 = 1 << 0;
const FLAG1_LIGHTBAR: u8 = 1 << 2;
const FLAG1_RELEASE_LEDS: u8 = 1 << 3;
const FLAG1_PLAYER_LEDS: u8 = 1 << 4;
const FLAG1_ATTENUATION: u8 = 1 << 6;
const FLAG2_LIGHTBAR_SETUP: u8 = 1 << 1;
const FLAG2_VIBRATION_V2: u8 = 1 << 2;

/// `lightbar_setup`: fade the firmware's own blue light out so the host's
/// colour shows.
const LIGHTBAR_LIGHT_OUT: u8 = 1 << 1;

/// Player LEDs: switch at once instead of fading.
pub const PLAYER_LEDS_INSTANT: u8 = 1 << 5;
/// The middle LED: player 1.
pub const PLAYER_1: u8 = 1 << 2;
/// The blue a host gives a first pad (`hid-playstation.c`), restored when the
/// game lets go.
pub const DEFAULT_LIGHTBAR: [u8; 3] = [0, 0, 128];

/// Trigger effect modes. Only `Off`, `Feedback`, `Weapon` and `Vibration` are
/// the modes the community calls official and expects in future firmware; the
/// other three are kept because real pads accept them today.
pub const TRIGGER_OFF_MODE: u8 = 0x05;
pub const TRIGGER_FEEDBACK: u8 = 0x21;
pub const TRIGGER_BOW: u8 = 0x22;
pub const TRIGGER_GALLOPING: u8 = 0x23;
pub const TRIGGER_WEAPON: u8 = 0x25;
pub const TRIGGER_VIBRATION: u8 = 0x26;
pub const TRIGGER_MACHINE: u8 = 0x27;

/// An adaptive trigger's effect in the pad's own encoding.
///
/// The travel is cut into ten zones (0 at rest, 9 fully pulled); zone effects
/// carry a strength 1..8 per zone, packed three bits each.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Trigger {
    pub mode: u8,
    /// First zone the effect acts in (0..9).
    pub start: u8,
    /// Last zone, for the modes that name a range.
    pub end: u8,
    /// 1..8: resistance, catch, first foot, amplitude A.
    pub strength: u8,
    /// Bow snap-back (1..8), galloping second foot (0..7), machine amplitude B
    /// (0..7).
    pub second: u8,
    /// Vibration / galloping / machine frequency in Hz.
    pub frequency: u8,
    /// Machine period.
    pub period: u8,
}

impl Trigger {
    /// The effect that frees a trigger.
    pub const OFF: Trigger = Trigger {
        mode: TRIGGER_OFF_MODE,
        start: 0,
        end: 0,
        strength: 0,
        second: 0,
        frequency: 0,
        period: 0,
    };

    /// Even resistance from `start` to the end of the travel.
    pub fn feedback(start: u8, strength: u8) -> Trigger {
        Trigger {
            mode: TRIGGER_FEEDBACK,
            start,
            strength,
            ..Trigger::default()
        }
    }

    /// A gun trigger: resistance from `start` that gives way with a click at
    /// `end` (2 <= start < end <= 8).
    pub fn weapon(start: u8, end: u8, strength: u8) -> Trigger {
        Trigger {
            mode: TRIGGER_WEAPON,
            start,
            end,
            strength,
            ..Trigger::default()
        }
    }

    /// A buzz from `start` on at `amplitude` (1..8) and `frequency` Hz.
    pub fn vibration(start: u8, amplitude: u8, frequency: u8) -> Trigger {
        Trigger {
            mode: TRIGGER_VIBRATION,
            start,
            strength: amplitude,
            frequency,
            ..Trigger::default()
        }
    }

    /// The ten parameter bytes of the effect.
    pub fn params(&self) -> [u8; 10] {
        let mut p = [0u8; 10];
        match self.mode {
            TRIGGER_OFF_MODE => {}
            TRIGGER_FEEDBACK | TRIGGER_VIBRATION => {
                let (active, values) = uniform_zones(self.start, self.strength);
                p[0..2].copy_from_slice(&active.to_le_bytes());
                p[2..6].copy_from_slice(&values.to_le_bytes());
                if self.mode == TRIGGER_VIBRATION {
                    p[8] = self.frequency;
                }
            }
            TRIGGER_WEAPON => {
                // The click is always past the start, inside the usable travel.
                let start = self.start.clamp(2, 7);
                let end = self.end.clamp(start + 1, 8);
                let zones = (1u16 << start) | (1u16 << end);
                p[0..2].copy_from_slice(&zones.to_le_bytes());
                p[2] = strength_bits(self.strength);
            }
            TRIGGER_BOW => {
                let zones = boundary_zones(self.start, self.end);
                p[0..2].copy_from_slice(&zones.to_le_bytes());
                p[2] = strength_bits(self.strength) | (strength_bits(self.second) << 3);
            }
            TRIGGER_GALLOPING => {
                let zones = boundary_zones(self.start, self.end);
                p[0..2].copy_from_slice(&zones.to_le_bytes());
                p[2] = (self.second & 0x07) | ((strength_bits(self.strength)) << 3);
                p[3] = self.frequency;
            }
            TRIGGER_MACHINE => {
                let zones = boundary_zones(self.start, self.end);
                p[0..2].copy_from_slice(&zones.to_le_bytes());
                p[2] = (strength_bits(self.strength)) | ((self.second & 0x07) << 3);
                p[3] = self.frequency;
                p[4] = self.period;
            }
            // An unknown mode frees the trigger rather than leaving it stuck.
            _ => p[0] = 0,
        }
        p
    }
}

/// Zones `from`..9 at `strength`: returns the active mask and the packed
/// three-bit levels (`strength - 1`).
fn uniform_zones(from: u8, strength: u8) -> (u16, u32) {
    let from = usize::from(from.min(9));
    let level = strength.clamp(1, 8);
    let mut active = 0u16;
    let mut values = 0u32;
    for zone in from..10 {
        active |= 1 << zone;
        values |= u32::from(level - 1) << (3 * zone);
    }
    (active, values)
}

fn boundary_zones(start: u8, end: u8) -> u16 {
    (1u16 << start.min(9)) | (1u16 << end.min(9))
}

fn strength_bits(strength: u8) -> u8 {
    strength.clamp(1, 8) - 1
}

/// How the pad takes vibration: newer firmware has a better rumble emulation,
/// picked with its own flag; the legacy one works on every firmware.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Vibration {
    Legacy,
    #[default]
    V2,
}

impl Vibration {
    /// From the product and the firmware's update version (feature report
    /// `0x20`, bytes 44..46). The Edge always has the newer emulation; SDL
    /// picks it from `0x0224`, the kernel from `0x0215` — we take SDL's, and
    /// legacy below it or when the version is unknown.
    pub fn of(product: u16, update_version: Option<u16>) -> Vibration {
        if product == DUALSENSE_EDGE || update_version.is_some_and(|v| v >= 0x0224) {
            Vibration::V2
        } else {
            Vibration::Legacy
        }
    }
}

/// What an output report changes; `None` leaves that part of the pad as it is.
///
/// `rumble` is always applied: `[0, 0]` clears the rumble flags, which also
/// re-arms the audio haptics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Output {
    /// Left (heavy, low) and right (light, high) motor, 0..255.
    pub rumble: [u8; 2],
    pub left_trigger: Option<Trigger>,
    pub right_trigger: Option<Trigger>,
    pub lightbar: Option<[u8; 3]>,
    /// The five LEDs under the touchpad, left to right in bits 0..4.
    pub player_leds: Option<u8>,
    /// `0` off, `1` on, `2` pulsing.
    pub mic_led: Option<u8>,
    /// Hand the lightbar to the host: the firmware's blue light fades out.
    /// Sent alone, before any colour.
    pub release_leds: bool,
    /// Undo the motor and trigger attenuation another host left in the pad.
    pub full_power: bool,
}

impl Output {
    /// Everything off: triggers free, motors still.
    pub fn release() -> Output {
        Output {
            left_trigger: Some(Trigger::OFF),
            right_trigger: Some(Trigger::OFF),
            ..Output::default()
        }
    }

    /// The block both transports carry.
    pub fn common(&self, vibration: Vibration) -> [u8; COMMON_LEN] {
        let mut c = [0u8; COMMON_LEN];
        if self.rumble != [0, 0] {
            // Classic rumble rather than the audio-driven haptics.
            c[FLAGS0] |= FLAG0_HAPTICS_SELECT;
            let [left, right] = match vibration {
                Vibration::V2 => {
                    c[FLAGS2] |= FLAG2_VIBRATION_V2;
                    self.rumble
                }
                Vibration::Legacy => {
                    c[FLAGS0] |= FLAG0_COMPATIBLE_VIBRATION;
                    // The old emulation shakes about twice as hard: SDL halves it.
                    self.rumble.map(|m| m >> 1)
                }
            };
            c[MOTOR_LEFT] = left;
            c[MOTOR_RIGHT] = right;
        }
        if self.full_power {
            c[FLAGS1] |= FLAG1_ATTENUATION;
            c[ATTENUATION] = 0;
        }
        if let Some(effect) = self.right_trigger {
            c[FLAGS0] |= FLAG0_RIGHT_TRIGGER;
            c[RIGHT_TRIGGER..RIGHT_TRIGGER + 11].copy_from_slice(&block(effect));
        }
        if let Some(effect) = self.left_trigger {
            c[FLAGS0] |= FLAG0_LEFT_TRIGGER;
            c[LEFT_TRIGGER..LEFT_TRIGGER + 11].copy_from_slice(&block(effect));
        }
        if self.release_leds {
            c[FLAGS1] |= FLAG1_RELEASE_LEDS;
            c[FLAGS2] |= FLAG2_LIGHTBAR_SETUP;
            c[LIGHTBAR_SETUP] = LIGHTBAR_LIGHT_OUT;
        }
        if let Some(rgb) = self.lightbar {
            c[FLAGS1] |= FLAG1_LIGHTBAR;
            c[LIGHTBAR..LIGHTBAR + 3].copy_from_slice(&rgb);
            c[LED_BRIGHTNESS] = 0;
        }
        if let Some(leds) = self.player_leds {
            c[FLAGS1] |= FLAG1_PLAYER_LEDS;
            c[PLAYER_LEDS] = leds;
        }
        if let Some(mode) = self.mic_led {
            c[FLAGS1] |= FLAG1_MIC_LED;
            c[MIC_LED] = mode & 0x03;
            // Power-save bit: clear means "mic muted" on the pad's own lights.
            c[POWER_SAVE] = if mode & 0x03 == 0 { 0x10 } else { 0x00 };
        }
        c
    }

    /// Report `0x02`, for a pad on USB.
    pub fn usb(&self, vibration: Vibration) -> [u8; USB_REPORT_LEN] {
        let mut r = [0u8; USB_REPORT_LEN];
        r[0] = USB_REPORT_ID;
        r[1..].copy_from_slice(&self.common(vibration));
        r
    }

    /// Report `0x31`, for a pad on Bluetooth: `seq` (0..15, wrapping) in the
    /// high nibble of the second byte, then the tag, the block, and the CRC-32
    /// of the seed byte and everything before it, little-endian.
    pub fn bluetooth(&self, vibration: Vibration, seq: u8) -> [u8; BT_REPORT_LEN] {
        let mut r = [0u8; BT_REPORT_LEN];
        r[0] = BT_REPORT_ID;
        r[1] = (seq & 0x0f) << 4;
        r[2] = BT_TAG;
        r[3..3 + COMMON_LEN].copy_from_slice(&self.common(vibration));
        let crc = bt_crc(&r[..BT_REPORT_LEN - 4]);
        r[BT_REPORT_LEN - 4..].copy_from_slice(&crc.to_le_bytes());
        r
    }
}

fn block(effect: Trigger) -> [u8; 11] {
    let mut out = [0u8; 11];
    out[0] = effect.mode;
    out[1..].copy_from_slice(&effect.params());
    out
}

/// The report that switches the pad's actuators to HD audio haptics
/// (dse-haptics / nondebug's initialisation block).
pub fn audio_haptics_init() -> [u8; USB_REPORT_LEN] {
    let mut r = [0u8; USB_REPORT_LEN];
    r[0] = USB_REPORT_ID;
    r[1] = 0xa0;
    r[2] = 0x80;
    r[5] = 0x7f;
    r[6] = 0x64;
    r[7] = 0x40;
    r[8] = 0x30;
    r[38] = 0x02;
    r
}

/// The CRC a Bluetooth output report ends with: CRC-32 over the seed byte
/// `0xA2` and the report.
pub fn bt_crc(report: &[u8]) -> u32 {
    !report.iter().fold(crc32_update(!0, BT_CRC_SEED), |crc, b| {
        crc32_update(crc, *b)
    })
}

/// One byte of the reflected CRC-32 (zlib's, polynomial `0xEDB88320`).
fn crc32_update(crc: u32, byte: u8) -> u32 {
    let mut crc = crc ^ u32::from(byte);
    for _ in 0..8 {
        crc = (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg());
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crc32(data: &[u8]) -> u32 {
        !data.iter().fold(!0, |crc, b| crc32_update(crc, *b))
    }

    #[test]
    fn crc_is_zlibs() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(bt_crc(b"123456789"), crc32(b"\xa2123456789"));
    }

    #[test]
    fn usb_report_layout() {
        let out = Output {
            rumble: [200, 40],
            right_trigger: Some(Trigger {
                mode: TRIGGER_WEAPON,
                start: 2,
                end: 5,
                strength: 5,
                ..Trigger::default()
            }),
            lightbar: Some([255, 128, 0]),
            player_leds: Some(PLAYER_1),
            ..Output::default()
        };
        let r = out.usb(Vibration::V2);
        assert_eq!(r.len(), 48);
        assert_eq!(r[0], USB_REPORT_ID);
        // Haptics select + right trigger in flags 0 (v2 rumble has no legacy
        // compatible-vibration bit), lightbar and player LEDs in flags 1.
        assert_eq!(r[1], 0b0000_0110);
        assert_eq!(r[2], 0b0001_0100);
        // Right motor, then left.
        assert_eq!(r[3..5], [40, 200]);
        // Trigger block: mode, zone bits (zones 2 and 5), strength - 1.
        assert_eq!(r[11], TRIGGER_WEAPON);
        assert_eq!(r[12..14], (1u16 << 2 | 1u16 << 5).to_le_bytes());
        assert_eq!(r[14], 4);
        // The left trigger is untouched.
        assert_eq!(r[22..33], [0; 11]);
        // flags2: the newer rumble emulation.
        assert_eq!(r[39], 0x04);
        assert_eq!(r[44], PLAYER_1);
        assert_eq!(r[45..48], [255, 128, 0]);
    }

    #[test]
    fn legacy_vibration_is_halved_and_flagged_the_old_way() {
        let c = Output {
            rumble: [200, 41],
            ..Output::default()
        }
        .common(Vibration::Legacy);
        assert_eq!(c[FLAGS0], 0x03);
        assert_eq!(c[MOTOR_RIGHT..=MOTOR_LEFT], [20, 100]);
        assert_eq!(c[FLAGS2], 0);
    }

    #[test]
    fn still_motors_send_no_rumble_flags() {
        assert_eq!(Output::default().common(Vibration::V2), [0; COMMON_LEN]);
        assert_eq!(Output::default().common(Vibration::Legacy), [0; COMMON_LEN]);
    }

    #[test]
    fn vibration_mode_of_the_firmware() {
        assert_eq!(Vibration::of(DUALSENSE, Some(0x0223)), Vibration::Legacy);
        assert_eq!(Vibration::of(DUALSENSE, Some(0x0224)), Vibration::V2);
        assert_eq!(Vibration::of(DUALSENSE, Some(0x0641)), Vibration::V2);
        assert_eq!(Vibration::of(DUALSENSE, None), Vibration::Legacy);
        assert_eq!(Vibration::of(DUALSENSE_EDGE, None), Vibration::V2);
    }

    #[test]
    fn release_frees_both_triggers_and_stops_the_motors() {
        let c = Output::release().common(Vibration::V2);
        assert_eq!(c[FLAGS0], FLAG0_LEFT_TRIGGER | FLAG0_RIGHT_TRIGGER);
        assert_eq!(c[MOTOR_RIGHT..=MOTOR_LEFT], [0, 0]);
        assert_eq!(c[RIGHT_TRIGGER..RIGHT_TRIGGER + 11][0], TRIGGER_OFF_MODE);
        assert_eq!(c[LEFT_TRIGGER..LEFT_TRIGGER + 11][0], TRIGGER_OFF_MODE);
        assert_eq!(c[FLAGS1], 0);
        assert_eq!(c[FLAGS2], 0);
    }

    #[test]
    fn full_power_clears_the_attenuation() {
        let c = Output {
            full_power: true,
            ..Output::default()
        }
        .common(Vibration::V2);
        assert_eq!(c[FLAGS1], FLAG1_ATTENUATION);
        assert_eq!(c[ATTENUATION], 0);
    }

    #[test]
    fn release_leds_resets_the_lights() {
        let mut expected = [0u8; BT_REPORT_LEN];
        expected[..5].copy_from_slice(&[0x31, 0x50, 0x10, 0x00, 0x08]);
        expected[3 + 38] = 0x02;
        expected[3 + 41] = 0x02;
        expected[74..].copy_from_slice(&0xc72d_0f94u32.to_le_bytes());
        let out = Output {
            release_leds: true,
            ..Output::default()
        };
        assert_eq!(out.bluetooth(Vibration::V2, 5), expected);
    }

    #[test]
    fn bluetooth_report_layout() {
        let out = Output {
            lightbar: Some([1, 2, 3]),
            ..Output::default()
        };
        let r = out.bluetooth(Vibration::V2, 0x13);
        assert_eq!(r.len(), 78);
        assert_eq!(r[..3], [0x31, 0x30, 0x10]);
        assert_eq!(r[3..50], out.common(Vibration::V2));
        assert_eq!(r[50..74], [0; 24]);
        let crc = u32::from_le_bytes(r[74..].try_into().unwrap());
        assert_eq!(crc, crc32(&[&[0xa2], &r[..74]].concat()));
        assert_ne!(out.bluetooth(Vibration::V2, 1)[74..], r[74..]);
    }

    #[test]
    fn mic_led_and_power_save() {
        let c = Output {
            mic_led: Some(1),
            ..Output::default()
        }
        .common(Vibration::V2);
        assert_eq!(c[FLAGS1], FLAG1_MIC_LED);
        assert_eq!(c[MIC_LED], 1);
        assert_eq!(c[POWER_SAVE], 0);

        let off = Output {
            mic_led: Some(0),
            ..Output::default()
        }
        .common(Vibration::V2);
        assert_eq!(off[MIC_LED], 0);
        assert_eq!(off[POWER_SAVE], 0x10);
    }

    #[test]
    fn trigger_encodings_match_the_references() {
        // DSX++'s README, Weapon {2, 6, 8}: zones 2 and 6, strength 8 → 7.
        assert_eq!(
            block(Trigger::weapon(2, 6, 8)),
            [0x25, 0x44, 0x00, 0x07, 0, 0, 0, 0, 0, 0, 0]
        );
        // dualsensectl's wiki, `weapon 2 3 8`.
        assert_eq!(
            block(Trigger::weapon(2, 3, 8))[..4],
            [0x25, 0x0c, 0x00, 0x07]
        );
        // The click always sits past the start.
        assert_eq!(Trigger::weapon(7, 3, 8).params()[0], 0x80);
        assert_eq!(Trigger::weapon(7, 3, 8).params()[1], 0x01);
        // Feedback from zone 3 at strength 6 (stored as 5, three bits each).
        assert_eq!(
            block(Trigger::feedback(3, 6)),
            [0x21, 0xf8, 0x03, 0x00, 0xda, 0xb6, 0x2d, 0, 0, 0, 0]
        );
        assert_eq!(
            block(Trigger::feedback(0, 1)),
            [0x21, 0xff, 0x03, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        // Vibration {2, 8, 20}: zones 2..9, amplitude 8 (stored 7), frequency 20.
        assert_eq!(
            block(Trigger::vibration(2, 8, 20)),
            [0x26, 0xfc, 0x03, 0xc0, 0xff, 0xff, 0x3f, 0, 0, 20, 0]
        );
        // Off is mode 0x05 and zeros.
        assert_eq!(block(Trigger::OFF), [0x05, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        // Unknown modes free the trigger.
        assert_eq!(
            block(Trigger {
                mode: 0x01,
                ..Trigger::default()
            })[0],
            0x01
        );
        assert_eq!(
            block(Trigger {
                mode: 0x99,
                ..Trigger::default()
            })[1..],
            [0; 10]
        );
    }

    #[test]
    fn audio_haptics_init_bytes() {
        let r = audio_haptics_init();
        assert_eq!(r[0], 0x02);
        assert_eq!(r[1], 0xa0);
        assert_eq!(r[2], 0x80);
        assert_eq!(r[5..9], [0x7f, 0x64, 0x40, 0x30]);
        assert_eq!(r[38], 0x02);
        assert!(r[9..38].iter().all(|b| *b == 0));
    }

    #[test]
    fn dualsense_ids() {
        assert!(is_dualsense(0x054c, 0x0ce6));
        assert!(is_dualsense(0x054c, 0x0df2));
        assert!(!is_dualsense(0x054c, 0x09cc));
    }
}
