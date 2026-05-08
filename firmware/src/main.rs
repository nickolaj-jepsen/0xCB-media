#![no_std]
#![no_main]

//! M7 milestone — the OLED now renders host-driven now-playing data instead
//! of a static milestone label, and the CDC ACM endpoint accepts COBS-framed
//! `proto::HostToDevice` messages from the PC daemon.
//!
//! The composite USB device is unchanged from M3 (CDC + HID), and matrix /
//! encoder / LED / HID-writer tasks are all carried over from M5 / M6.
//! What changed:
//!
//!   * `DISPLAY_STATE` — shared state mutated by CDC, read by the render loop.
//!   * `cdc_rx_loop` — replaces the M2 echo. Accumulates bytes until a 0x00
//!     delimiter and decodes the buffer as a postcard-COBS frame.
//!   * `display_loop` — runs at ~30 Hz: snapshots state, paints status glyph,
//!     title, artist, and a volume bar (or a "Disconnected" message if the
//!     host has been silent for 5 s+).
//!
//! Both loops run inside `main` via `embassy_futures::join` so they share
//! ownership of the `CdcAcmClass` and the OLED handle without spawn-lifetime
//! gymnastics.

use core::cell::RefCell;
use core::fmt::Write as _;

use defmt::{info, panic};
use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_futures::select::select;
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::i2c::{self, I2c};
use embassy_rp::peripherals::{DMA_CH0, I2C1, PIN_10, PIN_11, PIO0, USB};
use embassy_rp::pio::{InterruptHandler as PioInterruptHandler, Pio};
use embassy_rp::pio_programs::ws2812::{Grb, PioWs2812, PioWs2812Program};
use embassy_rp::usb::{Driver as UsbDriver, Instance as UsbInstance, InterruptHandler as UsbInterruptHandler};
use embassy_rp::Peri;
use embassy_sync::blocking_mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Ticker, Timer};
use embassy_usb::class::cdc_acm::{CdcAcmClass, Receiver as CdcReceiver, Sender as CdcSender, State as CdcState};
use embassy_usb::class::hid::{Config as HidConfig, HidBootProtocol, HidSubclass, HidWriter, State as HidStateT};
use embassy_usb::driver::EndpointError;
use embassy_usb::UsbDevice;
use embedded_graphics::{
    mono_font::{ascii::FONT_6X10, MonoTextStyleBuilder},
    pixelcolor::BinaryColor,
    prelude::*,
    primitives::{PrimitiveStyle, Rectangle},
    text::{Baseline, Text},
};
use proto::{DeviceToHost, HostToDevice};
use smart_leds::RGB8;
use ssd1306::{prelude::*, I2CDisplayInterface, Ssd1306Async};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

const NUM_LEDS: usize = 31;

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => PioInterruptHandler<PIO0>;
    DMA_IRQ_0 => embassy_rp::dma::InterruptHandler<DMA_CH0>;
    I2C1_IRQ => i2c::InterruptHandler<I2C1>;
    USBCTRL_IRQ => UsbInterruptHandler<USB>;
});

type UsbDrv = UsbDriver<'static, USB>;

/// USB HID Consumer Control usage codes (HID Usage Tables 1.4 §15).
/// Stable across Windows / macOS / Linux / Android / ChromeOS.
#[derive(Copy, Clone, Debug)]
#[repr(u16)]
pub enum ConsumerKey {
    Mute = 0x00E2,
    VolumeUp = 0x00E9,
    VolumeDown = 0x00EA,
    PlayPause = 0x00CD,
    Stop = 0x00B7,
    NextTrack = 0x00B5,
    PrevTrack = 0x00B6,
}

/// Channel of consumer-control events drained by `hid_writer_task`. Static
/// so any task in the firmware can `.send()` without owning a reference.
static CONSUMER_EVENTS: Channel<CriticalSectionRawMutex, ConsumerKey, 8> = Channel::new();

/// LED render commands fired by the matrix task and consumed by `led_task`.
#[derive(Copy, Clone, Debug)]
pub enum LedCommand {
    /// Flash the per-key LED at this index (0..=7).
    KeyPress { led: u8 },
}

static LED_EVENTS: Channel<CriticalSectionRawMutex, LedCommand, 16> = Channel::new();

/// Outgoing device-to-host events (currently just the encoder-click notice).
/// Drained by `cdc_tx_task` and written as COBS-framed postcard packets over
/// CDC ACM.
static DEVICE_TX_EVENTS: Channel<CriticalSectionRawMutex, DeviceToHost, 8> = Channel::new();

