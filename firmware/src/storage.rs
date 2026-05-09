//! Persistent settings stored in the QSPI flash tail.
//!
//! Layout: 16 KB at the end of flash, carved out in `memory.x` and exposed
//! as the flash-relative offsets `__config_start..__config_end`. One record
//! under `KEY_SETTINGS` holds the entire `Settings` struct, postcard-encoded.

use core::ops::Range;

use defmt::{info, warn};
use embassy_rp::flash::{Async, Flash};
use embassy_rp::peripherals::FLASH;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use sequential_storage::cache::NoCache;
use sequential_storage::map::{self, SerializationError, Value};
use serde::{Deserialize, Serialize};
use static_cell::StaticCell;

use crate::state::{GlowViz, HueMode, OledViz};

pub const FLASH_SIZE: usize = 4 * 1024 * 1024;
const SCHEMA_VERSION: u8 = 1;
const KEY_SETTINGS: u8 = 0;
const SCRATCH_LEN: usize = 128;

pub type SharedFlash = Mutex<CriticalSectionRawMutex, Flash<'static, FLASH, Async, FLASH_SIZE>>;

/// Persisted slice of `DisplayState`. `version` lets future schema changes
/// detect old blobs and fall back to defaults rather than mis-parse them.
#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    pub version: u8,
    pub oled_viz: OledViz,
    pub glow_viz: GlowViz,
    pub hue_mode: HueMode,
}

impl Settings {
    pub const fn new(oled_viz: OledViz, glow_viz: GlowViz, hue_mode: HueMode) -> Self {
        Self {
            version: SCHEMA_VERSION,
            oled_viz,
            glow_viz,
            hue_mode,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self::new(OledViz::Bars, GlowViz::Spec, HueMode::Static)
    }
}

impl<'a> Value<'a> for Settings {
    fn serialize_into(&self, buffer: &mut [u8]) -> Result<usize, SerializationError> {
        postcard::to_slice(self, buffer)
            .map(|s| s.len())
            .map_err(|_| SerializationError::BufferTooSmall)
    }

    fn deserialize_from(buffer: &'a [u8]) -> Result<Self, SerializationError> {
        postcard::from_bytes(buffer).map_err(|_| SerializationError::InvalidFormat)
    }
}

extern "C" {
    static __config_start: u32;
    static __config_end: u32;
}

fn config_range() -> Range<u32> {
    // `addr_of!` returns the symbol's address without materialising a
    // reference to the (uninitialised) extern static.
    let start = core::ptr::addr_of!(__config_start) as u32;
    let end = core::ptr::addr_of!(__config_end) as u32;
    start..end
}

static FLASH_CELL: StaticCell<SharedFlash> = StaticCell::new();

/// Park the flash driver behind a `&'static Mutex<…>` shared by `main`
/// (load on boot) and `matrix_task` (save on menu exit).
pub fn init(flash: Flash<'static, FLASH, Async, FLASH_SIZE>) -> &'static SharedFlash {
    FLASH_CELL.init(Mutex::new(flash))
}

/// Read the persisted settings, falling back to defaults on empty / corrupt
/// / version-mismatch / I/O error. Never panics — a missed read beats a
/// brick.
pub async fn load(flash: &'static SharedFlash) -> Settings {
    let mut buf = [0u8; SCRATCH_LEN];
    let mut guard = flash.lock().await;
    match map::fetch_item::<u8, Settings, _>(
        &mut *guard,
        config_range(),
        &mut NoCache::new(),
        &mut buf,
        &KEY_SETTINGS,
    )
    .await
    {
        Ok(Some(s)) if s.version == SCHEMA_VERSION => {
            info!("settings: loaded v{}", s.version);
            s
        }
        Ok(Some(s)) => {
            warn!("settings: unknown schema v{}, using defaults", s.version);
            Settings::default()
        }
        Ok(None) => {
            info!("settings: none stored, using defaults");
            Settings::default()
        }
        Err(e) => {
            warn!("settings: load failed ({:?}), using defaults", e);
            Settings::default()
        }
    }
}

/// Persist `settings`. Logs and returns on error rather than panicking.
pub async fn save(flash: &'static SharedFlash, settings: &Settings) {
    let mut buf = [0u8; SCRATCH_LEN];
    let mut guard = flash.lock().await;
    match map::store_item(
        &mut *guard,
        config_range(),
        &mut NoCache::new(),
        &mut buf,
        &KEY_SETTINGS,
        settings,
    )
    .await
    {
        Ok(()) => info!("settings: saved"),
        Err(e) => warn!("settings: save failed ({:?})", e),
    }
}
