//! M7 validation tool: send one hardcoded NowPlaying + Volume frame to the
//! macropad over CDC ACM. Use this to confirm the firmware's display loop
//! renders host-supplied data correctly before the full host daemon (M8)
//! exists.
//!
//! Usage:
//!   cargo run -p host --bin 0xcb-media-test-send -- /dev/ttyACM0
//!   cargo run -p host --bin 0xcb-media-test-send -- /dev/ttyACM0 "Title" "Artist" 47

use std::io::Write;
use std::time::Duration;

use anyhow::{anyhow, Result};
use heapless::String;
use proto::HostToDevice;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let device = args.next().unwrap_or_else(|| "/dev/ttyACM0".into());
    let title = args.next().unwrap_or_else(|| "Bohemian Rhapsody".into());
    let artist = args.next().unwrap_or_else(|| "Queen".into());
    let volume: u8 = args
        .next()
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(47);

    println!("opening {device}…");
    let mut port = serialport::new(&device, 115_200)
        .timeout(Duration::from_secs(2))
        .open()?;

    let mut title_buf: String<64> = String::new();
    title_buf
        .push_str(&title)
        .map_err(|_| anyhow!("title too long ({} chars, max 64)", title.len()))?;
    let mut artist_buf: String<32> = String::new();
    artist_buf
        .push_str(&artist)
        .map_err(|_| anyhow!("artist too long ({} chars, max 32)", artist.len()))?;

    let now_playing = HostToDevice::NowPlaying {
        title: title_buf,
        artist: artist_buf,
        is_playing: true,
    };

    let mut buf = [0u8; 256];

    let frame = postcard::to_slice_cobs(&now_playing, &mut buf)?;
    port.write_all(frame)?;
    println!("sent NowPlaying ({} bytes on the wire)", frame.len());

    std::thread::sleep(Duration::from_millis(50));

    let vol = HostToDevice::Volume {
        level: volume.min(100),
        muted: false,
    };
    let frame = postcard::to_slice_cobs(&vol, &mut buf)?;
    port.write_all(frame)?;
    println!("sent Volume({}%) ({} bytes on the wire)", volume.min(100), frame.len());

    println!("done. OLED should now show \"{title}\" / \"{artist}\" with a {volume}% bar.");
    Ok(())
}
