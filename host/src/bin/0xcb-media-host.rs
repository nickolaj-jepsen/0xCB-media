//! 0xcb-media-host — Linux daemon that streams the current PipeWire /
//! PulseAudio default-sink volume and an FFT-based audio visualizer to the
//! 0xCB-1337 macropad over USB CDC ACM. Designed to run as a per-user systemd
//! service via the NixOS module in this repo's `flake.nix`.
//!
//! Architecture: two blocking source threads (`volume`, `ping`) plus the viz
//! thread emit `proto::HostToDevice` messages — control frames into a bounded
//! channel and viz frames into a lock-free `ArcSwapOption` slot. The main
//! thread drains both and writes COBS-framed postcard frames to the serial
//! port, with simple reopen-on-error reconnect.

use std::io::{ErrorKind, Read, Write};
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use arc_swap::ArcSwapOption;
use clap::Parser;
use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender};
use proto::{DeviceToHost, HostToDevice};
use tracing::{debug, info, warn};

/// Latest 8-band visualizer frame produced by the viz thread. Lock-free swap
/// — the serial loop reads-and-sends whatever's freshest each iteration, so
/// backpressure can never build up if the device link stalls.
type VizSlot = Arc<ArcSwapOption<[u8; 8]>>;

#[derive(Parser, Debug)]
#[command(version, about = "0xCB-media host daemon")]
struct Args {
    /// CDC ACM serial device exposed by the macropad. `auto` (the default)
    /// scans `serialport::available_ports()` for the 0xCB:1337 USB VID:PID
    /// and uses the first match. An explicit path (e.g. `/dev/ttyACM0`)
    /// bypasses the scan. Reads `OXCB_MEDIA_SERIAL` so the systemd unit can
    /// pin the path via env.
    #[arg(long, default_value = "auto", env = "OXCB_MEDIA_SERIAL")]
    device: String,

    /// Baud rate. CDC ACM ignores this but `serialport` still wants a value.
    #[arg(long, default_value_t = 115_200)]
    baud: u32,

    /// How often (ms) to poll `wpctl` for system volume.
    #[arg(long, default_value_t = 250)]
    volume_poll_ms: u64,

    /// Keepalive interval (s). Firmware flips to "Disconnected" UI after 5 s
    /// of silence, so anything ≤ 4 keeps it happy.
    #[arg(long, default_value_t = 2)]
    ping_interval_s: u64,

    /// Disable the audio visualizer entirely. Default = ON.
    #[arg(long, default_value_t = false)]
    no_visualizer: bool,

    /// Target frame rate for visualizer updates. Clamped to 15..=120.
    #[arg(long, default_value_t = 60)]
    visualizer_fps: u32,

    /// PipeWire `target.object` name for the capture stream (a node name, e.g.
    /// `alsa_output.pci-0000_00_1b.0.analog-stereo.monitor`). Default = let
    /// PipeWire route us to the current default sink's monitor.
    #[arg(long)]
    visualizer_source: Option<String>,
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

    let viz_slot: VizSlot = Arc::new(ArcSwapOption::from(None));
    if !args.no_visualizer {
        let slot = viz_slot.clone();
        let fps = args.visualizer_fps.clamp(15, 120);
        let source = args.visualizer_source.clone();
        thread::Builder::new()
            .name("viz".into())
            .spawn(move || run_visualizer(slot, fps, source))
            .context("spawn viz thread")?;
    } else {
        info!("visualizer disabled via --no-visualizer");
    }

    drop(tx); // main loop only holds rx
    serial_loop(&args.device, args.baud, rx, viz_slot)
}

// ─── Serial main loop (interleaved RX + TX, single thread) ─────────────────

/// USB descriptor identifiers set in `firmware/src/main.rs` (VID 0xCB00,
/// PID 0x1337). Used to find the macropad among other CDC ACM devices.
const DEVICE_VID: u16 = 0xCB00;
const DEVICE_PID: u16 = 0x1337;

