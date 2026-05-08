# System architecture

How the firmware ended up structured, what the USB device looks like, and
how data flows from host MPRIS to the OLED. This doc reflects the shipping
v1 — for the planning-phase aspirations and dropped ideas, see
[`06-implementation-notes.md`](06-implementation-notes.md).

## Block view

```
                    ┌──────────────────────────────────────────────┐
                    │                  RP2040                       │
                    │                                               │
        GP25  ────► │ PIO0/SM0 + DMA0 ── PioWs2812 ─► 31-LED chain  │
                    │                                               │
        GP14  ────► │ Output (HIGH at boot) ─► TPS2553DBVR EN       │
                    │                                               │
   GP10/GP11  ────► │ GPIO IRQ ── gray-code state machine ──┐       │
                    │                                       │       │
   9 × matrix GPIO ►│ GPIO ── 1 ms debounced scan ──────────┤       │
                    │                                       ▼       │
                    │                              ┌────────────────┐│
                    │                              │ CONSUMER_EVENTS ││ embassy_sync
                    │                              │  (channel)     ││ Channel
                    │                              └────┬───────────┘│
                    │                                   │            │
                    │                              ┌────▼─────────┐  │
                    │                              │ hid_writer   │  │ task
                    │                              │  task        │  │
                    │                              │  (HID class) │  │
                    │                              └──────────────┘  │
                    │                                                │
                    │   ┌─────────────────┐    ┌──────────────────┐  │
                    │   │ DEVICE_TX_EVENTS│◄───│ matrix_task on   │  │
                    │   │ (channel)       │    │ encoder click    │  │
                    │   └────┬────────────┘    └──────────────────┘  │
                    │        │                                       │
                    │   ┌────▼─────────┐                             │
                    │   │ cdc_tx_task  │                             │
                    │   └─────────────-┘                             │
                    │                                                │
                    │   ┌──────────────┐  ┌─────────────────────┐    │
                    │   │ usb_task     │  │ main: join(         │    │
                    │   │ (UsbDevice)  │  │   display_loop @30Hz│    │
                    │   └──────────────┘  │   cdc_rx_loop       │    │
                    │                     │ )                   │    │
                    │                     └──┬─────────────┬────┘    │
                    │                        │             │         │
                    │   GP2/GP3 (I²C1) ──────►SSD1306      │         │
                    │                                      │         │
                    │                              ┌───────▼──────┐  │
                    │                              │ DISPLAY_STATE │ │
                    │                              │  (Mutex<     │  │
                    │                              │   RefCell>)  │  │
                    │                              └──────────────┘  │
                    └──────────────────────────────────────────────┘
                                               │
                                            USB-C
                                               ▼
                                       ┌──────────────┐
                                       │ Linux host   │
                                       │ (mpris,      │
                                       │  wpctl)      │
                                       └──────────────┘
```

## Tasks (Embassy `#[embassy_executor::task]`)

| Task | Where | Cadence | Responsibility |
|------|-------|---------|----------------|
| `usb_task` | spawned | event-driven | Run the embassy-usb device loop. |
| `hid_writer_task` | spawned | on event | Drain `CONSUMER_EVENTS`, emit press+release HID Consumer reports. |
| `cdc_tx_task` | spawned | on event | Drain `DEVICE_TX_EVENTS`, COBS-encode + send `DeviceToHost` packets. |
| `matrix_task` | spawned | 1 kHz tick | 5-tick integrator debounce on 9 inputs. On press: push HID + LED + (encoder-only) DEVICE_TX events. |
| `encoder_task` | spawned | event-driven | GPIO-IRQ gray-code decode of GP11/GP10 → push `VolumeUp`/`VolumeDown` to `CONSUMER_EVENTS`. |
| `led_task` | spawned | 60 Hz | Boot spiral → idle (per-key flashes from `LED_EVENTS`, underglow off). |
| `display_loop` | inline (`main`, joined) | 30 Hz | Snapshot `DISPLAY_STATE`, render, `display.flush().await`. |
| `cdc_rx_loop` | inline (`main`, joined) | event-driven | Read CDC packets, COBS-decode `HostToDevice`, mutate `DISPLAY_STATE`. |

