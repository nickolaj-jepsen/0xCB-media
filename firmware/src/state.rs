//! Shared state and inter-task channels.
//!
//! Every task module imports from here. The blocking-mutex `DISPLAY_STATE`
//! holds the live UI state (volume, viz bands, on-device menu) and the three
//! `Channel`s decouple producers from consumers so any task can fire without
//! holding a reference to the receiver.

use core::cell::RefCell;

use embassy_sync::blocking_mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant};

use proto::{DeviceToHost, HostToDevice};

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
pub static CONSUMER_EVENTS: Channel<CriticalSectionRawMutex, ConsumerKey, 8> = Channel::new();

/// LED render commands fired by the matrix task and consumed by `led_task`.
#[derive(Copy, Clone, Debug)]
pub enum LedCommand {
    /// Flash the per-key LED at this index (0..=7).
    KeyPress { led: u8 },
    /// Show the underglow ring as a volume gauge for a moment, then fade
    /// out. The actual level is read from `DISPLAY_STATE` at render time.
    VolumeChanged,
    /// The host just toggled mute on. Paint a red backdrop that fades to
    /// black across the underglow ring.
    Muted,
}

pub static LED_EVENTS: Channel<CriticalSectionRawMutex, LedCommand, 16> = Channel::new();

/// Outgoing device-to-host events (currently just the encoder-click notice).
/// Drained by `cdc_tx_task` and written as COBS-framed postcard packets over
/// CDC ACM.
pub static DEVICE_TX_EVENTS: Channel<CriticalSectionRawMutex, DeviceToHost, 8> = Channel::new();

#[derive(Clone, Copy)]
pub struct VolumeInfo {
    pub level: u8, // 0..=100
    pub muted: bool,
}

/// OLED visualizer style. To add a new style: add a variant, list it in `ALL`,
/// label it in `label()` / `long_label()`, and add a render arm in
/// `render_frame`.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum OledViz {
    Bars,
    Mirror,
    Dots,
    Off,
}

impl OledViz {
    pub const ALL: &'static [Self] = &[Self::Bars, Self::Mirror, Self::Dots, Self::Off];

    /// Short label (≤4 chars) for the right column of the main menu.
    pub fn label(self) -> &'static str {
        match self {
            Self::Bars => "bars",
            Self::Mirror => "mirr",
            Self::Dots => "dots",
            Self::Off => "off",
        }
    }

    /// Full label for the submenu list (room for longer text there).
    pub fn long_label(self) -> &'static str {
        match self {
            Self::Bars => "bars",
            Self::Mirror => "mirror",
            Self::Dots => "dots",
            Self::Off => "off",
        }
    }

    pub fn next(self) -> Self {
        cycle(Self::ALL, self, 1)
    }
    pub fn prev(self) -> Self {
        cycle(Self::ALL, self, -1)
    }
}

/// Underglow visualizer style. Same extension recipe as `OledViz`.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum GlowViz {
    Spec,
    Vu,
    Bass,
    Off,
}

impl GlowViz {
    pub const ALL: &'static [Self] = &[Self::Spec, Self::Vu, Self::Bass, Self::Off];

    pub fn label(self) -> &'static str {
        match self {
            Self::Spec => "spec",
            Self::Vu => "vu",
            Self::Bass => "bass",
            Self::Off => "off",
        }
    }

    pub fn long_label(self) -> &'static str {
        match self {
            Self::Spec => "spectrum",
            Self::Vu => "vu",
            Self::Bass => "bass",
            Self::Off => "off",
        }
    }

    pub fn next(self) -> Self {
        cycle(Self::ALL, self, 1)
    }
    pub fn prev(self) -> Self {
        cycle(Self::ALL, self, -1)
    }
}

/// Wrap-around step through a fixed slice of enum variants. Returns the first
/// element if `current` isn't found (shouldn't happen for our enums).
fn cycle<T: Copy + Eq>(all: &[T], current: T, step: i32) -> T {
    let len = all.len() as i32;
    let i = all.iter().position(|v| *v == current).unwrap_or(0) as i32;
    let next = ((i + step).rem_euclid(len)) as usize;
    all[next]
}

/// On-device menu navigation state. `Closed` = no menu, `Main` = top-level
/// list of selectors, `Sub` = a selector's submenu where the encoder cycles
/// the live value with no separate "draft" — current value *is* the selection.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum MenuView {
    Closed,
    Main,
    Sub(Selector),
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum Selector {
    OledViz,
    GlowViz,
}

/// Number of rows in the main menu (oled viz, glow viz, Bootloader). Used to
/// wrap encoder rotation when navigating the main menu.
pub const MENU_ITEM_COUNT: u8 = 3;