/// Resolve the `--device` argument to a concrete serial port name.
///
/// `arg == "auto"` triggers a USB enumeration; any other value is treated as
/// an explicit device path and returned verbatim. On `auto`, all CDC ACM
/// ports matching the firmware's VID:PID are logged and the first is
/// returned. Errors propagate back into `serial_loop`'s 2 s reopen backoff so
/// a hot replug recovers without restarting the daemon.
fn resolve_device(arg: &str) -> Result<String> {
    if arg != "auto" {
        return Ok(arg.to_string());
    }

    let ports = serialport::available_ports().context("serialport::available_ports")?;
    let matches: Vec<String> = ports
        .into_iter()
        .filter_map(|p| match p.port_type {
            serialport::SerialPortType::UsbPort(info)
                if info.vid == DEVICE_VID && info.pid == DEVICE_PID =>
            {
                Some(p.port_name)
            }
            _ => None,
        })
        .collect();

    match matches.as_slice() {
        [] => anyhow::bail!(
            "no USB device matching {:04X}:{:04X} found",
            DEVICE_VID,
            DEVICE_PID
        ),
        [only] => Ok(only.clone()),
        many => {
            info!(
                "multiple {:04X}:{:04X} devices found ({:?}); using {}",
                DEVICE_VID, DEVICE_PID, many, many[0]
            );
            Ok(many[0].clone())
        }
    }
}