/// Maps the 3×3 matrix slot (row-major) to the index of the per-key
/// SK6812MINI-E LED on the WS2812 chain. The chain order matches the
/// upstream Vial firmware's RGB matrix layout (see
/// `0xCB-dev/keeb-firmware-source/vial/1337/v5/info.json`); the encoder
/// position has no per-key LED, hence the `None`.
const MATRIX_TO_LED: [Option<u8>; 9] = [
    Some(1), Some(0), None,    // row 0 (col 2 = encoder click, no LED)
    Some(2), Some(3), Some(4), // row 1
    Some(7), Some(6), Some(5), // row 2
];

/// Hand-rolled HID report descriptor: a single application collection on
/// the Consumer page. Each report is `[report_id=1, usage_lsb, usage_msb]`.
/// Send `[1, 0, 0]` to mark "release". 26 bytes total.
#[rustfmt::skip]
const CONSUMER_REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x0C,        // Usage Page (Consumer Devices)
    0x09, 0x01,        // Usage      (Consumer Control)
    0xA1, 0x01,        // Collection (Application)
    0x85, 0x01,        //   Report ID (1)
    0x15, 0x00,        //   Logical Min (0)
    0x26, 0xFF, 0xFF,  //   Logical Max (0xFFFF)
    0x1A, 0x00, 0x00,  //   Usage Min (0)
    0x2A, 0xFF, 0xFF,  //   Usage Max (0xFFFF)
    0x75, 0x10,        //   Report Size (16 bits)
    0x95, 0x01,        //   Report Count (1 usage per report)
    0x81, 0x00,        //   Input (Data, Array, Absolute)
    0xC0,              // End Collection
];

// ─── Display state ─────────────────────────────────────────────────────────

#[derive(Clone)]
struct TrackInfo {
    title: heapless::String<64>,
    artist: heapless::String<32>,
    is_playing: bool,
}

#[derive(Clone, Copy)]
struct VolumeInfo {
    level: u8, // 0..=100
    muted: bool,
}

#[derive(Clone)]
struct DisplayState {
    track: Option<TrackInfo>,
    volume: VolumeInfo,
    /// Last time we received any frame from the host. Used to flip the OLED
    /// to "Disconnected" when the daemon dies or the cable is unplugged.
    last_message: Instant,
}

impl DisplayState {
    const fn new() -> Self {
        Self {
            track: None,
            volume: VolumeInfo { level: 0, muted: false },
            last_message: Instant::from_ticks(0),
        }
    }

    fn connected(&self) -> bool {
        // Treat the whole device as "never connected" until at least one frame
        // arrives — `last_message` starts at tick 0.
        self.last_message != Instant::from_ticks(0)
            && self.last_message.elapsed() < Duration::from_secs(5)
    }
}

static DISPLAY_STATE: blocking_mutex::Mutex<CriticalSectionRawMutex, RefCell<DisplayState>> =
    blocking_mutex::Mutex::new(RefCell::new(DisplayState::new()));

