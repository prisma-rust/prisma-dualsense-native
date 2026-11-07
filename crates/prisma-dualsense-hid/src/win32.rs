//! Raw Windows HID transport: enumeration, one overlapped read thread and a
//! synchronous write path. No external crate; the Win32 surface used here is
//! small and stable.
//!
//! The read handle is opened with `FILE_SHARE_READ | FILE_SHARE_WRITE`, so the
//! game (Unity's Input System) and this plugin each get a copy of every report.
//! The read thread keeps only the latest report in a fixed mailbox: the pad
//! pushes ~250 reports a second while the game polls once a frame, and for
//! touch/battery purposes only "where is the finger now" matters.

use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::report::{Output, Vibration, DUALSENSE, DUALSENSE_EDGE, SONY};

pub type Handle = *mut c_void;

const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_SHARE_READ: u32 = 0x1;
const FILE_SHARE_WRITE: u32 = 0x2;
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
const ERROR_IO_PENDING: u32 = 997;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 258;
const ERROR_NO_MORE_ITEMS: u32 = 259;
const DIGCF_PRESENT: u32 = 0x2;
const DIGCF_DEVICEINTERFACE: u32 = 0x10;
// HIDP_STATUS_SUCCESS is HIDP_STATUS(0x00110000), not Win32 ERROR_SUCCESS.
const HIDP_STATUS_SUCCESS: i32 = 0x0011_0000;
const USB_INPUT_REPORT_LENGTH: u16 = 64;
const USB_OUTPUT_REPORT_LENGTH: u16 = 48;
const MAX_REPORT: usize = 78;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Usb,
    Bluetooth,
}

impl Transport {
    /// 0 none, 1 USB, 2 Bluetooth (the value the C ABI reports).
    pub fn code(self) -> u8 {
        match self {
            Transport::Usb => 1,
            Transport::Bluetooth => 2,
        }
    }
}

/// An open pad. Dropping it releases the pad's actuators and closes the handles.
pub struct Pad {
    write: Handle,
    read: Handle,
    read_event: Handle,
    pub transport: Transport,
    pub product: u16,
    pub update_version: Option<u16>,
    pub vibration: Vibration,
    stop: Arc<AtomicBool>,
    lost: Arc<AtomicBool>,
    reads: Arc<AtomicU32>,
    reader: Option<JoinHandle<()>>,
    seq: u8,
}

/// The latest input report, copied, with a sequence number so a poller can tell
/// new data from the same report again.
pub struct Mailbox {
    pub buf: [u8; USB_INPUT_REPORT_LENGTH as usize],
    pub len: u32,
    pub seq: u32,
}

impl Default for Mailbox {
    fn default() -> Self {
        Mailbox {
            buf: [0; USB_INPUT_REPORT_LENGTH as usize],
            len: 0,
            seq: 0,
        }
    }
}

/// Copies the newest report out. Returns the sequence number of the copy.
pub fn read_mailbox(mailbox: &Mutex<Mailbox>, buffer: &mut [u8], len: &mut u32) -> u32 {
    let latest = mailbox.lock().unwrap_or_else(|e| e.into_inner());
    let copy = (latest.len as usize).min(buffer.len());
    buffer[..copy].copy_from_slice(&latest.buf[..copy]);
    *len = copy as u32;
    latest.seq
}

