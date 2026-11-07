//! The HID transport: [hidapi], so the same code runs on Windows (`hid.dll`),
//! Linux (`hidraw`, statically linked, no libudev) and macOS (IOKit, opened
//! shared so Unity's Input System and Steam can keep reading the pad).
//!
//! Why hidapi instead of hand-rolled platform code: the protocol, the writer
//! thread, the diffing and the reconnect logic are where the bugs live; how
//! bytes reach the device is a solved problem. It is also what horizon uses.
//!
//! [hidapi]: https://crates.io/crates/hidapi

use std::ffi::CString;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use hidapi::{BusType, HidApi, HidDevice};

use crate::report::{Output, Vibration, BT_REPORT_LEN, DUALSENSE, DUALSENSE_EDGE, SONY};

/// The newest input report, so the caller reads a whole report and never half
/// of one. `seq` changes on every publication.
pub struct Mailbox {
    pub buf: [u8; 64],
    pub len: u32,
    pub seq: u32,
}

impl Default for Mailbox {
    fn default() -> Self {
        Mailbox {
            buf: [0; 64],
            len: 0,
            seq: 0,
        }
    }
}

/// Copies the newest report out; returns its sequence number (0 = nothing yet).
pub fn read_mailbox(mailbox: &Mutex<Mailbox>, target: &mut [u8], length: &mut u32) -> u32 {
    let mailbox = mailbox.lock().unwrap_or_else(|e| e.into_inner());
    let copy = (mailbox.len as usize).min(target.len());
    target[..copy].copy_from_slice(&mailbox.buf[..copy]);
    *length = copy as u32;
    mailbox.seq
}

/// How the pad is attached; the report layout depends on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Usb,
    Bluetooth,
}

impl Transport {
    pub fn code(self) -> u8 {
        match self {
            Transport::Usb => 1,
            Transport::Bluetooth => 2,
        }
    }
}

/// Output reports over Bluetooth switch the pad to its full input report and
/// can blind any other reader (Steam Input, DS4Windows, DSX, reWASD), so it is
/// opt-in except on Linux, where hidraw multiplexes cleanly.
fn bluetooth_allowed() -> bool {
    cfg!(target_os = "linux")
        || std::env::var("PRISMA_DUALSENSE_BT").is_ok_and(|value| value == "1")
}

/// The device handle shared by the writer and the single reader thread.
struct Shared {
    device: HidDevice,
}

// hidapi allows one reader and one writer on the same device at a time, which
// is exactly how it is used here; `HidDevice` is `Send` but not `Sync` because
// two simultaneous reads are not allowed.
unsafe impl Sync for Shared {}

pub struct Pad {
    shared: Arc<Shared>,
    pub transport: Transport,
    pub vibration: Vibration,
    pub update_version: Option<u16>,
    stop: Arc<AtomicBool>,
    lost: Arc<AtomicBool>,
    reason: Arc<Mutex<String>>,
    reader: Option<JoinHandle<()>>,
    /// Bluetooth sequence nibble, incremented per report.
    seq: u8,
}

impl Pad {
    /// Encodes one output report into `buffer`; returns its length.
    pub fn encode(&mut self, out: &Output, buffer: &mut [u8; BT_REPORT_LEN]) -> usize {
        match self.transport {
            Transport::Usb => {
                buffer[..48].copy_from_slice(&out.usb(self.vibration));
                48
            }
            Transport::Bluetooth => {
                buffer.copy_from_slice(&out.bluetooth(self.vibration, self.seq));
                self.seq = (self.seq + 1) & 0x0f;
                BT_REPORT_LEN
            }
        }
    }

    pub fn write_raw(&self, report: &[u8]) -> Result<(), String> {
        self.shared
            .device
            .write(report)
            .map(|_| ())
            .map_err(|error| format!("hid_write failed: {error}"))
    }

    /// The read thread saw the device disappear.
    pub fn lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }

    /// Why the read thread gave up, if it did.
    pub fn error(&self) -> Option<String> {
        let reason = self.reason.lock().unwrap_or_else(|e| e.into_inner());
        if reason.is_empty() {
            None
        } else {
            Some(reason.clone())
        }
    }
}

impl Drop for Pad {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(reader) = self.reader.take() {
            // The reader waits at most one read timeout, so this cannot hang.
            let _ = reader.join();
        }
        // `shared` is the last handle: dropping it closes the device.
    }
}

