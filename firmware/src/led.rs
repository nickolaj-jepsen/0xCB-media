//! WS2812 chain driver task. Renders per-key press flashes, the volume
//! gauge, mute splash, and the underglow visualizer styles into a single
//! ~60 Hz frame (10 Hz when fully idle).

use embassy_futures::select::select;
use embassy_rp::peripherals::PIO0;
use embassy_rp::pio_programs::ws2812::{Grb, PioWs2812};
use embassy_time::{Duration, Instant, Ticker, Timer};
use smart_leds::RGB8;

use crate::state::{GlowViz, HueMode, LedCommand, DISPLAY_STATE, LED_EVENTS};

pub const NUM_LEDS: usize = 31;

const PER_KEY_END: usize = 8;
const UNDERGLOW_START: usize = 8;
const UNDERGLOW_END: usize = NUM_LEDS - 1;
const UNDERGLOW_COUNT: usize = UNDERGLOW_END - UNDERGLOW_START + 1;
/// Chain offset of the lower-index LED in the front-centre pair. The 23-LED
/// underglow enters at chain index 8 near the bottom-left and runs along
/// the bottom edge first, so chain offsets 2 and 3 (= LEDs 10 and 11)
/// straddle the bottom-centre gap. All symmetric viz (boot animation, spec
/// mirror, ripple) anchor on this gap so the centre always reads as a pair
/// of LEDs rather than a single off-centre one.
const FRONT_PIVOT_LO: usize = 2;
/// Chain offset of the higher-index LED in the front-centre pair.
const FRONT_PIVOT_HI: usize = FRONT_PIVOT_LO + 1;

// Per-LED-chip accent colours. The underglow WS2812B and the in-switch
// SK6812MINI-E render the same RGB code differently — SK6812 has a stronger
// green bias and looks yellow at the same G:R ratio, so we tune the
// constants by eye against the project accent `#CF6A4C` (warm orange).
const ACCENT_UNDERGLOW: RGB8 = RGB8 {
    r: 112,
    g: 32,
    b: 0,
};
const ACCENT_PERKEY: RGB8 = RGB8 { r: 96, g: 12, b: 0 };