impl Pad {
    /// Whether the read thread gave up (unplugged, or the node disappeared).
    pub fn lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }

    pub fn reports_read(&self) -> u32 {
        self.reads.load(Ordering::Relaxed)
    }

    /// Encodes one output report into `buffer` and returns its length. The
    /// Bluetooth sequence number advances here.
    pub fn encode(&mut self, out: &Output, buffer: &mut [u8; MAX_REPORT]) -> usize {
        match self.transport {
            Transport::Usb => {
                buffer[..48].copy_from_slice(&out.usb(self.vibration));
                48
            }
            Transport::Bluetooth => {
                buffer.copy_from_slice(&out.bluetooth(self.vibration, self.seq));
                self.seq = (self.seq + 1) & 0x0f;
                MAX_REPORT
            }
        }
    }

    pub fn write_raw(&self, report: &[u8]) -> Result<(), String> {
        let mut written = 0u32;
        let ok = unsafe {
            WriteFile(
                self.write,
                report.as_ptr() as *const c_void,
                report.len() as u32,
                &mut written,
                null_mut(),
            )
        } != 0
            && written == report.len() as u32;
        if !ok {
            return Err(format!("WriteFile failed: {}", unsafe { GetLastError() }));
        }
        Ok(())
    }
}

impl Drop for Pad {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        unsafe {
            CancelIoEx(self.read, null_mut());
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        unsafe {
            CloseHandle(self.read_event);
            CloseHandle(self.read);
            CloseHandle(self.write);
        }
    }
}

/// Whether a pad on Bluetooth may be written to. Any output report (even
/// reading a feature report) switches the pad to its full 0x31 input report
/// until it is turned off; readers that only understand the simple 0x01 report
/// would stop seeing it. Opt in explicitly.
pub fn bluetooth_allowed() -> bool {
    std::env::var_os("PRISMA_DUALSENSE_BT").is_some_and(|v| v == "1")
}

/// Opens the first DualSense: USB before Bluetooth.
pub fn open(mailbox: Arc<Mutex<Mailbox>>, reads: Arc<AtomicU32>) -> Result<Pad, String> {
    let mut candidates = enumerate()?;
    if candidates.is_empty() {
        return Err("no DualSense HID interface found".to_string());
    }
    // USB first, as SDL does; Bluetooth only when explicitly allowed.
    candidates.sort_by_key(|c| match c.transport {
        Transport::Usb => 0,
        Transport::Bluetooth => 1,
    });
    let allowed = bluetooth_allowed();
    for candidate in candidates {
        if candidate.transport == Transport::Bluetooth && !allowed {
            continue;
        }
        match open_candidate(candidate, Arc::clone(&mailbox), Arc::clone(&reads)) {
            Ok(pad) => return Ok(pad),
            Err(_) => continue,
        }
    }
    Err("DualSense found, but no interface could be opened".to_string())
}

struct Candidate {
    path: String,
    transport: Transport,
    product: u16,
}

fn open_candidate(
    candidate: Candidate,
    mailbox: Arc<Mutex<Mailbox>>,
    reads: Arc<AtomicU32>,
) -> Result<Pad, String> {
    let wide = to_wide(&candidate.path);
    let write = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null_mut(),
            OPEN_EXISTING,
            0,
            null_mut(),
        )
    };
    if write == INVALID_HANDLE_VALUE {
        return Err(format!("CreateFileW(write) failed: {}", unsafe {
            GetLastError()
        }));
    }
    let read = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            null_mut(),
        )
    };
    if read == INVALID_HANDLE_VALUE {
        unsafe { CloseHandle(write) };
        return Err(format!("CreateFileW(read) failed: {}", unsafe {
            GetLastError()
        }));
    }
    unsafe {
        HidD_SetNumInputBuffers(read, 64);
    }
    let read_event = unsafe { CreateEventW(null_mut(), 1, 0, null()) };
    if read_event.is_null() {
        unsafe {
            CloseHandle(read);
            CloseHandle(write);
        }
        return Err("CreateEventW failed".to_string());
    }

    let update_version = read_update_version(write);
    let vibration = Vibration::of(candidate.product, update_version);

    let stop = Arc::new(AtomicBool::new(false));
    let lost = Arc::new(AtomicBool::new(false));
    let reader = {
        let stop = Arc::clone(&stop);
        let lost = Arc::clone(&lost);
        let reads = Arc::clone(&reads);
        let mailbox = Arc::clone(&mailbox);
        let read = read as usize;
        let event = read_event as usize;
        thread::Builder::new()
            .name("prisma-dualsense-hid-read".into())
            .spawn(move || read_loop(read as Handle, event as Handle, stop, lost, reads, mailbox))
            .map_err(|e| format!("cannot spawn read thread: {e}"))?
    };

    Ok(Pad {
        write,
        read,
        read_event,
        transport: candidate.transport,
        product: candidate.product,
        update_version,
        vibration,
        stop,
        lost,
        reads,
        reader: Some(reader),
        seq: 0,
    })
}