/// Finds a DualSense and opens it. Prefers USB over Bluetooth and the gamepad
/// collection over the pad's other HID interfaces.
pub fn open(mailbox: Arc<Mutex<Mailbox>>, reads: Arc<AtomicU32>) -> Result<Pad, String> {
    let api = HidApi::new().map_err(|error| format!("hidapi init failed: {error}"))?;

    let mut candidates: Vec<Candidate> = Vec::new();
    for info in api.device_list() {
        if info.vendor_id() != SONY {
            continue;
        }
        let product = info.product_id();
        if product != DUALSENSE && product != DUALSENSE_EDGE {
            continue;
        }
        let transport = match info.bus_type() {
            BusType::Bluetooth => Transport::Bluetooth,
            // I2C/SPI are internal busses (Steam Deck); treat them as USB.
            _ => Transport::Usb,
        };
        // The gamepad collection is usage page 1, usage 5; the pad also exposes
        // a touchpad and a sensor collection that cannot take output reports.
        let gamepad = info.usage_page() == 0x01 && info.usage() == 0x05;
        candidates.push(Candidate {
            path: info.path().to_owned(),
            product,
            transport,
            gamepad,
            serial: info.serial_number().unwrap_or_default().to_owned(),
        });
    }

    if candidates.is_empty() {
        return Err("no DualSense found".to_string());
    }

    // USB first, then the gamepad collection, then whatever is left.
    candidates.sort_by_key(|candidate| {
        let usb = candidate.transport == Transport::Usb;
        let gamepad = candidate.gamepad;
        (!usb, !gamepad)
    });

    if !bluetooth_allowed() {
        candidates.retain(|candidate| candidate.transport == Transport::Usb);
        if candidates.is_empty() {
            return Err(
                "DualSense is only present over Bluetooth (set PRISMA_DUALSENSE_BT=1 or connect USB)"
                    .to_string(),
            );
        }
    }

    let mut last = String::new();
    for candidate in candidates {
        match open_candidate(&api, &candidate, Arc::clone(&mailbox), Arc::clone(&reads)) {
            Ok(pad) => return Ok(pad),
            Err(error) => last = error,
        }
    }
    Err(if last.is_empty() {
        "DualSense found, but no interface could be opened".to_string()
    } else {
        last
    })
}

struct Candidate {
    path: CString,
    product: u16,
    transport: Transport,
    gamepad: bool,
    serial: String,
}

fn open_candidate(
    api: &HidApi,
    candidate: &Candidate,
    mailbox: Arc<Mutex<Mailbox>>,
    reads: Arc<AtomicU32>,
) -> Result<Pad, String> {
    let device = api
        .open_path(&candidate.path)
        .map_err(|error| format!("open failed ({}): {error}", candidate.serial))?;

    // Blocking reads with a timeout: the reader thread then notices `stop`
    // without needing a cancelable OS call.
    let _ = device.set_blocking_mode(true);

    let update_version = read_update_version(&device);
    let vibration = Vibration::of(candidate.product, update_version);

    let stop = Arc::new(AtomicBool::new(false));
    let lost = Arc::new(AtomicBool::new(false));
    let reason = Arc::new(Mutex::new(String::new()));
    let shared = Arc::new(Shared { device });

    let reader = {
        let shared = Arc::clone(&shared);
        let stop = Arc::clone(&stop);
        let lost = Arc::clone(&lost);
        let reason = Arc::clone(&reason);
        let reads = Arc::clone(&reads);
        let mailbox = Arc::clone(&mailbox);
        thread::Builder::new()
            .name("prisma-dualsense-hid-read".into())
            .spawn(move || read_loop(shared, stop, lost, reason, reads, mailbox))
            .map_err(|error| format!("cannot spawn read thread: {error}"))?
    };

    Ok(Pad {
        shared,
        transport: candidate.transport,
        vibration,
        update_version,
        stop,
        lost,
        reason,
        reader: Some(reader),
        seq: 0,
    })
}

/// Feature report `0x20`: the firmware's update version, which picks the
/// vibration emulation. hidapi puts the report id in byte 0 and dualsensectl
/// reads the version at 44/45 of that buffer; measured on hardware it is
/// `0x0630` there, so this offset is the portable one (the old Win32 path
/// happened to match because `HidD_GetFeature` returns an unprefixed report).
fn read_update_version(device: &HidDevice) -> Option<u16> {
    let mut buffer = [0u8; 64];
    buffer[0] = 0x20;
    let read = device.get_feature_report(&mut buffer).ok()?;
    if read < 46 {
        return None;
    }
    Some(u16::from_le_bytes([buffer[44], buffer[45]]))
}

fn read_loop(
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    lost: Arc<AtomicBool>,
    reason: Arc<Mutex<String>>,
    reads: Arc<AtomicU32>,
    mailbox: Arc<Mutex<Mailbox>>,
) {
    let mut buffer = [0u8; 64];
    while !stop.load(Ordering::Acquire) {
        match shared.device.read_timeout(&mut buffer, 100) {
            // Timeout: nothing arrived, check whether we should stop.
            Ok(0) => continue,
            Ok(length) => {
                let mut mailbox = mailbox.lock().unwrap_or_else(|e| e.into_inner());
                let copy = length.min(mailbox.buf.len());
                mailbox.buf[..copy].copy_from_slice(&buffer[..copy]);
                mailbox.len = copy as u32;
                mailbox.seq = mailbox.seq.wrapping_add(1);
                drop(mailbox);
                reads.fetch_add(1, Ordering::Relaxed);
            }
            Err(error) => {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                // Unplugged. Do not spin: the writer thread drops the pad and
                // rescans.
                let mut slot = reason.lock().unwrap_or_else(|e| e.into_inner());
                *slot = format!("hid_read failed: {error}");
                lost.store(true, Ordering::Release);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mailbox_hands_over_a_whole_report() {
        let mailbox = Mutex::new(Mailbox::default());
        let mut target = [0u8; 64];
        let mut length = 0;

        assert_eq!(read_mailbox(&mailbox, &mut target, &mut length), 0);
        assert_eq!(length, 0);

        {
            let mut slot = mailbox.lock().unwrap();
            slot.buf[..3].copy_from_slice(&[0x01, 0xaa, 0xbb]);
            slot.len = 3;
            slot.seq = 7;
        }

        assert_eq!(read_mailbox(&mailbox, &mut target, &mut length), 7);
        assert_eq!(length, 3);
        assert_eq!(&target[..3], &[0x01, 0xaa, 0xbb]);
    }
}
