# Host-side integration — feeding the OLED

The keyboard sends media commands *to* the host as standard USB HID Consumer
Control reports — no driver, no daemon needed for those. The hard part is
the other direction: getting the **currently playing track** and **the
system volume** from the host *to* the keyboard so it can render them on
the OLED.

v1 ships **Linux only**. The proto schema is OS-agnostic, but the daemon's
source backends are not — adding Windows or macOS means adding new modules
without changing the wire format. See the bottom of this doc for the
deferred work.

## Linux: actual choices

| Need | Picked | Why |
|------|--------|-----|
| Now-playing data | [`mpris`](https://docs.rs/mpris) crate (synchronous D-Bus client) | Every Linux player worth using publishes MPRIS. The crate has a clean blocking API that fits a thread-per-source model. |
| Volume reading | shell out to `wpctl get-volume @DEFAULT_AUDIO_SINK@` | One process call every 250 ms, no native deps beyond what's already on a PipeWire system. `pulsectl` / `pipewire-rs` were considered and rejected because they pull native lib bindings for ~3 lines of saved code. |
| Wire framing | `postcard` + COBS | See [`02-firmware-stack.md`](02-firmware-stack.md#wire-protocol-postcard--cobs). |
| Concurrency | `std::thread` + `crossbeam-channel` | We have ≤4 long-lived threads, no async story is needed. tokio buys us nothing here. |
| Serial I/O | `serialport` 4.x | Cross-platform, blocking, well-understood. |
| Logging | `tracing` + `tracing-subscriber` | Standard. `RUST_LOG=debug` flips to verbose. |
| CLI | `clap` (derive) with the `env` feature | `--device` falls back to `OXCB_MEDIA_SERIAL` so the systemd unit can pass it in via env. |

### Souvlaki was the wrong fit

[`souvlaki`](https://docs.rs/souvlaki) wraps Linux MPRIS / Windows SMTC /
macOS MediaRemote behind one type. It looked attractive during planning,
but it's designed for *publishing* media metadata (telling the OS what
*you* are playing). What we want is the inverse — *querying* what some
other app is playing. Using souvlaki here would have us fighting the
abstraction.

For a future cross-platform expansion:

- Linux: `mpris` (current).
- Windows: `gsmtc` or the `windows` crate's `Windows::Media::Control`.
- macOS: private `MediaRemote.framework` via the `objc2` crate; flaky,
  Apple breaks it occasionally — likely best as a "no track info on macOS"
  fallback.

## Daemon shape

Three blocking source threads pump events into one bounded channel; the
main thread drains the channel and writes COBS-framed postcard frames to
the serial port.

```
mpris thread ──┐
volume thread ─┼──▶ crossbeam_channel (bounded(64), HostToDevice)
ping thread ───┘                                     │
                                                     ▼
                                       ┌─────────────────────────┐
                                       │ main thread:            │
                                       │  - poll port for RX     │
                                       │    (DeviceToHost frames)│
                                       │  - drain channel for TX │
                                       │  - reopen on write fail │
                                       └─────────────────────────┘
                                                     │
                                                     ▼
                                                /dev/ttyACM0
```

Threads:

| Thread | What | When |
|--------|------|------|
| `mpris` | `PlayerFinder::find_active()`, then iterate `player.events()`. On each event publish a `NowPlaying` frame. | Event-driven; respawns every 2 s if the player goes away. |
| `volume` | Run `wpctl get-volume @DEFAULT_AUDIO_SINK@`, parse, send `Volume` if changed. | Every 250 ms (configurable via `--volume-poll-ms`). |
| `ping` | Send `Ping`. | Every 2 s (configurable via `--ping-interval-s`). Firmware times out at 5 s, so any value ≤4 keeps it happy. |
| main | Reads port (10 ms timeout) → decodes any `DeviceToHost` frames; then `recv_timeout(50 ms)` for TX → COBS-frames + writes. | Continuous interleaved poll. |

Single-threaded RX/TX (rather than spawning a reader thread with
`try_clone()`) keeps the serial port owned by exactly one place, no Mutex
games around the file descriptor.

## Wire schema (`proto/src/lib.rs`)

```rust
pub const MAX_FRAME_LEN: usize = 256;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum HostToDevice {
    NowPlaying {
        title: heapless::String<64>,
        artist: heapless::String<32>,
        is_playing: bool,
    },
    Volume { level: u8, muted: bool }, // level 0..=100
    Clear,
    Ping,
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
  - `NowPlaying` whenever MPRIS fires an event (track change, play/pause).
  - `Volume` whenever the polled wpctl reading changes.
  - `Ping` every 2 s — keepalive to prevent the firmware's 5 s
    "Disconnected" UI fallback.
  - `Clear` when the active MPRIS player disappears.

- **Device → host:**
  - `EncoderClick` when matrix `[0,2]` is pressed. Currently the daemon just
    logs it (alongside the HID Mute the firmware already sent). Future
    versions could use it for, e.g., switching between MPRIS players.
  - `Pong` reserved — never actually sent in v1.

### Reconnect handling

CDC ACM survives USB suspend transparently. If the daemon crashes and
restarts, the firmware keeps showing the last frame for ~5 s, then drops
to its `Disconnected` UI. The matrix and HID stay live throughout — only
the OLED is affected.

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
`PrivateTmp`, and `RestrictAddressFamilies=AF_UNIX`. The daemon only needs
the user's session D-Bus and the serial device; the sandbox enforces that.

## Permissions

- `/dev/ttyACM0` needs read+write for the user. Quickest path:
  `users.users.$USER.extraGroups = [ "dialout" ]` in NixOS, then re-login.
- For one-off tests outside a deployed setup, `chmod 666 /dev/ttyACM0`
  works.
- A udev rule that grants the device to `plugdev` based on
  `idVendor=cb00 idProduct=1337` is the cleaner long-term option.

## Security notes

- The macropad sends standard HID Consumer Control reports. Risk surface
  is identical to a normal media keyboard.
- The CDC ACM channel is duplex but only carries `proto`-typed messages on
  both sides. Nothing is `eval`'d; postcard rejects malformed frames.
- The systemd unit forbids any non-Unix sockets, so a compromised daemon
  can't open a network connection to exfiltrate.

## Out of scope for v1

- Windows host (`gsmtc` backend would slot in as a new module behind a
  cfg).
- macOS host.
- Album art over CDC (would require chunked CDC writes + a small image
  decoder + a different OLED pixel area).
- Two-way config (e.g. `HostToDevice::SetKeymap` to change bindings at
  runtime). The current keymap is compiled in.
