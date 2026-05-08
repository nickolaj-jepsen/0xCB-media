# 0xCB-1337 rev5.0 — Hardware Reference

Everything in this document is sourced from the official hardware repo
[`0xCB-dev/0xCB-1337`](https://github.com/0xCB-dev/0xCB-1337) (rev5.0/) and the
official QMK firmware port at
[`0xCB-dev/keeb-firmware-source`](https://github.com/0xCB-dev/keeb-firmware-source/tree/main/vial/1337/v5).
The QMK firmware shipping in the upstream `qmk/qmk_firmware` tree is for the
older ATmega32U4 revisions and **does not apply** to rev5.0.

## Identifying your revision

If you bought the macropad recently from KeebSupply you have rev5.0 (RP2040).
Other ways to confirm:

- A **flash drive named `RPI-RP2`** appears when you hold the **top-right key
  (encoder click)** while plugging in the USB cable. RP2040-only behavior.
- The PCB silk shows `1337-v5.0` (older revs are silked `v3.0`, `v4.0`, etc.).
- The MCU is a 7×7 mm QFN-56 package, not a TQFP-44 (32U4).

## Block diagram

```
   USB-C ──► USBLC6-2P6 ──► RP2040 USB PHY
                              │
                              │ QSPI
                              ├──── W25Q32JVUUIQ (4 MB external flash)
                              │
                              │ I²C1 (GP2/GP3)
                              ├──── FPC J2 ──── SSD1306 128×64 OLED
                              │
                              │ GPIO direct matrix (3×3)
                              ├──── 8 × Choc V1 hotswap + EC11 click
                              │
                              │ GP10/GP11
                              ├──── EC11 quadrature
                              │
                              │ GP25 (single data line, level-shifted via 74LVC1T45)
                              ├──── 8 × SK6812MINI-E (per-key) → 23 × WS2812B (underglow)
                              │
                              │ GP14
                              └──── TPS2553DBVR enable (5 V load switch for RGB)
```

## Bill of Materials (rev5.0)

Pulled from
[`rev5.0/kikit/prod/bom.csv`](https://github.com/0xCB-dev/0xCB-1337/blob/main/rev5.0/kikit/prod/bom.csv):

| Designator | Value | Footprint | LCSC |
|------------|-------|-----------|------|
| U6 | RP2040 | QFN-56 (7×7 mm) | C2040 |
| U5 | W25Q32JVUUIQ (4 MB QSPI flash) | USON-8 (3×4 mm) | C2999380 |
| U1 | USBLC6-2P6 (USB ESD) | SOT-666 | C2827693 |
| U2 | XC6210B332MR-G (3.3 V LDO) | SOT-23-5 | C47719 |
| U3 | TPS2553DBVR (load switch, RGB power) | SOT-23-6 | C55266 |
| U4 | 74LVC1T45 (level shifter, RGB data) | SOT-563 | C352970 |
| Y1 | 12 MHz crystal | SMD 2520-4Pin | C2149204 |
| J1 | USB-C 2.0 receptacle | GT-USB-7014C | C963373 |
| J2 | FPC 12-pin, 0.5 mm pitch | FFC-SMD | C388698 |
| MX1, MX2, MX4–MX9 | Kailh Choc V1 hotswap | — | C5156480 |
| MX3 | EC11 rotary encoder (replaces a key) | EC11 | C143817 |
| D1–D8 | SK6812MINI-E (per-key RGB, in-switch) | MX_SK6812MINI-E | C5149201 |
| D9–D31 | WS2812B-2020 (underglow RGB, 23 pcs) | PLCC4 2.0×2.0 mm | C2976072 |
| F1 | 1 A polyfuse | 0603 | C210357 |
| FB1, FB2 | 600R ferrite bead | 0402 | C160977 |
| Various | 100 nF, 1 µF, 10 µF caps; 1k/5k1/10k/27R/40k resistors | — | — |

**Total LEDs in chain: 31** (8 SK6812MINI-E + 23 WS2812B, daisy-chained on the
single GP25 data line). The level shifter (74LVC1T45) translates the RP2040's
3.3 V output to the 5 V the RGB chain expects. The TPS2553DBVR on GP14 is a load
switch that gates 5 V power to the LEDs — must be enabled before LEDs work.

## Pinout (RP2040 GPIO)

Sourced from
[`vial/1337/v5/info.json`](https://github.com/0xCB-dev/keeb-firmware-source/blob/main/vial/1337/v5/info.json)
and [`vial/1337/v5/config.h`](https://github.com/0xCB-dev/keeb-firmware-source/blob/main/vial/1337/v5/config.h).

### Key matrix (direct, 3×3 — no diodes)

```
                col 0     col 1     col 2
row 0  ┌───────────────────────────────────┐
       │  GP27  │  GP29  │  GP9 (encoder click)
row 1  │  GP26  │  GP28  │  GP8
row 2  │  GP18  │  GP17  │  GP12
       └───────────────────────────────────┘
```

The encoder push-button shares matrix position `[0, 2]` (GP9). All keys are
direct-wired to a GPIO; there is no scan matrix and no diodes — every key must
be configured with an internal pull-up and read as active-low.

### Encoder (EC11 quadrature)

| Signal | Pin |
|--------|-----|
| A      | GP11 |
| B      | GP10 |
| Resolution | 4 pulses per detent |

The encoder click is matrix `[0, 2]` (GP9), shared with the matrix.

### OLED (SSD1306, 128×64, I²C1)

| Signal | Pin |
|--------|-----|
| SDA | GP2 |
| SCL | GP3 |
| Driver | I²C1 (`I2CD1` in ChibiOS / `I2C1` peripheral on RP2040) |

The OLED sits on a small flex-cable daughter board behind the encoder; J2 (FPC
12-pin, 0.5 mm) carries I²C plus power. Default I²C address for SSD1306 panels
is `0x3C`.

### RGB (WS2812-compatible chain)

| Signal | Pin |
|--------|-----|
| Data out | GP25 (via 74LVC1T45 level shift) |
| Power enable | GP14 → TPS2553DBVR enable (active high) |
| Total LEDs | 31 (indexes 0–30) |
| Order | Per-key first (8 LEDs, indices 0–7), then underglow (indices 8–30) |

`v5.c` enables the LEDs by setting GP14 high and waiting ~20 ms before driving
data; do this in firmware init before sending any LED frames.

### USB

USB-C, USB 2.0 full-speed device, driven directly by the RP2040's onboard PHY.
ESD protection by USBLC6-2P6 inline.

### Pins available for SWD / debug / expansion

The RP2040's dedicated `SWCLK` / `SWDIO` pins are **not** GPIOs and are always
available for debugging via probe.rs (see
[`03-probe-rs.md`](03-probe-rs.md)). Whether they are physically broken out on
rev5.0 depends on the board — they are typically reachable on the test pads on
the bottom of the PCB. Inspect the rev5.0 schematic for `SWCLK` / `SWDIO` test
points: <https://github.com/0xCB-dev/0xCB-1337/blob/main/rev5.0/pcb.kicad_sch>.

GPIOs not used by any peripheral and free for repurposing if needed:

```
GP0, GP1, GP4, GP5, GP6, GP7, GP13, GP15, GP16,
GP19, GP20, GP21, GP22, GP23, GP24
```

(GP24 is sometimes wired as USB VBUS detect on RP2040 boards — check the
schematic before reusing it.)

## Bootloader entry

The RP2040 has a UF2 bootloader **etched into ROM**, so the device is
unbrickable by firmware flashing alone. There is no physical reset button on
rev5.0.

The "hold a key while plugging in" trick is **firmware-assisted** — it works
only because the running firmware reads that key at boot and calls the ROM
function `reset_to_usb_boot()`. It is not a hardware feature of the board.
What works depends on which firmware is currently on the device:

| Currently flashed firmware | Bootloader entry |
|----------------------------|------------------|
| Stock 0xCB Vial QMK (v5)   | Hold encoder click (GP9) while plugging in. QMK's bootmagic does the rest. |
| Our `0xCB-media` firmware  | Same — we replicate the QMK bootmagic. See `firmware/src/main.rs`. |
| MicroPython                | Connect to the REPL on `/dev/ttyACM0`, run `import machine; machine.bootloader()`. |
| No firmware / unknown      | Use a debug probe and `probe-rs erase --chip RP2040 --hard`, **or** short the test pad for `QSPI_SS_N` to GND while power-cycling. The latter is a documented but fiddly RP2040 universal recovery. |

Once in bootloader mode, a USB mass-storage device named **`RPI-RP2`** appears.
Drag a `.uf2` onto it and the board reboots into the new firmware.

## OLED font / bitmap notes

The QMK port ships a custom 7-bit ASCII font at
[`vial/1337/v5/gfxfont.c`](https://github.com/0xCB-dev/keeb-firmware-source/blob/main/vial/1337/v5/gfxfont.c)
(`OLED_FONT_END 223`). For the Rust port, [`embedded-graphics`](https://docs.rs/embedded-graphics/)
provides `MonoTextStyle` and bitmap fonts that integrate with the `ssd1306` crate's
`BufferedGraphicsMode` — you almost certainly don't need a custom font.

## Schematic & PCB files

- KiCad schematic: [`rev5.0/pcb.kicad_sch`](https://github.com/0xCB-dev/0xCB-1337/blob/main/rev5.0/pcb.kicad_sch)
- KiCad PCB: [`rev5.0/pcb.kicad_pcb`](https://github.com/0xCB-dev/0xCB-1337/blob/main/rev5.0/pcb.kicad_pcb)
- KiCad project: [`rev5.0/pcb.kicad_pro`](https://github.com/0xCB-dev/0xCB-1337/blob/main/rev5.0/pcb.kicad_pro)
- Interactive BOM (humanpnp): under `rev5.0/kikit/`
- Flex cable design: [`rev5.0/flex-cable/rev1.0/`](https://github.com/0xCB-dev/0xCB-1337/tree/main/rev5.0/flex-cable/rev1.0)
- Earlier revs (PDF schematics): rev2.0 and rev3.0 ship `Schematic-1337.pdf`

## Reference upstream firmware

The official RP2040 QMK port (Vial flavour) lives at
<https://github.com/0xCB-dev/keeb-firmware-source/tree/main/vial/1337/v5>. Useful
for cross-referencing pin mappings, OLED I²C address, and RGB init sequence.