fn apply_host_message(msg: HostToDevice) {
    DISPLAY_STATE.lock(|state| {
        let mut s = state.borrow_mut();
        s.last_message = Instant::now();
        match msg {
            HostToDevice::NowPlaying { title, artist, is_playing } => {
                s.track = Some(TrackInfo { title, artist, is_playing });
            }
            HostToDevice::Volume { level, muted } => {
                s.volume = VolumeInfo { level, muted };
            }
            HostToDevice::Clear => {
                s.track = None;
            }
            HostToDevice::Ping => { /* timestamp already updated */ }
        }
    });
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    // Claim all 9 matrix inputs up front. Index = row*3+col. Bootmagic and the
    // matrix scanner share these — bootmagic just samples [0][2] (GP9 / encoder
    // click) before the matrix task takes ownership.
    let matrix: [Input<'static>; 9] = [
        Input::new(p.PIN_27, Pull::Up), // [0][0]
        Input::new(p.PIN_29, Pull::Up), // [0][1]
        Input::new(p.PIN_9,  Pull::Up), // [0][2] — encoder click
        Input::new(p.PIN_26, Pull::Up), // [1][0]
        Input::new(p.PIN_28, Pull::Up), // [1][1]
        Input::new(p.PIN_8,  Pull::Up), // [1][2]
        Input::new(p.PIN_18, Pull::Up), // [2][0]
        Input::new(p.PIN_17, Pull::Up), // [2][1]
        Input::new(p.PIN_12, Pull::Up), // [2][2]
    ];

    // Bootmagic: hold the encoder click at boot → drop into RP2040 ROM USB
    // bootloader. Replicates the official 0xCB Vial firmware behaviour.
    Timer::after(Duration::from_millis(20)).await;
    if matrix[2].is_low() {
        info!("bootmagic: encoder click held, entering USB bootloader");
        embassy_rp::rom_data::reset_to_usb_boot(0, 0);
        #[allow(clippy::empty_loop)]
        loop {}
    }

    info!("0xCB-media firmware booted (M7)");

    // RGB load switch on, settle.
    let _rgb_enable = Output::new(p.PIN_14, Level::High);
    Timer::after(Duration::from_millis(20)).await;

    // PIO0 / SM0 → WS2812 chain on GP25 (31 LEDs, GRB).
    let Pio { mut common, sm0, .. } = Pio::new(p.PIO0, Irqs);
    let ws2812_prg = PioWs2812Program::new(&mut common);
    let ws2812 = PioWs2812::<'_, PIO0, 0, NUM_LEDS, Grb>::new(
        &mut common,
        sm0,
        p.DMA_CH0,
        Irqs,
        p.PIN_25,
        &ws2812_prg,
    );
    spawner.spawn(led_task(ws2812).unwrap());

    // EC11 encoder via plain GPIO interrupts — pin A on GP11, pin B on GP10.
    spawner.spawn(encoder_task(p.PIN_11, p.PIN_10).unwrap());

    // I²C1 OLED on (SCL=GP3, SDA=GP2).
    let i2c = I2c::new_async(p.I2C1, p.PIN_3, p.PIN_2, Irqs, i2c::Config::default());
    let interface = I2CDisplayInterface::new(i2c);
    let mut display = Ssd1306Async::new(interface, DisplaySize128x64, DisplayRotation::Rotate0)
        .into_buffered_graphics_mode();
    display.init().await.unwrap();

    let text_style = MonoTextStyleBuilder::new()
        .font(&FONT_6X10)
        .text_color(BinaryColor::On)
        .build();

    // ─── USB: composite CDC ACM + HID Consumer Control ──────────────────────
    let usb_driver = UsbDriver::new(p.USB, Irqs);

    let mut config = embassy_usb::Config::new(0xCB00, 0x1337);
    config.manufacturer = Some("0xCB");
    config.product = Some("1337-media");
    config.serial_number = Some("0xCB-media-0");
    config.max_power = 100;
    config.max_packet_size_0 = 64;
    config.composite_with_iads = true;
    config.device_class = 0xEF;
    config.device_sub_class = 0x02;
    config.device_protocol = 0x01;

    static CONFIG_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static CONTROL_BUF: StaticCell<[u8; 64]> = StaticCell::new();
    let mut builder = embassy_usb::Builder::new(
        usb_driver,
        config,
        CONFIG_DESC.init([0; 256]),
        BOS_DESC.init([0; 256]),
        &mut [],
        CONTROL_BUF.init([0; 64]),
    );

    static CDC_STATE: StaticCell<CdcState> = StaticCell::new();
    let cdc_class = CdcAcmClass::new(&mut builder, CDC_STATE.init(CdcState::new()), 64);
    let (cdc_tx, mut cdc_rx) = cdc_class.split();

    static HID_STATE: StaticCell<HidStateT> = StaticCell::new();
    let hid_writer = HidWriter::<UsbDrv, 8>::new(
        &mut builder,
        HID_STATE.init(HidStateT::new()),
        HidConfig {
            report_descriptor: CONSUMER_REPORT_DESCRIPTOR,
            request_handler: None,
            poll_ms: 10,
            max_packet_size: 8,
            hid_subclass: HidSubclass::No,
            hid_boot_protocol: HidBootProtocol::None,
        },
    );

    let usb = builder.build();
    spawner.spawn(usb_task(usb).unwrap());
    spawner.spawn(hid_writer_task(hid_writer).unwrap());
    spawner.spawn(matrix_task(matrix).unwrap());
    spawner.spawn(cdc_tx_task(cdc_tx).unwrap());

    info!("USB started; awaiting host connection");

    // ─── Display loop and CDC RX run concurrently in main ──────────────────
    let display_fut = async {
        let mut ticker = Ticker::every(Duration::from_millis(33));
        loop {
            ticker.next().await;
            render_frame(&mut display, &text_style);
            display.flush().await.unwrap();
        }
    };

    let cdc_fut = async {
        loop {
            cdc_rx.wait_connection().await;
            info!("CDC connected");
            let _ = cdc_rx_loop(&mut cdc_rx).await;
            info!("CDC disconnected");
        }
    };

    join(display_fut, cdc_fut).await;
}

