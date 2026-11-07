//! The device owner: a writer thread that coalesces semantic state, writes only
//! what changed, rescans on loss, and keeps the pad off-safe.
//!
//! The calling (Unity) thread never touches the device: `set_state` and
//! `set_rumble` copy a small `Copy` value into a fixed action queue and wake the
//! writer. That matters because a Bluetooth write can take milliseconds.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::input::{self, InputState};
use crate::report::{Output, Trigger, DEFAULT_LIGHTBAR, PLAYER_1};
use crate::win32::{self, Mailbox, Pad};

/// The pad's steady state: what it shows while no timed vibration is playing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PadState {
    pub lightbar: Option<[u8; 3]>,
    pub player_leds: Option<u8>,
    /// 0 off, 1 on, 2 pulsing.
    pub mic_led: Option<u8>,
    pub left_trigger: Option<Trigger>,
    pub right_trigger: Option<Trigger>,
    /// Motors held while no rumble shape is active.
    pub motors: [u8; 2],
}

impl PadState {
    /// Everything off, but leave the lights alone.
    pub fn release() -> PadState {
        PadState {
            left_trigger: Some(Trigger::OFF),
            right_trigger: Some(Trigger::OFF),
            ..PadState::default()
        }
    }
}

/// One step of a vibration shape: the motors' levels for a number of
/// milliseconds (1..65535).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Step {
    /// Left (heavy) motor, 0..255.
    pub left: u8,
    /// Right (light) motor, 0..255.
    pub right: u8,
    pub millis: u16,
}

