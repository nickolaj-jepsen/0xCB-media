#![no_std]
#![no_main]

//! Composite USB device (HID Consumer Control + CDC ACM) for a 0xCB-1337 rev5
//! macropad. Module layout:
//!
//! - `state`: shared statics + types (`DISPLAY_STATE`, channels, enum types,
//!   `apply_host_message`, `apply_encoder_step`).
//! - `display`: OLED rendering, pure functions over a `DisplayState` snapshot.
//! - `led`: WS2812 chain driver task + glow visualizer renderers.
//! - `input`: matrix scan + rotary encoder tasks.
//! - `usb`: composite device runner, HID writer, CDC TX, CDC RX loop, HID
//!   report descriptor.
//!
//! `main` here is just orchestration: peripheral init, USB builder, task
//! spawns, and the joined display loop / CDC connection loop. The display
//! loop and `cdc_rx_loop` aren't spawned tasks — their handle types
//! (`Ssd1306Async`, `CdcReceiver`) carry lifetimes that aren't easily
//! `'static`, so they live as async blocks `join`ed inside `main`.

mod display;
mod input;
mod led;
mod state;
mod storage;
mod usb;

use defmt::info;
use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::i2c::{self, I2c};
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1, I2C1, PIO0, USB};
use embassy_rp::pio::{InterruptHandler as PioInterruptHandler, Pio};
use embassy_rp::pio_programs::ws2812::{Grb, PioWs2812, PioWs2812Program};
use embassy_rp::usb::{Driver as UsbDriver, InterruptHandler as UsbInterruptHandler};
use embassy_time::{Duration, Ticker, Timer};
use embassy_usb::class::cdc_acm::{CdcAcmClass, State as CdcState};
use embassy_usb::class::hid::{
    Config as HidConfig, HidBootProtocol, HidSubclass, HidWriter, State as HidStateT,
};
use embedded_graphics::{
    mono_font::{ascii::FONT_6X10, MonoTextStyleBuilder},
    pixelcolor::BinaryColor,
};
use ssd1306::{prelude::*, I2CDisplayInterface, Ssd1306Async};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

use crate::display::{render_frame, OledVizState};
use crate::input::{encoder_task, matrix_task};
use crate::led::{led_task, NUM_LEDS};
use crate::usb::{
    cdc_rx_loop, cdc_tx_task, hid_writer_task, usb_task, UsbDrv, CONSUMER_REPORT_DESCRIPTOR,
};

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => PioInterruptHandler<PIO0>;
    // RP2040 has a single DMA_IRQ_0 shared by every channel; each
    // `InterruptHandler<T>` only services its own channel's wakers, so we
    // bind one per channel we use async-style (CH0 = WS2812, CH1 = flash).
    DMA_IRQ_0 => embassy_rp::dma::InterruptHandler<DMA_CH0>, embassy_rp::dma::InterruptHandler<DMA_CH1>;
    I2C1_IRQ => i2c::InterruptHandler<I2C1>;
    USBCTRL_IRQ => UsbInterruptHandler<USB>;
});

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    // Claim all 9 matrix inputs up front. Index = row*3+col. Bootmagic and the
    // matrix scanner share these — bootmagic just samples [0][2] (GP9 / encoder
    // click) before the matrix task takes ownership.
    let matrix: [Input<'static>; 9] = [
        Input::new(p.PIN_27, Pull::Up), // [0][0]
        Input::new(p.PIN_29, Pull::Up), // [0][1]
        Input::new(p.PIN_9, Pull::Up),  // [0][2] — encoder click
        Input::new(p.PIN_26, Pull::Up), // [1][0]
        Input::new(p.PIN_28, Pull::Up), // [1][1]
        Input::new(p.PIN_8, Pull::Up),  // [1][2]
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

    // Persisted settings: load before tasks spawn so the first viz/glow
    // frame already reflects the user's last choice. Must run after the
    // bootmagic check above — that's the only recovery path if flash logic
    // ever goes wrong.
    let flash =
        embassy_rp::flash::Flash::<_, embassy_rp::flash::Async, { storage::FLASH_SIZE }>::new(
            p.FLASH, p.DMA_CH1, Irqs,
        );
    let flash = storage::init(flash);
    let settings = storage::load(flash).await;
    state::DISPLAY_STATE.lock(|d| {
        let mut d = d.borrow_mut();
        d.oled_viz = settings.oled_viz;
        d.glow_viz = settings.glow_viz;
        d.hue_mode = settings.hue_mode;
    });

    // RGB load switch on, settle.
    let _rgb_enable = Output::new(p.PIN_14, Level::High);
    Timer::after(Duration::from_millis(20)).await;

    // PIO0 / SM0 → WS2812 chain on GP25 (31 LEDs, GRB).
    let Pio {
        mut common, sm0, ..
    } = Pio::new(p.PIO0, Irqs);
    let ws2812_prg = PioWs2812Program::new(&mut common);
    let ws2812 = PioWs2812::<'_, PIO0, 0, NUM_LEDS, Grb>::new(
        &mut common,
        sm0,
        p.DMA_CH0,
        Irqs,
        p.PIN_25,
        &ws2812_prg,
    );
    spawner.spawn(led_task(ws2812).expect("led_task spawn"));

    // EC11 encoder via plain GPIO interrupts — pin A on GP11, pin B on GP10.
    spawner.spawn(encoder_task(p.PIN_11, p.PIN_10).expect("encoder_task spawn"));

    // I²C1 OLED on (SCL=GP3, SDA=GP2).
    let i2c = I2c::new_async(p.I2C1, p.PIN_3, p.PIN_2, Irqs, i2c::Config::default());
    let interface = I2CDisplayInterface::new(i2c);
    let mut display = Ssd1306Async::new(interface, DisplaySize128x64, DisplayRotation::Rotate0)
        .into_buffered_graphics_mode();
    display.init().await.expect("OLED I2C init");

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
    spawner.spawn(usb_task(usb).expect("usb_task spawn"));
    spawner.spawn(hid_writer_task(hid_writer).expect("hid_writer_task spawn"));
    spawner.spawn(matrix_task(matrix, flash).expect("matrix_task spawn"));
    spawner.spawn(cdc_tx_task(cdc_tx).expect("cdc_tx_task spawn"));

    info!("USB started; awaiting host connection");

    // ─── Display loop and CDC RX run concurrently in main ──────────────────
    let display_fut = async {
        let mut ticker = Ticker::every(Duration::from_millis(33));
        let mut last_ok = true;
        let mut viz_state = OledVizState::new();
        loop {
            ticker.next().await;
            render_frame(&mut display, &text_style, &mut viz_state);
            // I2C glitches (cable wiggle, EMI) shouldn't kill the firmware —
            // log on transition and let the next tick retry.
            match display.flush().await {
                Ok(()) => {
                    if !last_ok {
                        info!("OLED flush recovered");
                        last_ok = true;
                    }
                }
                Err(_) => {
                    if last_ok {
                        defmt::warn!("OLED flush failed; continuing");
                        last_ok = false;
                    }
                }
            }
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
