# Firmware stack — Rust on RP2040

The crates we actually use, with the exact versions the build was last
verified against. Some of these differ from the versions originally proposed
during planning because Embassy and friends moved to 1.0/0.10 releases in
the interim — see [`06-implementation-notes.md`](06-implementation-notes.md)
for the gotchas that surfaced along the way.

## Toolchain

```fish
# Rust target for RP2040 (Cortex-M0+) — pinned in rust-toolchain.toml.
rustup target add thumbv6m-none-eabi

# UF2 generation — fast path that doesn't need a debug probe.
cargo install elf2uf2-rs --locked

# Optional: probe.rs for SWD flashing + RTT logging via a debug probe.
cargo install probe-rs-tools --locked
```

On NixOS this is all wired up by the `flake.nix` devshell:

```fish
nix develop
```

The shell pulls in `rustup`, `probe-rs`, `elf2uf2-rs`, plus `pkg-config`,
`udev`, `dbus`, and `systemd` for the host crate's link-time deps.

> If you build the **firmware** crate, you need to either `cd firmware/` first
> (so its `.cargo/config.toml` sets `target = "thumbv6m-none-eabi"`), or pass
> `--target thumbv6m-none-eabi -p firmware` from the workspace root. Running
> `cargo build` from the workspace root only builds the host-side crates
> (`proto`, `host`) — the workspace's `default-members` exclude `firmware`
> precisely to avoid trying to cross-compile it without the right target arg.

## Async runtime: Embassy