/// LED chain controller. On boot, plays a one-shot symmetric outward sweep
/// from the front-centre gap, then idles black and flashes the per-key LED
/// briefly when the matrix task reports a press.
#[embassy_executor::task]
pub async fn led_task(mut ws2812: PioWs2812<'static, PIO0, 0, NUM_LEDS, Grb>) {
    const SPIRAL_STEP_MS: u64 = 85;
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
    // Where the gauge starts (gauge cell 0) and which way it wraps.
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

    // Symmetric outward boot animation: each step lights the next pair of
    // LEDs walking out from the front-centre gap, ending with the lone
    // diametrically-opposite LED. Reads as a wave radiating from the front
    // of the device — coherent with the gap-centred ripple/spec viz below.
    let pair_count = UNDERGLOW_COUNT.div_ceil(2);
    for step in 0..pair_count {
        let lo_off = (FRONT_PIVOT_LO + UNDERGLOW_COUNT - step) % UNDERGLOW_COUNT;
        let hi_off = (FRONT_PIVOT_HI + step) % UNDERGLOW_COUNT;
        frame[UNDERGLOW_START + lo_off] = ACCENT_UNDERGLOW;
        if lo_off != hi_off {
            frame[UNDERGLOW_START + hi_off] = ACCENT_UNDERGLOW;
        }
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

    // Viz state owned by the task — local so the borrow checker doesn't
    // tangle with the shared `DISPLAY_STATE` mutex. The underglow frame is
    // cleared before each viz renderer runs, so stale ripples / trails
    // can't bleed across mode changes.
    let mut prev_bass: u8 = 0;
    let mut ripple_cooldown: u8 = 0;
    let mut ripples: [Option<u16>; 4] = [None; 4]; // pos in 8.8 fixed
    let mut comet_pos_q8: u32 = 0;

    loop {
        // Wake on either the (possibly idle-rate) ticker or any LED_EVENTS
        // arrival.
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

        // Snapshot viz state once per tick.
        let (bands_fresh, viz_bands, glow_viz, hue_mode) = DISPLAY_STATE.lock(|s| {
            let s = s.borrow();
            (s.bands_fresh(), s.bands, s.glow_viz, s.hue_mode)
        });
        let glow_active = bands_fresh && glow_viz != GlowViz::Off;
        let dyn_accent = frame_accent(&viz_bands, hue_mode);

        // Per-key LEDs: press-flash decay only. The press_brightness array
        // ramps to 255 on a key event (matrix_task) and decays back to 0.
        for i in 0..PER_KEY_END {
            let factor = press_brightness[i] as u32;
            frame[i] = scale_rgb(ACCENT_PERKEY, factor);
            press_brightness[i] = press_brightness[i].saturating_sub(PRESS_DECAY);
        }

        match effect {
            Some(UnderglowEffect::Mute(start))
                if start.elapsed() < Duration::from_millis(MUTE_FADE_MS as u64) =>
            {
                let elapsed = start.elapsed().as_millis() as u32;
                let envelope = 255 - (elapsed * 255) / MUTE_FADE_MS;
                let color = scale_rgb(MUTE_COLOR, envelope);
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
                    frame[UNDERGLOW_START + chain_offset as usize] =
                        scale_rgb(ACCENT_UNDERGLOW, factor);
                }
            }
            _ => {
                effect = None;
                // Always clear before rendering a viz: the new modes draw
                // sparse content and we don't want stale pixels from the
                // previous tick.
                for px in &mut frame[UNDERGLOW_START..NUM_LEDS] {
                    *px = RGB8::default();
                }
                if glow_active {
                    match glow_viz {
                        GlowViz::Spec => render_glow_spec(&mut frame, &viz_bands, dyn_accent),
                        GlowViz::Ripple => {
                            render_glow_ripple(
                                &mut frame,
                                &viz_bands,
                                &mut ripples,
                                &mut prev_bass,
                                &mut ripple_cooldown,
                                dyn_accent,
                            );
                        }
                        GlowViz::Comet => {
                            render_glow_comet(
                                &mut frame,
                                &viz_bands,
                                &mut comet_pos_q8,
                                dyn_accent,
                            );
                        }
                        GlowViz::Off => {} // unreachable — glow_active false
                    }
                } else {
                    // Bands stale: also let inertial state decay so when
                    // music resumes we don't snap to whatever was last live.
                    ripples = [None; 4];
                    prev_bass = 0;
                    ripple_cooldown = 0;
                }
            }
        }

        // Cap the per-key and underglow rates: if everything is fully dark
        // and no effect is mid-fade, drop to IDLE_TICK_MS until something
        // wakes us. Recreating a Ticker with a new period fires immediately
        // on the next .next(), so the transition isn't sticky.
        let idle = press_brightness.iter().all(|&b| b == 0) && effect.is_none() && !glow_active;
        let target_ms = if idle { IDLE_TICK_MS } else { ACTIVE_TICK_MS };
        if target_ms != current_tick_ms {
            current_tick_ms = target_ms;
            ticker = Ticker::every(Duration::from_millis(current_tick_ms));
        }

        ws2812.write(&frame).await;
    }
}

/// Spectrum mirrored around the front-of-device gap. Both centre LEDs
/// (FRONT_PIVOT_LO/HI) sit at distance 0.5 — band 0 (bass) — and the
/// diametrically-opposite LED at distance 11.5 anchors band 7 (treble).
/// All distance arithmetic runs in 2x resolution so half-integer distances
/// stay integer.
fn render_glow_spec(frame: &mut [RGB8; NUM_LEDS], bands: &[u8; 8], accent: RGB8) {
    // Pivot in 2x resolution: between offset FRONT_PIVOT_LO and FRONT_PIVOT_HI.
    let pivot_2x = (2 * FRONT_PIVOT_LO + 1) as i32;
    let n_2x = (2 * UNDERGLOW_COUNT) as i32;
    // dist_2x ranges over the odd values {1, 3, 5, …, UNDERGLOW_COUNT}; we
    // map dist_2x=1 → band 0 and dist_2x=UNDERGLOW_COUNT → band 7.
    let max_dist_2x = UNDERGLOW_COUNT as u32;
    let denom = max_dist_2x.saturating_sub(1).max(1);
    for i in 0..UNDERGLOW_COUNT {
        let raw = (2 * i as i32 - pivot_2x).rem_euclid(n_2x);
        let dist_2x = raw.min(n_2x - raw) as u32;
        let band_pos = (dist_2x.saturating_sub(1) * 7 * 256) / denom;
        let bi = (band_pos / 256) as usize;
        let frac = band_pos % 256;
        let v0 = bands[bi.min(7)] as u32;
        let v1 = bands[(bi + 1).min(7)] as u32;
        let lin = (v0 * (256 - frac) + v1 * frac) / 256;
        // Square-law gamma so quiet noise stays dark and beats stand out
        // instead of glowing the whole ring.
        let factor = (lin * lin) / 255;
        frame[UNDERGLOW_START + i] = scale_rgb(accent, factor);
    }
}