// ─── Display render ────────────────────────────────────────────────────────

fn render_frame<D>(
    display: &mut D,
    text_style: &embedded_graphics::mono_font::MonoTextStyle<'_, BinaryColor>,
) where
    D: DrawTarget<Color = BinaryColor>,
{
    let snapshot = DISPLAY_STATE.lock(|state| state.borrow().clone());
    let _ = display.clear(BinaryColor::Off);

    if !snapshot.connected() {
        let _ = Text::with_baseline(
            "Disconnected",
            Point::new(28, 28),
            *text_style,
            Baseline::Top,
        )
        .draw(display);
        let _ = Text::with_baseline(
            "(no host daemon)",
            Point::new(16, 42),
            *text_style,
            Baseline::Top,
        )
        .draw(display);
        return;
    }

    // Status glyph (top-left, 12px tall slot).
    let glyph = match snapshot.track.as_ref() {
        Some(t) if t.is_playing => ">",
        Some(_) => "||",
        None => "-",
    };
    let _ = Text::with_baseline(glyph, Point::new(2, 0), *text_style, Baseline::Top).draw(display);

    // Title + artist, or a placeholder when no track is set.
    if let Some(t) = snapshot.track.as_ref() {
        let _ = Text::with_baseline(&t.title, Point::new(20, 0), *text_style, Baseline::Top).draw(display);
        let _ = Text::with_baseline(&t.artist, Point::new(20, 16), *text_style, Baseline::Top).draw(display);
    } else {
        let _ = Text::with_baseline(
            "(no track playing)",
            Point::new(8, 8),
            *text_style,
            Baseline::Top,
        )
        .draw(display);
    }

    // Volume bar — bottom of the panel. Outline + filled inner rect.
    let outline = Rectangle::new(Point::new(2, 50), Size::new(102, 9))
        .into_styled(PrimitiveStyle::with_stroke(BinaryColor::On, 1));
    let _ = outline.draw(display);

    if snapshot.volume.muted {
        let _ = Text::with_baseline("MUTE", Point::new(38, 36), *text_style, Baseline::Top).draw(display);
    } else {
        let level = snapshot.volume.level.min(100) as u32;
        if level > 0 {
            let inner = Rectangle::new(Point::new(3, 51), Size::new(level, 7))
                .into_styled(PrimitiveStyle::with_fill(BinaryColor::On));
            let _ = inner.draw(display);
        }
        // "Vol nn%" right-aligned-ish above the bar.
        let mut buf: heapless::String<8> = heapless::String::new();
        let _ = write!(&mut buf, "{}%", snapshot.volume.level);
        let _ = Text::with_baseline(&buf, Point::new(108, 50), *text_style, Baseline::Top).draw(display);
    }
}

// ─── CDC receive loop ──────────────────────────────────────────────────────

async fn cdc_rx_loop<'d, T: UsbInstance + 'd>(
    class: &mut CdcReceiver<'d, UsbDriver<'d, T>>,
) -> Result<(), Disconnected> {
    let mut packet_buf = [0u8; 64];
    let mut frame_buf = [0u8; proto::MAX_FRAME_LEN];
    let mut frame_pos: usize = 0;

    loop {
        let n = class.read_packet(&mut packet_buf).await?;
        for &b in &packet_buf[..n] {
            if b == 0 {
                if frame_pos > 0 {
                    match postcard::from_bytes_cobs::<HostToDevice>(&mut frame_buf[..frame_pos]) {
                        Ok(msg) => {
                            info!("rx host msg, {} bytes", frame_pos);
                            apply_host_message(msg);
                        }
                        Err(_) => info!("dropped malformed frame ({} bytes)", frame_pos),
                    }
                    frame_pos = 0;
                }
            } else if frame_pos < frame_buf.len() {
                frame_buf[frame_pos] = b;
                frame_pos += 1;
            } else {
                // Buffer overflow — sender is sending more than MAX_FRAME_LEN
                // before a delimiter. Discard and resync.
                frame_pos = 0;
            }
        }
    }
}

