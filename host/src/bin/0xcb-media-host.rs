//! 0xcb-media-host — Linux daemon that bridges MPRIS now-playing data and the
//! current PipeWire / PulseAudio default-sink volume to the 0xCB-1337 macropad
//! over USB CDC ACM. Designed to run as a per-user systemd service via the
//! NixOS module in this repo's `flake.nix`.
//!
//! Architecture: three blocking source threads (`mpris`, `volume`, `ping`)
//! emit `proto::HostToDevice` messages into a bounded channel. The main
//! thread drains the channel and writes COBS-framed postcard frames to the
//! serial port, with simple reopen-on-error reconnect.

use std::io::{ErrorKind, Read, Write};
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender};
use heapless::String as HString;
use mpris::{PlaybackStatus, Player, PlayerFinder};
use proto::{DeviceToHost, HostToDevice};
use tracing::{debug, info, warn};

#[derive(Parser, Debug)]
#[command(version, about = "0xCB-media host daemon")]
struct Args {
    /// CDC ACM serial device exposed by the macropad. Reads
    /// `OXCB_MEDIA_SERIAL` so the systemd unit can pass the path in via env.
    #[arg(long, default_value = "/dev/ttyACM0", env = "OXCB_MEDIA_SERIAL")]
    device: String,

    /// Baud rate. CDC ACM ignores this but `serialport` still wants a value.
    #[arg(long, default_value_t = 115_200)]
    baud: u32,

    /// MPRIS bus name to lock onto, e.g. `org.mpris.MediaPlayer2.spotify`.
    /// Default = whichever player MPRIS reports as currently active.
    #[arg(long)]
    mpris_player: Option<String>,

    /// How often (ms) to poll `wpctl` for system volume.
    #[arg(long, default_value_t = 250)]
    volume_poll_ms: u64,

    /// Keepalive interval (s). Firmware flips to "Disconnected" UI after 5 s
    /// of silence, so anything ≤ 4 keeps it happy.
    #[arg(long, default_value_t = 2)]
    ping_interval_s: u64,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    info!(
        "0xcb-media-host starting; device={} baud={}",
        args.device, args.baud
    );

    let (tx, rx) = bounded::<HostToDevice>(64);

    {
        let tx = tx.clone();
        let pinned = args.mpris_player.clone();
        thread::Builder::new()
            .name("mpris".into())
            .spawn(move || run_mpris(tx, pinned))
            .context("spawn mpris thread")?;
    }
    {
        let tx = tx.clone();
        let interval = Duration::from_millis(args.volume_poll_ms);
        thread::Builder::new()
            .name("volume".into())
            .spawn(move || run_volume(tx, interval))
            .context("spawn volume thread")?;
    }
    {
        let tx = tx.clone();
        let interval = Duration::from_secs(args.ping_interval_s);
        thread::Builder::new()
            .name("ping".into())
            .spawn(move || run_ping(tx, interval))
            .context("spawn ping thread")?;
    }

    drop(tx); // main loop only holds rx
    serial_loop(&args.device, args.baud, rx)
}

// ─── Serial main loop (interleaved RX + TX, single thread) ─────────────────

