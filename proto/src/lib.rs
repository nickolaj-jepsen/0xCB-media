#![no_std]

//! Wire protocol shared between firmware and host daemon.
//!
//! Frames are postcard-COBS encoded over USB CDC ACM. A single zero byte
//! delimits frames in either direction.

use serde::{Deserialize, Serialize};

/// Maximum encoded length of any single frame, including the COBS overhead
/// byte and the trailing zero. Sized comfortably above the largest current
/// variant (a `Visualizer` with 8 bands).
pub const MAX_FRAME_LEN: usize = 256;

/// Messages flowing host → device.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum HostToDevice {
    Volume {
        level: u8, // 0..=100
        muted: bool,
    },
    Ping,
    /// 8 log-spaced spectrum band magnitudes (0..=255) covering ~40 Hz to
    /// 16 kHz. Sent at ~60 Hz while audio is flowing on the host's default
    /// sink; the firmware uses these to drive the OLED bars and the
    /// underglow ring. A gap of >500 ms drops the OLED bars.
    Visualizer {
        bands: [u8; 8],
    },
}

/// Messages flowing device → host.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum DeviceToHost {
    Pong,
    EncoderClick,
}
