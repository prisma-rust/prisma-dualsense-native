// Raw report dump: prints the whole input report and the decoded fields, plus
// the old (DualShock 4) gyro/accel offsets next to the real ones, so a layout
// change is obvious.
//
// Usage: cargo run -p prisma-dualsense-hid --example hid-dump -- [seconds]
//
// The read handle is opened shared, so this runs alongside Unity and each gets
// its own copy of every report.

use prisma_dualsense_hid::device::Control;
use prisma_dualsense_hid::input::InputState;
use std::thread;
use std::time::{Duration, Instant};

fn main() {
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(4);

    let control = Control::start();
    let mut buffer = [0u8; 64];
    let mut state = InputState::default();
    let mut previous = [0u8; 64];
    let mut have_previous = false;
    let mut reports = 0u32;
    let mut printed = 0u32;
    let started = Instant::now();
    let deadline = started + Duration::from_secs(seconds);
    let mut announced = false;

    while Instant::now() < deadline {
        let status = control.status();
        if status.open && !announced {
            announced = true;
            println!(
                "device: transport={} vibration={} firmware=0x{:04x}",
                match status.transport {
                    1 => "usb",
                    2 => "bluetooth",
                    _ => "?",
                },
                match status.vibration {
                    1 => "legacy",
                    2 => "v2",
                    _ => "?",
                },
                status.update_version
            );
        }
        let mut size = 0u32;
        match control.poll_input(&mut buffer, &mut size, &mut state) {
            1 => {
                reports += 1;
                let length = size as usize;
                let sampled = printed == 0 || started.elapsed().as_millis() / 750 > printed as u128;
                if sampled && printed < 6 {
                    printed += 1;
                    println!(
                        "=== t={:.2}s size={length} ===",
                        started.elapsed().as_secs_f32()
                    );
                    println!("hex : {}", hex(&buffer[..length]));
                    decode_candidates(&buffer[..length], &state);
                    if have_previous {
                        println!(
                            "delta vs prev: {}",
                            diff(&previous[..length], &buffer[..length])
                        );
                    }
                }
                previous[..length].copy_from_slice(&buffer[..length]);
                have_previous = true;
            }
            -1 => println!("unrecognised report id 0x{:02x}", buffer[0]),
            _ => {}
        }
        if !status.open && !control.last_error().is_empty() && !announced {
            eprintln!("waiting for a pad: {}", control.last_error());
        }
        thread::sleep(Duration::from_millis(4));
    }

    println!("--- reports read: {reports} ---");
}

fn decode_candidates(report: &[u8], state: &InputState) {
    let base = if report[0] == 0x31 { 1 } else { 0 };
    let i16_at = |offset: usize| -> Option<i16> {
        let index = base + offset;
        (index + 1 < report.len()).then(|| i16::from_le_bytes([report[index], report[index + 1]]))
    };

    println!(
        "gyro  @0x0D(old) {:?}   decoded {:?}",
        triple(i16_at, 13),
        state.gyro
    );
    println!(
        "accel @0x13(old) {:?}   decoded {:?}",
        triple(i16_at, 19),
        state.accel
    );
    if state.status_valid != 0 {
        println!(
            "status {:02x} {:02x} {:02x} -> battery {}% (level {}) charging {}",
            state.status0,
            state.status1,
            state.status2,
            state.battery_percent(),
            state.battery_level(),
            charging_text(state.charging()),
        );
        println!(
            "jack: headset={} microphone={} muted={}",
            state.headphone_plugged(),
            state.microphone_plugged(),
            state.microphone_muted()
        );
    } else {
        println!("status: report too short");
    }
    if state.any_touch() {
        let touch = state.primary_touch();
        println!("touch: id={} ({}, {})", touch.id, touch.x, touch.y);
    }
}

fn triple(f: impl Fn(usize) -> Option<i16>, offset: usize) -> String {
    match (f(offset), f(offset + 2), f(offset + 4)) {
        (Some(x), Some(y), Some(z)) => format!("({x}, {y}, {z})"),
        _ => "out of range".to_string(),
    }
}

fn charging_text(value: u8) -> &'static str {
    match value {
        0x0 => "discharging",
        0x1 => "charging",
        0x2 => "charged",
        0xa => "voltage/temperature out of range",
        0xb => "temperature error",
        0xf => "charging error",
        _ => "unknown",
    }
}

fn diff(previous: &[u8], current: &[u8]) -> String {
    let mut parts = Vec::new();
    for (index, (a, b)) in previous.iter().zip(current.iter()).enumerate() {
        if a != b {
            parts.push(format!("{index:02x}:{a:02x}>{b:02x}"));
        }
    }
    if parts.len() > 24 {
        parts.truncate(24);
        parts.push("...".to_string());
    }
    parts.join(" ")
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[allow(dead_code)]
fn text(value: &[u8]) -> String {
    let length = value
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(value.len());
    String::from_utf8_lossy(&value[..length]).into_owned()
}
