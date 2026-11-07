//! A persistent WASAPI render stream on the DualSense's four-channel USB
//! endpoint.
//!
//! The old implementation created and tore down an `IAudioClient` for every
//! `play` call; that restarts the stream per call and the actuator channels
//! never form a continuous signal. Here the client is opened once, an
//! event-driven render thread keeps it fed from a fixed-size ring buffer, and
//! submissions are plain copies into that ring. Idle is silence, not a stopped
//! device.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use windows::core::{Result, HRESULT};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eRender, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;

/// 10 ms shared-mode buffer: low latency without waking the thread too often.
const BUFFER_DURATION_100NS: i64 = 100_000;
/// One second of audio in the ring; a source that falls further behind is
/// dropped rather than played late.
const RING_SECONDS: usize = 1;
const CHANNELS: usize = 4;

pub struct Session {
    client: IAudioClient,
    shared: Arc<Shared>,
    event: HANDLE,
    thread: Option<JoinHandle<()>>,
    pub name: String,
    pub rate: u32,
    pub buffer_frames: u32,
}

// The COM interfaces are apartment-neutral (the stream is created in the MTA)
// and every mutating call is guarded by the ring mutex.
unsafe impl Send for Session {}
unsafe impl Sync for Session {}

struct Shared {
    state: Mutex<Ring>,
    wake: Condvar,
    stop: AtomicBool,
    running: AtomicBool,
    underruns: AtomicU32,
    error: Mutex<String>,
}

/// A fixed-size interleaved four-channel ring, expressed in frames.
struct Ring {
    data: Vec<f32>,
    head: usize,
    len: usize,
    capacity: usize,
    /// Reused by the resampler so a rate mismatch does not allocate per call.
    scratch: Vec<f32>,
}

impl Ring {
    fn new(capacity: usize) -> Ring {
        Ring {
            data: vec![0.0; capacity * CHANNELS],
            head: 0,
            len: 0,
            capacity,
            scratch: Vec::new(),
        }
    }

    /// Copies quad frames in, dropping the oldest frames when full so playback
    /// stays current instead of lagging behind. Returns frames accepted.
    fn push(&mut self, frames: &[f32]) -> usize {
        let mut incoming = frames.len() / CHANNELS;
        if incoming == 0 {
            return 0;
        }
        // A burst larger than the ring keeps its newest frames, not its oldest.
        let frames = if incoming > self.capacity {
            incoming -= self.capacity;
            &frames[incoming * CHANNELS..]
        } else {
            frames
        };
        let incoming = frames.len() / CHANNELS;
        let overflow = (self.len + incoming).saturating_sub(self.capacity);
        if overflow > 0 {
            self.head = (self.head + overflow) % self.capacity;
            self.len -= overflow;
        }
        let tail = (self.head + self.len) % self.capacity;
        let first = incoming.min(self.capacity - tail);
        self.data[tail * CHANNELS..(tail + first) * CHANNELS]
            .copy_from_slice(&frames[..first * CHANNELS]);
        let rest = incoming - first;
        if rest > 0 {
            self.data[..rest * CHANNELS].copy_from_slice(&frames[first * CHANNELS..]);
        }
        self.len += incoming;
        incoming
    }

    fn push_scratch(&mut self) -> usize {
        let scratch = std::mem::take(&mut self.scratch);
        let accepted = self.push(&scratch);
        self.scratch = scratch;
        accepted
    }

    /// Copies up to `frames` out; returns how many frames were written.
    fn pop(&mut self, target: &mut [f32]) -> usize {
        let wanted = (target.len() / CHANNELS).min(self.len);
        if wanted == 0 {
            return 0;
        }
        let first = wanted.min(self.capacity - self.head);
        target[..first * CHANNELS]
            .copy_from_slice(&self.data[self.head * CHANNELS..(self.head + first) * CHANNELS]);
        let rest = wanted - first;
        if rest > 0 {
            target[first * CHANNELS..wanted * CHANNELS]
                .copy_from_slice(&self.data[..rest * CHANNELS]);
        }
        self.head = (self.head + wanted) % self.capacity;
        self.len -= wanted;
        wanted
    }

    fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }
}

struct ComGuard {
    initialized_here: bool,
}

impl ComGuard {
    fn new() -> Result<Self> {
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let changed_mode = HRESULT(0x80010106u32 as i32);
        if result.is_err() && result != changed_mode {
            return Err(result.into());
        }
        Ok(Self {
            initialized_here: result.is_ok(),
        })
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.initialized_here {
            unsafe { CoUninitialize() };
        }
    }
}

impl Session {
    /// Finds the DualSense's four-channel 32-bit endpoint and opens a stream on
    /// it. The render thread idles in silence until [`Session::start`].
    pub fn open() -> Result<Session> {
        let _com = ComGuard::new()?;
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
        let (device, name) = find_endpoint(&enumerator)?;
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
        let format_ptr = unsafe { client.GetMixFormat()? };
        let format = unsafe { *format_ptr };
        // WAVEFORMATEX is packed; copy the fields out before formatting them.
        let channels = format.nChannels;
        let bits = format.wBitsPerSample;
        if channels != 4 || bits != 32 {
            unsafe { CoTaskMemFree(Some(format_ptr as _)) };
            return Err(windows::core::Error::new(
                HRESULT(0x80004005u32 as i32),
                format!(
                    "endpoint is not four-channel 32-bit PCM: {name} ({channels} channels, {bits} bits)"
                ),
            ));
        }
        let rate = format.nSamplesPerSec;
        let init = unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                BUFFER_DURATION_100NS,
                0,
                format_ptr,
                None,
            )
        };
        unsafe { CoTaskMemFree(Some(format_ptr as _)) };
        init?;

        let buffer_frames = unsafe { client.GetBufferSize()? };
        let render: IAudioRenderClient = unsafe { client.GetService()? };
        let event = unsafe { CreateEventW(None, false, false, None)? };
        // With AUDCLNT_STREAMFLAGS_EVENTCALLBACK the client refuses to start
        // (AUDCLNT_E_EVENTHANDLE_NOT_SET) until its event is registered.
        if let Err(error) = unsafe { client.SetEventHandle(event) } {
            let _ = unsafe { CloseHandle(event) };
            return Err(error);
        }

        let capacity = (rate as usize * RING_SECONDS).max(buffer_frames as usize * 2);
        let shared = Arc::new(Shared {
            state: Mutex::new(Ring::new(capacity)),
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
            running: AtomicBool::new(false),
            underruns: AtomicU32::new(0),
            error: Mutex::new(String::new()),
        });

        let thread = {
            let worker = Worker {
                client: client.clone(),
                render,
                event: event.0 as usize,
                buffer_frames,
                shared: Arc::clone(&shared),
            };
            thread::Builder::new()
                .name("prisma-dualsense-audio".into())
                .spawn(move || worker.run())
                .ok()
        };

        Ok(Session {
            client,
            shared,
            event,
            thread,
            name,
            rate,
            buffer_frames,
        })
    }

    pub fn is_open(&self) -> bool {
        self.thread.is_some() && !self.event.0.is_null()
    }

    pub fn is_running(&self) -> bool {
        self.shared.running.load(Ordering::Acquire)
    }

    pub fn underruns(&self) -> u32 {
        self.shared.underruns.load(Ordering::Relaxed)
    }

    pub fn queued_frames(&self) -> u32 {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len as u32
    }

    pub fn last_error(&self) -> String {
        self.shared
            .error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn set_error(&self, error: impl Into<String>) {
        let mut slot = self.shared.error.lock().unwrap_or_else(|e| e.into_inner());
        *slot = error.into();
    }

    /// Starts the endpoint. Idempotent.
    pub fn start(&self) -> Result<()> {
        if self.shared.running.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        {
            let mut ring = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            ring.clear();
        }
        if let Err(error) = unsafe { self.client.Start() } {
            self.shared.running.store(false, Ordering::Release);
            return Err(error);
        }
        self.shared.wake.notify_all();
        Ok(())
    }

    /// Stops the endpoint and drops queued audio. Idempotent.
    pub fn stop(&self) {
        if !self.shared.running.swap(false, Ordering::AcqRel) {
            return;
        }
        {
            let mut ring = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            ring.clear();
        }
        if let Err(error) = unsafe { self.client.Stop() } {
            self.set_error(error.to_string());
        }
    }

    /// Pushes interleaved four-channel PCM. Resamples when `sample_rate`
    /// differs from the endpoint's. Returns the number of source frames
    /// accepted.
    pub fn submit_quad(&self, samples: &[f32], sample_rate: u32) -> usize {
        let source_frames = samples.len() / CHANNELS;
        if source_frames == 0 {
            return 0;
        }
        let mut ring = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if sample_rate == self.rate || sample_rate == 0 {
            return ring.push(samples);
        }
        let target_frames =
            (source_frames as u64 * self.rate as u64 / u64::from(sample_rate)) as usize;
        ring.scratch.clear();
        ring.scratch.resize(target_frames * CHANNELS, 0.0);
        for frame in 0..target_frames {
            let position = frame as f64 * f64::from(sample_rate) / f64::from(self.rate);
            let index = position.floor() as usize;
            let fraction = (position - index as f64) as f32;
            let next = (index + 1).min(source_frames - 1);
            for channel in 0..CHANNELS {
                let a = samples[index * CHANNELS + channel];
                let b = samples[next * CHANNELS + channel];
                ring.scratch[frame * CHANNELS + channel] = a + (b - a) * fraction;
            }
        }
        ring.push_scratch()
    }

    /// Pushes stereo haptics onto the actuator channels (1/2 stay silent).
    pub fn submit_haptics(&self, samples: &[f32], sample_rate: u32) -> usize {
        let mut quad = Vec::with_capacity(samples.len() * 2);
        for frame in samples.chunks_exact(2) {
            quad.extend_from_slice(&[0.0, 0.0, frame[0], frame[1]]);
        }
        self.submit_quad(&quad, sample_rate)
    }

    /// Plays a rumble shape on the actuators.
    pub fn play_rumble(&self, steps: &[crate::synth::RumbleStep]) -> usize {
        let mut waveform = Vec::new();
        crate::synth::waveform(steps, self.rate, &mut waveform);
        self.submit_quad(&waveform, self.rate)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.wake.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = unsafe { self.client.Stop() };
        if !self.event.0.is_null() {
            unsafe {
                let _ = CloseHandle(self.event);
            }
        }
    }
}