fn serial_loop(device: &str, baud: u32, rx: Receiver<HostToDevice>) -> Result<()> {
    let mut tx_buf = [0u8; 256];
    let mut rx_frame = [0u8; proto::MAX_FRAME_LEN];
    let mut rx_chunk = [0u8; 64];

    loop {
        info!("opening {}", device);
        let mut port = match serialport::new(device, baud)
            // Short timeout — main loop just polls, never long-blocks on reads.
            .timeout(Duration::from_millis(10))
            .open()
        {
            Ok(p) => p,
            Err(e) => {
                warn!("open failed: {}; retry in 2 s", e);
                thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        info!("serial connected");
        // Frame state resets per connection — partial bytes from a dead
        // session aren't relevant to a fresh one.
        let mut rx_pos: usize = 0;

        loop {
            // Drain any pending bytes from the device (DeviceToHost frames).
            match port.read(&mut rx_chunk) {
                Ok(n) if n > 0 => {
                    if let Err(e) = ingest_device_frames(&rx_chunk[..n], &mut rx_frame, &mut rx_pos) {
                        warn!("rx decode error: {}", e);
                    }
                }
                Ok(_) => {} // 0 bytes — nothing pending
                Err(e) if e.kind() == ErrorKind::TimedOut => {} // expected, no data
                Err(e) => {
                    warn!("serial read failed: {}; reopening", e);
                    break;
                }
            }

            // Send any queued host→device message (50 ms cap so RX stays responsive).
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(msg) => {
                    let frame = match postcard::to_slice_cobs(&msg, &mut tx_buf) {
                        Ok(f) => f,
                        Err(e) => {
                            warn!("postcard encode failed: {}", e);
                            continue;
                        }
                    };
                    if let Err(e) = port.write_all(frame) {
                        warn!("serial write failed: {}; reopening", e);
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {} // nothing to send right now
                Err(RecvTimeoutError::Disconnected) => {
                    info!("all senders dropped, exiting");
                    return Ok(());
                }
            }
        }
    }
}

/// Feed bytes from the device into the COBS framer; on every 0x00 delimiter
/// try to decode the buffered bytes as a `DeviceToHost`.
fn ingest_device_frames(
    bytes: &[u8],
    frame_buf: &mut [u8],
    frame_pos: &mut usize,
) -> Result<()> {
    for &b in bytes {
        if b == 0 {
            if *frame_pos > 0 {
                match postcard::from_bytes_cobs::<DeviceToHost>(&mut frame_buf[..*frame_pos]) {
                    Ok(msg) => handle_device_message(msg),
                    Err(e) => debug!("malformed device frame ({} bytes): {}", *frame_pos, e),
                }
                *frame_pos = 0;
            }
        } else if *frame_pos < frame_buf.len() {
            frame_buf[*frame_pos] = b;
            *frame_pos += 1;
        } else {
            *frame_pos = 0; // overflow — resync at next delimiter
        }
    }
    Ok(())
}

fn handle_device_message(msg: DeviceToHost) {
    match msg {
        DeviceToHost::EncoderClick => {
            info!("device → host: encoder click");
            // Hook for future MPRIS player switching, custom actions, etc.
            // For now we just log; HID Mute already fires from the firmware.
        }
        DeviceToHost::Pong => debug!("device → host: pong"),
    }
}

// ─── MPRIS source thread ───────────────────────────────────────────────────

fn run_mpris(tx: Sender<HostToDevice>, pinned: Option<String>) {
    loop {
        match try_run_mpris(&tx, pinned.as_deref()) {
            Ok(()) => debug!("mpris event stream ended cleanly"),
            Err(e) => debug!("mpris loop error: {}", e),
        }
        // Player went away or D-Bus hiccup — let the firmware know and retry.
        let _ = tx.try_send(HostToDevice::Clear);
        thread::sleep(Duration::from_secs(2));
    }
}

fn try_run_mpris(tx: &Sender<HostToDevice>, pinned: Option<&str>) -> Result<()> {
    let finder = PlayerFinder::new().context("PlayerFinder::new")?;

    let player = if let Some(bus) = pinned {
        finder
            .find_by_name(bus)
            .with_context(|| format!("find_by_name({bus})"))?
    } else {
        finder.find_active().context("find_active")?
    };

    info!("tracking MPRIS player: {}", player.identity());

    publish_track(&player, tx);

    let events = player.events().context("player.events")?;
    for event in events {
        match event {
            Ok(_) => publish_track(&player, tx),
            Err(e) => {
                debug!("mpris event error: {}", e);
                break;
            }
        }
    }
    Ok(())
}

fn publish_track(player: &Player, tx: &Sender<HostToDevice>) {
    let metadata = match player.get_metadata() {
        Ok(m) => m,
        Err(e) => {
            debug!("get_metadata: {}", e);
            return;
        }
    };
    let status = player.get_playback_status().unwrap_or(PlaybackStatus::Stopped);

    let title = metadata.title().unwrap_or("").to_string();
    let artist = metadata
        .artists()
        .map(|a| a.join(", "))
        .unwrap_or_default();

    let msg = HostToDevice::NowPlaying {
        title: heapless_truncate::<64>(&title),
        artist: heapless_truncate::<32>(&artist),
        is_playing: matches!(status, PlaybackStatus::Playing),
    };

    if tx.try_send(msg).is_err() {
        debug!("channel full, dropping NowPlaying");
    }
}

/// Push as many UTF-8 chars as fit into a `heapless::String<N>`. Anything
/// beyond capacity is silently truncated — the OLED font can only render
/// ASCII anyway.
fn heapless_truncate<const N: usize>(s: &str) -> HString<N> {
    let mut out = HString::<N>::new();
    for c in s.chars() {
        if out.push(c).is_err() {
            break;
        }
    }
    out
}

// ─── Volume source thread ──────────────────────────────────────────────────

fn run_volume(tx: Sender<HostToDevice>, interval: Duration) {
    let mut last: Option<(u8, bool)> = None;
    loop {
        if let Some(reading) = poll_wpctl_volume() {
            if last != Some(reading) {
                debug!("volume: level={} muted={}", reading.0, reading.1);
                let _ = tx.try_send(HostToDevice::Volume {
                    level: reading.0,
                    muted: reading.1,
                });
                last = Some(reading);
            }
        }
        thread::sleep(interval);
    }
}

/// `wpctl get-volume @DEFAULT_AUDIO_SINK@` returns either:
///     "Volume: 0.47\n"
///     "Volume: 0.47 [MUTED]\n"
/// We parse the float, multiply by 100, clamp to u8.
fn poll_wpctl_volume() -> Option<(u8, bool)> {
    let output = Command::new("wpctl")
        .args(["get-volume", "@DEFAULT_AUDIO_SINK@"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = std::str::from_utf8(&output.stdout).ok()?;
    let mut tokens = s.split_whitespace();
    let _label = tokens.next()?; // "Volume:"
    let vol_str = tokens.next()?;
    let vol: f32 = vol_str.parse().ok()?;
    let level = (vol * 100.0).round().clamp(0.0, 100.0) as u8;
    let muted = s.contains("[MUTED]");
    Some((level, muted))
}

// ─── Ping source thread ────────────────────────────────────────────────────

fn run_ping(tx: Sender<HostToDevice>, interval: Duration) {
    loop {
        thread::sleep(interval);
        let _ = tx.try_send(HostToDevice::Ping);
    }
}
