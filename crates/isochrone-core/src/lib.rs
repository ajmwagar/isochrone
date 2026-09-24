//! Wire format and media-clock arithmetic for AES67-style audio streams.
//!
//! What a receiver needs before it can hold any opinion about timing: how to
//! read a packet, and how to compare two counters that wrap. Both are small
//! and both are where the bugs that survive testing live -- byte order that
//! is wrong identically on both ends, and sequence comparison that works
//! until sixty-five seconds in.
//!
//! No I/O and no threads. The transport and the playout buffer sit above
//! this; keeping the parsing and the arithmetic separate is what lets them
//! be tested exhaustively without a network or a sound card.

pub mod packet;
pub mod sequence;

pub use packet::{Encoding, Header, ParseError};
pub use sequence::{Sequence, Timestamp};

/// A stream's unchanging shape.
///
/// Carried together because these four are only meaningful as a set: frames
/// per packet is derived from the packet time and the rate, and a receiver
/// that learns them separately can hold a coherent-looking combination that
/// no sender ever sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFormat {
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub encoding: Encoding,
    /// Frames per packet, per channel. AES67 at 48kHz uses 48 (1ms) or 6
    /// (125us); the latter is the low-latency profile and costs eight times
    /// the packet rate to save 875 microseconds.
    pub frames_per_packet: u32,
}

impl StreamFormat {
    /// The 1ms AES67 baseline: 48kHz, stereo, L24.
    #[must_use]
    pub fn aes67_48k_stereo() -> Self {
        Self {
            sample_rate_hz: 48_000,
            channels: 2,
            encoding: Encoding::L24,
            frames_per_packet: 48,
        }
    }

    /// Payload bytes one packet carries.
    #[must_use]
    pub fn payload_bytes(self) -> usize {
        self.frames_per_packet as usize
            * self.channels as usize
            * self.encoding.bytes_per_sample()
    }

    /// How much time one packet represents.
    ///
    /// Derived rather than configured, because a packet time that disagrees
    /// with the frames actually in the packet is a receiver that drifts by
    /// construction -- and it drifts smoothly, so it looks like clock error
    /// rather than arithmetic.
    #[must_use]
    pub fn packet_duration(self) -> core::time::Duration {
        core::time::Duration::from_nanos(
            u64::from(self.frames_per_packet) * 1_000_000_000 / u64::from(self.sample_rate_hz),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_aes67_baseline_is_one_millisecond_of_stereo_l24() {
        let format = StreamFormat::aes67_48k_stereo();
        assert_eq!(format.payload_bytes(), 288);
        assert_eq!(format.packet_duration(), core::time::Duration::from_millis(1));
        assert_eq!(
            packet::frames_in_payload(format.payload_bytes(), 2, Encoding::L24),
            Ok(48)
        );
    }

    /// The low-latency profile: eight times the packet rate to save 875us.
    #[test]
    fn the_125_microsecond_profile_is_consistent_too() {
        let format = StreamFormat {
            frames_per_packet: 6,
            ..StreamFormat::aes67_48k_stereo()
        };
        assert_eq!(format.payload_bytes(), 36);
        assert_eq!(
            format.packet_duration(),
            core::time::Duration::from_nanos(125_000)
        );
    }

    /// Rates that do not divide evenly must not silently round: at 44.1kHz a
    /// truncated packet duration accumulates into exactly the drift this
    /// project exists to remove.
    #[test]
    fn an_awkward_rate_keeps_nanosecond_precision() {
        let format = StreamFormat {
            sample_rate_hz: 44_100,
            frames_per_packet: 441,
            ..StreamFormat::aes67_48k_stereo()
        };
        assert_eq!(format.packet_duration(), core::time::Duration::from_millis(10));
    }
}
