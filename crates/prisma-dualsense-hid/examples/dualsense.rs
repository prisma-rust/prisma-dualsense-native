// Plays each output on a real DualSense and reads back what the pad reports, so
// a change to the report can be checked on hardware.
//
// Usage: cargo run -p prisma-dualsense-hid --example dualsense -- [all|light|triggers|rumble]

use prisma_dualsense_hid::device::{Control, PadState, Rumble, Step};
use prisma_dualsense_hid::input::InputState;
use prisma_dualsense_hid::report::{Trigger, PLAYER_1, TRIGGER_WEAPON};
use std::thread;
use std::time::Duration;

fn main() {
    let which = std::env::args().nth(1).unwrap_or_else(|| "all".to_string());
    let control = Control::start();
    if !wait_for_pad(&control) {
        eprintln!("no DualSense found ({})", control.last_error());
        std::process::exit(1);
    }
    println!(
        "DualSense over {} (firmware {})",
        if control.status().transport == 1 {
            "USB"
        } else {
            "Bluetooth"
        },
        control.status().update_version
    );

    if which == "all" || which == "light" {
        light(&control);
    }
    if which == "all" || which == "triggers" {
        triggers(&control);
    }
    if which == "all" || which == "rumble" {
        rumble(&control);
    }

    control.set_state(PadState::release());
    thread::sleep(Duration::from_millis(200));
    control.release();
    println!("released");
}

fn wait_for_pad(control: &Control) -> bool {
    for _ in 0..50 {
        if control.status().open {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

fn light(control: &Control) {
    for (name, colour) in [
        ("red", [255, 0, 0]),
        ("green", [0, 255, 0]),
        ("blue", [0, 0, 255]),
    ] {
        println!("lightbar: {name}");
        control.set_state(PadState {
            lightbar: Some(colour),
            player_leds: Some(PLAYER_1),
            ..PadState::default()
        });
        thread::sleep(Duration::from_millis(700));
    }
    control.set_state(PadState::default());
}

fn triggers(control: &Control) {
    println!("triggers: weapon 2..5 strength 8, feedback from zone 3");
    control.set_state(PadState {
        right_trigger: Some(Trigger {
            mode: TRIGGER_WEAPON,
            start: 2,
            end: 5,
            strength: 8,
            ..Trigger::default()
        }),
        left_trigger: Some(Trigger::feedback(3, 5)),
        ..PadState::default()
    });
    thread::sleep(Duration::from_secs(3));
    control.set_state(PadState {
        right_trigger: Some(Trigger::OFF),
        left_trigger: Some(Trigger::OFF),
        ..PadState::default()
    });
}

fn rumble(control: &Control) {
    println!("rumble: a hit that dies away");
    control.set_rumble(Rumble {
        steps: [
            Step {
                left: 230,
                right: 180,
                millis: 50,
            },
            Step {
                left: 130,
                right: 90,
                millis: 120,
            },
            Step {
                left: 60,
                right: 40,
                millis: 200,
            },
            Step::default(),
            Step::default(),
            Step::default(),
            Step::default(),
            Step::default(),
        ],
        len: 3,
    });
    thread::sleep(Duration::from_millis(600));
}

#[allow(dead_code)]
fn read_back(control: &Control) {
    let mut raw = [0u8; 64];
    let mut state = InputState::default();
    let mut size = 0;
    if control.poll_input(&mut raw, &mut size, &mut state) == 1 {
        println!(
            "battery {}% charging {} touch {}",
            state.battery_percent(),
            state.is_charging(),
            state.any_touch()
        );
    }
}