fn serial_loop(device: &str, baud: u32, rx: Receiver<HostToDevice>, viz: VizSlot) -> Result<()> {
    let mut tx_buf = [0u8; 256];
    let mut rx_frame = [0u8; proto::MAX_FRAME_LEN];
    let mut rx_chunk = [0u8; 64];

    // Cap viz send rate (host can produce >60 Hz under bursty PipeWire callbacks).
    let viz_min_interval = Duration::from_millis(15);

    loop {
        let resolved = match resolve_device(device) {
            Ok(d) => d,
            Err(e) => {
                warn!("device resolve failed: {}; retry in 2 s", e);
                thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        info!("opening {}", resolved);
        let mut port = match serialport::new(&resolved, baud)
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
        // Greet the device. The firmware replies with its own Hello carrying
        // its compiled-against PROTO_VERSION; mismatches are logged below.
        match postcard::to_slice_cobs(
            &HostToDevice::Hello {
                proto_version: proto::PROTO_VERSION,
            },
            &mut tx_buf,
        ) {
            Ok(frame) => {
                if let Err(e) = port.write_all(frame) {
                    warn!("hello write failed: {}; reopening", e);
                    continue;
                }
            }
            Err(e) => warn!("postcard encode (hello) failed: {}", e),
        }
        // Frame state resets per connection — partial bytes from a dead
        // session aren't relevant to a fresh one.
        let mut rx_pos: usize = 0;
        let mut last_viz_sent: Option<[u8; 8]> = None;
        let mut last_viz_time = Instant::now() - Duration::from_secs(1);

        'inner: loop {
            // Drain any pending bytes from the device (DeviceToHost frames).
            match port.read(&mut rx_chunk) {
                Ok(n) if n > 0 => {
                    if let Err(e) = ingest_device_frames(&rx_chunk[..n], &mut rx_frame, &mut rx_pos)
                    {
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

            // Pump latest viz frame if rate-limit elapsed and value changed.
            if last_viz_time.elapsed() >= viz_min_interval {
                if let Some(arc) = viz.load_full() {
                    let bands = *arc;
                    if last_viz_sent != Some(bands) {
                        let msg = HostToDevice::Visualizer { bands };
                        match postcard::to_slice_cobs(&msg, &mut tx_buf) {
                            Ok(frame) => {
                                if let Err(e) = port.write_all(frame) {
                                    warn!("serial write failed: {}; reopening", e);
                                    break 'inner;
                                }
                                last_viz_sent = Some(bands);
                                last_viz_time = Instant::now();
                            }
                            Err(e) => warn!("postcard encode (viz) failed: {}", e),
                        }
                    }
                }
            }

            // Send any queued host→device message. Short timeout so the viz
            // pump above stays responsive.
            match rx.recv_timeout(Duration::from_millis(5)) {
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
fn ingest_device_frames(bytes: &[u8], frame_buf: &mut [u8], frame_pos: &mut usize) -> Result<()> {
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
            // Hook for future custom actions. For now we just log; HID Mute
            // already fires from the firmware.
        }
        DeviceToHost::Pong => debug!("device → host: pong"),
        DeviceToHost::Hello { proto_version } => {
            if proto_version == proto::PROTO_VERSION {
                info!("device proto version: {}", proto_version);
            } else {
                warn!(
                    "device proto version {} != host {}; continuing",
                    proto_version,
                    proto::PROTO_VERSION
                );
            }
        }
    }
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

// ─── Visualizer source thread ──────────────────────────────────────────────
//
// Captures stereo f32 samples from the default sink monitor via PipeWire,
// runs a windowed FFT, bins to 8 log-spaced bands, dB-scales + smooths, and
// stores the latest [u8; 8] frame into the shared `VizSlot`. The serial loop
// picks the value up at its own rate.

const FFT_SIZE: usize = 1024;
/// Band edges in Hz, 9 values → 8 log-ish bands from sub-bass to top end.
const BAND_EDGES_HZ: [f32; 9] = [
    40.0, 90.0, 200.0, 450.0, 1000.0, 2200.0, 4800.0, 10000.0, 16000.0,
];
const NOISE_FLOOR_DB: f32 = -60.0;
const PEAK_DB: f32 = -10.0;
const ATTACK: f32 = 0.85; // fast rise
const RELEASE: f32 = 0.20; // slow fall

fn run_visualizer(slot: VizSlot, fps: u32, source: Option<String>) {
    pipewire::init();
    loop {
        match pw_run(&slot, fps, source.as_deref()) {
            Ok(()) => debug!("pipewire viz loop exited cleanly"),
            Err(e) => warn!("pipewire viz error: {}; retry in 2 s", e),
        }
        thread::sleep(Duration::from_secs(2));
    }
}

fn pw_run(slot: &VizSlot, fps: u32, source: Option<&str>) -> Result<()> {
    use pipewire as pw;
    use pw::properties::properties;
    use pw::spa;
    use spa::pod::Pod;

    let mainloop = pw::main_loop::MainLoop::new(None).context("MainLoop::new")?;
    let context = pw::context::Context::new(&mainloop).context("Context::new")?;
    let _core = context.connect(None).context("Context::connect")?;

    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::STREAM_CAPTURE_SINK => "true",
        *pw::keys::APP_NAME => "0xcb-media-host",
    };
    if let Some(name) = source {
        // PW_KEY_TARGET_OBJECT — gated behind feature `v0_3_44` in `pw::keys`,
        // but the underlying property string is stable.
        props.insert("target.object", name);
    }

    let stream = pw::stream::Stream::new(&_core, "0xcb-media-viz", props).context("Stream::new")?;

    let mut planner = realfft::RealFftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_SIZE);

    let user_data = VizUserData {
        fft: fft.clone(),
        window: build_hann_window(FFT_SIZE),
        ring: AudioRing::new(FFT_SIZE * 4),
        input_buf: fft.make_input_vec(),
        spectrum: fft.make_output_vec(),
        scratch: fft.make_scratch_vec(),
        smoothed: [0.0; 8],
        last_emit: Instant::now() - Duration::from_secs(1),
        send_interval: Duration::from_micros(1_000_000 / fps as u64),
        slot: slot.clone(),
        format: spa::param::audio::AudioInfoRaw::new(),
    };

    let _listener = stream
        .add_local_listener_with_user_data(user_data)
        .param_changed(|_, ud, id, param| {
            let Some(param) = param else { return };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((mt, mst)) = pw::spa::param::format_utils::parse_format(param) else {
                return;
            };
            if mt != pw::spa::param::format::MediaType::Audio
                || mst != pw::spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            if ud.format.parse(param).is_ok() {
                info!(
                    "viz capturing rate={} channels={}",
                    ud.format.rate(),
                    ud.format.channels()
                );
            }
        })
        .process(|stream, ud| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let chans = ud.format.channels().max(1) as usize;
            let data = &mut datas[0];
            let chunk_size = data.chunk().size() as usize;
            let Some(samples) = data.data() else { return };
            let n_bytes = chunk_size.min(samples.len());
            let n_samples = n_bytes / std::mem::size_of::<f32>();
            // Mix interleaved stereo (or mono / surround) → mono into the ring.
            ud.ring.push_mixed_mono(&samples[..n_bytes], chans);
            let _ = n_samples;

            if ud.ring.filled < FFT_SIZE {
                return;
            }
            if ud.last_emit.elapsed() < ud.send_interval {
                return;
            }
            ud.ring.read_latest(FFT_SIZE, &mut ud.input_buf);
            apply_window(&mut ud.input_buf, &ud.window);
            if ud
                .fft
                .process_with_scratch(&mut ud.input_buf, &mut ud.spectrum, &mut ud.scratch)
                .is_err()
            {
                return;
            }
            let bands = compute_bands(
                &ud.spectrum,
                FFT_SIZE,
                ud.format.rate().max(1),
                &mut ud.smoothed,
            );
            ud.slot.store(Some(Arc::new(bands)));
            ud.last_emit = Instant::now();
        })
        .register()
        .map_err(|e| anyhow::anyhow!("register listener: {:?}", e))?;

    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .map_err(|e| anyhow::anyhow!("pod serialize: {:?}", e))?
    .0
    .into_inner();

    let mut params = [Pod::from_bytes(&values).context("pod from_bytes")?];

    stream
        .connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .context("Stream::connect")?;

    mainloop.run();
    Ok(())
}

struct VizUserData {
    fft: Arc<dyn realfft::RealToComplex<f32>>,
    window: Vec<f32>,
    ring: AudioRing,
    input_buf: Vec<f32>,
    spectrum: Vec<realfft::num_complex::Complex<f32>>,
    scratch: Vec<realfft::num_complex::Complex<f32>>,
    smoothed: [f32; 8],
    last_emit: Instant,
    send_interval: Duration,
    slot: VizSlot,
    format: pipewire::spa::param::audio::AudioInfoRaw,
}

struct AudioRing {
    buf: Vec<f32>,
    write_pos: usize,
    filled: usize,
}

impl AudioRing {
    fn new(capacity: usize) -> Self {
        Self {
            buf: vec![0.0; capacity],
            write_pos: 0,
            filled: 0,
        }
    }

    /// Push interleaved samples (any channel count) as a mono mix-down.
    fn push_mixed_mono(&mut self, bytes: &[u8], channels: usize) {
        let cap = self.buf.len();
        let frame_bytes = channels * std::mem::size_of::<f32>();
        if frame_bytes == 0 {
            return;
        }
        for frame in bytes.chunks_exact(frame_bytes) {
            let mut sum = 0.0f32;
            for c in 0..channels {
                let off = c * std::mem::size_of::<f32>();
                let arr: [u8; 4] = match frame[off..off + 4].try_into() {
                    Ok(a) => a,
                    Err(_) => continue,
                };
                sum += f32::from_le_bytes(arr);
            }
            let mono = sum / channels as f32;
            self.buf[self.write_pos] = mono;
            self.write_pos = (self.write_pos + 1) % cap;
            if self.filled < cap {
                self.filled += 1;
            }
        }
    }

    fn read_latest(&self, n: usize, dst: &mut [f32]) {
        debug_assert!(n <= self.filled);
        let cap = self.buf.len();
        let start = (self.write_pos + cap - n) % cap;
        for (i, slot) in dst.iter_mut().take(n).enumerate() {
            *slot = self.buf[(start + i) % cap];
        }
    }
}

fn build_hann_window(n: usize) -> Vec<f32> {
    let denom = (n - 1) as f32;
    (0..n)
        .map(|i| 0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / denom).cos()))
        .collect()
}

fn apply_window(buf: &mut [f32], w: &[f32]) {
    for (b, &h) in buf.iter_mut().zip(w.iter()) {
        *b *= h;
    }
}

fn compute_bands(
    spectrum: &[realfft::num_complex::Complex<f32>],
    fft_size: usize,
    sample_rate: u32,
    smoothed: &mut [f32; 8],
) -> [u8; 8] {
    let bin_hz = sample_rate as f32 / fft_size as f32;
    let nyquist_bin = spectrum.len();
    let mut out = [0u8; 8];
    for i in 0..8 {
        let lo = ((BAND_EDGES_HZ[i] / bin_hz).round() as usize).max(1);
        let hi = ((BAND_EDGES_HZ[i + 1] / bin_hz).round() as usize).min(nyquist_bin);
        let (sum, count) = if hi > lo {
            let mut s = 0.0f32;
            for c in &spectrum[lo..hi] {
                s += (c.re * c.re + c.im * c.im).sqrt();
            }
            (s, (hi - lo) as f32)
        } else {
            (0.0, 1.0)
        };
        let mag = sum / count / (fft_size as f32 * 0.5);
        let db = 20.0 * (mag + 1e-9).log10();
        let t = ((db.clamp(NOISE_FLOOR_DB, PEAK_DB) - NOISE_FLOOR_DB) / (PEAK_DB - NOISE_FLOOR_DB))
            .clamp(0.0, 1.0);
        let target = t * 255.0;
        let prev = smoothed[i];
        let coeff = if target > prev { ATTACK } else { RELEASE };
        smoothed[i] = prev + coeff * (target - prev);
        out[i] = smoothed[i].round().clamp(0.0, 255.0) as u8;
    }
    out
}