`display_loop` and `cdc_rx_loop` aren't tasks — they're async blocks
joined inside `main`. Reason: the `Ssd1306Async<...>` and `CdcReceiver<...>`
types have lifetimes tied to local buffers that aren't easily made
`'static` for spawn. Joining them in `main` is shorter and clearer.

All inter-task communication is `embassy_sync::channel::Channel` (with
`CriticalSectionRawMutex`) or
`blocking_mutex::Mutex<CriticalSectionRawMutex, RefCell<DisplayState>>`
for the shared display snapshot.

## USB device descriptor

VID `0xCB00`, PID `0x1337` (matches the original 0xCB-1337 device identity):

```
Device:
  VID  = 0xCB00
  PID  = 0x1337
  Manufacturer = "0xCB"
  Product      = "1337-media"
  bDeviceClass         = 0xEF (Misc)
  bDeviceSubClass      = 0x02 (Common)
  bDeviceProtocol      = 0x01 (IAD)

Configuration: 1
  IAD → CDC (interfaces 0+1)
  IAD → HID Consumer (interface 2)

  Interface 0: CDC Communications (notification EP, INT IN)
  Interface 1: CDC Data (BULK IN/OUT, 64 B max packet)
  Interface 2: HID, Boot=No, Report Descriptor below
```

`config.composite_with_iads = true` is required so `embassy-usb` emits the
Interface Association Descriptors that the host needs to group the two
classes correctly.

### HID report descriptor (Consumer Control)

26 bytes, hand-written:

```
Usage Page (Consumer Devices)         05 0C
Usage      (Consumer Control)         09 01
Collection (Application)              A1 01
    Report ID (1)                     85 01
    Logical Min (0)                   15 00
    Logical Max (0xFFFF)              26 FF FF
    Usage Min (0x0000)                1A 00 00
    Usage Max (0xFFFF)                2A FF FF
    Report Size (16)                  75 10
    Report Count (1)                  95 01
    Input (Data, Array, Absolute)     81 00
End Collection                        C0
```

Each report on the wire is 3 bytes: `[report_id=1, usage_lsb, usage_msb]`.
"Release" is `[1, 0, 0]`.

### Consumer usage codes we send

| Function | Code (16-bit, little-endian on the wire) |
|----------|------------------------------------------|
| Volume Up | `0x00E9` |
| Volume Down | `0x00EA` |
| Mute | `0x00E2` |
| Play/Pause | `0x00CD` |
| Stop | `0x00B7` |
| Next track | `0x00B5` |
| Previous track | `0x00B6` |

Stable across Windows, macOS, Linux, ChromeOS, Android.

### Default keymap

3×3 direct matrix, indices match `firmware/src/main.rs`'s `matrix` array
(row-major):

| Pos | GPIO | Default action | Per-key LED |
|-----|------|----------------|-------------|
| (0,0) | GP27 | Prev Track | LED 1 |
| (0,1) | GP29 | Play/Pause | LED 0 |
| (0,2) | GP9  | Mute (encoder click) | (none — encoder) |
| (1,0) | GP26 | Next Track | LED 2 |
| (1,1) | GP28 | Stop | LED 3 |
| (1,2) | GP8  | (unbound) | LED 4 |
| (2,0) | GP18 | (unbound) | LED 7 |
| (2,1) | GP17 | (unbound) | LED 6 |
| (2,2) | GP12 | (unbound) | LED 5 |

Encoder rotation: CW → Volume Up, CCW → Volume Down.

Edit `KEYMAP` in `firmware/src/main.rs` to rebind. No runtime config —
takes a re-flash. Vial integration was considered and dropped (overkill
for 9 inputs).

