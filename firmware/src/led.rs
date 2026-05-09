//! WS2812 chain driver task. Renders per-key press flashes, the volume gauge,
//! mute splash, and the underglow visualizer styles into a single ~60 Hz frame
//! (10 Hz when fully idle).

use embassy_futures::select::select;
use embassy_rp::peripherals::PIO0;
use embassy_rp::pio_programs::ws2812::{Grb, PioWs2812};
use embassy_time::{Duration, Instant, Ticker, Timer};
use smart_leds::RGB8;

use crate::state::{GlowViz, LedCommand, DISPLAY_STATE, LED_EVENTS};

pub const NUM_LEDS: usize = 31;

/// LED chain controller. On boot, plays a one-shot underglow spiral mirroring
/// the upstream Vial `v5.c` startup effect, then idles black and flashes the
/// per-key LED briefly when the matrix task reports a press.
#[embassy_executor::task]
pub async fn led_task(mut ws2812: PioWs2812<'static, PIO0, 0, NUM_LEDS, Grb>) {
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
    const ACCENT_UNDERGLOW: RGB8 = RGB8 {
        r: 112,
        g: 32,
        b: 0,
    };
    const ACCENT_PERKEY: RGB8 = RGB8 { r: 96, g: 12, b: 0 };
    const PRESS_DECAY: u8 = 18;
    // 60 Hz while anything's animating, 10 Hz when fully idle. Idle cuts
    // ~83 % of the WS2812 chain writes when nothing's lit, with up to 100 ms
    // of latency to wake the viz once audio resumes (matrix + volume events
    // wake immediately via LED_EVENTS).
    const ACTIVE_TICK_MS: u64 = 16;
    const IDLE_TICK_MS: u64 = 100;
    // Volume gauge: when the host pushes a Volume frame, fill the underglow
    // ring proportionally to `level / 100`. Held at full intensity for
    // VOL_GAUGE_HOLD_MS, then fades to black across VOL_GAUGE_FADE_MS so the
    // ring returns to the idle dark state. Sub-LED precision (1 LED = 256
    // units) keeps the leading edge smooth at 1 % volume increments.
    const VOL_GAUGE_HOLD_MS: u32 = 1500;
    const VOL_GAUGE_FADE_MS: u32 = 700;
    const VOL_GAUGE_TOTAL_MS: u64 = (VOL_GAUGE_HOLD_MS + VOL_GAUGE_FADE_MS) as u64;
    // Where the gauge starts (gauge cell 0) and which way it wraps. The
    // underglow chain enters at ~9 o'clock (chain LED 8) and runs anti-
    // clockwise around the perimeter, so chain offset 5 (= LED 13, the
    // SPIRAL_PIVOT) is roughly 6 o'clock; flipping `GAUGE_REVERSED` walks
    // the chain backwards so the visual sweep goes clockwise.
    const GAUGE_START_OFFSET: u32 = 22;
    const GAUGE_REVERSED: bool = true;
    // Mute effect: a red backdrop that fades linearly to black.
    const MUTE_FADE_MS: u32 = 1000;
    const MUTE_COLOR: RGB8 = RGB8 { r: 140, g: 0, b: 0 };

    enum UnderglowEffect {
        Gauge(Instant),
        Mute(Instant),
    }

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
    let mut effect: Option<UnderglowEffect> = None;
    let mut current_tick_ms: u64 = ACTIVE_TICK_MS;
    let mut ticker = Ticker::every(Duration::from_millis(current_tick_ms));

    loop {
        // Wake on either the (possibly idle-rate) ticker or any LED_EVENTS
        // arrival — the drain below still consumes the message, so this
        // select is purely a wakeup. The viz frame path doesn't push to
        // LED_EVENTS, so on viz transitions we wait up to IDLE_TICK_MS to
        // notice — acceptable per the plan.
        select(ticker.next(), LED_EVENTS.ready_to_receive()).await;

        while let Ok(cmd) = LED_EVENTS.try_receive() {
            match cmd {
                LedCommand::KeyPress { led } => {
                    if (led as usize) < PER_KEY_END {
                        press_brightness[led as usize] = 255;
                    }
                }
                LedCommand::VolumeChanged => {
                    effect = Some(UnderglowEffect::Gauge(Instant::now()));
                }
                LedCommand::Muted => {
                    effect = Some(UnderglowEffect::Mute(Instant::now()));
                }
            }
        }

        // Snapshot viz state once per tick — direct read avoids saturating
        // `LED_EVENTS` at 60 Hz host frame rate. `glow_viz_active` gates the
        // underglow side independently from the OLED visualizer (menu row 1).
        let (viz_active, viz_bands, glow_viz) = DISPLAY_STATE.lock(|s| {
            let s = s.borrow();
            (s.glow_viz_active(), s.bands, s.glow_viz)
        });

        for i in 0..PER_KEY_END {
            let press = press_brightness[i] as u16;
            frame[i] = RGB8 {
                r: ((ACCENT_PERKEY.r as u16 * press) / 255) as u8,
                g: ((ACCENT_PERKEY.g as u16 * press) / 255) as u8,
                b: ((ACCENT_PERKEY.b as u16 * press) / 255) as u8,
            };
            press_brightness[i] = press_brightness[i].saturating_sub(PRESS_DECAY);
        }

        match effect {
            Some(UnderglowEffect::Mute(start))
                if start.elapsed() < Duration::from_millis(MUTE_FADE_MS as u64) =>
            {
                let elapsed = start.elapsed().as_millis() as u32;
                let envelope = 255 - (elapsed * 255) / MUTE_FADE_MS;
                let color = RGB8 {
                    r: ((MUTE_COLOR.r as u32 * envelope) / 255) as u8,
                    g: ((MUTE_COLOR.g as u32 * envelope) / 255) as u8,
                    b: ((MUTE_COLOR.b as u32 * envelope) / 255) as u8,
                };
                for px in &mut frame[UNDERGLOW_START..NUM_LEDS] {
                    *px = color;
                }
            }
            Some(UnderglowEffect::Gauge(start))
                if start.elapsed() < Duration::from_millis(VOL_GAUGE_TOTAL_MS) =>
            {
                let elapsed = start.elapsed().as_millis() as u32;
                let envelope = if elapsed < VOL_GAUGE_HOLD_MS {
                    255
                } else {
                    let into_fade = elapsed - VOL_GAUGE_HOLD_MS;
                    255 - (into_fade * 255) / VOL_GAUGE_FADE_MS
                };
                let level = DISPLAY_STATE.lock(|s| {
                    let s = s.borrow();
                    if s.volume.muted {
                        0u32
                    } else {
                        s.volume.level.min(100) as u32
                    }
                });
                let fill_units = (level * UNDERGLOW_COUNT as u32 * 256) / 100;
                let count = UNDERGLOW_COUNT as u32;
                for i in 0..UNDERGLOW_COUNT {
                    let led_start = i as u32 * 256;
                    let intensity = if fill_units >= led_start + 256 {
                        255
                    } else if fill_units > led_start {
                        ((fill_units - led_start) * 255) / 256
                    } else {
                        0
                    };
                    let factor = (intensity * envelope) / 255;
                    let gauge_step = i as u32;
                    let chain_offset = if GAUGE_REVERSED {
                        (GAUGE_START_OFFSET + count - gauge_step) % count
                    } else {
                        (GAUGE_START_OFFSET + gauge_step) % count
                    };
                    frame[UNDERGLOW_START + chain_offset as usize] = RGB8 {
                        r: ((ACCENT_UNDERGLOW.r as u32 * factor) / 255) as u8,
                        g: ((ACCENT_UNDERGLOW.g as u32 * factor) / 255) as u8,
                        b: ((ACCENT_UNDERGLOW.b as u32 * factor) / 255) as u8,
                    };
                }
            }
            _ => {
                effect = None;
                if viz_active {
                    match glow_viz {
                        GlowViz::Spec => render_glow_spec(
                            &mut frame,
                            &viz_bands,
                            UNDERGLOW_START,
                            UNDERGLOW_COUNT,
                            SPIRAL_PIVOT,
                            ACCENT_UNDERGLOW,
                        ),
                        GlowViz::Vu => render_glow_uniform(
                            &mut frame,
                            avg_band(&viz_bands),
                            UNDERGLOW_START,
                            UNDERGLOW_COUNT,
                            ACCENT_UNDERGLOW,
                        ),
                        GlowViz::Bass => render_glow_uniform(
                            &mut frame,
                            // Average the two lowest bands so a kick reads
                            // across both sub-bass and bass bins.
                            ((viz_bands[0] as u16 + viz_bands[1] as u16) / 2) as u8,
                            UNDERGLOW_START,
                            UNDERGLOW_COUNT,
                            ACCENT_UNDERGLOW,
                        ),
                        GlowViz::Off => {} // unreachable — viz_active false
                    }
                } else {
                    for px in &mut frame[UNDERGLOW_START..NUM_LEDS] {
                        *px = RGB8::default();
                    }
                }
            }
        }

        // Cap the per-key and underglow rates: if everything is fully dark
        // and no effect is mid-fade, drop to IDLE_TICK_MS until something
        // wakes us. Recreating a Ticker with a new period fires immediately
        // on the next .next(), so the transition isn't sticky.
        let idle = press_brightness.iter().all(|&b| b == 0) && effect.is_none() && !viz_active;
        let target_ms = if idle { IDLE_TICK_MS } else { ACTIVE_TICK_MS };
        if target_ms != current_tick_ms {
            current_tick_ms = target_ms;
            ticker = Ticker::every(Duration::from_millis(current_tick_ms));
        }

        ws2812.write(&frame).await;
    }
}