// ─── Spawned tasks ─────────────────────────────────────────────────────────

#[embassy_executor::task]
async fn usb_task(mut usb: UsbDevice<'static, UsbDrv>) -> ! {
    usb.run().await
}

/// LED chain controller. On boot, plays a one-shot underglow spiral mirroring
/// the upstream Vial `v5.c` startup effect, then idles black and flashes the
/// per-key LED briefly when the matrix task reports a press.
#[embassy_executor::task]
async fn led_task(mut ws2812: PioWs2812<'static, PIO0, 0, NUM_LEDS, Grb>) {
    const PER_KEY_END: usize = 8;
    const UNDERGLOW_START: usize = 8;
    const UNDERGLOW_END: usize = NUM_LEDS - 1;
    const UNDERGLOW_COUNT: usize = UNDERGLOW_END - UNDERGLOW_START + 1;
    const SPIRAL_PIVOT: usize = 13;
    const SPIRAL_STEP_MS: u64 = 85;
    // Per-LED-chip accent colours. The underglow WS2812B and the in-switch
    // SK6812MINI-E render the same RGB code differently — SK6812 has a
    // stronger green bias and looks yellow at the same G:R ratio, so we tune
    // the constants by eye against the project accent `#CF6A4C` (warm orange).
    const ACCENT_UNDERGLOW: RGB8 = RGB8 { r: 112, g: 32, b: 0 };
    const ACCENT_PERKEY: RGB8 = RGB8 { r: 96, g: 12, b: 0 };
    const PRESS_DECAY: u8 = 18;
    const TICK_MS: u64 = 16; // ~60 Hz idle/press render

    let mut frame = [RGB8::default(); NUM_LEDS];

    for step in 0..UNDERGLOW_COUNT {
        let offset = (SPIRAL_PIVOT - UNDERGLOW_START + step) % UNDERGLOW_COUNT;
        let led = UNDERGLOW_START + offset;
        frame[led] = ACCENT_UNDERGLOW;
        ws2812.write(&frame).await;
        Timer::after(Duration::from_millis(SPIRAL_STEP_MS)).await;
    }
    Timer::after(Duration::from_millis(400)).await;
    for px in frame.iter_mut() {
        *px = RGB8::default();
    }
    ws2812.write(&frame).await;

    let mut press_brightness: [u8; 8] = [0; 8];
    let mut ticker = Ticker::every(Duration::from_millis(TICK_MS));

    loop {
        ticker.next().await;

        while let Ok(LedCommand::KeyPress { led }) = LED_EVENTS.try_receive() {
            if (led as usize) < PER_KEY_END {
                press_brightness[led as usize] = 255;
            }
        }

        for i in 0..PER_KEY_END {
            let b = press_brightness[i] as u16;
            frame[i] = RGB8 {
                r: ((ACCENT_PERKEY.r as u16 * b) / 255) as u8,
                g: ((ACCENT_PERKEY.g as u16 * b) / 255) as u8,
                b: ((ACCENT_PERKEY.b as u16 * b) / 255) as u8,
            };
            press_brightness[i] = press_brightness[i].saturating_sub(PRESS_DECAY);
        }
        for i in UNDERGLOW_START..NUM_LEDS {
            frame[i] = RGB8::default();
        }

        ws2812.write(&frame).await;
    }
}

/// CDC ACM TX side. Drains `DEVICE_TX_EVENTS`, postcard+COBS encodes each
/// message, writes one packet to the host. Lives in its own task so the main
/// loop can keep the RX side responsive.
#[embassy_executor::task]
async fn cdc_tx_task(mut tx: CdcSender<'static, UsbDrv>) {
    let mut buf = [0u8; proto::MAX_FRAME_LEN];
    loop {
        tx.wait_connection().await;
        loop {
            let msg = DEVICE_TX_EVENTS.receive().await;
            let frame = match postcard::to_slice_cobs(&msg, &mut buf) {
                Ok(f) => f,
                Err(_) => continue,
            };
            if tx.write_packet(frame).await.is_err() {
                break; // host gone — wait for reconnect
            }
        }
    }
}

