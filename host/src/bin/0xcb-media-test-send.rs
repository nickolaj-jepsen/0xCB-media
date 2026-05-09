//! Manual postcard frame sender. Useful for poking the firmware without
//! running the full daemon.
//!
//! Usage:
//!   cargo run -p host --bin 0xcb-media-test-send -- /dev/ttyACM0
//!     → send one Volume(47%) frame and exit.
//!   cargo run -p host --bin 0xcb-media-test-send -- /dev/ttyACM0 73
//!     → send Volume(73%).
//!   cargo run -p host --bin 0xcb-media-test-send -- /dev/ttyACM0 --viz
//!     → swept-band synthetic visualizer pattern for ~6 s.

use std::io::Write;
use std::time::Duration;

use anyhow::Result;
use proto::HostToDevice;

fn main() -> Result<()> {
    let raw: Vec<std::string::String> = std::env::args().skip(1).collect();
    let viz_mode = raw.iter().any(|a| a == "--viz");
    let positional: Vec<&str> = raw
        .iter()
        .filter(|a| a.as_str() != "--viz")
        .map(|s| s.as_str())
        .collect();

    let device = positional
        .first()
        .copied()
        .unwrap_or("/dev/ttyACM0")
        .to_string();

    if viz_mode {
        return run_viz(&device);
    }

    let volume: u8 = positional
        .get(1)
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(47);

    println!("opening {device}…");
    let mut port = serialport::new(&device, 115_200)
        .timeout(Duration::from_secs(2))
        .open()?;

    let mut buf = [0u8; 256];
    let vol = HostToDevice::Volume {
        level: volume.min(100),
        muted: false,
    };
    let frame = postcard::to_slice_cobs(&vol, &mut buf)?;
    port.write_all(frame)?;
    println!(
        "sent Volume({}%) ({} bytes on the wire)",
        volume.min(100),
        frame.len()
    );
    Ok(())
}

/// Stream a synthetic 8-band swept pattern to the device for ~6 s. Useful for
/// validating the firmware viz path without requiring PipeWire / a daemon.
fn run_viz(device: &str) -> Result<()> {
    println!("opening {device}…");
    let mut port = serialport::new(device, 115_200)
        .timeout(Duration::from_secs(2))
        .open()?;
    let mut buf = [0u8; 256];

    let frame_ms = 33u64; // ~30 Hz
    let total_frames = 6_000 / frame_ms; // ~6 seconds

    println!(
        "streaming synthetic viz frames at {} Hz for {} frames…",
        1000 / frame_ms,
        total_frames
    );
    for step in 0..total_frames {
        let t = step as f32 / 30.0;
        let mut bands = [0u8; 8];
        for (i, b) in bands.iter_mut().enumerate() {
            // Phase-shifted sine per band → looks like a roving wave.
            let phase = t * std::f32::consts::TAU * 0.6 + i as f32 * 0.6;
            let v = (phase.sin() * 0.5 + 0.5).powi(2);
            *b = (v * 255.0) as u8;
        }
        let msg = HostToDevice::Visualizer { bands };
        let frame = postcard::to_slice_cobs(&msg, &mut buf)?;
        port.write_all(frame)?;
        std::thread::sleep(Duration::from_millis(frame_ms));
    }
    println!("done. Bars should fade off the OLED within ~500 ms.");
    Ok(())
}
