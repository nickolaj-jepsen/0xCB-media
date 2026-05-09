# Host-side integration — feeding the OLED + RGB ring

The keyboard sends media commands *to* the host as standard USB HID Consumer
Control reports — no driver, no daemon needed for those. The hard part is
the other direction: getting the **system volume** and an **8-band audio
spectrum** from the host *to* the keyboard so it can render them on the
OLED and the underglow LED ring.

v1 ships **Linux only**. The proto schema is OS-agnostic, but the daemon's
source backends are not — adding Windows or macOS means adding new modules
without changing the wire format. See the bottom of this doc for the
deferred work.

## Linux: actual choices

| Need | Picked | Why |
|------|--------|-----|
| Volume reading | shell out to `wpctl get-volume @DEFAULT_AUDIO_SINK@` | One process call every 250 ms, no native deps beyond what's already on a PipeWire system. `pulsectl` / `pipewire-rs` were considered for this and rejected because they pull native lib bindings for ~3 lines of saved code. |
| Audio capture for FFT | [`pipewire`](https://docs.rs/pipewire) (real bindings, via `libspa`) | We need raw float samples from the default sink monitor, which means a real PipeWire stream. The wpctl shell-out is fine for a scalar reading; for sample data we need the real thing. |
| FFT | [`realfft`](https://docs.rs/realfft) | Real-input FFT (half-spectrum output). 1024-point window at typical 48 kHz = ~21 ms latency, plenty fast at 60 Hz output. |
| Wire framing | `postcard` + COBS | See [`02-firmware-stack.md`](02-firmware-stack.md#wire-protocol-postcard--cobs). |
| Concurrency | `std::thread` + `crossbeam-channel` for control frames, `arc-swap` for the latest viz frame | We have ≤4 long-lived threads, no async story is needed. The viz frame is "always replace, last writer wins" so a lock-free `ArcSwapOption<[u8; 8]>` keeps the serial loop from ever blocking on the FFT thread. |
| Serial I/O | `serialport` 4.x | Cross-platform, blocking, well-understood. |
| Logging | `tracing` + `tracing-subscriber` | Standard. `RUST_LOG=debug` flips to verbose. |
| CLI | `clap` (derive) with the `env` feature | `--device` falls back to `OXCB_MEDIA_SERIAL` so the systemd unit can pass it in via env. |

## Daemon shape

Two control source threads pump events into one bounded channel; a
dedicated viz thread runs the PipeWire stream + FFT and stores the latest
frame into an `ArcSwapOption`. The main thread interleaves three things on
the same serial port: drains the control channel, polls the viz slot, and
reads inbound `DeviceToHost` frames.

```
volume thread ─┬──▶ crossbeam_channel (bounded(64), HostToDevice)
ping thread ───┘                                     │
                                                     │
viz thread (PipeWire stream → realfft → 8-band) ──▶ ArcSwapOption<[u8; 8]>
                                                     │
                                                     ▼
                                       ┌─────────────────────────┐
                                       │ main thread:            │
                                       │  - poll port for RX     │
                                       │    (DeviceToHost frames)│
                                       │  - drain channel for TX │
                                       │  - load+send latest viz │
                                       │    (rate-limited)       │
                                       │  - reopen on write fail │
                                       └─────────────────────────┘
                                                     │
                                                     ▼
                                                /dev/ttyACM0
```

Threads:

| Thread | What | When |
|--------|------|------|
| `volume` | Run `wpctl get-volume @DEFAULT_AUDIO_SINK@`, parse, send `Volume` if changed. | Every 250 ms (configurable via `--volume-poll-ms`). |
| `ping` | Send `Ping`. | Every 2 s (configurable via `--ping-interval-s`). Firmware times out at 5 s, so any value ≤4 keeps it happy. |
| `viz` | PipeWire capture stream on the default sink monitor (or `--visualizer-source`); buffers samples in a small ring; on each callback, runs a Hann-windowed real-FFT over the most recent 1024 samples, bins to 8 log-spaced bands (~40 Hz to 16 kHz), dB-scales, attack/release smooths, and stores the latest `[u8; 8]` into the lock-free slot. | Driven by PipeWire callbacks; output rate-limited to `--visualizer-fps` (default 60, clamped 15..=120). |
| main | Reads port (10 ms timeout) → decodes any `DeviceToHost` frames; pumps the latest viz frame if changed; then `recv_timeout(5 ms)` for control TX → COBS-frames + writes. | Continuous interleaved poll. |

Single-threaded RX/TX (rather than spawning a reader thread with
`try_clone()`) keeps the serial port owned by exactly one place, no Mutex
games around the file descriptor.

## Wire schema (`proto/src/lib.rs`)

```rust
pub const MAX_FRAME_LEN: usize = 256;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum HostToDevice {
    Volume { level: u8, muted: bool }, // level 0..=100
    Ping,
    Visualizer { bands: [u8; 8] },     // log-spaced spectrum, 0..=255
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum DeviceToHost {
    Pong,
    EncoderClick,
}
```

Both directions use `postcard::to_slice_cobs` / `postcard::from_bytes_cobs`.
Frames are zero-byte delimited on the wire.

### Cadence

- **Host → device:**
  - `Volume` whenever the polled wpctl reading changes.
  - `Ping` every 2 s — keepalive to prevent the firmware's 5 s
    "Disconnected" UI fallback.
  - `Visualizer` at up to 60 Hz while audio is flowing on the default sink.
    The serial loop only re-sends if the band values changed (silence won't
    saturate the link), and rate-limits to one frame per 15 ms regardless.

- **Device → host:**
  - `EncoderClick` when matrix `[0,2]` is pressed. Currently the daemon just
    logs it (alongside the HID Mute the firmware already sent).
  - `Pong` reserved — never actually sent in v1.

### Visualizer freshness

The firmware treats incoming bands as stale after 500 ms with no
`Visualizer` frame. When stale, the OLED bars vanish and the underglow ring
goes dark. So the daemon doesn't need to send anything special when audio
stops — just stop sending viz frames and the device drops back to its idle
state on its own.

The user can also locally toggle the visualizer off via the key directly
below the encoder; that flips a flag inside the firmware's `DISPLAY_STATE`,
so the daemon can keep streaming and the firmware just ignores it.

### Reconnect handling

CDC ACM survives USB suspend transparently. If the daemon crashes and
restarts, the firmware keeps showing the last frame for ~5 s, then drops
to its `Disconnected` UI. The matrix and HID stay live throughout — only
the OLED + underglow are affected.

If `port.write_all()` fails, the daemon's main loop reopens the port (with
2 s backoff) and starts a fresh frame buffer.

## NixOS deployment

The repo's `flake.nix` exports `nixosModules.default` which wires the
daemon as a per-user systemd service. In a system config:

```nix
{
  imports = [ inputs.zero-x-cb-media.nixosModules.default ];

  services."0xcb-media-host" = {
    enable = true;
    # serialDevice = "/dev/ttyACM0";   # default; passed to daemon as
                                       # OXCB_MEDIA_SERIAL env var.
    # extraArgs = [];                  # forwarded to the daemon CLI
  };

  users.users.${username}.extraGroups = [ "dialout" ]; # /dev/ttyACM0 access
}
```

The unit is hardened: `ProtectSystem=strict`, `ProtectHome=read-only`,
`PrivateTmp`, and `RestrictAddressFamilies=AF_UNIX`. The daemon needs the
PipeWire socket (under `XDG_RUNTIME_DIR`) and the serial device; the
sandbox enforces that.

## Permissions

- `/dev/ttyACM0` needs read+write for the user. Quickest path:
  `users.users.$USER.extraGroups = [ "dialout" ]` in NixOS, then re-login.
- For one-off tests outside a deployed setup, `chmod 666 /dev/ttyACM0`
  works.
- A udev rule that grants the device to `plugdev` based on
  `idVendor=cb00 idProduct=1337` is the cleaner long-term option.
- The viz thread joins the user's PipeWire session via the standard
  `XDG_RUNTIME_DIR/pipewire-0` socket — no extra perms needed beyond a
  normal desktop session.

## Security notes

- The macropad sends standard HID Consumer Control reports. Risk surface
  is identical to a normal media keyboard.
- The CDC ACM channel is duplex but only carries `proto`-typed messages on
  both sides. Nothing is `eval`'d; postcard rejects malformed frames.
- The systemd unit forbids any non-Unix sockets, so a compromised daemon
  can't open a network connection to exfiltrate.

## Out of scope for v1

- Windows host (the viz path would need a WASAPI loopback capture instead
  of PipeWire).
- macOS host.
- Now-playing metadata. Earlier iterations bridged MPRIS to a track-info
  panel on the OLED; that was dropped in favour of the visualizer using
  the full panel width.
- Two-way config (e.g. `HostToDevice::SetKeymap` to change bindings at
  runtime). The current keymap is compiled in.