/// Feature report `0x20`, bytes 44..46: the firmware's update version. Unknown
/// on older firmware, which just means legacy vibration.
fn read_update_version(handle: Handle) -> Option<u16> {
    let mut buffer = [0u8; 64];
    buffer[0] = 0x20;
    let read = unsafe { HidD_GetFeature(handle, buffer.as_mut_ptr() as *mut c_void, 64) };
    if read == 0 {
        return None;
    }
    Some(u16::from_le_bytes([buffer[44], buffer[45]]))
}

fn read_loop(
    handle: Handle,
    event: Handle,
    stop: Arc<AtomicBool>,
    lost: Arc<AtomicBool>,
    reads: Arc<AtomicU32>,
    mailbox: Arc<Mutex<Mailbox>>,
) {
    let mut buffer = [0u8; USB_INPUT_REPORT_LENGTH as usize];
    let mut overlapped = Overlapped {
        internal: 0,
        internal_high: 0,
        offset: 0,
        offset_high: 0,
        event,
    };
    while !stop.load(Ordering::Acquire) {
        unsafe {
            ResetEvent(event);
        }
        let mut read = 0u32;
        let started = unsafe {
            ReadFile(
                handle,
                buffer.as_mut_ptr() as *mut c_void,
                buffer.len() as u32,
                &mut read,
                &mut overlapped,
            )
        };
        if started == 0 && unsafe { GetLastError() } != ERROR_IO_PENDING {
            // The node is gone (unplugged). Do not spin: report it and stop; the
            // writer thread drops the pad and rescans.
            lost.store(true, Ordering::Release);
            return;
        }
        loop {
            match unsafe { WaitForSingleObject(event, 100) } {
                WAIT_OBJECT_0 => break,
                WAIT_TIMEOUT if stop.load(Ordering::Acquire) => return,
                WAIT_TIMEOUT => continue,
                _ => {
                    lost.store(true, Ordering::Release);
                    return;
                }
            }
        }
        let mut done = 0u32;
        let ok = unsafe { GetOverlappedResult(handle, &mut overlapped, &mut done, 0) } != 0;
        if !ok || done == 0 {
            if stop.load(Ordering::Acquire) {
                return;
            }
            lost.store(true, Ordering::Release);
            return;
        }
        {
            let mut latest = mailbox.lock().unwrap_or_else(|e| e.into_inner());
            let copy = (done as usize).min(latest.buf.len());
            latest.buf[..copy].copy_from_slice(&buffer[..copy]);
            latest.len = copy as u32;
            latest.seq = latest.seq.wrapping_add(1);
        }
        reads.fetch_add(1, Ordering::Relaxed);
    }
}