/// Bass-kick ripple. A rising-edge on the low-band magnitude spawns an
/// expanding ring that travels outward from the front pivot in both
/// directions, with a 3-LED head-and-tail so it reads at speed.
/// `cooldown` debounces sustained bass so we don't pile ripples on top of
/// each other every frame the bass is loud.
fn render_glow_ripple(
    frame: &mut [RGB8; NUM_LEDS],
    bands: &[u8; 8],
    ripples: &mut [Option<u16>; 4],
    prev_bass: &mut u8,
    cooldown: &mut u8,
    accent: RGB8,
) {
    // 0.4 LEDs/frame at 60 Hz → ~9.6 LEDs/sec, ~1.2 sec to traverse a
    // half-ring. Range = half the ring (we mirror through the pivot).
    const RIPPLE_SPEED_Q8: u16 = 102;
    const RIPPLE_RANGE_Q8: u16 = (UNDERGLOW_COUNT as u16 / 2) * 256;
    // Tuned for typical 8-band magnitudes: hot bass usually peaks 100..200.
    // Spawn on a noticeable rise above a low absolute floor; cooldown stops
    // the same kick spawning on every consecutive frame.
    const KICK_MIN: u8 = 70;
    const KICK_RISE: u8 = 15;
    const COOLDOWN_FRAMES: u8 = 8;
    // Brightness for LEDs at distance 0 / 1 / 2 from the leading edge — a
    // single pixel ripple was too thin to register at this speed.
    const TRAIL_FALLOFF: [u32; 3] = [255, 160, 70];

    let bass = ((bands[0] as u16 + bands[1] as u16) / 2) as u8;
    let kick_now = bass > KICK_MIN && bass.saturating_sub(*prev_bass) > KICK_RISE;
    *prev_bass = bass;
    *cooldown = cooldown.saturating_sub(1);

    if kick_now && *cooldown == 0 {
        for slot in ripples.iter_mut() {
            if slot.is_none() {
                *slot = Some(0);
                *cooldown = COOLDOWN_FRAMES;
                break;
            }
        }
    }

    for slot in ripples.iter_mut() {
        if let Some(pos_q8) = slot.as_mut() {
            *pos_q8 = pos_q8.saturating_add(RIPPLE_SPEED_Q8);
            if *pos_q8 >= RIPPLE_RANGE_Q8 {
                *slot = None;
                continue;
            }
            let pos = (*pos_q8 / 256) as i32;
            // Squared falloff: `1 - (pos/range)^2` mapped to 0..256.
            let progress = (*pos_q8 as u32 * 256) / RIPPLE_RANGE_Q8 as u32;
            let inv = 256u32.saturating_sub(progress);
            let envelope = (inv * inv) / 256;

            for (trail_dist, &trail_b) in TRAIL_FALLOFF.iter().enumerate() {
                let trail_pos = pos - trail_dist as i32;
                if trail_pos < 0 {
                    break;
                }
                let factor = (envelope * trail_b) / 256;
                let pixel = scale_rgb(accent, factor);
                // Two heads: the lo head walks down from FRONT_PIVOT_LO, the
                // hi head walks up from FRONT_PIVOT_HI. At trail_pos=0 they
                // sit on the centre pair (so a fresh ripple lights both
                // centre LEDs); each subsequent step pushes them one LED
                // further out.
                let lo_off = ((FRONT_PIVOT_LO as i32) - trail_pos)
                    .rem_euclid(UNDERGLOW_COUNT as i32) as usize;
                let hi_off = ((FRONT_PIVOT_HI as i32) + trail_pos)
                    .rem_euclid(UNDERGLOW_COUNT as i32) as usize;
                frame[UNDERGLOW_START + lo_off] = rgb_max(frame[UNDERGLOW_START + lo_off], pixel);
                frame[UNDERGLOW_START + hi_off] = rgb_max(frame[UNDERGLOW_START + hi_off], pixel);
            }
        }
    }
}

