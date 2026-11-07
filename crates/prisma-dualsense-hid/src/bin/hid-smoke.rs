// Minimal end-to-end check: opens the pad through the same path Unity uses,
// waits for it, reports input counts, plays a short vibration and releases.
//
// Usage: cargo run -p prisma-dualsense-hid --bin hid-smoke

use prisma_dualsense_hid::device::{Control, Rumble, Step};
use prisma_dualsense_hid::input::InputState;
use std::thread;
use std::time::{Duration, Instant};

fn main() {
    let control = Control::start();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !control.status().open && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    let status = control.status();
    if !status.open {
        eprintln!("HID_OPEN_FAILED: {}", control.last_error());
        std::process::exit(1);
    }
    println!(
        "HID_OPEN_OK transport={} vibration={} firmware=0x{:04x} reports={}",
        status.transport, status.vibration, status.update_version, status.reports_read
    );

    let mut raw = [0u8; 64];
    let mut state = InputState::default();
    let mut size = 0u32;
    let mut reports = 0u32;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if control.poll_input(&mut raw, &mut size, &mut state) == 1 {
            reports += 1;
        }
        thread::sleep(Duration::from_millis(8));
    }
    println!("HID_REPORTS: {reports}");

    control.set_rumble(Rumble {
        steps: [
            Step {
                left: 200,
                right: 120,
                millis: 150,
            },
            Step::default(),
            Step::default(),
            Step::default(),
            Step::default(),
            Step::default(),
            Step::default(),
            Step::default(),
        ],
        len: 1,
    });
    thread::sleep(Duration::from_millis(250));
    control.release();
    thread::sleep(Duration::from_millis(100));
    println!("HID_RELEASE_OK sent={}", control.status().reports_sent);
}
