// Lists the active render endpoints and, when the pad's four-channel endpoint
// is present, plays a short actuator waveform through the persistent stream.
//
// Usage: cargo run -p prisma-dualsense-audio --bin audio-smoke

#[cfg(windows)]
use prisma_dualsense_audio::synth::RumbleStep;

#[cfg(not(windows))]
fn main() {
    // The endpoint is WASAPI-only; the crate still builds elsewhere so the
    // shared pcm/synth code stays testable on every platform.
    eprintln!("audio-smoke: the DualSense audio endpoint is Windows-only (WASAPI)");
}

#[cfg(windows)]
fn main() {
    let session = match prisma_dualsense_audio::session::Session::open() {
        Ok(session) => session,
        Err(error) => {
            eprintln!("AUDIO_OPEN_FAILED: {error}");
            std::process::exit(1);
        }
    };
    println!(
        "AUDIO_OPEN_OK: {} | {} Hz | {} frames/period",
        session.name, session.rate, session.buffer_frames
    );
    if let Err(error) = session.start() {
        eprintln!("AUDIO_START_FAILED: {error}");
        std::process::exit(1);
    }
    let played = session.play_rumble(&[RumbleStep {
        left: 220,
        right: 140,
        millis: 300,
    }]);
    println!("AUDIO_RUMBLE: {played} frames queued");
    std::thread::sleep(std::time::Duration::from_millis(800));
    session.stop();
    println!("AUDIO_UNDERRUNS: {}", session.underruns());
    println!("AUDIO_DONE");
}