/// The render thread. It owns copies of the COM interfaces.
struct Worker {
    client: IAudioClient,
    render: IAudioRenderClient,
    event: usize,
    buffer_frames: u32,
    shared: Arc<Shared>,
}

unsafe impl Send for Worker {}

impl Worker {
    fn run(self) {
        let event = HANDLE(self.event as *mut c_void);
        loop {
            if self.shared.stop.load(Ordering::Acquire) {
                break;
            }
            if !self.shared.running.load(Ordering::Acquire) {
                let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
                let _ = self
                    .shared
                    .wake
                    .wait_timeout(state, std::time::Duration::from_millis(20));
                continue;
            }
            if unsafe { WaitForSingleObject(event, 50) } != WAIT_OBJECT_0 {
                continue;
            }
            if let Err(error) = self.fill() {
                let mut slot = self.shared.error.lock().unwrap_or_else(|e| e.into_inner());
                *slot = error.to_string();
            }
        }
    }

    fn fill(&self) -> Result<()> {
        let mut ring = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let padding = unsafe { self.client.GetCurrentPadding()? };
        let available = self.buffer_frames.saturating_sub(padding);
        if available == 0 {
            return Ok(());
        }
        let data = unsafe { self.render.GetBuffer(available)? };
        let target = unsafe {
            std::slice::from_raw_parts_mut(data as *mut f32, available as usize * CHANNELS)
        };
        let copied = ring.pop(target);
        if copied < available as usize {
            // Silence while idle is intentional. An underrun is a buffer we
            // started filling and could not finish.
            if copied > 0 {
                self.shared.underruns.fetch_add(1, Ordering::Relaxed);
            }
            target[copied * CHANNELS..].fill(0.0);
        }
        unsafe { self.render.ReleaseBuffer(available, 0)? };
        Ok(())
    }
}

