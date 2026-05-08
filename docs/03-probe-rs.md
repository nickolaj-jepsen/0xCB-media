# probe.rs — flashing & debugging the RP2040

> **Status: optional in v1.** The shipping firmware was developed
> entirely via UF2 + bootmagic (encoder click held while plugging in).
> probe.rs stays supported via `firmware/.cargo/config.toml`'s
> `runner = "probe-rs run --chip RP2040"` for the day someone wants
> defmt logs over RTT — but it isn't on the critical path. If you only
> ever do `cargo build` + `elf2uf2-rs` + `cp`, you can ignore this doc.

[probe.rs](https://probe.rs) is a Rust-native embedded toolchain that replaces
OpenOCD + GDB + a flasher with a single `probe-rs` binary. It speaks SWD/JTAG
over CMSIS-DAP, ST-Link, J-Link, FTDI, and the official Raspberry Pi Debug
Probe, and supports the RP2040 as a first-class target.

## Why use a debug probe at all?

The RP2040 has a UF2 bootloader in ROM, so you can technically flash forever
just by holding the encoder click while plugging in. That works, but loses you:

- **Sub-second flash cycles.** UF2 is reboot → enumerate as MSC → copy →
  reboot. A debug probe writes flash directly in ~1–2 s.
- **`defmt` over RTT.** Real-time logs streamed from the device while it runs.
- **Breakpoints, single-step, memory inspection** in `probe-rs gdb` or VS Code
  with the probe-rs Debug Adapter.
- **No more re-pressing the encoder** every iteration.

For a media controller you'll iterate on the OLED rendering and HID descriptors
constantly, so the probe pays for itself within a day.

## Probe options

Any one of these works:

| Probe | Notes |
|-------|-------|
| **Raspberry Pi Debug Probe** ([product page](https://www.raspberrypi.com/products/debug-probe/)) | Official ~$12 device, plug-and-play CMSIS-DAP + UART. Best ergonomics. |
| **A second Raspberry Pi Pico** flashed with [`debugprobe`](https://github.com/raspberrypi/debugprobe) firmware | If you have a spare Pico, flash it with `debugprobe_on_pico.uf2` and you have a free probe. |
| **Any CMSIS-DAP probe** (DAPLink, Picoprobe variants, J-Link, ST-Link v2/v3, FT2232H) | All recognised by probe-rs. |

The official Pi Debug Probe is the lowest-friction option.

## Wiring (target ↔ probe)

The RP2040 exposes SWD on dedicated pins (not GPIO). On rev5.0 of the 1337
they're on the **bottom-side test pads** — confirm pad locations against the
[rev5.0 schematic](https://github.com/0xCB-dev/0xCB-1337/blob/main/rev5.0/pcb.kicad_sch).

Minimum 3 wires:

| Target signal | Pin / pad |
|---------------|-----------|
| `SWDIO` | RP2040 dedicated SWDIO pin (test pad) |
| `SWCLK` | RP2040 dedicated SWCLK pin (test pad) |
| `GND`   | Any ground pad |

Plus optional but useful:

| Signal | Use |
|--------|-----|
| `UART0 TX` (GP0) → probe RX | UART log fallback when RTT isn't running |
| `UART0 RX` (GP1) ← probe TX | (rarely needed) |

The 1337 stays USB-powered — **don't** also feed VBUS from the probe.

If the rev5.0 PCB doesn't break SWD out to a friendly header, you have two
options:

1. **Solder a 3-wire pigtail** to the SWDIO/SWCLK/GND pads and route it out
   the case.
2. **Skip the probe and rely on UF2 + serial logs over CDC ACM** — slower
   iteration but no soldering.

## Install

Linux / macOS:

```fish
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/probe-rs/probe-rs/releases/latest/download/probe-rs-tools-installer.sh | sh
```

NixOS — package is already in nixpkgs:

```fish
nix-shell -p probe-rs
```

Or in a `flake.nix` devshell:

```nix
buildInputs = [ pkgs.probe-rs ];
```

Verify:

```fish
probe-rs --version
probe-rs list   # connect probe first; should print probe info
```

## udev rules (Linux)

Without these, you'll need `sudo` to talk to the probe.

```fish
curl -L https://probe.rs/files/69-probe-rs.rules \
  | sudo tee /etc/udev/rules.d/69-probe-rs.rules
sudo udevadm control --reload
sudo udevadm trigger
```

On NixOS, add the rules declaratively in your config:

```nix
services.udev.packages = [ pkgs.probe-rs ];
```

systemd v258+ also wants a `plugdev` group:

```fish
sudo groupadd --system plugdev
sudo usermod -a -G plugdev $USER
```

(re-login after adding yourself to the group.)

## Target name

The RP2040 ships in probe-rs's built-in target list. Use the chip name string:

```
RP2040
```

Pass it to every command via `--chip RP2040`.

## Common commands

```fish
# Inspect connected probes
probe-rs list

# Erase the chip (stop a runaway firmware)
probe-rs erase --chip RP2040

# Flash a built ELF (probe-rs writes the .text/.data sections)
probe-rs download --chip RP2040 target/thumbv6m-none-eabi/release/0xcb-media-fw

# Flash + run + stream defmt logs (the dev loop)
probe-rs run --chip RP2040 target/thumbv6m-none-eabi/release/0xcb-media-fw

# GDB server
probe-rs gdb --chip RP2040
```

The most useful one is `probe-rs run` — combine it with cargo's `runner` config
and you get `cargo run` flashing the target every iteration.

## `cargo run` integration

`firmware/.cargo/config.toml`:

```toml
[target.thumbv6m-none-eabi]
runner = "probe-rs run --chip RP2040"

[build]
target = "thumbv6m-none-eabi"

[env]
DEFMT_LOG = "info"
```

Now `cd firmware && cargo run --release` flashes and streams logs.

## VS Code debug adapter

probe-rs ships a Debug Adapter Protocol implementation. Install the
[probe-rs VS Code extension](https://marketplace.visualstudio.com/items?itemName=probe-rs.probe-rs-debugger),
then add a `launch.json` block:

```jsonc
{
  "type": "probe-rs-debug",
  "request": "launch",
  "name": "Flash + debug 0xCB-media-fw",
  "cwd": "${workspaceFolder}/firmware",
  "chip": "RP2040",
  "flashingConfig": { "flashingEnabled": true },
  "coreConfigs": [
    {
      "programBinary": "target/thumbv6m-none-eabi/debug/0xcb-media-fw",
      "rttEnabled": true
    }
  ]
}
```

## UF2 fallback

If something has bricked SWD (extremely rare on RP2040 since the bootloader is
ROM) or you want to flash without a probe:

```fish
# Build
cargo build --release

# Convert ELF → UF2
elf2uf2-rs target/thumbv6m-none-eabi/release/0xcb-media-fw firmware.uf2

# Hold encoder click while plugging in → RPI-RP2 mass storage appears
# Drag firmware.uf2 onto the RPI-RP2 volume
```

You can automate the encoder-click trick in firmware via the
`reset_to_bootloader` ROM call, but only after you've already flashed working
firmware once.

## Troubleshooting

- **"No connected probes found"** — udev rules missing, or probe not in
  CMSIS-DAP mode. Run `probe-rs list` after replugging.
- **"Failed to read CHIP_ID register"** — wiring fault on SWDIO/SWCLK, or the
  target has no power (USB-C disconnected).
- **Flash succeeds, RTT shows nothing** — `DEFMT_LOG` is unset, or your
  `defmt-rtt` panic-handler isn't initialised. Add `use defmt_rtt as _;` and
  `use panic_probe as _;` in `main.rs`.
- **Probe-rs and OpenOCD fight** — make sure no other tool (rp-rs picotool,
  OpenOCD) is holding the probe.