[Embassy](https://embassy.dev/) handles concurrency between the USB stack,
OLED redraws, encoder reads, matrix scanning, and LED rendering without us
writing a scheduler.

| Crate | Version | Purpose |
|-------|---------|---------|
| [`embassy-rp`](https://docs.rs/embassy-rp) | 0.10.0 | RP2040 HAL: GPIO, I²C, USB, PIO, DMA |
| [`embassy-executor`](https://docs.rs/embassy-executor) | 0.10.0 | Async executor (`platform-cortex-m`, `executor-thread`, `executor-interrupt`) |
| [`embassy-time`](https://docs.rs/embassy-time) | 0.5.1 | Async timers, `Duration` / `Instant` |
| [`embassy-sync`](https://docs.rs/embassy-sync) | 0.8.0 | Channels, signals, mutexes |
| [`embassy-usb`](https://docs.rs/embassy-usb) | 0.6.0 | Async USB device stack (CDC ACM, HID) |
| [`embassy-futures`](https://docs.rs/embassy-futures) | 0.1.2 | `join`, `select` combinators |

⚠️ **embassy-executor 0.10 spawn API change**: task functions now return
`Result<SpawnToken<S>, SpawnError>` and `Spawner::spawn(token)` returns `()`.
The pattern is `spawner.spawn(my_task(args).unwrap())` — the `.unwrap()` is
on the Result returned by the macro, not on the spawn call.

⚠️ **`platform-cortex-m` feature** replaces older `arch-cortex-m`. Easy to
miss when copying older example code.

Embassy's RP examples were the gold reference throughout:

- `examples/rp/src/bin/usb_serial.rs` — Builder + `StaticCell` pattern.
- `examples/rp/src/bin/usb_hid_keyboard.rs` — HID class instantiation.
- `examples/rp/src/bin/usb_midi.rs` — proves >2 USB classes coexist.
- `examples/rp/src/bin/i2c_async_embassy.rs` — `bind_interrupts!` + `I2c::new_async`. Pin order is `(scl, sda)`.
- `examples/rp/src/bin/pio_ws2812.rs` — WS2812 init + `.write(&data).await`.
- `examples/rp/src/bin/pio_rotary_encoder.rs` — PIO encoder. **We don't use it** (see [implementation notes](06-implementation-notes.md#pio-encoder-hung-during-init)).

## Cortex-M0+ atomic gotcha

The RP2040 is Cortex-M0+, which has no native atomic CAS instruction. Many
crates in the Embassy ecosystem (and `static_cell` itself) need atomics, so
you must explicitly enable `portable-atomic`'s critical-section emulation:

```toml
# firmware/Cargo.toml
portable-atomic = { version = "1.5", features = ["critical-section"] }
```

Without this, you get cryptic `compare_exchange requires atomic CAS but not
available on this target by default` errors. The critical-section impl that
backs it is provided by `embassy-rp`'s `critical-section-impl` feature.

## OLED driver: ssd1306

Crate: [`ssd1306`](https://docs.rs/ssd1306) `0.10.0`, MIT/Apache-2.0.

Used:

- `Ssd1306Async` (gated on the `async` feature) — integrates with
  `embassy-rp`'s `i2c::I2c<'_, _, Async>`.
- `BufferedGraphicsMode` — in-RAM 1024-byte framebuffer + `embedded-graphics`
  integration. Renders at ~30 Hz with no measurable CPU pressure.

Init shape:

```rust
let i2c = embassy_rp::i2c::I2c::new_async(p.I2C1, p.PIN_3, p.PIN_2, Irqs, Default::default());
let interface = ssd1306::I2CDisplayInterface::new(i2c);
let mut display = Ssd1306Async::new(interface, DisplaySize128x64, DisplayRotation::Rotate0)
    .into_buffered_graphics_mode();
display.init().await.unwrap();
```

⚠️ `i2c::I2c::new_async` takes `(peripheral, scl, sda, irqs, config)` — SCL
first, SDA second. Easy to swap by accident; the bus then silently does
nothing.

## USB

We chose **embassy-usb directly** over `usbd-human-interface-device` (which
runs on the older sync `usb-device` stack and doesn't share peripherals
nicely with Embassy tasks) and over **RMK** (which is a great keyboard
framework, but our custom OLED rendering would have us fighting it instead
of using it).

The composite device exposes:

- **CDC ACM** (interfaces 0+1, IAD-grouped) for `proto` messages.
- **HID Consumer Control** (interface 2) for media keys.

Notes:

- `composite_with_iads = true` plus class codes `0xEF` / `0x02` / `0x01` are
  required for the host to correctly group the two classes via Interface
  Association Descriptors.
- `HidSubclass::No` and `HidBootProtocol::None` (not `NoSubclass`/`Default`).
- The HID descriptor is hand-rolled — see
  [`05-architecture.md`](05-architecture.md#hid-report-descriptor-consumer-control).
- After `CdcAcmClass::new(...)` we call `.split()` to get separate `Sender`
  and `Receiver` halves so the TX side can run as a task while the RX side
  stays in `main`'s join.

## WS2812 / SK6812 RGB chain

Used [`embassy_rp::pio_programs::ws2812::PioWs2812`](https://docs.rs/embassy-rp/latest/embassy_rp/pio_programs/ws2812/struct.PioWs2812.html)
on PIO0 / SM0 / DMA_CH0 / GP25. The 31-LED chain is 8 SK6812MINI-E (per-key)
followed by 23 WS2812B (underglow) — both speak the same WS2812 timing.

⚠️ **Embassy 0.10 added a 4th type parameter `ORDER`**:

```rust
PioWs2812::<'_, PIO0, 0, NUM_LEDS, Grb>::new(...)
//                                  ^^^ — required, defaults to Grb on `new`
```

Both LED types use GRB byte order. Constants live in
`embassy_rp::pio_programs::ws2812::{Grb, Rgb, Grbw, Rgbw}`.

⚠️ **SK6812MINI-E and WS2812B render colour differently.** SK6812 has a
stronger green bias and looks yellow at the same G:R ratio that reads as
warm orange on WS2812B. The firmware uses two separately-tuned `RGB8`
constants (`ACCENT_UNDERGLOW` and `ACCENT_PERKEY`) to compensate.

⚠️ **GP14 (TPS2553DBVR enable) must be high before LEDs work.** The first
firmware iteration toggled GP14 to "blink" and saw nothing — the LEDs reset
on power loss, so toggling power without sending fresh pixel data looks
like nothing. Solution: GP14 stays high; blink/fade is done by writing
fresh frames.

## Logging: defmt + RTT

Used when a debug probe is connected (UF2 flow doesn't get logs):

```toml
defmt = "1.0.1"
defmt-rtt = "1.0.0"
panic-probe = { version = "1.0.0", features = ["print-defmt"] }
```

⚠️ `defmt::unwrap!` requires the inner Result's error to implement
`defmt::Format`. `display_interface::DisplayError` and embassy's
`SpawnError` don't, so use plain `.unwrap()` for those.

`firmware/.cargo/config.toml`:

```toml
[target.thumbv6m-none-eabi]
runner = "probe-rs run --chip RP2040"

[build]
target = "thumbv6m-none-eabi"

[env]
DEFMT_LOG = "info"
```

## Wire protocol: postcard + COBS

For host↔device messaging we picked **postcard with COBS framing** over
JSON because:

- Compact binary (each message is <20 bytes typical, <80 worst case).
- Single `#[derive(Serialize, Deserialize)]` schema in the `proto` crate
  shared between firmware and host.
- COBS gives a clean delimiter (zero byte) for stream framing.

```toml
postcard = { version = "1", default-features = false }
heapless = { version = "0.8", features = ["serde"] }
```

⚠️ heapless 0.8 `String::push_str` returns `Result<(), ()>` (no const
`TryFrom<&str>`). To build a `String<N>` from a `&str` on the host side:

```rust
let mut s: heapless::String<N> = heapless::String::new();
s.push_str(text).map_err(|_| anyhow!("string too long"))?;
```

## Memory layout

Stock RP2040 + W25Q32 (4 MB external QSPI flash):

```
MEMORY {
    BOOT2 : ORIGIN = 0x10000000, LENGTH = 0x100
    FLASH : ORIGIN = 0x10000100, LENGTH = 4096K - 0x100
    RAM   : ORIGIN = 0x20000000, LENGTH = 264K
}
```

`firmware/build.rs` writes a copy of `memory.x` into `OUT_DIR` and tells
`cortex-m-rt` to link `link.x` + `defmt.x`.

## Final crate layout

What actually got built (single workspace, three members):

```
0xCB-media/
├── Cargo.toml                     # workspace = [proto, firmware, host]
├── flake.nix                      # devShell + NixOS module
├── rust-toolchain.toml
├── proto/                         # no_std-compatible shared schema
│   ├── Cargo.toml
│   └── src/lib.rs                 # HostToDevice / DeviceToHost
├── firmware/                      # no_std, runs on RP2040
│   ├── Cargo.toml
│   ├── memory.x
│   ├── build.rs
│   ├── .cargo/config.toml         # target = thumbv6m-none-eabi
│   └── src/main.rs                # everything in one file (~600 LOC)
└── host/                          # std, runs on Linux PC
    ├── Cargo.toml
    └── src/bin/
        ├── 0xcb-media-host.rs     # MPRIS / wpctl daemon
        └── 0xcb-media-test-send.rs # one-shot frame tester
```

The firmware deliberately stayed as a single `main.rs` file — at ~600 LOC
it's small enough that splitting modules would obscure rather than clarify.
The candidate split (matrix.rs / encoder.rs / display.rs / leds.rs / cdc.rs)
from the planning phase was never needed.
