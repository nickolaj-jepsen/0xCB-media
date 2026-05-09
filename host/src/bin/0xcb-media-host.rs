//! 0xcb-media-host — Linux daemon that streams the current PipeWire default
//! sink's volume and an FFT-based audio visualizer to the 0xCB-1337 macropad
//! over USB CDC ACM. Designed to run as a per-user systemd service via the
//! NixOS module in this repo's `flake.nix`.
//!
//! Architecture: two source threads (a PipeWire `volume` mainloop and a
//! `ping` keepalive) plus the viz thread emit `proto::HostToDevice`
//! messages — control frames into a bounded channel and viz frames into a
//! lock-free `ArcSwapOption` slot. The main thread drains both and writes
//! COBS-framed postcard frames to the serial port, with simple
//! reopen-on-error reconnect.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::rc::Rc;
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
        thread::Builder::new()
            .name("volume".into())
            .spawn(move || run_volume_pipewire(tx))
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
//
// Native PipeWire volume tracker. Walks the registry to find the default
// audio sink (via the `default` Metadata's `default.audio.sink` JSON
// property), binds a Node proxy to it, subscribes to its `Props` param, and
// pushes `HostToDevice::Volume { level, muted }` whenever channelVolumes or
// mute change. Re-binds on default-sink switches so the OLED keeps tracking
// after `pactl set-default-sink`.

fn run_volume_pipewire(tx: Sender<HostToDevice>) {
    pipewire::init();
    loop {
        match pw_volume_run(&tx) {
            Ok(()) => debug!("pipewire volume loop exited cleanly"),
            Err(e) => warn!("pipewire volume error: {}; retry in 2 s", e),
        }
        thread::sleep(Duration::from_secs(2));
    }
}

/// State shared between the registry / metadata / node listeners. All
/// callbacks run on the same thread (the pipewire mainloop), so an
/// `Rc<RefCell<…>>` is sufficient — no atomics needed.
struct VolState {
    tx: Sender<HostToDevice>,
    last_sent: Option<(u8, bool)>,
    last_muted: bool,
    last_max_volume: f32,
    /// Sink node name resolved from `default.audio.sink` metadata.
    default_sink_name: Option<String>,
    /// Registry id → node name, accumulated as `global` events arrive.
    nodes_by_id: HashMap<u32, String>,
    /// Keep proxies + listeners alive for the duration of the loop.
    metadata: Option<(
        pipewire::metadata::Metadata,
        pipewire::metadata::MetadataListener,
    )>,
    node: Option<(pipewire::node::Node, pipewire::node::NodeListener)>,
    bound_node_id: Option<u32>,
}

fn pw_volume_run(tx: &Sender<HostToDevice>) -> Result<()> {
    use pipewire as pw;

    let mainloop = pw::main_loop::MainLoop::new(None).context("MainLoop::new")?;
    let context = pw::context::Context::new(&mainloop).context("Context::new")?;
    let core = context.connect(None).context("Context::connect")?;
    let registry = Rc::new(core.get_registry().context("Core::get_registry")?);

    let state = Rc::new(RefCell::new(VolState {
        tx: tx.clone(),
        last_sent: None,
        last_muted: false,
        last_max_volume: 0.0,
        default_sink_name: None,
        nodes_by_id: HashMap::new(),
        metadata: None,
        node: None,
        bound_node_id: None,
    }));

    let _global_listener = {
        let state = state.clone();
        let registry_for_global = registry.clone();
        let state_for_remove = state.clone();
        registry
            .add_listener_local()
            .global(move |obj| on_registry_global(obj, &state, &registry_for_global))
            .global_remove(move |id| on_registry_remove(id, &state_for_remove))
            .register()
    };

    mainloop.run();
    Ok(())
}

