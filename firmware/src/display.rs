//! OLED rendering: pure functions over a `DisplayState` snapshot. The display
//! loop in `main` owns the framebuffer; this module just turns state into
//! pixels.

use embedded_graphics::{
    mono_font::MonoTextStyle,
    pixelcolor::BinaryColor,
    prelude::*,
    primitives::{Line, PrimitiveStyle, Rectangle},
    text::{Baseline, Text},
};

use crate::state::{DisplayState, GlowViz, HueMode, MenuView, OledViz, Selector, DISPLAY_STATE};

// ─── Confirm ─────────────────────────────────────────────────────────────────

fn render_confirm<D>(display: &mut D, text_style: &MonoTextStyle<'_, BinaryColor>)
where
    D: DrawTarget<Color = BinaryColor>,
{
    // FONT_6X10: 6 px/char wide, 10 px tall. Display = 128×64.
    // "Reboot to"  = 9 ch → x=37; "bootloader?" = 11 ch → x=31
    // "OK=confirm" = 10 ch → x=34; "back=cancel" = 11 ch → x=31
    let _ = Text::with_baseline("Reboot to", Point::new(37, 8), *text_style, Baseline::Top)
        .draw(display);
    let _ = Text::with_baseline("bootloader?", Point::new(31, 20), *text_style, Baseline::Top)
        .draw(display);
    let _ = Text::with_baseline("OK=confirm", Point::new(34, 40), *text_style, Baseline::Top)
        .draw(display);
    let _ = Text::with_baseline("back=cancel", Point::new(31, 52), *text_style, Baseline::Top)
        .draw(display);
}

/// Width of the time-history pane shared by waterfall + particles. The viz
/// pane spans x=2..=112 — 110 px before the volume bar at x=118.
const VIZ_W: usize = 110;
/// Number of vertical rows in the particle heat field. Each row maps to 2 px
/// of screen height, so 32 rows × 2 px = 64 px — covers the full display.
const PART_ROWS: usize = 32;

/// Persistent state for the OLED visualizers that need history across
/// frames. Owned by `main`'s display future and threaded into `render_frame`.
pub struct OledVizState {
    waterfall_hist: [[u8; 8]; VIZ_W], // ring buffer of 8-band columns
    waterfall_head: usize,            // index of the most recently written column
    particle_heat: [[u8; PART_ROWS]; 8],
    needle_peak: u8,
    needle_peak_age: u16, // frames since the peak last rose
    last_viz: OledViz,
}

impl OledVizState {
    pub const fn new() -> Self {
        Self {
            waterfall_hist: [[0; 8]; VIZ_W],
            waterfall_head: 0,
            particle_heat: [[0; PART_ROWS]; 8],
            needle_peak: 0,
            needle_peak_age: 0,
            last_viz: OledViz::Bars,
        }
    }

    fn reset(&mut self) {
        self.waterfall_hist = [[0; 8]; VIZ_W];
        self.waterfall_head = 0;
        self.particle_heat = [[0; PART_ROWS]; 8];
        self.needle_peak = 0;
        self.needle_peak_age = 0;
    }
}

pub fn render_frame<D>(
    display: &mut D,
    text_style: &MonoTextStyle<'_, BinaryColor>,
    state: &mut OledVizState,
) where
    D: DrawTarget<Color = BinaryColor>,
{
    let snapshot = DISPLAY_STATE.lock(|s| s.borrow().clone());
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

    if snapshot.menu_open() {
        render_menu(display, text_style, &snapshot);
        return;
    }

    // Wipe persistent buffers when the user cycles to a different mode so
    // stale particles/waterfall content doesn't leak between styles.
    if snapshot.oled_viz != state.last_viz {
        state.reset();
        state.last_viz = snapshot.oled_viz;
    }

    if snapshot.oled_viz_active() {
        match snapshot.oled_viz {
            OledViz::Bars => render_bars(display, &snapshot.bands),
            OledViz::Waterfall => render_waterfall(display, &snapshot.bands, state),
            OledViz::Radial => render_radial(display, &snapshot.bands),
            OledViz::VuNeedle => render_vu_needle(display, &snapshot.bands, state),
            OledViz::Particles => render_particles(display, &snapshot.bands, state),
            OledViz::Off => {} // unreachable — gated above
        }
    } else {
        // No fresh audio frames: don't keep showing the last waterfall column
        // or smouldering particle heat once the music stops.
        state.reset();
    }

    // Vertical volume bar pinned to the right edge. Drawn last so a long
    // title or a tall spectrum bar can't bleed into it. 8 px wide outline,
    // 6×58 inner fill anchored to the bottom and growing with the level.
    let outline = Rectangle::new(Point::new(118, 2), Size::new(8, 60))
        .into_styled(PrimitiveStyle::with_stroke(BinaryColor::On, 1));
    let _ = outline.draw(display);
    if snapshot.volume.muted {
        let stripe = Rectangle::new(Point::new(118, 30), Size::new(8, 4))
            .into_styled(PrimitiveStyle::with_fill(BinaryColor::On));
        let _ = stripe.draw(display);
    } else {
        let level = snapshot.volume.level.min(100) as u32;
        if level > 0 {
            let h = (level * 58 + 50) / 100;
            let top = 3 + (58 - h) as i32;
            let inner = Rectangle::new(Point::new(119, top), Size::new(6, h))
                .into_styled(PrimitiveStyle::with_fill(BinaryColor::On));
            let _ = inner.draw(display);
        }
    }
}

// ─── Bars ───────────────────────────────────────────────────────────────────
//
// Shared geometry for the 8-column bar viz. 8 columns × 13 px wide with a
// 1 px gap = 111 px, anchored at x=2; ends at x=112, leaving a 5 px gutter
// before the volume outline at x=118.
const BAR_W: u32 = 13;
const BAR_GAP: i32 = 1;
const BAR_LEFT: i32 = 2;

fn bar_x(i: usize) -> i32 {
    BAR_LEFT + i as i32 * (BAR_W as i32 + BAR_GAP)
}

fn render_bars<D>(display: &mut D, bands: &[u8; 8])
where
    D: DrawTarget<Color = BinaryColor>,
{
    const BOTTOM: i32 = 63;
    const MAX_H: i32 = 60;
    for (i, &v) in bands.iter().enumerate() {
        let h = (v as i32 * MAX_H) / 255;
        if h <= 0 {
            continue;
        }
        let x = bar_x(i);
        let y = BOTTOM - h + 1;
        let _ = Rectangle::new(Point::new(x, y), Size::new(BAR_W, h as u32))
            .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
            .draw(display);
    }
}

// ─── Waterfall ──────────────────────────────────────────────────────────────
//
// Time scrolls left-to-right. Each frame stamps the latest 8-band sample at
// the head of a ring buffer; render iterates columns left=oldest to
// right=newest. Vertical layout: 8 bands stacked, bass at the bottom. Each
// band gets an 8-px-tall slice; magnitude maps to a fill height within the
// slice so each column reads as 8 mini-bars.

fn render_waterfall<D>(display: &mut D, bands: &[u8; 8], state: &mut OledVizState)
where
    D: DrawTarget<Color = BinaryColor>,
{
    state.waterfall_head = (state.waterfall_head + 1) % VIZ_W;
    state.waterfall_hist[state.waterfall_head] = *bands;

    // Render: column 0 (leftmost on screen) = oldest; column VIZ_W-1 = newest.
    // Oldest sits at history[(head + 1) mod VIZ_W].
    for c in 0..VIZ_W {
        let idx = (state.waterfall_head + 1 + c) % VIZ_W;
        let col = &state.waterfall_hist[idx];
        let x = BAR_LEFT + c as i32;
        for (b, &m) in col.iter().enumerate() {
            // band 0 (bass) at the bottom; band 7 (treble) at the top.
            let slice_top = ((7 - b) as i32) * 8;
            let fill = (m as i32 * 8) / 255;
            if fill <= 0 {
                continue;
            }
            let y = slice_top + 8 - fill;
            let _ = Rectangle::new(Point::new(x, y), Size::new(1, fill as u32))
                .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
                .draw(display);
        }
    }
}

// ─── Radial ─────────────────────────────────────────────────────────────────
//
// 8 spokes from the centre of the viz pane, each band drives one spoke's
// length. Spokes at multiples of 45°, scaled to MAX_R pixels at full
// magnitude.
const RADIAL_CX: i32 = 57;
const RADIAL_CY: i32 = 32;
// Unit-vector endpoints at maximum-magnitude distance (28 px), rounded.
// Order: 0°, 45°, 90°, …
// (clockwise on screen, with screen y inverted from math y so 90° points up).
const SPOKE_AT_MAX: [(i8, i8); 8] = [
    (28, 0),
    (20, -20),
    (0, -28),
    (-20, -20),
    (-28, 0),
    (-20, 20),
    (0, 28),
    (20, 20),
];

fn render_radial<D>(display: &mut D, bands: &[u8; 8])
where
    D: DrawTarget<Color = BinaryColor>,
{
    for (i, &m) in bands.iter().enumerate() {
        if m == 0 {
            continue;
        }
        let (dx, dy) = SPOKE_AT_MAX[i];
        let ex = RADIAL_CX + (dx as i32 * m as i32) / 255;
        let ey = RADIAL_CY + (dy as i32 * m as i32) / 255;
        let _ = Line::new(Point::new(RADIAL_CX, RADIAL_CY), Point::new(ex, ey))
            .into_styled(PrimitiveStyle::with_stroke(BinaryColor::On, 1))
            .draw(display);
    }
    // Centre dot so a quiet song still has something to look at.
    let _ = Rectangle::new(Point::new(RADIAL_CX - 1, RADIAL_CY - 1), Size::new(3, 3))
        .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
        .draw(display);
}

// ─── VU Needle ──────────────────────────────────────────────────────────────
//
// Tape-deck style analog dial. Pivot near the bottom centre of the viz pane,
// needle sweeps an arc above. Average band energy drives the angle; a
// peak-hold tick lags behind and decays.
const NEEDLE_PIVOT_X: i32 = 57;
const NEEDLE_PIVOT_Y: i32 = 56;
// Endpoints at a 50 px arc radius for 16 evenly-spaced sweep positions, from
// idx 0 (low energy, 150°) to idx 15 (high energy, 30°). Generated from
// (round(cos(θ)*50), round(-sin(θ)*50)) with θ ∈ {150°, 142°, …, 30°}.
const NEEDLE_LUT: [(i8, i8); 16] = [
    (-43, -25),
    (-39, -31),
    (-35, -36),
    (-29, -40),
    (-23, -44),
    (-17, -47),
    (-10, -49),
    (-3, -50),
    (3, -50),
    (10, -49),
    (17, -47),
    (23, -44),
    (29, -40),
    (35, -36),
    (39, -31),
    (43, -25),
];

fn render_vu_needle<D>(display: &mut D, bands: &[u8; 8], state: &mut OledVizState)
where
    D: DrawTarget<Color = BinaryColor>,
{
    let avg = avg_band(bands);
    let needle_idx = ((avg as u32 * 15) / 255) as usize;

    // Arc outline: sample 32 positions along the same sweep as the needle
    // LUT (interpolating between adjacent LUT entries for smoother coverage)
    // and stamp a single pixel at each. 32 dots reads as a continuous arc on
    // a 1bpp display at this radius.
    for i in 0..32 {
        let t = (i * 15) / 31; // 0..15
        let frac = ((i * 15) % 31) as i32; // 0..30
        let (x0, y0) = NEEDLE_LUT[t];
        let (x1, y1) = NEEDLE_LUT[(t + 1).min(15)];
        let dx = x0 as i32 + ((x1 as i32 - x0 as i32) * frac) / 31;
        let dy = y0 as i32 + ((y1 as i32 - y0 as i32) * frac) / 31;
        let px = NEEDLE_PIVOT_X + dx;
        let py = NEEDLE_PIVOT_Y + dy;
        let _ = Pixel(Point::new(px, py), BinaryColor::On).draw(display);
    }

    // Peak-hold: rise instantly to the new energy, decay slowly after a
    // ~250 ms hold. needle_peak_age counts frames since peak last rose; the
    // display loop runs at ~30 Hz so 8 frames ≈ 250 ms.
    if avg >= state.needle_peak {
        state.needle_peak = avg;
        state.needle_peak_age = 0;
    } else {
        state.needle_peak_age = state.needle_peak_age.saturating_add(1);
        if state.needle_peak_age > 8 {
            state.needle_peak = state.needle_peak.saturating_sub(4);
        }
    }
    let peak_idx = ((state.needle_peak as u32 * 15) / 255) as usize;
    let (px_dx, px_dy) = NEEDLE_LUT[peak_idx];
    // Tick: a 3-pixel chunk on the arc at the peak position.
    for d in -1..=1 {
        let _ = Pixel(
            Point::new(
                NEEDLE_PIVOT_X + px_dx as i32 + d,
                NEEDLE_PIVOT_Y + px_dy as i32,
            ),
            BinaryColor::On,
        )
        .draw(display);
    }

    // Needle line from pivot to the current angle's arc point.
    let (n_dx, n_dy) = NEEDLE_LUT[needle_idx];
    let nx = NEEDLE_PIVOT_X + n_dx as i32;
    let ny = NEEDLE_PIVOT_Y + n_dy as i32;
    let _ = Line::new(
        Point::new(NEEDLE_PIVOT_X, NEEDLE_PIVOT_Y),
        Point::new(nx, ny),
    )
    .into_styled(PrimitiveStyle::with_stroke(BinaryColor::On, 1))
    .draw(display);

    // Pivot pip so the line has somewhere to attach visually.
    let _ = Rectangle::new(
        Point::new(NEEDLE_PIVOT_X - 2, NEEDLE_PIVOT_Y - 1),
        Size::new(5, 3),
    )
    .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
    .draw(display);
}

// ─── Particles ──────────────────────────────────────────────────────────────
//
// Doom-style fire field: 8 columns × PART_ROWS rows. Bands inject heat into
// the bottom row each tick; heat propagates upward with horizontal diffusion
// and a per-step decay so the field flickers and fades.
//
// Render: each heat cell maps to a 1 px wide × 2 px tall block in the viz
// pane; pixel-on uses Bayer 4×4 ordered dither so quiet content shows up as
// sparse twinkles and loud content fills in solid.

const BAYER4: [[u8; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];

fn render_particles<D>(display: &mut D, bands: &[u8; 8], state: &mut OledVizState)
where
    D: DrawTarget<Color = BinaryColor>,
{
    const DECAY: u8 = 3;

    // Heat propagates from r=PART_ROWS-1 (the bottom, where bands inject)
    // toward r=0 (the top, where flames cool out). Each cell pulls from the
    // row below with horizontal diffusion onto its two neighbours, then
    // subtracts a constant decay.
    let mut new_heat = [[0u8; PART_ROWS]; 8];
    #[allow(clippy::needless_range_loop)]
    for r in 0..PART_ROWS - 1 {
        for c in 0..8 {
            let cl = (c + 7) % 8;
            let cr = (c + 1) % 8;
            let src_r = r + 1;
            let avg = (state.particle_heat[c][src_r] as u16 * 2
                + state.particle_heat[cl][src_r] as u16
                + state.particle_heat[cr][src_r] as u16)
                / 4;
            new_heat[c][r] = (avg as u8).saturating_sub(DECAY);
        }
    }
    // Bottom row: keep most of the previous heat (fades naturally) and
    // overlay the latest band magnitude. Avoids one-frame flickers when a
    // band drops to zero.
    for c in 0..8 {
        let prev = state.particle_heat[c][PART_ROWS - 1];
        let prev_kept = prev.saturating_sub(DECAY);
        new_heat[c][PART_ROWS - 1] = prev_kept.max(bands[c]);
    }
    state.particle_heat = new_heat;

    // Render: each (col, row) → 1 col-wide × 2 row-tall block. Hot rows
    // (r=PART_ROWS-1) anchor at the bottom; cold trails (r=0) reach the top.
    // Pixel-on uses Bayer 4×4 ordered dither so quiet content shows up as
    // sparse twinkles and loud content fills in solid.
    const COL_W: i32 = 14;
    const ROW_H: i32 = 2;
    for c in 0..8 {
        for r in 0..PART_ROWS {
            let heat = state.particle_heat[c][r];
            if heat == 0 {
                continue;
            }
            let block_x = BAR_LEFT + c as i32 * COL_W;
            let block_y = r as i32 * ROW_H;
            for dy in 0..ROW_H {
                for dx in 0..COL_W {
                    let px = block_x + dx;
                    let py = block_y + dy;
                    let threshold = (BAYER4[(py & 3) as usize][(px & 3) as usize] + 1) * 16;
                    if (heat as u16) > threshold as u16 {
                        let _ = Pixel(Point::new(px, py), BinaryColor::On).draw(display);
                    }
                }
            }
        }
    }
}

fn avg_band(bands: &[u8; 8]) -> u8 {
    let sum: u32 = bands.iter().map(|&b| b as u32).sum();
    (sum / 8) as u8
}

// ─── Menu ───────────────────────────────────────────────────────────────────

fn render_menu<D>(
    display: &mut D,
    text_style: &MonoTextStyle<'_, BinaryColor>,
    snapshot: &DisplayState,
) where
    D: DrawTarget<Color = BinaryColor>,
{
    match snapshot.menu {
        MenuView::Closed => {} // shouldn't reach here — render_frame guards
        MenuView::Main => render_main_menu(display, text_style, snapshot),
        MenuView::Sub(sel) => render_submenu(display, text_style, sel, snapshot),
        MenuView::Confirm => render_confirm(display, text_style),
    }
}

fn render_main_menu<D>(
    display: &mut D,
    text_style: &MonoTextStyle<'_, BinaryColor>,
    snapshot: &DisplayState,
) where
    D: DrawTarget<Color = BinaryColor>,
{
    // Title at y=2; five rows below at 14, 24, 34, 44, 54 (10 px spacing).
    // FONT_6X10 glyphs are 10 px tall, so the last row ends at y=63 — fits
    // exactly inside the 64 px display.
    let _ =
        Text::with_baseline("MENU", Point::new(52, 2), *text_style, Baseline::Top).draw(display);

    let items: [(&str, &str); 4] = [
        ("oled viz", snapshot.oled_viz.label()),
        ("glow viz", snapshot.glow_viz.label()),
        ("hue", snapshot.hue_mode.label()),
        ("Bootloader", "Go"),
    ];
    for (i, (label, right)) in items.iter().enumerate() {
        let y = 14 + (i as i32) * 10;
        if i as u8 == snapshot.main_selection {
            let _ = Text::with_baseline(">", Point::new(4, y), *text_style, Baseline::Top)
                .draw(display);
        }
        let _ =
            Text::with_baseline(label, Point::new(14, y), *text_style, Baseline::Top).draw(display);
        let _ =
            Text::with_baseline(right, Point::new(98, y), *text_style, Baseline::Top).draw(display);
    }
}

fn render_submenu<D>(
    display: &mut D,
    text_style: &MonoTextStyle<'_, BinaryColor>,
    selector: Selector,
    snapshot: &DisplayState,
) where
    D: DrawTarget<Color = BinaryColor>,
{
    // Submenus list every option for the selector. The `>` cursor sits on
    // the *current* enum value. The longest list (OledViz, 6 rows) needs
    // the full 64 px height, so submenus skip the title bar and start rows
    // at y=2 with 10 px spacing.
    let draw_row = |i: usize, label: &str, selected: bool, display: &mut D| {
        let y = 2 + (i as i32) * 10;
        if selected {
            let _ = Text::with_baseline(">", Point::new(4, y), *text_style, Baseline::Top)
                .draw(display);
        }
        let _ =
            Text::with_baseline(label, Point::new(14, y), *text_style, Baseline::Top).draw(display);
    };

    match selector {
        Selector::OledViz => {
            for (i, &variant) in OledViz::ALL.iter().enumerate() {
                draw_row(
                    i,
                    variant.long_label(),
                    variant == snapshot.oled_viz,
                    display,
                );
            }
        }
        Selector::GlowViz => {
            for (i, &variant) in GlowViz::ALL.iter().enumerate() {
                draw_row(
                    i,
                    variant.long_label(),
                    variant == snapshot.glow_viz,
                    display,
                );
            }
        }
        Selector::HueMode => {
            for (i, &variant) in HueMode::ALL.iter().enumerate() {
                draw_row(
                    i,
                    variant.long_label(),
                    variant == snapshot.hue_mode,
                    display,
                );
            }
        }
    }
}