/// Always-moving comet head with a fading trail. Both speed and brightness
/// are audio-driven: average band energy controls how fast the head walks
/// the ring, and instantaneous bass amplitude drives the head brightness so
/// kicks visibly flash through the trail.
fn render_glow_comet(
    frame: &mut [RGB8; NUM_LEDS],
    bands: &[u8; 8],
    pos_q8: &mut u32,
    accent: RGB8,
) {
    // ~0.094 LEDs/frame baseline at 60 Hz → ~5 sec per lap when silent. At
    // peak energy `BASE + 255*3 ≈ 789` Q8 = 3 LEDs/frame → ~0.5 sec per lap.
    // Big swing makes the audio reactivity unmistakable.
    const BASE_SPEED_Q8: u32 = 24;
    const TRAIL_FALLOFF: [u32; 6] = [255, 200, 130, 80, 40, 15];

    let energy = avg_band(bands) as u32;
    let bass = (bands[0] as u32 + bands[1] as u32) / 2;
    let speed = BASE_SPEED_Q8 + energy * 3;
    let total_q8 = (UNDERGLOW_COUNT as u32) * 256;
    *pos_q8 = (*pos_q8 + speed) % total_q8;

    // Brightness floor of 80 keeps the comet faintly visible during quiet;
    // bass adds up to 255 on top so kicks flash.
    let head_brightness = (80 + bass).min(255);
    let head = (*pos_q8 / 256) as usize;
    for (i, &b) in TRAIL_FALLOFF.iter().enumerate() {
        let off = (head + UNDERGLOW_COUNT - i) % UNDERGLOW_COUNT;
        let factor = (b * head_brightness) / 255;
        let pixel = scale_rgb(accent, factor);
        frame[UNDERGLOW_START + off] = rgb_max(frame[UNDERGLOW_START + off], pixel);
    }
}

/// Per-frame underglow accent. `Static` returns the fixed warm-orange
/// `ACCENT_UNDERGLOW`; `Dynamic` biases R up on bass and G up on treble so
/// the same renderers shift toward red on a kick and toward yellow-warm on
/// hat-driven sections.
fn frame_accent(bands: &[u8; 8], mode: HueMode) -> RGB8 {
    match mode {
        HueMode::Static => ACCENT_UNDERGLOW,
        HueMode::Dynamic => {
            let bass = bands[0..2].iter().copied().max().unwrap_or(0);
            let treb = bands[5..8].iter().copied().max().unwrap_or(0);
            RGB8 {
                r: (ACCENT_UNDERGLOW.r as u16 + bass as u16 / 4).min(255) as u8,
                g: (ACCENT_UNDERGLOW.g as u16 + treb as u16 / 4).min(255) as u8,
                b: ACCENT_UNDERGLOW.b,
            }
        }
    }
}

fn scale_rgb(c: RGB8, factor: u32) -> RGB8 {
    RGB8 {
        r: ((c.r as u32 * factor) / 255) as u8,
        g: ((c.g as u32 * factor) / 255) as u8,
        b: ((c.b as u32 * factor) / 255) as u8,
    }
}

fn rgb_max(a: RGB8, b: RGB8) -> RGB8 {
    RGB8 {
        r: a.r.max(b.r),
        g: a.g.max(b.g),
        b: a.b.max(b.b),
    }
}

fn avg_band(bands: &[u8; 8]) -> u8 {
    let sum: u32 = bands.iter().map(|&b| b as u32).sum();
    (sum / 8) as u8
}