fn on_registry_global(
    obj: &pipewire::registry::GlobalObject<&pipewire::spa::utils::dict::DictRef>,
    state: &Rc<RefCell<VolState>>,
    registry: &Rc<pipewire::registry::Registry>,
) {
    use pipewire::types::ObjectType;

    match &obj.type_ {
        ObjectType::Metadata => {
            // Only care about the `default` metadata (where default.audio.sink lives).
            let is_default = obj
                .props
                .as_ref()
                .and_then(|p| p.get("metadata.name"))
                .map(|n| n == "default")
                .unwrap_or(false);
            if !is_default {
                return;
            }
            let proxy: pipewire::metadata::Metadata = match registry.bind(obj) {
                Ok(p) => p,
                Err(e) => {
                    warn!("bind metadata failed: {}", e);
                    return;
                }
            };
            let listener = {
                let state = state.clone();
                let registry = registry.clone();
                proxy
                    .add_listener_local()
                    .property(move |_subject, key, _type, value| {
                        if key == Some("default.audio.sink") {
                            let name = value.and_then(parse_default_sink_name);
                            on_default_sink_changed(name, &state, &registry);
                        }
                        0
                    })
                    .register()
            };
            state.borrow_mut().metadata = Some((proxy, listener));
        }
        ObjectType::Node => {
            let Some(name) = obj.props.as_ref().and_then(|p| p.get("node.name")) else {
                return;
            };
            let name = name.to_string();
            let id = obj.id;
            let mut s = state.borrow_mut();
            s.nodes_by_id.insert(id, name.clone());
            // If we already know the default sink name and it's this node, bind it.
            if s.default_sink_name.as_deref() == Some(&name) && s.bound_node_id != Some(id) {
                drop(s);
                bind_sink_node(id, state, registry);
            }
        }
        _ => {}
    }
}

fn on_registry_remove(id: u32, state: &Rc<RefCell<VolState>>) {
    let mut s = state.borrow_mut();
    s.nodes_by_id.remove(&id);
    if s.bound_node_id == Some(id) {
        s.node = None;
        s.bound_node_id = None;
    }
}

fn on_default_sink_changed(
    name: Option<String>,
    state: &Rc<RefCell<VolState>>,
    registry: &Rc<pipewire::registry::Registry>,
) {
    let target_id = {
        let mut s = state.borrow_mut();
        s.default_sink_name = name.clone();
        match name {
            Some(n) => s
                .nodes_by_id
                .iter()
                .find(|(_, v)| v.as_str() == n)
                .map(|(k, _)| *k),
            None => None,
        }
    };
    let Some(id) = target_id else { return };
    if state.borrow().bound_node_id != Some(id) {
        bind_sink_node(id, state, registry);
    }
}

fn bind_sink_node(
    id: u32,
    state: &Rc<RefCell<VolState>>,
    registry: &Rc<pipewire::registry::Registry>,
) {
    use pipewire::spa::param::ParamType;

    // Re-issue a registry::bind by id by faking a GlobalObject. Simpler path:
    // call the raw spa interface via `Registry::bind` only when we have the
    // GlobalObject; here we only have the id, so look up via a fresh global
    // listener pass would be heavy. Instead, fetch a Node proxy via the
    // typed binder by creating a minimal owned GlobalObject.
    let owned = pipewire::registry::GlobalObject {
        id,
        permissions: pipewire::permissions::PermissionFlags::empty(),
        type_: pipewire::types::ObjectType::Node,
        version: 0,
        props: None::<pipewire::properties::Properties>,
    };
    let proxy: pipewire::node::Node = match registry.bind(&owned) {
        Ok(p) => p,
        Err(e) => {
            warn!("bind node {} failed: {}", id, e);
            return;
        }
    };
    proxy.subscribe_params(&[ParamType::Props]);

    let listener = {
        let state = state.clone();
        proxy
            .add_listener_local()
            .param(move |_seq, id, _index, _next, param| {
                if id != ParamType::Props {
                    return;
                }
                let Some(pod) = param else { return };
                if let Some((level, muted)) = parse_props_pod(pod, &state) {
                    push_volume(level, muted, &state);
                }
            })
            .register()
    };

    let mut s = state.borrow_mut();
    s.node = Some((proxy, listener));
    s.bound_node_id = Some(id);
    info!("bound default sink node id {}", id);
}

fn push_volume(level: u8, muted: bool, state: &Rc<RefCell<VolState>>) {
    let mut s = state.borrow_mut();
    if s.last_sent == Some((level, muted)) {
        return;
    }
    debug!("volume: level={} muted={}", level, muted);
    let _ = s.tx.try_send(HostToDevice::Volume { level, muted });
    s.last_sent = Some((level, muted));
}

