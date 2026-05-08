#![no_std]

//! Wire protocol shared between firmware and host daemon.
//!
//! Frames are postcard-COBS encoded over USB CDC ACM. A single zero byte
//! delimits frames in either direction.

use heapless::String;
use serde::{Deserialize, Serialize};

/// Maximum encoded length of any single frame, including the COBS overhead
/// byte and the trailing zero. Sized so a [`HostToDevice::NowPlaying`] with
/// fully-utilised title and artist always fits.
pub const MAX_FRAME_LEN: usize = 256;

/// Messages flowing host → device.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum HostToDevice {
    NowPlaying {
        title: String<64>,
        artist: String<32>,
        is_playing: bool,
    },
    Volume {
        level: u8, // 0..=100
        muted: bool,
    },
    Clear,
    Ping,
}

/// Messages flowing device → host.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum DeviceToHost {
    Pong,
    EncoderClick,
}