#[derive(Clone)]
pub struct DisplayState {
    pub volume: VolumeInfo,
    /// Last time we received any frame from the host. Used to flip the OLED
    /// to "Disconnected" when the daemon dies or the cable is unplugged.
    pub last_message: Instant,
    /// Latest spectrum bands from the host. `last_visualizer` gates whether
    /// they're considered fresh enough to render.
    pub bands: [u8; 8],
    pub last_visualizer: Instant,
    /// Active OLED visualizer style. `Off` blanks the left pane.
    pub oled_viz: OledViz,
    /// Active underglow visualizer style. `Off` keeps the ring dark.
    pub glow_viz: GlowViz,
    /// On-device menu state. Drives input routing in `matrix_task` /
    /// `encoder_task` and a full-screen replacement in `render_frame`.
    pub menu: MenuView,
    /// Currently highlighted main-menu row, `0..MENU_ITEM_COUNT`. Only
    /// meaningful when `menu == MenuView::Main`.
    pub main_selection: u8,
}

impl DisplayState {
    const fn new() -> Self {
        Self {
            volume: VolumeInfo {
                level: 0,
                muted: false,
            },
            last_message: Instant::from_ticks(0),
            bands: [0; 8],
            last_visualizer: Instant::from_ticks(0),
            oled_viz: OledViz::Bars,
            glow_viz: GlowViz::Spec,
            menu: MenuView::Closed,
            main_selection: 0,
        }
    }

    pub fn connected(&self) -> bool {
        // Treat the whole device as "never connected" until at least one frame
        // arrives — `last_message` starts at tick 0.
        self.last_message != Instant::from_ticks(0)
            && self.last_message.elapsed() < Duration::from_secs(5)
    }

    pub fn bands_fresh(&self) -> bool {
        self.last_visualizer != Instant::from_ticks(0)
            && self.last_visualizer.elapsed() < Duration::from_millis(500)
    }

    pub fn oled_viz_active(&self) -> bool {
        self.oled_viz != OledViz::Off && self.bands_fresh()
    }

    pub fn glow_viz_active(&self) -> bool {
        self.glow_viz != GlowViz::Off && self.bands_fresh()
    }

    pub fn menu_open(&self) -> bool {
        !matches!(self.menu, MenuView::Closed)
    }
}

pub static DISPLAY_STATE: blocking_mutex::Mutex<CriticalSectionRawMutex, RefCell<DisplayState>> =
    blocking_mutex::Mutex::new(RefCell::new(DisplayState::new()));

pub fn apply_host_message(msg: HostToDevice) {
    DISPLAY_STATE.lock(|state| {
        let mut s = state.borrow_mut();
        s.last_message = Instant::now();
        match msg {
            HostToDevice::Volume { level, muted } => {
                let prev_muted = s.volume.muted;
                s.volume = VolumeInfo { level, muted };
                let event = if muted && !prev_muted {
                    LedCommand::Muted
                } else {
                    LedCommand::VolumeChanged
                };
                let _ = LED_EVENTS.try_send(event);
            }
            HostToDevice::Ping => { /* timestamp already updated */ }
            HostToDevice::Visualizer { bands } => {
                s.bands = bands;
                s.last_visualizer = Instant::now();
            }
            HostToDevice::Hello { proto_version } => {
                if proto_version != proto::PROTO_VERSION {
                    defmt::warn!(
                        "host proto version {} != firmware {}; continuing",
                        proto_version,
                        proto::PROTO_VERSION
                    );
                }
                let _ = DEVICE_TX_EVENTS.try_send(DeviceToHost::Hello {
                    proto_version: proto::PROTO_VERSION,
                });
            }
        }
    });
}

/// Handle one encoder detent. Returns `true` if the menu consumed it (no HID
/// emitted), `false` if the menu was closed (caller should send Volume HID).
///
/// `step` is +1 for clockwise, -1 for counter-clockwise. In a submenu both
/// directions cycle the live enum value; on the main menu they move the
/// highlighted row.
pub fn apply_encoder_step(step: i8) -> bool {
    DISPLAY_STATE.lock(|s| {
        let mut s = s.borrow_mut();
        match s.menu {
            MenuView::Closed => false,
            MenuView::Main => {
                let n = MENU_ITEM_COUNT;
                if step >= 0 {
                    s.main_selection = (s.main_selection + 1) % n;
                } else {
                    s.main_selection = (s.main_selection + n - 1) % n;
                }
                true
            }
            MenuView::Sub(Selector::OledViz) => {
                s.oled_viz = if step >= 0 {
                    s.oled_viz.next()
                } else {
                    s.oled_viz.prev()
                };
                true
            }
            MenuView::Sub(Selector::GlowViz) => {
                s.glow_viz = if step >= 0 {
                    s.glow_viz.next()
                } else {
                    s.glow_viz.prev()
                };
                true
            }
        }
    })
}