## OLED layout (128 × 64, FONT_6X10)

```
┌────────────────────────────────────────────────────────────┐ y=0
│ > Bohemian Rhapsody                                        │
│   Queen                                                    │
│                                                            │
│ ┌────────────────────────────────────────────────┐ 47%     │
│ │████████████████████░░░░░░░░░░░░░░░░░░░░░░░░░░│         │
│ └────────────────────────────────────────────────┘         │
└────────────────────────────────────────────────────────────┘ y=63
```

- **Status glyph** top-left: `>` (playing) / `||` (paused) / `-` (no track).
  ASCII because FONT_6X10 doesn't have unicode play/pause/stop glyphs.
- **Title** row 1, **artist** row 2 — both truncate at the right edge
  (no scrolling in v1).
- **Volume bar** is a 102×9 rectangle outline with a 0..100 px filled
  inner. `MUTE` text replaces the bar when `muted = true`.
- "Disconnected" UI fills the centre of the panel when no host frame has
  arrived in the last 5 s.

`embedded-graphics` + `BufferedGraphicsMode` makes the whole render
~50 LOC.

## Boot sequence

1. `embassy_rp::init()` → clocks at 125 MHz.
2. Claim 9 matrix inputs as `Input<'static>`, all with internal pull-ups.
3. **Bootmagic**: 20 ms settle, then if `matrix[2]` (encoder click, GP9)
   reads low → call `embassy_rp::rom_data::reset_to_usb_boot(0, 0)` and
   loop forever. Replicates the official 0xCB Vial firmware behaviour.
4. Drive GP14 (RGB power enable) HIGH, wait 20 ms (mirrors `v5.c`).
5. Set up PIO0/SM0/DMA0 for WS2812 → spawn `led_task`.
6. Spawn `encoder_task` on GP10/GP11.
7. Init I²C1 + SSD1306, run `display.init().await`.
8. Build the composite USB device; split CDC into Sender/Receiver.
9. Spawn `usb_task`, `hid_writer_task`, `matrix_task`, `cdc_tx_task`.
10. Enter `join(display_loop, cdc_rx_loop)` in main — runs forever.

## Failure modes (and what to check)

| Symptom | Likely cause |
|---------|--------------|
| Underglow doesn't blink at boot | LED data never sent — `led_task` panicked or PIO setup failed. |
| OLED dark on boot | I²C1 wiring issue, GP14 still low (LEDs steal display power), or SSD1306 init blocked on bus that's pulled low. |
| Encoder reads doubled detents / random direction | Contact bounce. The gray-code state machine + 150 µs settle handles this; if it regresses, raise the settle time. |
| Volume keys do nothing on host | Wrong report ID, or HID descriptor doesn't match what we're writing. `lsusb -v -d cb00:1337` should show `Usage Page (Consumer)` in the parsed descriptor. |
| Keyboard sends a continuous stream of one media key | Forgot the release report after the press. |
| Daemon's `NowPlaying` never appears on OLED | Another process (picocom, screen) is holding the CDC port; daemon's open will fail or its writes will go elsewhere. |
| OLED says "Disconnected" while daemon is running | Daemon is talking to the wrong device, or the firmware-side CDC RX loop got starved. Run with `RUST_LOG=debug` and confirm `serial connected` and per-message logs. |

## Resolved planning-phase questions

(For history; no action needed.)

1. **SWDIO/SWCLK breakout on rev5.0?** — Not needed. UF2 + bootmagic is
   sufficient for development. Probe path stays optional.
2. **Vial vs static keymap?** — Static, compiled in.
3. **Single workspace vs separate repos?** — Single workspace; shared
   `proto` crate is the value.
4. **`embassy-rp::pio_programs::ws2812` vs custom?** — Built-in, via
   `PioWs2812<_, _, _, 31, Grb>`. Works for both SK6812 and WS2812B since
   their timing is identical.