/// A vibration shape, capped so the queue stays allocation-free.
pub const MAX_RUMBLE_STEPS: usize = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rumble {
    pub steps: [Step; MAX_RUMBLE_STEPS],
    pub len: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StatusSnapshot {
    pub open: bool,
    pub transport: u8,
    pub vibration: u8,
    pub update_version: u16,
    pub lost: bool,
    pub scans_left: u32,
    pub reports_sent: u32,
    pub write_failures: u32,
    pub reports_read: u32,
    pub last_report_len: u8,
}

#[derive(Default)]
struct Status {
    open: bool,
    lost: bool,
    transport: u8,
    vibration: u8,
    update_version: u16,
    scans_left: u32,
    reports_sent: u32,
    write_failures: u32,
}

#[derive(Clone, Copy, Debug)]
enum Action {
    State(PadState),
    Rumble(Rumble),
    Release,
    /// Switch the actuators to (or out of) HD audio haptics.
    AudioHaptics(bool),
}

const ACTION_QUEUE: usize = 8;

#[derive(Clone, Copy)]
struct Pending {
    queue: [Option<Action>; ACTION_QUEUE],
    head: usize,
    len: usize,
}

impl Default for Pending {
    fn default() -> Self {
        Pending {
            queue: [None; ACTION_QUEUE],
            head: 0,
            len: 0,
        }
    }
}

impl Pending {
    fn push(&mut self, action: Action) {
        if self.len == ACTION_QUEUE {
            // Drop the oldest: the newest instruction wins.
            self.head = (self.head + 1) % ACTION_QUEUE;
            self.len -= 1;
        }
        let tail = (self.head + self.len) % ACTION_QUEUE;
        self.queue[tail] = Some(action);
        self.len += 1;
    }

    fn pop(&mut self) -> Option<Action> {
        if self.len == 0 {
            return None;
        }
        let action = self.queue[self.head].take();
        self.head = (self.head + 1) % ACTION_QUEUE;
        self.len -= 1;
        action
    }
}

struct Inner {
    pending: Mutex<Pending>,
    wake: Condvar,
    stop: AtomicBool,
    rescan: AtomicBool,
    status: Mutex<Status>,
    last_error: Mutex<String>,
    last_report: Mutex<([u8; 78], u8)>,
    mailbox: Arc<Mutex<Mailbox>>,
    reads: Arc<AtomicU32>,
    delivered: AtomicU32,
}

impl Inner {
    fn set_error(&self, message: impl Into<String>) {
        let mut slot = self.last_error.lock().unwrap_or_else(|e| e.into_inner());
        *slot = message.into();
    }

    fn status(&self) -> StatusSnapshot {
        let status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        let last_len = self.last_report.lock().unwrap_or_else(|e| e.into_inner()).1;
        StatusSnapshot {
            open: status.open,
            transport: status.transport,
            vibration: status.vibration,
            update_version: status.update_version,
            lost: status.lost,
            scans_left: status.scans_left,
            reports_sent: status.reports_sent,
            write_failures: status.write_failures,
            reports_read: self.reads.load(Ordering::Relaxed),
            last_report_len: last_len,
        }
    }
}

/// Owns the writer thread. Dropping it releases the pad and joins the thread.
pub struct Control {
    inner: Arc<Inner>,
    writer: Option<JoinHandle<()>>,
}

impl Control {
    pub fn start() -> Control {
        let inner = Arc::new(Inner {
            pending: Mutex::new(Pending::default()),
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
            rescan: AtomicBool::new(false),
            status: Mutex::new(Status::default()),
            last_error: Mutex::new(String::new()),
            last_report: Mutex::new(([0; 78], 0)),
            mailbox: Arc::new(Mutex::new(Mailbox::default())),
            reads: Arc::new(AtomicU32::new(0)),
            delivered: AtomicU32::new(0),
        });
        let writer = {
            let inner = Arc::clone(&inner);
            thread::Builder::new()
                .name("prisma-dualsense-hid-writer".into())
                .spawn(move || Writer::new(inner).run())
                .ok()
        };
        Control { inner, writer }
    }

    /// Show this state from now on. Cheap and non-blocking.
    pub fn set_state(&self, state: PadState) {
        self.push(Action::State(state));
    }

    /// Play a vibration shape, replacing the one playing.
    pub fn set_rumble(&self, rumble: Rumble) {
        self.push(Action::Rumble(rumble));
    }

    /// Free the triggers and stop the motors.
    pub fn release(&self) {
        self.push(Action::Release);
    }

    /// `true`: send the HD-audio haptics initialisation report.
    /// `false`: a release report, which hands the actuators back to emulation.
    pub fn audio_haptics(&self, enable: bool) {
        self.push(Action::AudioHaptics(enable));
    }

    /// Look for the pad again (a controller was connected).
    pub fn rescan(&self) {
        self.inner.rescan.store(true, Ordering::Release);
        self.inner.wake.notify_one();
    }

    pub fn status(&self) -> StatusSnapshot {
        self.inner.status()
    }

    pub fn last_error(&self) -> String {
        self.inner
            .last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Copies the last report actually written (for the debug panel's hex line).
    pub fn last_report(&self, buffer: &mut [u8]) -> usize {
        let report = self
            .inner
            .last_report
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let copy = (report.1 as usize).min(buffer.len());
        buffer[..copy].copy_from_slice(&report.0[..copy]);
        copy
    }

    /// 1: a new report was decoded, 0: no new report, -1: unknown/short report.
    pub fn poll_input(&self, raw: &mut [u8], raw_len: &mut u32, out: &mut InputState) -> i32 {
        let seq = win32::read_mailbox(&self.inner.mailbox, raw, raw_len);
        if *raw_len == 0 {
            return 0;
        }
        if seq == self.inner.delivered.load(Ordering::Acquire) {
            return 0;
        }
        self.inner.delivered.store(seq, Ordering::Release);
        if !input::decode(raw, *raw_len as usize, out) {
            return -1;
        }
        1
    }

    fn push(&self, action: Action) {
        {
            let mut pending = self.inner.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.push(action);
        }
        self.inner.wake.notify_one();
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Release);
        self.inner.wake.notify_all();
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

/// Looks for the pad this many more times, a second apart, after a scan finds
/// nothing (the HID node can appear after the controller).
const SCAN_TRIES: u32 = 5;

/// The sensor clock of a Bluetooth input report 0x31: its state block starts at
/// byte 2 and holds the clock at 27..31, little-endian.
const ANIMATION_OVER: u32 = 10_200_000;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Lights {
    /// Over Bluetooth, until the pad's connection animation is over.
    Waiting(Instant),
    /// They may be set, after the reset report that hands them to the host.
    Ready,
    Reset,
}

struct Writer {
    inner: Arc<Inner>,
    pad: Option<Pad>,
    state: PadState,
    /// What the pad was last sent; `None` after (re)opening.
    sent: Option<(PadState, [u8; 2])>,
    lights: Lights,
    /// The vibration playing, and where in it we are.
    rumble: Rumble,
    cursor: usize,
    step_end: Option<Instant>,
    scans_left: u32,
    next_scan: Instant,
}

impl Writer {
    fn new(inner: Arc<Inner>) -> Writer {
        Writer {
            inner,
            pad: None,
            state: PadState::default(),
            sent: None,
            lights: Lights::Ready,
            rumble: Rumble::default(),
            cursor: 0,
            step_end: None,
            scans_left: 1,
            next_scan: Instant::now(),
        }
    }

    fn run(mut self) {
        loop {
            if self.inner.stop.load(Ordering::Acquire) {
                self.release();
                break;
            }
            self.apply_pending();
            if self.inner.rescan.swap(false, Ordering::AcqRel) {
                if self.pad.as_ref().is_some_and(|pad| pad.lost()) {
                    self.lost("rescan requested");
                }
                self.scans_left = SCAN_TRIES;
                self.next_scan = Instant::now();
            }
            if self.pad.as_ref().is_some_and(|pad| pad.lost()) {
                self.lost("unplugged");
            }
            let now = Instant::now();
            if self.pad.is_none() && self.scans_left > 0 && now >= self.next_scan {
                self.scan();
            }
            self.watch_animation();
            self.advance_rumble();
            self.flush();
            self.wait();
        }
    }

    fn apply_pending(&mut self) {
        loop {
            let action = {
                let mut pending = self.inner.pending.lock().unwrap_or_else(|e| e.into_inner());
                pending.pop()
            };
            let Some(action) = action else {
                break;
            };
            match action {
                Action::State(state) => self.state = state,
                Action::Rumble(rumble) => {
                    self.rumble = rumble;
                    self.cursor = 0;
                    self.step_end = None;
                }
                Action::Release => {
                    self.rumble = Rumble::default();
                    self.cursor = 0;
                    self.step_end = None;
                    self.state = PadState {
                        lightbar: self.state.lightbar,
                        player_leds: self.state.player_leds,
                        mic_led: self.state.mic_led,
                        ..PadState::release()
                    };
                    self.write(&Output::release());
                }
                Action::AudioHaptics(enable) => {
                    if enable {
                        self.send_raw(&crate::report::audio_haptics_init());
                    } else {
                        self.write(&Output::release());
                    }
                    // Neither report reflects the tracked state: resend it next.
                    self.sent = None;
                }
            }
        }
    }

    fn wait(&mut self) {
        let now = Instant::now();
        let waiting = matches!(self.lights, Lights::Waiting(_)) && self.pad.is_some();
        let wake = [
            self.step_end,
            (self.pad.is_none() && self.scans_left > 0).then_some(self.next_scan),
            waiting.then(|| now + Duration::from_millis(250)),
        ]
        .into_iter()
        .flatten()
        .min();
        let pending = self.inner.pending.lock().unwrap_or_else(|e| e.into_inner());
        if pending.len > 0 {
            return;
        }
        match wake {
            Some(at) => {
                let (_guard, _timed_out) = self
                    .inner
                    .wake
                    .wait_timeout(pending, at.saturating_duration_since(now))
                    .unwrap_or_else(|e| e.into_inner());
            }
            None => drop(self.inner.wake.wait(pending)),
        }
    }

    fn scan(&mut self) {
        self.scans_left -= 1;
        self.next_scan = Instant::now() + Duration::from_secs(1);
        match win32::open(
            Arc::clone(&self.inner.mailbox),
            Arc::clone(&self.inner.reads),
        ) {
            Ok(pad) => {
                {
                    let mut status = self.inner.status.lock().unwrap_or_else(|e| e.into_inner());
                    status.open = true;
                    status.lost = false;
                    status.transport = pad.transport.code();
                    status.vibration = match pad.vibration {
                        crate::report::Vibration::Legacy => 1,
                        crate::report::Vibration::V2 => 2,
                    };
                    status.update_version = pad.update_version.unwrap_or(0);
                    status.scans_left = 0;
                }
                {
                    let mut mailbox = self.inner.mailbox.lock().unwrap_or_else(|e| e.into_inner());
                    *mailbox = Mailbox::default();
                }
                self.inner.delivered.store(0, Ordering::Release);
                self.lights = match pad.transport {
                    win32::Transport::Usb => Lights::Ready,
                    win32::Transport::Bluetooth => {
                        Lights::Waiting(Instant::now() + Duration::from_secs(1))
                    }
                };
                self.pad = Some(pad);
                self.sent = None;
                self.rumble = Rumble::default();
                self.cursor = 0;
                self.step_end = None;
                self.scans_left = 0;
                self.inner.set_error(String::new());
            }
            Err(error) => {
                self.inner.set_error(error);
            }
        }
    }

    /// Over Bluetooth, read the pad's clock until its connection animation is
    /// over (the pad paints over the lightbar until then).
    fn watch_animation(&mut self) {
        let Lights::Waiting(deadline) = self.lights else {
            return;
        };
        if self.pad.is_none() {
            return;
        }
        let mut over = Instant::now() >= deadline;
        if let Ok(mailbox) = self.inner.mailbox.lock() {
            if mailbox.len >= 34 && mailbox.buf[0] == crate::input::BLUETOOTH_REPORT_ID {
                let clock = u32::from_le_bytes([
                    mailbox.buf[2 + 27],
                    mailbox.buf[2 + 28],
                    mailbox.buf[2 + 29],
                    mailbox.buf[2 + 30],
                ]);
                if clock >= ANIMATION_OVER {
                    over = true;
                }
            }
        }
        if over {
            self.lights = Lights::Ready;
        }
    }

    fn advance_rumble(&mut self) {
        let now = Instant::now();
        if self.rumble.len == 0 {
            self.step_end = None;
            return;
        }
        if self.cursor >= self.rumble.len {
            self.rumble.len = 0;
            self.step_end = None;
            return;
        }
        match self.step_end {
            Some(end) if end > now => {}
            Some(_) => {
                self.cursor += 1;
                self.step_end = self
                    .rumble
                    .steps
                    .get(self.cursor)
                    .map(|step| now + Duration::from_millis(u64::from(step.millis.max(1))));
                if self.cursor >= self.rumble.len {
                    self.rumble.len = 0;
                    self.step_end = None;
                }
            }
            None => {
                self.step_end = Some(
                    now + Duration::from_millis(u64::from(
                        self.rumble.steps[self.cursor].millis.max(1),
                    )),
                );
            }
        }
    }

    fn motors(&self) -> [u8; 2] {
        if self.rumble.len > 0 && self.cursor < self.rumble.len {
            [
                self.rumble.steps[self.cursor].left,
                self.rumble.steps[self.cursor].right,
            ]
        } else {
            self.state.motors
        }
    }

    /// Sends what changed; a failed write means the pad is gone.
    fn flush(&mut self) {
        if self.pad.is_none() {
            return;
        }
        let motors = self.motors();
        let mut state = self.state;
        if matches!(self.lights, Lights::Waiting(_)) {
            state.lightbar = None;
            state.player_leds = None;
        }
        if self.sent == Some((state, motors)) {
            return;
        }

        // Every report carries the motors: one without them stops the rumble.
        let mut first = Output {
            rumble: motors,
            ..Output::default()
        };
        if self.sent.is_none() {
            first.full_power = true;
        }
        let lit = state.lightbar.is_some() || state.player_leds.is_some();
        if lit && self.lights == Lights::Ready {
            // The reset goes alone, before any colour.
            first.release_leds = true;
            self.lights = Lights::Reset;
        }
        if (first.full_power || first.release_leds) && !self.write(&first) {
            return;
        }

        let mut out = Output {
            rumble: motors,
            ..Output::default()
        };
        let old = self.sent.map(|(s, _)| s);
        if old.is_none_or(|s| s.left_trigger != state.left_trigger) {
            out.left_trigger = state.left_trigger;
        }
        if old.is_none_or(|s| s.right_trigger != state.right_trigger) {
            out.right_trigger = state.right_trigger;
        }
        if old.is_none_or(|s| s.lightbar != state.lightbar) {
            out.lightbar = state.lightbar;
        }
        if old.is_none_or(|s| s.player_leds != state.player_leds) {
            out.player_leds = state.player_leds;
        }
        if old.is_none_or(|s| s.mic_led != state.mic_led) {
            out.mic_led = state.mic_led;
        }
        if self.write(&out) {
            self.sent = Some((state, motors));
        }
    }

    fn write(&mut self, out: &Output) -> bool {
        let Some(pad) = self.pad.as_mut() else {
            return false;
        };
        let mut buffer = [0u8; 78];
        let len = pad.encode(out, &mut buffer);
        match pad.write_raw(&buffer[..len]) {
            Ok(()) => {
                {
                    let mut status = self.inner.status.lock().unwrap_or_else(|e| e.into_inner());
                    status.reports_sent = status.reports_sent.wrapping_add(1);
                }
                {
                    let mut last = self
                        .inner
                        .last_report
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    last.0 = buffer;
                    last.1 = len as u8;
                }
                true
            }
            Err(error) => {
                {
                    let mut status = self.inner.status.lock().unwrap_or_else(|e| e.into_inner());
                    status.write_failures = status.write_failures.wrapping_add(1);
                }
                self.inner.set_error(error.clone());
                self.lost(&error);
                false
            }
        }
    }

    /// Writes a report that was not produced from `PadState` (the HD haptics
    /// initialisation block).
    fn send_raw(&mut self, report: &[u8]) -> bool {
        let Some(pad) = self.pad.as_mut() else {
            return false;
        };
        match pad.write_raw(report) {
            Ok(()) => {
                let mut last = self
                    .inner
                    .last_report
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let copy = report.len().min(last.0.len());
                last.0[..copy].copy_from_slice(&report[..copy]);
                last.1 = copy as u8;
                true
            }
            Err(error) => {
                self.inner.set_error(error.clone());
                self.lost(&error);
                false
            }
        }
    }

    /// Hand the pad back: free the triggers, stop the motors, restore a host's
    /// default lightbar/player LEDs if this game changed them.
    fn release(&mut self) {
        let Some(pad) = self.pad.as_mut() else {
            return;
        };
        let release = Output {
            lightbar: self.state.lightbar.map(|_| DEFAULT_LIGHTBAR),
            player_leds: self.state.player_leds.map(|_| PLAYER_1),
            ..Output::release()
        };
        let mut buffer = [0u8; 78];
        let len = pad.encode(&release, &mut buffer);
        let _ = pad.write_raw(&buffer[..len]);
    }

    fn lost(&mut self, why: &str) {
        self.inner.set_error(why.to_string());
        {
            let mut status = self.inner.status.lock().unwrap_or_else(|e| e.into_inner());
            status.open = false;
            status.lost = true;
            status.reports_sent = 0;
            status.scans_left = SCAN_TRIES;
        }
        self.pad = None;
        self.sent = None;
        self.rumble = Rumble::default();
        self.cursor = 0;
        self.step_end = None;
        self.scans_left = SCAN_TRIES;
        self.next_scan = Instant::now();
    }
}
