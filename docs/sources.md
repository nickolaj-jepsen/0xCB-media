# Sources

Every external reference cited in `docs/` and `README.md`, grouped by topic.
Dates next to GitHub URLs are not pinned; the linked branches are `main` /
`master` as of May 2026.

## 0xCB-1337 hardware

- KeebSupply product page — <https://keeb.supply/products/0xcb-1337>
- Hardware repo (KiCad, BOM, STEP files) — <https://github.com/0xCB-dev/0xCB-1337>
- Quick-start guide — <https://docs.keeb.supply/0xcb-1337/>
- Quick-start guide source — <https://github.com/0xCB-dev/docs.keeb.supply/blob/main/content/0xcb-1337/guide/index.md>
- rev5.0 BOM — <https://github.com/0xCB-dev/0xCB-1337/blob/main/rev5.0/kikit/prod/bom.csv>
- rev5.0 schematic (KiCad source) — <https://github.com/0xCB-dev/0xCB-1337/blob/main/rev5.0/pcb.kicad_sch>
- 0xCB libs / footprints / symbols — <https://github.com/0xCB-dev/0xCB-libs>

## 0xCB firmware (Vial QMK port for rev5.0)

- Firmware monorepo — <https://github.com/0xCB-dev/keeb-firmware-source>
- 1337 v5 directory — <https://github.com/0xCB-dev/keeb-firmware-source/tree/main/vial/1337/v5>
- info.json (RP2040 pinout, encoder, RGB matrix) — <https://github.com/0xCB-dev/keeb-firmware-source/blob/main/vial/1337/v5/info.json>
- config.h (OLED, I²C, RGB enable pin) — <https://github.com/0xCB-dev/keeb-firmware-source/blob/main/vial/1337/v5/config.h>
- v5.c (RGB enable + startup spiral) — <https://github.com/0xCB-dev/keeb-firmware-source/blob/main/vial/1337/v5/v5.c>

## QMK upstream (older revs only)

- 0xcb/1337 in QMK — <https://github.com/qmk/qmk_firmware/tree/master/keyboards/0xcb/1337>
- QMK on RP2040 — <https://docs.qmk.fm/platformdev_rp2040>

## Flashing & docs.keeb.supply

- RP2040 hardware overview — <https://github.com/0xCB-dev/docs.keeb.supply/blob/main/content/basics/hardware/rp2040/index.md>
- Flashing controllers — <https://github.com/0xCB-dev/docs.keeb.supply/blob/main/content/basics/firmware/flashing/index.md>

## RP2040

- Datasheet — <https://datasheets.raspberrypi.com/rp2040/rp2040-datasheet.pdf>
- Pico SDK — <https://github.com/raspberrypi/pico-sdk>

## probe.rs

- Project site — <https://probe.rs>
- Documentation root — <https://probe.rs/docs/>
- Installation — <https://probe.rs/docs/getting-started/installation/>
- Probe setup (udev rules, supported probes) — <https://probe.rs/docs/getting-started/probe-setup/>
- Built-in target list — <https://probe.rs/targets/>
- VS Code extension — <https://marketplace.visualstudio.com/items?itemName=probe-rs.probe-rs-debugger>

## Debug probes for RP2040

- Raspberry Pi Debug Probe (product) — <https://www.raspberrypi.com/products/debug-probe/>
- Raspberry Pi Debug Probe (docs) — <https://www.raspberrypi.com/documentation/microcontrollers/debug-probe.html>
- `debugprobe` firmware (Pico-as-probe) — <https://github.com/raspberrypi/debugprobe>

## Embedded Rust ecosystem

- Embassy — <https://embassy.dev/> · repo: <https://github.com/embassy-rs/embassy>
- `embassy-rp` (RP2040 HAL) — <https://docs.rs/embassy-rp>
- `embassy-usb` — <https://docs.rs/embassy-usb>
- Embassy RP examples (USB HID, I²C async, PIO encoder) — <https://github.com/embassy-rs/embassy/tree/main/examples/rp/src/bin>
- `ssd1306` driver — <https://docs.rs/ssd1306> · repo: <https://github.com/rust-embedded-community/ssd1306>
- `embedded-graphics` — <https://docs.rs/embedded-graphics>
- `usbd-human-interface-device` (alternative HID stack) — <https://docs.rs/usbd-human-interface-device>
- RMK keyboard framework — <https://docs.rs/rmk> · site: <https://rmk.rs>
- `defmt` — <https://defmt.ferrous-systems.com/> · `defmt-rtt` — <https://docs.rs/defmt-rtt>
- `postcard` (binary serde) — <https://docs.rs/postcard>
- `elf2uf2-rs` — <https://github.com/JoNil/elf2uf2-rs>

## USB HID Consumer Control reference

- HID Usage Tables 1.4 (Consumer Page §15) — <https://usb.org/sites/default/files/hut1_4.pdf>
- "HID Multimedia Dial" walkthrough (descriptor + codes) — <https://hw-by-design.blogspot.com/2018/07/hid-multimedia-dial.html>
- Custom USB HID descriptor: media + keyboard — <https://notes.iopush.net/blog/2016/custom-usb-hid-device-descriptor-media-keyboard/>

## Host-side volume + visualizer

- `serialport` (cross-platform host serial) — <https://docs.rs/serialport>
- `pipewire` (Rust bindings for libpipewire) — <https://docs.rs/pipewire>
- PipeWire native API reference — <https://docs.pipewire.org/>
- `realfft` (real-input FFT, half-spectrum output) — <https://docs.rs/realfft>
- `arc-swap` (lock-free `Arc` swap; the latest-viz-frame slot) — <https://docs.rs/arc-swap>
- `crossbeam-channel` — <https://docs.rs/crossbeam-channel>
- `wpctl` (WirePlumber CLI used to read default-sink volume) — <https://pipewire.pages.freedesktop.org/wireplumber/daemon/wpctl.html>
