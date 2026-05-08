# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project shape

Cargo workspace with three crates targeting two architectures:

- `firmware/` — `no_std`, Embassy-on-RP2040, target `thumbv6m-none-eabi`. Composite USB device (HID Consumer Control + CDC ACM). Runs on a 0xCB-1337 rev5.0 macropad.
- `host/` — Linux-only daemon binary (`0xcb-media-host`) plus a manual test sender (`0xcb-media-test-send`). Bridges MPRIS now-playing + `wpctl` volume to the macropad over CDC ACM.
- `proto/` — `no_std`-friendly serde schema (`HostToDevice`, `DeviceToHost`) shared between the two. Wire format: postcard + COBS framing, one frame per `0x00` delimiter, `MAX_FRAME_LEN = 256`.

`Cargo.toml` sets `default-members = ["proto", "host"]` so workspace-wide commands skip the firmware (which can't cross-compile without its own target config).

## Commands

Always work inside `nix develop` — `pkg-config`, `libudev`, `dbus`, `probe-rs`, `elf2uf2-rs`, etc. come from the flake's devShell.

```fish
# Host + proto (workspace defaults)
cargo build                  # debug build of host + proto
cargo clippy -p host -p proto --all-targets -- -D warnings
cargo test -p host -p proto --all-targets
cargo test -p host -- some_test_name      # single test

# Firmware — MUST run from firmware/ (see "Cargo cwd matters" below)
cd firmware && cargo build --release
cd firmware && cargo clippy --all-targets -- -D warnings
cd firmware && cargo run --release        # flash via probe-rs (debug probe required)

# Format check (matches CI)
cargo fmt --all -- --check

# Build firmware UF2 + flash via bootmagic + open serial — see justfile
just flash                   # build → img → wait for RPI-RP2 mount → cp → chmod /dev/ttyACM0
just host                    # run daemon with RUST_LOG=debug
```

CI runs (see `.github/workflows/ci.yml`): `cargo fmt --check`, clippy on host+proto, clippy on firmware (from `firmware/`), `cargo test` on host+proto, and `nix build .#host` / `nix build .#firmware`.

Nix package builds: `nix build .#host` and `nix build .#firmware`. The flake also exports `nixosModules.default` (per-user systemd unit for the daemon).

## Cargo cwd matters (firmware target selection)

`firmware/.cargo/config.toml` pins `target = "thumbv6m-none-eabi"`. Cargo walks up from CWD looking for `.cargo/config.toml`, so:

- `cd firmware && cargo build` works (finds the config, cross-compiles).
- `cargo build -p firmware` from the workspace root silently uses the host target and breaks on Cortex-M asm.
- Workspace `default-members` excludes `firmware` to make bare `cargo check`/`cargo build`/`cargo test` from the root safe.

When editing firmware, always `cd firmware` first, or use `cargo <cmd> --manifest-path firmware/Cargo.toml` (CI does the latter).

## Firmware architecture (single-file design)

All firmware lives in `firmware/src/main.rs` (~750 LOC). The original plan called for splitting into modules; it never crossed the threshold where splitting helps. Don't pre-emptively split — the natural seams (if it ever grows) are `proto.rs` (types + channels), `usb.rs` (descriptor + composite setup), one file per task.

Key shared statics that any task can reach:

- `CONSUMER_EVENTS` — `Channel<_, ConsumerKey, 8>`. Matrix + encoder push, `hid_writer_task` drains.
- `LED_EVENTS` — `Channel<_, LedCommand, 16>`. Matrix + CDC RX push, `led_task` drains.
- `DEVICE_TX_EVENTS` — `Channel<_, DeviceToHost, 8>`. Drains via `cdc_tx_task`.
- `DISPLAY_STATE` — `blocking_mutex::Mutex<CriticalSectionRawMutex, RefCell<DisplayState>>`. CDC RX writes, the inline display loop reads.

Tasks: `usb_task`, `hid_writer_task`, `cdc_tx_task`, `matrix_task`, `encoder_task`, `led_task` are all `#[embassy_executor::task]`. The display loop and `cdc_rx_loop` are **not** spawned tasks — they're async blocks `join`ed inside `main` because their handle types (`Ssd1306Async`, `CdcReceiver`) carry lifetimes that aren't easily `'static`. Don't try to spawn them.

Bootmagic (hold encoder click while plugging in → ROM bootloader) is **firmware-only**. Hardware has no reset button. The check at the top of `main` calls `embassy_rp::rom_data::reset_to_usb_boot(0, 0)` if `matrix[2]` (GP9) reads low at boot. Any firmware change that breaks this loses the only recovery path short of a debug probe or shorting QSPI_SS_N to GND.

## RP2040-specific gotchas

- **Atomic CAS**: Cortex-M0+ has no native CAS. `portable-atomic = { version = "1.5", features = ["critical-section"] }` is required so `static_cell`, `embassy-sync`, etc. compile. The critical-section impl comes from `embassy-rp`'s `critical-section-impl` feature. Removing either gives cryptic build errors deep in dependencies.
- **PIO encoder hangs on this board**: `embassy_rp::pio_programs::rotary_encoder::PioEncoder` consistently hangs during construction on rev5.0 (both PIO0/SM1 and PIO1/SM0). Replaced with a GPIO-IRQ + 16-entry gray-code state machine (`QDEC` table in `encoder_task`) plus a 150 µs settle for bounce. Do not switch back to the PIO version without a debug probe to diagnose.
- **WS2812 chain**: Don't toggle GP14 (TPS2553DBVR enable) to "blink" — the LEDs reset to off and need a fresh frame to display. Keep GP14 high and animate frames at ~60 Hz.
- **SK6812MINI-E (per-key, indices 0–7) vs WS2812B (underglow, 8–30) render colour differently** despite shared timing/byte-order. Two tuned constants (`ACCENT_PERKEY`, `ACCENT_UNDERGLOW`) hit the same visible accent (`#CF6A4C`).
- **embassy-usb 0.6 API quirks**: `HidSubclass::No` (not `NoSubclass`/`None`), `HidBootProtocol::None` (not `Default`), `PioWs2812` takes a 4th type parameter `ORDER` (`Grb`).
- **embassy 0.10 spawn API**: tasks return `Result<SpawnToken, SpawnError>`; pattern is `spawner.spawn(my_task(args).unwrap())`. The arch feature is `platform-cortex-m`, not `arch-cortex-m`.
- **heapless 0.8**: no const `TryFrom<&str>`. Build via `push_str` (or per-char in a loop for explicit truncation, as `heapless_truncate` does in the host crate).

`docs/06-implementation-notes.md` has the long form of all of these.

## Wire protocol invariants (proto crate)

`HostToDevice::NowPlaying { title: String<64>, artist: String<32>, is_playing }`, `Volume { level: u8 (0..=100), muted }`, `Clear`, `Ping`. `DeviceToHost::Pong`, `EncoderClick`. Frames are postcard-COBS; both sides decode by buffering until `0x00` and calling `postcard::from_bytes_cobs`. If you change capacities or add variants, also bump `MAX_FRAME_LEN` in `proto/src/lib.rs` and check `tx_buf` / `frame_buf` sizes on both sides.

The firmware flips its OLED to "Disconnected" after 5 s without any host frame. The daemon's ping thread defaults to 2 s — keep `ping_interval_s ≤ 4` if you change defaults.

## Host daemon

`host/src/bin/0xcb-media-host.rs` — three blocking source threads (`mpris`, `volume`, `ping`) feed a bounded `crossbeam_channel` of `HostToDevice`; a single serial loop drains the channel, COBS-encodes, and writes. The same loop also reads `DeviceToHost` frames (currently logs `EncoderClick`). Reopens the port with 2 s backoff on any I/O error.

Volume comes from shelling out to `wpctl get-volume @DEFAULT_AUDIO_SINK@` and parsing `Volume: 0.47 [MUTED]?`. The daemon depends on PipeWire/WirePlumber being installed on the host. MPRIS comes from D-Bus via the `mpris` crate.

`OXCB_MEDIA_SERIAL` env var sets the device path (NixOS module wires this).

## Hardware target

**rev5.0 only** (RP2040, 4 MB QSPI flash, 31-LED chain, EC11 encoder, SSD1306 128×64 OLED on I²C1). Earlier rev1.0–rev4.0 were ATmega32U4 — the upstream QMK `keyboard.json` still says so; this project does not target those. Pinout in `docs/01-hardware.md`. Memory layout in `firmware/memory.x` (BOOT2 0x10000000+0x100, FLASH 4MB-256, RAM 264K).

## Style

- License is GPL-2.0-or-later (matches upstream 0xCB firmware).
- This is a fish shell environment on NixOS. Use fish-compatible syntax in any shell snippets you write.
- README warns "this project is vibecoded" — match the existing tone in comments / docs.