/// Drain the shared consumer-event channel and emit press+release HID reports.
#[embassy_executor::task]
async fn hid_writer_task(mut writer: HidWriter<'static, UsbDrv, 8>) {
    loop {
        let key = CONSUMER_EVENTS.receive().await;
        let usage = key as u16;
        let press = [0x01, (usage & 0xFF) as u8, ((usage >> 8) & 0xFF) as u8];
        let release = [0x01, 0x00, 0x00];

        if let Err(e) = writer.write(&press).await {
            defmt::warn!("hid press write failed: {:?}", e);
        }
        Timer::after(Duration::from_millis(5)).await;
        if let Err(e) = writer.write(&release).await {
            defmt::warn!("hid release write failed: {:?}", e);
        }
    }
}

/// 3×3 direct key matrix layout. Order matches the `matrix` array in `main`
/// (row-major). `None` = unbound key.
const KEYMAP: [Option<ConsumerKey>; 9] = [
    Some(ConsumerKey::PrevTrack),
    Some(ConsumerKey::PlayPause),
    Some(ConsumerKey::Mute),
    Some(ConsumerKey::NextTrack),
    Some(ConsumerKey::Stop),
    None,
    None,
    None,
    None,
];

#[embassy_executor::task]
async fn matrix_task(matrix: [Input<'static>; 9]) {
    const DEBOUNCE_TICKS: u8 = 5;
    let mut counters: [u8; 9] = [0; 9];
    let mut pressed: [bool; 9] = [false; 9];

    loop {
        Timer::after(Duration::from_millis(1)).await;

        for i in 0..9 {
            let low = matrix[i].is_low();
            if low && counters[i] < DEBOUNCE_TICKS {
                counters[i] += 1;
            } else if !low && counters[i] > 0 {
                counters[i] -= 1;
            }

            let was_pressed = pressed[i];
            if counters[i] == DEBOUNCE_TICKS {
                pressed[i] = true;
            } else if counters[i] == 0 {
                pressed[i] = false;
            }

            if pressed[i] && !was_pressed {
                if let Some(key) = KEYMAP[i] {
                    let _ = CONSUMER_EVENTS.try_send(key);
                }
                if let Some(led) = MATRIX_TO_LED[i] {
                    let _ = LED_EVENTS.try_send(LedCommand::KeyPress { led });
                }
                // Encoder click (matrix [0,2]) also surfaces to the host so
                // the daemon can do something custom with it (e.g. switch
                // active MPRIS player) on top of the HID Mute it already sent.
                if i == 2 {
                    let _ = DEVICE_TX_EVENTS.try_send(DeviceToHost::EncoderClick);
                }
            }
        }
    }
}

/// Quadrature transition lookup. Index = `(prev << 2) | curr`, where each
/// nibble holds `(A << 1) | B`. Returns +1 / -1 for valid transitions, 0 for
/// invalid or no-change (which is what contact bounce looks like).
#[rustfmt::skip]
const QDEC: [i8; 16] = [
     0, -1,  1,  0,
     1,  0,  0, -1,
    -1,  0,  0,  1,
     0,  1, -1,  0,
];

#[embassy_executor::task]
async fn encoder_task(pin_a: Peri<'static, PIN_11>, pin_b: Peri<'static, PIN_10>) {
    let mut a = embassy_rp::gpio::Input::new(pin_a, embassy_rp::gpio::Pull::Up);
    let mut b = embassy_rp::gpio::Input::new(pin_b, embassy_rp::gpio::Pull::Up);

    let read = |a: &embassy_rp::gpio::Input, b: &embassy_rp::gpio::Input| -> u8 {
        ((a.is_high() as u8) << 1) | (b.is_high() as u8)
    };

    let mut prev = read(&a, &b);
    let mut accumulator: i8 = 0;

    loop {
        select(a.wait_for_any_edge(), b.wait_for_any_edge()).await;
        Timer::after(Duration::from_micros(150)).await;

        let curr = read(&a, &b);
        if curr == prev {
            continue;
        }
        accumulator += QDEC[((prev << 2) | curr) as usize];
        prev = curr;

        if accumulator >= 4 {
            accumulator = 0;
            CONSUMER_EVENTS.send(ConsumerKey::VolumeUp).await;
        } else if accumulator <= -4 {
            accumulator = 0;
            CONSUMER_EVENTS.send(ConsumerKey::VolumeDown).await;
        }
    }
}

struct Disconnected;

impl From<EndpointError> for Disconnected {
    fn from(val: EndpointError) -> Self {
        match val {
            EndpointError::BufferOverflow => panic!("CDC buffer overflow"),
            EndpointError::Disabled => Disconnected,
        }
    }
}
