# 0xCB-media

> [!WARNING]
> **This project is vibecoded.** Run at your own risk. No warranties, no support, just a fun weekend hack.

Custom firmware that turns the [0xCB-1337](https://keeb.supply/products/0xcb-1337)
macropad into a desktop media controller, plus the host-side service that feeds
it data.

The macropad becomes:

- **A USB media remote** — buttons send `Play/Pause`, `Next`, `Previous`, `Mute`
  via USB HID Consumer Control reports.
- **A volume knob** — the EC11 rotary encoder maps to `Volume Up`/`Volume Down`
  consumer reports, with the click pre-bound to `Mute`.
- **An audio visualizer + volume display** — the onboard 128×64 SSD1306 OLED
  shows an 8-band FFT spectrum of whatever's playing on the host's default
  PipeWire sink, alongside a vertical volume bar. The 23-LED underglow ring
  pulses with the same spectrum. Toggle the visualizer on/off with the key
  below the encoder.

The firmware is written in Rust on top of [Embassy](https://embassy.dev/) for the
RP2040, and is flashed/debugged with [probe.rs](https://probe.rs).

## Hardware

The 1337 is sold as a single product, but the repo has six hardware revisions.
**rev5.0** — the version currently shipping from KeebSupply — is built around
the **RP2040**. Earlier revisions (rev1.0–rev4.0) used an ATmega32U4, which is
why the upstream
[QMK keyboard.json](https://github.com/qmk/qmk_firmware/blob/master/keyboards/0xcb/1337/keyboard.json)
still says `atmega32u4`. **This project targets rev5.0 only.**

Confirmed from `rev5.0/kikit/prod/bom.csv`:

| Item             | Part                    | Qty     |
|------------------|-------------------------|---------|
| MCU              | RP2040 (QFN-56)         | 1       |
| External flash   | W25Q32JVUUIQ (4 MB QSPI)| 1       |
| Crystal          | 12 MHz                  | 1       |
| Switches (hotswap, Choc V1) | Kailh hotswap | 8       |
| Rotary encoder   | EC11                    | 1       |
| Per-key RGB      | SK6812MINI-E            | 8       |
| Underglow RGB    | WS2812B                 | 23      |
| OLED             | SSD1306 128×64 (I²C, via FPC) | 1 |
| USB              | USB-C 2.0               | 1       |
| USB protection   | USBLC6-2P6              | 1       |
| Power switch     | TPS2553DBVR (RGB load switch) | 1 |
| Level shifter    | 74LVC1T45               | 1       |

Pinout, BOM, and bootloader entry are documented in
[`docs/01-hardware.md`](docs/01-hardware.md).

## Architecture

```
┌─────────────────────────┐         USB-C             ┌──────────────────────────┐
│         PC host         │ ─── HID Consumer ────▶    │       0xCB-1337          │
│                         │ ─── CDC ACM      ◀──▶     │   (RP2040, Embassy)      │
│  ┌───────────────────┐  │   (postcard+COBS:         │                          │
│  │ 0xcb-media-host   │  │    Volume, Visualizer,    │   ┌──────────────────┐   │
│  │   ├── volume      │  │    Ping → device          │   │  USB HID Consumer│   │
│  │   ├── pw-viz (FFT)│  │    EncoderClick ← device) │   │  Play/Pause, Mute│   │
│  │   └── ping        │  │                           │   │  Vol Up/Down ... │   │
│  └───────────────────┘  │                           │   └──────────────────┘   │
└─────────────────────────┘                           │   ┌──────────────────┐   │
                                                      │   │ SSD1306 OLED     │   │
                                                      │   │ █ ▌ █ ▆ ▆ ▃ ▂ ▁║│   │
                                                      │   │ █ █ █ █ ▅ ▄ ▃ ▂║│   │
                                                      │   └──────────────────┘   │
                                                      │   31-LED ring: spectrum  │
                                                      └──────────────────────────┘
```

Three crates:

1. **`firmware/`** — Rust no-std crate flashed to the RP2040. Composite USB
   device exposing HID Consumer Control (media keys + volume) and CDC ACM
   (for the volume + visualizer feed). Drives the OLED, encoder, key matrix,
   and the 31-LED RGB chain.
2. **`host/`** — Linux daemon (`0xcb-media-host`) that streams the current
   PipeWire/WirePlumber default-sink volume and an 8-band FFT spectrum of
   the audio coming out of that sink to the macropad over CDC ACM. Watches
   `DeviceToHost::EncoderClick` from the firmware for custom actions
   (currently just logged).
3. **`proto/`** — Shared `no_std`-friendly serde schema for the wire format
   (postcard + COBS framing).

See [`docs/05-architecture.md`](docs/05-architecture.md) for the full design.

## Status

**Working.** Built and validated end-to-end on a 0xCB-1337 rev5.0 with a
NixOS host running PipeWire + Spotify. v1 is Linux only. Windows / macOS
are deferred — the proto crate is OS-agnostic, only the daemon's source
backends would need to be added.

## Documentation

| File | What's in it |
|------|--------------|
| [`docs/01-hardware.md`](docs/01-hardware.md) | 0xCB-1337 rev5.0 hardware: BOM, pinout, bootloader entry, schematic links |
| [`docs/02-firmware-stack.md`](docs/02-firmware-stack.md) | Rust toolchain, embassy-rp / embassy-usb / ssd1306 versions actually used, atomic CAS gotcha, crate skeleton |
| [`docs/03-probe-rs.md`](docs/03-probe-rs.md) | probe.rs install, debug probe options, RP2040 flashing & RTT logging (optional — UF2 was sufficient for v1) |
| [`docs/04-host-integration.md`](docs/04-host-integration.md) | wpctl + PipeWire FFT daemon design, wire schema, NixOS module deployment |
| [`docs/05-architecture.md`](docs/05-architecture.md) | Final firmware task layout, USB descriptors, OLED layout, boot sequence |
| [`docs/06-implementation-notes.md`](docs/06-implementation-notes.md) | Surprises and gotchas from the actual build — bootmagic-is-firmware, PIO encoder hang, SK6812 vs WS2812B colour, embassy 0.10 API quirks |
| [`docs/sources.md`](docs/sources.md) | Every URL referenced in the docs |

## Quick start

Inside this repo:

```fish
# Enter the dev shell — pulls rustup, probe-rs, elf2uf2-rs, libudev, dbus.
nix develop
```

### 1. Build + flash the firmware

```fish
# Build the firmware ELF + UF2.
cd firmware && cargo build --release && cd ..
elf2uf2-rs target/thumbv6m-none-eabi/release/firmware target/firmware.uf2

# Put the macropad in BOOTSEL mode: hold the encoder click while plugging in
# the USB-C cable (the firmware's bootmagic detects this and jumps to ROM).
# A USB mass storage device named "RPI-RP2" appears.

# Drop the UF2 onto it — the board reboots into the new firmware.
cp target/firmware.uf2 /run/media/$USER/RPI-RP2/
```

Subsequent re-flashes only need the bootmagic + `cp` — `cargo build` rebuilds
the ELF, then re-run `elf2uf2-rs`.

If you have a debug probe (Pi Debug Probe or a second Pico flashed with
`debugprobe`), `cargo run --release` from `firmware/` flashes via probe.rs
and streams `defmt` logs.

### 2. Run the host daemon

Foreground, with logs to stderr:

```fish
cargo run --release -p host --bin 0xcb-media-host
```

Useful flags:

```fish
# Verbose logging
RUST_LOG=debug cargo run --release -p host --bin 0xcb-media-host

# Disable the visualizer entirely (no PipeWire capture, no FFT thread)
cargo run --release -p host --bin 0xcb-media-host -- --no-visualizer

# Pin the visualizer to a specific PipeWire node (e.g. a non-default sink monitor)
cargo run --release -p host --bin 0xcb-media-host -- \
  --visualizer-source alsa_output.pci-0000_00_1b.0.analog-stereo.monitor

# Custom serial path (defaults to /dev/ttyACM0 or $OXCB_MEDIA_SERIAL)
cargo run --release -p host --bin 0xcb-media-host -- --device /dev/ttyACM1
```

### 3. (NixOS) install as a per-user systemd service

The `flake.nix` exports a NixOS module that wires the daemon as a per-user
systemd unit:

```nix
# flake.nix in your system config
{
  inputs.zero-x-cb-media.url = "github:you/0xCB-media";

  outputs = { self, nixpkgs, zero-x-cb-media, ... }: {
    nixosConfigurations.your-host = nixpkgs.lib.nixosSystem {
      modules = [
        zero-x-cb-media.nixosModules.default
        ({ ... }: {
          services."0xcb-media-host".enable = true;
          users.users.you.extraGroups = [ "dialout" ];   # /dev/ttyACM0 access
        })
      ];
    };
  };
}
```

After `nixos-rebuild switch`:

```fish
systemctl --user status 0xcb-media-host
```

### Testing the device without the daemon

There's a helper binary that can push one `Volume` frame or stream a
synthetic visualizer pattern, useful when you want to validate the firmware
without PipeWire in the loop:

```fish
# Send Volume(73%)
cargo run --release -p host --bin 0xcb-media-test-send -- /dev/ttyACM0 73

# ~6 s of swept-band synthetic spectrum
cargo run --release -p host --bin 0xcb-media-test-send -- /dev/ttyACM0 --viz
```

## Default keymap

```
                col 0          col 1          col 2 (encoder click on row 0)
row 0   ┌────────────────────────────────────────────────────────────┐
        │  Prev Track  │  Play/Pause  │       Mute                  │
row 1   │  Next Track  │  Stop        │       Toggle visualizer     │
row 2   │  (unbound)   │  (unbound)   │       (unbound)             │
        └────────────────────────────────────────────────────────────┘
```

Encoder rotation: CW = Volume Up, CCW = Volume Down. The "Toggle visualizer"
key (matrix `[1,2]`, the one directly below the encoder) is firmware-only —
it flips a flag in `DISPLAY_STATE`, so the host stays unaware and keeps
streaming. Edit `KEYMAP` in
[`firmware/src/main.rs`](firmware/src/main.rs) to remap; takes a re-flash.

## License

GPL-2.0-or-later, matching the upstream 0xCB firmware.

## Credits

- 0xCB-1337 hardware by [Conor Burns / 0xCB](https://github.com/0xCB-dev) — OSHWA
  certified DE000121.
- Built on [Embassy](https://embassy.dev) and the
  [rust-embedded](https://github.com/rust-embedded) ecosystem.