/// Pull `channelVolumes` and `mute` out of a Props pod. Both fields may be
/// absent on partial updates — fall back to the last seen values stored in
/// `state`.
fn parse_props_pod(
    pod: &pipewire::spa::pod::Pod,
    state: &Rc<RefCell<VolState>>,
) -> Option<(u8, bool)> {
    use libspa::pod::deserialize::PodDeserializer;
    use libspa::pod::{Value, ValueArray};
    use libspa::sys::{SPA_PROP_channelVolumes, SPA_PROP_mute};

    let (_, value): (_, Value) = PodDeserializer::deserialize_from(pod.as_bytes()).ok()?;
    let Value::Object(obj) = value else {
        return None;
    };

    let mut max_vol: Option<f32> = None;
    let mut muted: Option<bool> = None;

    for prop in &obj.properties {
        if prop.key == SPA_PROP_channelVolumes {
            if let Value::ValueArray(ValueArray::Float(vols)) = &prop.value {
                max_vol = vols.iter().cloned().fold(None, |acc, v| {
                    Some(match acc {
                        None => v,
                        Some(m) => m.max(v),
                    })
                });
            }
        } else if prop.key == SPA_PROP_mute {
            if let Value::Bool(b) = prop.value {
                muted = Some(b);
            }
        }
    }

    if max_vol.is_none() && muted.is_none() {
        return None;
    }

    let mut s = state.borrow_mut();
    if let Some(v) = max_vol {
        s.last_max_volume = v;
    }
    if let Some(m) = muted {
        s.last_muted = m;
    }
    let lin = s.last_max_volume.max(0.0);
    let muted_now = s.last_muted;
    drop(s);
    // wpctl displays cubic-mapped volume; cube-root the linear sample so the
    // user-visible numbers match what they'd see from `wpctl get-volume`.
    let level = (lin.cbrt() * 100.0).round().clamp(0.0, 100.0) as u8;
    Some((level, muted_now))
}

/// Extract `name` from the JSON value PipeWire stores in `default.audio.sink`,
/// which looks like `{"name":"alsa_output.pci-…"}`. Hand-rolled rather than
/// pulling in `serde_json` for one field.
fn parse_default_sink_name(json: &str) -> Option<String> {
    const NEEDLE: &str = r#""name":""#;
    let start = json.find(NEEDLE)? + NEEDLE.len();
    let end = json[start..].find('"')?;
    Some(json[start..start + end].to_string())
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
        last_audible: Instant::now() - Duration::from_secs(1),
        is_silent: true,
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

            // Silence gate: peak amplitude over the raw window. ≥ -55 dBFS
            // (~0.00178 linear) counts as audible. After 500 ms below the
            // floor, drop the slot to None so the device's `last_visualizer`
            // ages out and the OLED bars vanish.
            let peak = ud.input_buf.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
            if peak >= SILENCE_THRESHOLD {
                ud.last_audible = Instant::now();
                if ud.is_silent {
                    debug!("viz: audio resumed (peak={:.4})", peak);
                    ud.is_silent = false;
                }
            } else if !ud.is_silent
                && ud.last_audible.elapsed() >= Duration::from_millis(SILENCE_HOLD_MS)
            {
                debug!("viz: audio silent for {} ms; pausing", SILENCE_HOLD_MS);
                ud.slot.store(None);
                ud.is_silent = true;
            }
            if ud.is_silent {
                return;
            }

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

/// Linear amplitude floor (≈ -55 dBFS) below which a window is treated as
/// silent. Peak rather than RMS — slightly cheaper and equally good for
/// "anything audible vs nothing playing".
const SILENCE_THRESHOLD: f32 = 0.00178;
/// How long the peak must stay below the threshold before we pause emission.
/// Long enough that a quiet rest in a track doesn't drop the bars; short
/// enough that the OLED reverts within ~half a second of pause.
const SILENCE_HOLD_MS: u64 = 500;

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
    last_audible: Instant,
    is_silent: bool,
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