/// Every HID interface that claims to be a DualSense with the right report
/// sizes. Interfaces that cannot be opened for a probe are skipped.
fn enumerate() -> Result<Vec<Candidate>, String> {
    let mut hid_guid = Guid {
        data1: 0,
        data2: 0,
        data3: 0,
        data4: [0; 8],
    };
    unsafe {
        HidD_GetHidGuid(&mut hid_guid);
    }
    let set = unsafe {
        SetupDiGetClassDevsW(
            &hid_guid,
            null(),
            null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    };
    if set == INVALID_HANDLE_VALUE {
        return Err("SetupDiGetClassDevsW failed".to_string());
    }
    let result = enumerate_from_set(set, &hid_guid);
    unsafe {
        SetupDiDestroyDeviceInfoList(set);
    }
    result
}

fn enumerate_from_set(set: Handle, guid: &Guid) -> Result<Vec<Candidate>, String> {
    let mut found = Vec::new();
    for index in 0..u32::MAX {
        let mut interface = DeviceInterfaceData {
            cb_size: std::mem::size_of::<DeviceInterfaceData>() as u32,
            interface_class_guid: *guid,
            flags: 0,
            reserved: null_mut(),
        };
        if unsafe { SetupDiEnumDeviceInterfaces(set, null_mut(), guid, index, &mut interface) } == 0
        {
            let error = unsafe { GetLastError() };
            if error == ERROR_NO_MORE_ITEMS {
                break;
            }
            return Err(format!("SetupDiEnumDeviceInterfaces failed: {error}"));
        }
        let Some(path) = interface_path(set, &mut interface) else {
            continue;
        };
        let wide = to_wide(&path);
        let probe = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null_mut(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        if probe == INVALID_HANDLE_VALUE {
            continue;
        }
        let mut attributes = HidAttributes {
            size: std::mem::size_of::<HidAttributes>() as u32,
            vendor_id: 0,
            product_id: 0,
            version: 0,
        };
        let matched = unsafe { HidD_GetAttributes(probe, &mut attributes) } != 0
            && attributes.vendor_id == SONY
            && matches!(attributes.product_id, DUALSENSE | DUALSENSE_EDGE);
        let correct_caps = matched && correct_report_sizes(probe);
        unsafe {
            CloseHandle(probe);
        }
        if !correct_caps {
            continue;
        }
        let upper = path.to_ascii_uppercase();
        let transport = if upper.contains("BTHENUM") || upper.contains("BTHLEDEVICE") {
            Transport::Bluetooth
        } else {
            Transport::Usb
        };
        found.push(Candidate {
            path,
            transport,
            product: attributes.product_id,
        });
    }
    Ok(found)
}

fn interface_path(set: Handle, interface: &mut DeviceInterfaceData) -> Option<String> {
    let mut required = 0u32;
    unsafe {
        SetupDiGetDeviceInterfaceDetailW(set, interface, null_mut(), 0, &mut required, null_mut());
    }
    if required == 0 {
        return None;
    }
    let mut detail = vec![0u8; required as usize];
    // SP_DEVICE_INTERFACE_DETAIL_DATA_W: DWORD cbSize, then DevicePath.
    unsafe {
        *(detail.as_mut_ptr() as *mut u32) = if std::mem::size_of::<usize>() == 8 {
            8
        } else {
            6
        };
    }
    if unsafe {
        SetupDiGetDeviceInterfaceDetailW(
            set,
            interface,
            detail.as_mut_ptr() as *mut c_void,
            required,
            &mut required,
            null_mut(),
        )
    } == 0
    {
        return None;
    }
    let path = unsafe { wide_to_string_unaligned(detail.as_ptr().add(4)) };
    (!path.is_empty()).then_some(path)
}

fn correct_report_sizes(probe: Handle) -> bool {
    let mut preparsed = null_mut();
    if unsafe { HidD_GetPreparsedData(probe, &mut preparsed) } == 0 {
        return false;
    }
    let mut caps = HidpCaps::default();
    let ok = unsafe { HidP_GetCaps(preparsed, &mut caps) } == HIDP_STATUS_SUCCESS
        && caps.input_report_byte_length == USB_INPUT_REPORT_LENGTH
        && caps.output_report_byte_length == USB_OUTPUT_REPORT_LENGTH;
    unsafe {
        HidD_FreePreparsedData(preparsed);
    }
    ok
}

// ---------------------------------------------------------------------------
// Win32 surface
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

#[repr(C)]
struct HidAttributes {
    size: u32,
    vendor_id: u16,
    product_id: u16,
    version: u16,
}

#[repr(C)]
#[derive(Default)]
struct HidpCaps {
    usage: u16,
    usage_page: u16,
    input_report_byte_length: u16,
    output_report_byte_length: u16,
    feature_report_byte_length: u16,
    reserved: [u16; 17],
    number_link_collection_nodes: u16,
    number_input_button_caps: u16,
    number_input_value_caps: u16,
    number_input_data_indices: u16,
    number_output_button_caps: u16,
    number_output_value_caps: u16,
    number_output_data_indices: u16,
    number_feature_button_caps: u16,
    number_feature_value_caps: u16,
    number_feature_data_indices: u16,
}

#[repr(C)]
struct DeviceInterfaceData {
    cb_size: u32,
    interface_class_guid: Guid,
    flags: u32,
    reserved: *mut c_void,
}

#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: Handle,
}

#[link(name = "hid")]
extern "system" {
    fn HidD_GetHidGuid(guid: *mut Guid);
    fn HidD_GetAttributes(handle: Handle, attributes: *mut HidAttributes) -> i32;
    fn HidD_GetPreparsedData(handle: Handle, data: *mut *mut c_void) -> i32;
    fn HidD_FreePreparsedData(data: *mut c_void) -> i32;
    fn HidD_SetNumInputBuffers(handle: Handle, number_buffers: u32) -> i32;
    fn HidD_GetFeature(handle: Handle, buffer: *mut c_void, length: u32) -> i32;
    fn HidP_GetCaps(data: *mut c_void, caps: *mut HidpCaps) -> i32;
}

#[link(name = "setupapi")]
extern "system" {
    fn SetupDiGetClassDevsW(
        class_guid: *const Guid,
        enumerator: *const u16,
        hwnd_parent: Handle,
        flags: u32,
    ) -> Handle;
    fn SetupDiEnumDeviceInterfaces(
        device_info_set: Handle,
        device_info_data: Handle,
        interface_class_guid: *const Guid,
        member_index: u32,
        data: *mut DeviceInterfaceData,
    ) -> i32;
    fn SetupDiGetDeviceInterfaceDetailW(
        device_info_set: Handle,
        interface_data: *mut DeviceInterfaceData,
        detail_data: *mut c_void,
        detail_data_size: u32,
        required_size: *mut u32,
        device_info_data: Handle,
    ) -> i32;
    fn SetupDiDestroyDeviceInfoList(device_info_set: Handle) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security_attributes: Handle,
        creation_disposition: u32,
        flags: u32,
        template_file: Handle,
    ) -> Handle;
    fn ReadFile(
        handle: Handle,
        buffer: *mut c_void,
        length: u32,
        read: *mut u32,
        overlapped: *mut Overlapped,
    ) -> i32;
    fn WriteFile(
        handle: Handle,
        buffer: *const c_void,
        length: u32,
        written: *mut u32,
        overlapped: Handle,
    ) -> i32;
    fn CancelIoEx(handle: Handle, overlapped: *mut Overlapped) -> i32;
    fn GetOverlappedResult(
        handle: Handle,
        overlapped: *mut Overlapped,
        transferred: *mut u32,
        wait: i32,
    ) -> i32;
    fn CreateEventW(
        attributes: Handle,
        manual_reset: i32,
        initial_state: i32,
        name: *const u16,
    ) -> Handle;
    fn ResetEvent(event: Handle) -> i32;
    fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
    fn CloseHandle(handle: Handle) -> i32;
    fn GetLastError() -> u32;
}

unsafe fn wide_to_string_unaligned(pointer: *const u8) -> String {
    let mut values = Vec::new();
    let mut offset = 0usize;
    loop {
        let value = std::ptr::read_unaligned(pointer.add(offset) as *const u16);
        if value == 0 {
            break;
        }
        values.push(value);
        offset += 2;
    }
    String::from_utf16_lossy(&values)
}

fn to_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