fn find_endpoint(enumerator: &IMMDeviceEnumerator) -> Result<(IMMDevice, String)> {
    let collection = unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)? };
    let count = unsafe { collection.GetCount()? };
    for index in 0..count {
        let device = unsafe { collection.Item(index)? };
        let name = device_friendly_name(&device).unwrap_or_else(|_| "<unknown>".to_string());
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
        let format_ptr = unsafe { client.GetMixFormat()? };
        let format = unsafe { *format_ptr };
        let channels = format.nChannels;
        let bits = format.wBitsPerSample;
        unsafe { CoTaskMemFree(Some(format_ptr as _)) };
        if channels == 4 && bits == 32 && is_dualsense_name(&name) {
            return Ok((device, name));
        }
    }
    Err(windows::core::Error::new(
        HRESULT(0x80004005u32 as i32),
        "no active DualSense four-channel 32-bit endpoint found",
    ))
}

fn is_dualsense_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("dualsense") || lower.contains("wireless controller")
}

fn device_friendly_name(device: &IMMDevice) -> Result<String> {
    let store: IPropertyStore = unsafe { device.OpenPropertyStore(STGM_READ)? };
    let value = unsafe { store.GetValue(&PKEY_Device_FriendlyName)? };
    let name = unsafe {
        if value.Anonymous.Anonymous.vt == VT_LPWSTR {
            let pointer = value.Anonymous.Anonymous.Anonymous.pwszVal;
            if pointer.is_null() {
                String::new()
            } else {
                pointer.to_string()?
            }
        } else {
            String::new()
        }
    };
    unsafe { PropVariantClear(&value as *const _ as *mut _)? };
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_wraps_and_drops_the_oldest() {
        let mut ring = Ring::new(4);
        // Three frames in, two out: head moves to 2.
        let frames: Vec<f32> = (0..3).flat_map(|i| [i as f32; 4]).collect();
        assert_eq!(ring.push(&frames), 3);
        let mut out = [0.0f32; 4 * 2];
        assert_eq!(ring.pop(&mut out), 2);
        assert_eq!(out[0], 0.0);
        assert_eq!(out[4], 1.0);
        // Push four frames into two free slots: the one still queued (2) is
        // dropped, and the four new frames wrap across the end of the ring.
        let frames: Vec<f32> = (10..14).flat_map(|i| [i as f32; 4]).collect();
        assert_eq!(ring.push(&frames), 4);
        assert_eq!(ring.len, 4);
        let mut all = [0.0f32; 4 * 4];
        assert_eq!(ring.pop(&mut all), 4);
        for (index, frame) in all.chunks_exact(4).enumerate() {
            assert_eq!(frame[0], 10.0 + index as f32);
        }
    }

    #[test]
    fn ring_survives_a_burst_larger_than_capacity() {
        let mut ring = Ring::new(3);
        let frames: Vec<f32> = (0..5).flat_map(|i| [i as f32; 4]).collect();
        assert_eq!(ring.push(&frames), 3);
        let mut out = [0.0f32; 4 * 3];
        assert_eq!(ring.pop(&mut out), 3);
        assert_eq!(out[0], 2.0);
        assert_eq!(out[8], 4.0);
    }
}