/// Spectrum mirrored around the front-of-device LED (chain index = `pivot` ≈
/// 6 o'clock, the side closest to the user). Bass pulses at the centre and
/// treble walks both ways toward the back of the ring, so a kick lands in
/// front of you instead of dragging across the whole strip.
fn render_glow_spec(
    frame: &mut [RGB8; NUM_LEDS],
    bands: &[u8; 8],
    underglow_start: usize,
    underglow_count: usize,
    pivot: usize,
    accent: RGB8,
) {
    let center_offset = pivot as i32 - underglow_start as i32;
    let count_i32 = underglow_count as i32;
    let half = count_i32 / 2;
    for i in 0..underglow_count {
        let raw = (i as i32 - center_offset).rem_euclid(count_i32);
        // `dist` is 0..=half; lerp between adjacent bands in 8.8 fixed point.
        let dist = raw.min(count_i32 - raw);
        let band_pos = (dist as u32 * 7 * 256) / half as u32;
        let bi = (band_pos / 256) as usize;
        let frac = band_pos % 256;
        let v0 = bands[bi.min(7)] as u32;
        let v1 = bands[(bi + 1).min(7)] as u32;
        let lin = (v0 * (256 - frac) + v1 * frac) / 256;
        // Square-law gamma so quiet noise stays dark and beats stand out
        // instead of glowing the whole ring.
        let factor = (lin * lin) / 255;
        frame[underglow_start + i] = scale_rgb(accent, factor);
    }
}

/// Uniform-brightness fill across the whole underglow ring. Used by both `Vu`
/// (input = average band magnitude → ring breathes with overall energy) and
/// `Bass` (input = low-band magnitude → ring only pulses on kick hits).
fn render_glow_uniform(
    frame: &mut [RGB8; NUM_LEDS],
    magnitude: u8,
    underglow_start: usize,
    underglow_count: usize,
    accent: RGB8,
) {
    let lin = magnitude as u32;
    let factor = (lin * lin) / 255;
    let color = scale_rgb(accent, factor);
    for px in &mut frame[underglow_start..underglow_start + underglow_count] {
        *px = color;
    }
}

fn scale_rgb(c: RGB8, factor: u32) -> RGB8 {
    RGB8 {
        r: ((c.r as u32 * factor) / 255) as u8,
        g: ((c.g as u32 * factor) / 255) as u8,
        b: ((c.b as u32 * factor) / 255) as u8,
    }
}

fn avg_band(bands: &[u8; 8]) -> u8 {
    let sum: u32 = bands.iter().map(|&b| b as u32).sum();
    (sum / 8) as u8
}
