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

/// Wire-format version. Bumped when the meaning of an existing variant
/// changes, capacities shift, or `MAX_FRAME_LEN` moves. Appending a new
/// variant at the end of either enum is forward-compatible (postcard returns
/// `Err` on the older side, which both decoders already log+drop) and does
/// not require a version bump.
pub const PROTO_VERSION: u8 = 1;

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
    /// Sent by the host once per serial connect. The firmware echoes a
    /// `DeviceToHost::Hello` so the daemon can log version drift. Mismatch is
    /// non-fatal in v1 — both sides log and continue.
    Hello {
        proto_version: u8,
    },
}

/// Messages flowing device → host.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum DeviceToHost {
    Pong,
    EncoderClick,
    /// Reply to `HostToDevice::Hello`. Reports the version the firmware was
    /// compiled against.
    Hello {
        proto_version: u8,
    },
}
