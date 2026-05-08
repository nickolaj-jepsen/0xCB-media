# Implementation notes

Surprises and gotchas that came up during the actual build, beyond what
the planning docs anticipated. If you're picking this project up cold or
porting it to a different RP2040 board, these are the bits that cost the
most time the first time around.

## Bootloader entry is firmware-only, not hardware

The "hold encoder click while plugging in" trick only works because the
running firmware checks for it at boot and calls
`embassy_rp::rom_data::reset_to_usb_boot(0, 0)`. It is **not** a hardware
feature of the board — the rev5.0 has no physical reset button.

That means:

- The official Vial QMK firmware works.
- Our firmware works (bootmagic was added in M0).
- **MicroPython firmware does not** — the only way out is the REPL:
  `import machine; machine.bootloader()`.
- A board with corrupted firmware can still be recovered by shorting the
  W25Q32's `QSPI_SS_N` to GND while power-cycling — the universal RP2040
  rescue path. Or use a debug probe and `probe-rs erase --chip RP2040`.

## PIO encoder hung during init

`embassy_rp::pio_programs::rotary_encoder::PioEncoder` consistently hung
during construction on this board, both on PIO0/SM1 (sharing PIO0 with the
WS2812 driver) and PIO1/SM0 (a fresh PIO instance). The firmware would
boot, run bootmagic, then go silent — no LEDs, no USB, no OLED — meaning
something panicked synchronously between the encoder spawn and the next
await.

Without a debug probe to read the panic message, root-causing wasn't
worth it. Replaced with a plain GPIO-interrupt encoder and a gray-code
state machine in software:

- `wait_for_any_edge()` on either pin
- 150 µs settle to skip past contact bounce
- 16-entry transition lookup table (`QDEC[]`) — invalid transitions
  return 0, valid CW → +1, valid CCW → -1
- Accumulate ±1 per transition; emit `VolumeUp`/`VolumeDown` when the
  accumulator hits ±4 (EC11 = 4 transitions per detent)

This is more robust against bounce than the PIO program anyway. The PIO
encoder driver's PIO assembly is `wait 1 pin 1 / wait 0 pin 1 / in pins, 2 / push`
which doesn't filter bounce on its own; we'd have ended up adding the
state machine on top regardless.

## SK6812MINI-E and WS2812B render colour differently

Both LED chips share the same WS2812 timing and `GRB` byte order, so they
chain happily on the same data line. But their colour rendering differs
noticeably:

- SK6812MINI-E (in-switch, indices 0–7) has a stronger green bias.
  `(96, 28, 0)` reads as yellow on these.
- WS2812B (underglow, indices 8–30) is more neutral. The same triple
  reads as warm orange.

The firmware splits into two tuned constants (`ACCENT_UNDERGLOW = (112, 32, 0)`
and `ACCENT_PERKEY = (96, 12, 0)`) so both chains land on roughly the same
visible colour for the project accent (`#CF6A4C`).

## Don't toggle GP14 to "blink"

GP14 enables the TPS2553DBVR load switch that gates 5 V power to the LED
chain. Toggling it without sending pixel data produces no visible blink:
WS2812-family LEDs reset to off when power returns and need a fresh frame
to display anything.

The fix is to keep GP14 high and animate by writing fresh frames at a
modest rate (~60 Hz works well).

## Embassy 0.10 spawn API change

Task functions no longer return `SpawnToken<S>` directly — they return
`Result<SpawnToken<S>, SpawnError>` to surface arena-allocation failures.
The pattern is now:

```rust
spawner.spawn(my_task(args).unwrap()); // .unwrap() on the Result
```

`Spawner::spawn(token)` returns `()` instead of `Result<(), SpawnError>`.

The `arch-cortex-m` feature is also gone — use `platform-cortex-m` instead.

## Cortex-M0+ atomic CAS via portable-atomic

The RP2040 has no native `compare_exchange` instruction. Anything in the
dependency graph that needs atomic CAS (`static_cell`, `embassy-sync`,
some serde derives) must route through `portable-atomic` with the
critical-section emulation backend:

```toml
portable-atomic = { version = "1.5", features = ["critical-section"] }
```

The critical-section impl that backs the emulation comes from
`embassy-rp`'s `critical-section-impl` feature, which is already in our
feature list. Without `portable-atomic`'s `critical-section` feature you
get cryptic build errors deep in `static_cell`.

## embassy-usb 0.6 API quirks

A handful of small renames vs older docs:

- `HidSubclass::No` (not `NoSubclass` / `None`).
- `HidBootProtocol::None` (not `Default`).
- `PioWs2812` gained a 4th type parameter `ORDER` (use `Grb` for
  WS2812/SK6812).

Generic-over-driver helpers (e.g. `cdc_rx_loop`) need an explicit lifetime
+ `T: UsbInstance + 'd` bound — type aliases like
`type UsbDrv = UsbDriver<'static, USB>` are fine for spawned tasks but
make sub-functions fight the borrow checker if reused there.

## Cargo workspace cwd matters

`firmware/.cargo/config.toml` sets `target = "thumbv6m-none-eabi"`. Cargo
walks up from CWD looking for `.cargo/config.toml`, so:

- Running `cargo build -p firmware` from the **workspace root** silently
  uses the host target (x86_64-linux-gnu) and breaks on Cortex-M-only
  asm in `cortex-m`.
- Running it from `firmware/` works because the right config is found.
- Workspace root has `default-members = ["proto", "host"]` so `cargo
  check` from there only checks the host-side crates and doesn't try
  the impossible.

For the host crate the inverse holds: build from anywhere, but you need
the flake's devShell so `pkg-config` can find `libudev` / `dbus`.

## heapless 0.8 String construction

`heapless::String<N>` doesn't have a const `TryFrom<&str>` in 0.8. Build
strings via `push_str`:

```rust
let mut s: heapless::String<N> = heapless::String::new();
s.push_str(text).map_err(|_| anyhow!("string too long"))?;
```

Truncation at capacity (when copying possibly-too-long external strings)
needs to be explicit:

```rust
let mut out = heapless::String::<N>::new();
for c in s.chars() {
    if out.push(c).is_err() { break; }
}
```

## Cargo bin auto-detection vs `[[bin]]`

If `src/main.rs` exists AND `[[bin]] path = "src/main.rs"` is declared,
cargo errors out with "duplicate binary name" because auto-detection and
the manual declaration both fire. The cleanest fix is to put both
binaries under `src/bin/<binary-name>.rs` and remove the explicit
`[[bin]]` blocks — `0xcb-media-host.rs` and `0xcb-media-test-send.rs`
in our case.

## Single 600-LOC `firmware/src/main.rs`

The original plan called for splitting the firmware into per-concern
modules (`usb.rs`, `hid.rs`, `cdc.rs`, `matrix.rs`, `encoder.rs`,
`display.rs`, `leds.rs`). At ~600 LOC the single-file shape never crossed
the threshold where splitting would help — the modules would mostly be
short, and the shared types (`ConsumerKey`, `LedCommand`,
`DisplayState`) plus the static channels would have to live somewhere
visible to all of them anyway.

If a future refactor wants to split, the natural seams are: `proto.rs`
(types + channels), `usb.rs` (descriptor + composite setup), and one file
per task.

## Tested only on Linux

The host daemon is Linux-specific (uses MPRIS via D-Bus + `wpctl`
shell-out). The proto crate is OS-agnostic so adding a Windows backend
(via `gsmtc`) or a macOS backend should be a contained module addition,
but neither is in v1.
