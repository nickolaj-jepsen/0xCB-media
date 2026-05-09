//! Matrix scan + rotary encoder tasks. Both push into shared channels in
//! `state` rather than holding direct references to the consumers.

use defmt::info;
use embassy_futures::select::select;
use embassy_rp::gpio::Input;
use embassy_rp::peripherals::{PIN_10, PIN_11};
use embassy_rp::Peri;
use embassy_time::{Duration, Timer};

use proto::DeviceToHost;

use crate::state::{
    apply_encoder_step, ConsumerKey, LedCommand, MenuView, Selector, CONSUMER_EVENTS,
    DEVICE_TX_EVENTS, DISPLAY_STATE, LED_EVENTS,
};

/// Maps the 3×3 matrix slot (row-major) to the index of the per-key
/// SK6812MINI-E LED on the WS2812 chain. The chain order matches the
/// upstream Vial firmware's RGB matrix layout (see
/// `0xCB-dev/keeb-firmware-source/vial/1337/v5/info.json`); the encoder
/// position has no per-key LED, hence the `None`.
const MATRIX_TO_LED: [Option<u8>; 9] = [
    Some(1),
    Some(0),
    None, // row 0 (col 2 = encoder click, no LED)
    Some(2),
    Some(3),
    Some(4), // row 1
    Some(7),
    Some(6),
    Some(5), // row 2
];

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
pub async fn matrix_task(matrix: [Input<'static>; 9]) {
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
                let menu_open = DISPLAY_STATE.lock(|s| s.borrow().menu_open());

                // Suppress media-key HID (PrevTrack/PlayPause/NextTrack/Stop)
                // while the menu is open so navigation can't fire them.
                // Encoder click (i==2 → Mute) is intentionally exempt — Mute
                // stays available from inside the menu.
                if !menu_open || i == 2 {
                    if let Some(key) = KEYMAP[i] {
                        let _ = CONSUMER_EVENTS.try_send(key);
                    }
                }
                if let Some(led) = MATRIX_TO_LED[i] {
                    let _ = LED_EVENTS.try_send(LedCommand::KeyPress { led });
                }
                // Encoder click (matrix [0,2]) also surfaces to the host so
                // the daemon can hook custom actions on it (currently just
                // logged) on top of the HID Mute it already sent.
                if i == 2 {
                    let _ = DEVICE_TX_EVENTS.try_send(DeviceToHost::EncoderClick);
                }
                // matrix[5] (key below the encoder) is the OK key. KEYMAP[5] =
                // None, so no HID gating concerns. State transitions:
                //   Closed             → Main, main_selection=0
                //   Main + sel 0       → Sub(OledViz)
                //   Main + sel 1       → Sub(GlowViz)
                //   Main + sel 2       → enter USB bootloader (no return)
                //   Sub(_)             → back to Main
                if i == 5 {
                    let enter_bootloader = DISPLAY_STATE.lock(|s| {
                        let mut s = s.borrow_mut();
                        match s.menu {
                            MenuView::Closed => {
                                s.menu = MenuView::Main;
                                s.main_selection = 0;
                                false
                            }
                            MenuView::Main => match s.main_selection {
                                0 => {
                                    s.menu = MenuView::Sub(Selector::OledViz);
                                    false
                                }
                                1 => {
                                    s.menu = MenuView::Sub(Selector::GlowViz);
                                    false
                                }
                                2 => true,
                                _ => false,
                            },
                            MenuView::Sub(_) => {
                                s.menu = MenuView::Main;
                                false
                            }
                        }
                    });
                    // Bootloader entry: identical to the boot-time bootmagic
                    // path, just user-triggered from the menu instead of
                    // requiring a re-plug while holding the encoder.
                    if enter_bootloader {
                        info!("menu: entering USB bootloader");
                        embassy_rp::rom_data::reset_to_usb_boot(0, 0);
                        #[allow(clippy::empty_loop)]
                        loop {}
                    }
                }
                // matrix[8] (bottom-right) is the back/close key:
                //   Sub(_) → Main (back one level)
                //   Main   → Closed (exit menu)
                // Unbound when menu isn't open.
                if i == 8 && menu_open {
                    DISPLAY_STATE.lock(|s| {
                        let mut s = s.borrow_mut();
                        s.menu = match s.menu {
                            MenuView::Sub(_) => MenuView::Main,
                            _ => MenuView::Closed,
                        };
                    });
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
pub async fn encoder_task(pin_a: Peri<'static, PIN_11>, pin_b: Peri<'static, PIN_10>) {
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
            if !apply_encoder_step(1) {
                CONSUMER_EVENTS.send(ConsumerKey::VolumeUp).await;
            }
        } else if accumulator <= -4 {
            accumulator = 0;
            if !apply_encoder_step(-1) {
                CONSUMER_EVENTS.send(ConsumerKey::VolumeDown).await;
            }
        }
    }
}
