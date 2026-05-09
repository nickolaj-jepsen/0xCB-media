//! OLED rendering: pure functions over a `DisplayState` snapshot. The display
//! loop in `main` owns the framebuffer; this module just turns state into
//! pixels.

use embedded_graphics::{
    mono_font::MonoTextStyle,
    pixelcolor::BinaryColor,
    prelude::*,
    primitives::{PrimitiveStyle, Rectangle},
    text::{Baseline, Text},
};

use crate::state::{
    DisplayState, GlowViz, MenuView, OledViz, Selector, DISPLAY_STATE, MENU_ITEM_COUNT,
};

pub fn render_frame<D>(display: &mut D, text_style: &MonoTextStyle<'_, BinaryColor>)
where
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

    if snapshot.menu_open() {
        render_menu(display, text_style, &snapshot);
        return;
    }

    // Left pane: visualizer while audio is flowing. Style picked from
    // `snapshot.oled_viz`; `Off` is filtered out by `oled_viz_active()`.
    if snapshot.oled_viz_active() {
        match snapshot.oled_viz {
            OledViz::Bars => render_bars(display, &snapshot.bands),
            OledViz::Mirror => render_mirror(display, &snapshot.bands),
            OledViz::Dots => render_dots(display, &snapshot.bands),
            OledViz::Off => {} // unreachable — gated above
        }
    }

    // Vertical volume bar pinned to the right edge. Drawn last so a long
    // title or a tall spectrum bar can't bleed into it. 8 px wide outline,
    // 6×58 inner fill anchored to the bottom and growing with the level.
    let outline = Rectangle::new(Point::new(118, 2), Size::new(8, 60))
        .into_styled(PrimitiveStyle::with_stroke(BinaryColor::On, 1));
    let _ = outline.draw(display);
    if snapshot.volume.muted {
        // Solid stripe across the middle = muted indicator.
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

// Shared geometry for all OLED visualizer styles. 8 columns × 13 px wide with
// a 1 px gap = 111 px, anchored at x=2; ends at x=112, leaving a 5 px gutter
// before the volume outline at x=118.
const VIZ_BAR_W: u32 = 13;
const VIZ_GAP: i32 = 1;
const VIZ_LEFT: i32 = 2;

fn viz_column_x(i: usize) -> i32 {
    VIZ_LEFT + i as i32 * (VIZ_BAR_W as i32 + VIZ_GAP)
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
        let x = viz_column_x(i);
        let y = BOTTOM - h + 1;
        let _ = Rectangle::new(Point::new(x, y), Size::new(VIZ_BAR_W, h as u32))
            .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
            .draw(display);
    }
}

/// Centre-anchored bars: each band grows up *and* down from a horizontal mid
/// line, like a stereo VU meter. Half-height ≈ 30 px each direction.
fn render_mirror<D>(display: &mut D, bands: &[u8; 8])
where
    D: DrawTarget<Color = BinaryColor>,
{
    const CENTER_Y: i32 = 32;
    const MAX_HALF: i32 = 30;
    for (i, &v) in bands.iter().enumerate() {
        let h = (v as i32 * MAX_HALF) / 255;
        if h <= 0 {
            continue;
        }
        let x = viz_column_x(i);
        // One rectangle spanning [center - h, center + h] — height = 2*h.
        let y = CENTER_Y - h;
        let _ = Rectangle::new(Point::new(x, y), Size::new(VIZ_BAR_W, (h * 2) as u32))
            .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
            .draw(display);
    }
}

/// Peak-dot mode: a single short cap at the height each bar would reach. Cheap
/// and the empty space below makes individual hits easier to read at a glance.
fn render_dots<D>(display: &mut D, bands: &[u8; 8])
where
    D: DrawTarget<Color = BinaryColor>,
{
    const BOTTOM: i32 = 63;
    const MAX_H: i32 = 60;
    const DOT_H: u32 = 3;
    for (i, &v) in bands.iter().enumerate() {
        let h = (v as i32 * MAX_H) / 255;
        if h <= 0 {
            continue;
        }
        let x = viz_column_x(i);
        let y = (BOTTOM - h + 1).max(0);
        let _ = Rectangle::new(Point::new(x, y), Size::new(VIZ_BAR_W, DOT_H))
            .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
            .draw(display);
    }
}

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
    }
}

fn render_main_menu<D>(
    display: &mut D,
    text_style: &MonoTextStyle<'_, BinaryColor>,
    snapshot: &DisplayState,
) where
    D: DrawTarget<Color = BinaryColor>,
{
    // Full-screen replacement: title up top, three rows below. `>` cursor
    // marks the highlighted row — simpler than inverting the text background
    // and reads fine on the 128×64 OLED. Right column shows the active option
    // for selectors or "Go" for actions; "Bootloader" lives there too.
    let _ =
        Text::with_baseline("MENU", Point::new(52, 2), *text_style, Baseline::Top).draw(display);

    let items: [(&str, &str); MENU_ITEM_COUNT as usize] = [
        ("oled viz", snapshot.oled_viz.label()),
        ("glow viz", snapshot.glow_viz.label()),
        ("Bootloader", "Go"),
    ];
    for (i, (label, right)) in items.iter().enumerate() {
        let y = 22 + (i as i32) * 14;
        if i as u8 == snapshot.main_selection {
            let _ = Text::with_baseline(">", Point::new(4, y), *text_style, Baseline::Top)
                .draw(display);
        }
        let _ =
            Text::with_baseline(label, Point::new(14, y), *text_style, Baseline::Top).draw(display);
        let _ = Text::with_baseline(right, Point::new(104, y), *text_style, Baseline::Top)
            .draw(display);
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
    // Submenus list every option for the selector. The `>` cursor sits on the
    // *current* enum value — no separate draft index; encoder rotation mutates
    // the live value so the change is visible the moment you exit the menu.
    let title = match selector {
        Selector::OledViz => "OLED VIZ",
        Selector::GlowViz => "GLOW VIZ",
    };
    // Title at x=40 (FONT_6X10 → 8 chars × 6 = 48 px gives room either side).
    let _ = Text::with_baseline(title, Point::new(40, 2), *text_style, Baseline::Top).draw(display);

    // Helper to draw one row given an index, label, and "is selected".
    let draw_row = |i: usize, label: &str, selected: bool, display: &mut D| {
        let y = 18 + (i as i32) * 12;
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
    }
}
