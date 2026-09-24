//! The RTP header and the samples under it.
//!
//! Both are big-endian, and every machine this will ever run on is not.
//! That mismatch is the single most productive source of bugs in audio
//! networking, because getting it wrong does not fail: a byte-swapped 24-bit
//! sample is still a number, still in range, and still plays. It just sounds
//! like violent noise, and it sounds like violent noise identically on both
//! ends, so the two implementations agree with each other and disagree with
//! everyone else's.
//!
//! So the conversions here are written out a byte at a time rather than
//! transmuted. It is not slower in any way that matters at 48kHz, and it
//! cannot silently inherit the host's opinion about byte order.

use crate::sequence::{Sequence, Timestamp};

/// Fixed RTP header length, RFC 3550. Contributing sources would extend it;
/// this rejects them rather than parsing a variable header it will never
/// legitimately receive from an audio sender.
pub const HEADER_BYTES: usize = 12;

/// RTP version 2, the only version in use since 1996.
const VERSION: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// Fewer bytes than a header.
    Truncated { got: usize },
    /// Not RTP version 2.
    UnsupportedVersion(u8),
    /// Padding, extensions or CSRCs. Legal RTP, but an AES67-style audio
    /// sender emits none of them, so their presence means this is some other
    /// stream and guessing at its layout would produce noise.
    UnsupportedLayout,
    /// The payload does not divide into whole frames -- a truncated packet,
    /// or the wrong channel count or sample format for this stream.
    PartialFrame { payload: usize, frame: usize },
}

/// How samples are carried on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// 16-bit linear PCM, big-endian, RFC 3551.
    L16,
    /// 24-bit linear PCM, big-endian, RFC 3190. AES67's baseline.
    L24,
}

impl Encoding {
    #[must_use]
    pub fn bytes_per_sample(self) -> usize {
        match self {
            Self::L16 => 2,
            Self::L24 => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub payload_type: u8,
    pub sequence: Sequence,
    pub timestamp: Timestamp,
    pub ssrc: u32,
    /// Set on the first packet after a gap, per RFC 3551. A receiver may use
    /// it to stop concealing rather than waiting to infer the resumption.
    pub marker: bool,
}

impl Header {
    /// Write the 12-byte header.
    #[must_use]
    pub fn encode(&self) -> [u8; HEADER_BYTES] {
        let mut out = [0u8; HEADER_BYTES];
        out[0] = VERSION << 6;
        out[1] = (u8::from(self.marker) << 7) | (self.payload_type & 0x7f);
        out[2..4].copy_from_slice(&self.sequence.0.to_be_bytes());
        out[4..8].copy_from_slice(&self.timestamp.0.to_be_bytes());
        out[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
        out
    }

    /// Read a header, returning it with the payload that follows.
    pub fn decode(bytes: &[u8]) -> Result<(Self, &[u8]), ParseError> {
        if bytes.len() < HEADER_BYTES {
            return Err(ParseError::Truncated { got: bytes.len() });
        }
        let version = bytes[0] >> 6;
        if version != VERSION {
            return Err(ParseError::UnsupportedVersion(version));
        }
        // Padding, extension, or any contributing sources. Each would change
        // where the payload starts, and an audio sender sets none of them.
        let padding = bytes[0] & 0b0010_0000 != 0;
        let extension = bytes[0] & 0b0001_0000 != 0;
        let csrc_count = bytes[0] & 0b0000_1111;
        if padding || extension || csrc_count != 0 {
            return Err(ParseError::UnsupportedLayout);
        }
        let header = Self {
            marker: bytes[1] & 0x80 != 0,
            payload_type: bytes[1] & 0x7f,
            sequence: Sequence(u16::from_be_bytes([bytes[2], bytes[3]])),
            timestamp: Timestamp(u32::from_be_bytes([
                bytes[4], bytes[5], bytes[6], bytes[7],
            ])),
            ssrc: u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
        };
        Ok((header, &bytes[HEADER_BYTES..]))
    }
}

/// Samples in one packet, per channel.
///
/// Reported rather than assumed because it is what advances the timestamp,
/// and a receiver that assumes a packet time will mis-place every packet
/// from a sender configured differently -- silently, since the audio still
/// decodes.
pub fn frames_in_payload(
    payload: usize,
    channels: usize,
    encoding: Encoding,
) -> Result<usize, ParseError> {
    let frame = channels * encoding.bytes_per_sample();
    if frame == 0 || payload % frame != 0 {
        return Err(ParseError::PartialFrame { payload, frame });
    }
    Ok(payload / frame)
}

/// Decode interleaved big-endian PCM into normalised samples.
///
/// f32 in -1.0..=1.0 because that is what every resampler and audio callback
/// wants, and converting once at the edge beats converting repeatedly in the
/// middle.
pub fn decode_samples(
    payload: &[u8],
    encoding: Encoding,
    out: &mut Vec<f32>,
) -> Result<(), ParseError> {
    let width = encoding.bytes_per_sample();
    if payload.len() % width != 0 {
        return Err(ParseError::PartialFrame {
            payload: payload.len(),
            frame: width,
        });
    }
    out.clear();
    out.reserve(payload.len() / width);
    for chunk in payload.chunks_exact(width) {
        let value = match encoding {
            Encoding::L16 => f32::from(i16::from_be_bytes([chunk[0], chunk[1]])) / 32_768.0,
            Encoding::L24 => {
                // Sign-extend 24 bits into 32. Shifting the three bytes into
                // the HIGH end and arithmetic-shifting back down does that
                // for free; assembling them in the low end instead reads
                // every negative sample as a large positive one, which is
                // full-scale noise on exactly half the waveform.
                let raw = (i32::from(chunk[0]) << 24)
                    | (i32::from(chunk[1]) << 16)
                    | (i32::from(chunk[2]) << 8);
                (raw >> 8) as f32 / 8_388_608.0
            }
        };
        out.push(value);
    }
    Ok(())
}

/// Encode normalised samples as interleaved big-endian PCM.
///
/// Clamps rather than wrapping. A sample arriving slightly over full scale
/// is a rounding artefact; wrapping it turns the loudest moment of the
/// programme into its opposite, which is the worst possible click.
pub fn encode_samples(samples: &[f32], encoding: Encoding, out: &mut Vec<u8>) {
    out.clear();
    out.reserve(samples.len() * encoding.bytes_per_sample());
    for &sample in samples {
        let clamped = sample.clamp(-1.0, 1.0);
        match encoding {
            Encoding::L16 => {
                let value = (clamped * 32_767.0).round() as i16;
                out.extend_from_slice(&value.to_be_bytes());
            }
            Encoding::L24 => {
                let value = (clamped * 8_388_607.0).round() as i32;
                let bytes = value.to_be_bytes();
                // Drop the sign-extension byte; the remaining three carry
                // the value and its sign in two's complement.
                out.extend_from_slice(&bytes[1..4]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Header {
        Header {
            payload_type: 97,
            sequence: Sequence(0x1234),
            timestamp: Timestamp(0xDEAD_BEEF),
            ssrc: 0x0BAD_F00D,
            marker: false,
        }
    }

    /// Pinned against the wire layout rather than against the decoder, or
    /// the pair could agree on something nobody else can read.
    #[test]
    fn the_header_is_big_endian_on_the_wire() {
        let bytes = header().encode();
        assert_eq!(bytes[0], 0x80, "version 2, no padding/extension/csrc");
        assert_eq!(bytes[1], 97, "marker clear, payload type 97");
        assert_eq!(&bytes[2..4], &[0x12, 0x34]);
        assert_eq!(&bytes[4..8], &[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(&bytes[8..12], &[0x0B, 0xAD, 0xF0, 0x0D]);
    }

    #[test]
    fn a_header_survives_a_round_trip_with_its_payload() {
        let mut wire = header().encode().to_vec();
        wire.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
        let (decoded, payload) = Header::decode(&wire).unwrap();
        assert_eq!(decoded, header());
        assert_eq!(payload, &[1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn the_marker_bit_does_not_leak_into_the_payload_type() {
        let marked = Header {
            marker: true,
            ..header()
        };
        let bytes = marked.encode();
        assert_eq!(bytes[1], 0x80 | 97);
        let (decoded, _) = Header::decode(&bytes).unwrap();
        assert!(decoded.marker);
        assert_eq!(decoded.payload_type, 97);
    }

    /// Legal RTP this sender never emits. Parsing it as if the payload began
    /// at byte 12 would feed the decoder header bytes as audio.
    #[test]
    fn layouts_that_move_the_payload_are_refused_not_guessed_at() {
        for first_byte in [0x80 | 0x20, 0x80 | 0x10, 0x80 | 0x01] {
            let mut bytes = header().encode();
            bytes[0] = first_byte;
            assert_eq!(
                Header::decode(&bytes),
                Err(ParseError::UnsupportedLayout),
                "first byte {first_byte:#04x}"
            );
        }
    }

    #[test]
    fn short_and_wrong_version_headers_are_rejected() {
        assert_eq!(
            Header::decode(&[0x80, 97, 0, 1]),
            Err(ParseError::Truncated { got: 4 })
        );
        let mut bytes = header().encode();
        bytes[0] = 0x40;
        assert_eq!(
            Header::decode(&bytes),
            Err(ParseError::UnsupportedVersion(1))
        );
    }

    /// The sign-extension trap: assembling three bytes in the low end reads
    /// every negative sample as a large positive one -- full-scale noise on
    /// half the waveform, which still "decodes".
    #[test]
    fn negative_l24_samples_survive_sign_extension() {
        // -1.0, -0.5, 0.0, +0.5 at 24-bit full scale.
        let wire = [
            0x80, 0x00, 0x00, // -8388608
            0xC0, 0x00, 0x00, // -4194304
            0x00, 0x00, 0x00, // 0
            0x40, 0x00, 0x00, // +4194304
        ];
        let mut out = Vec::new();
        decode_samples(&wire, Encoding::L24, &mut out).unwrap();
        assert!((out[0] + 1.0).abs() < 1e-6, "{}", out[0]);
        assert!((out[1] + 0.5).abs() < 1e-6, "{}", out[1]);
        assert!(out[2].abs() < 1e-9);
        assert!((out[3] - 0.5).abs() < 1e-6, "{}", out[3]);
    }

    #[test]
    fn samples_round_trip_through_both_encodings() {
        let samples = [-1.0_f32, -0.25, 0.0, 0.25, 0.75];
        for (encoding, tolerance) in [(Encoding::L16, 1e-4), (Encoding::L24, 1e-6)] {
            let mut wire = Vec::new();
            encode_samples(&samples, encoding, &mut wire);
            assert_eq!(wire.len(), samples.len() * encoding.bytes_per_sample());
            let mut back = Vec::new();
            decode_samples(&wire, encoding, &mut back).unwrap();
            for (before, after) in samples.iter().zip(&back) {
                assert!(
                    (before - after).abs() < tolerance,
                    "{encoding:?}: {before} -> {after}"
                );
            }
        }
    }

    /// Wrapping an over-range sample turns the loudest moment of the
    /// programme into its opposite -- the worst click available.
    #[test]
    fn over_range_samples_clamp_rather_than_wrap() {
        let mut wire = Vec::new();
        encode_samples(&[2.0, -2.0], Encoding::L24, &mut wire);
        let mut back = Vec::new();
        decode_samples(&wire, Encoding::L24, &mut back).unwrap();
        assert!(back[0] > 0.99, "{}", back[0]);
        assert!(back[1] < -0.99, "{}", back[1]);
    }

    /// A payload that does not divide evenly is the wrong channel count or a
    /// truncated packet. Both decode into plausible garbage if not caught.
    #[test]
    fn a_partial_frame_is_an_error_rather_than_a_shrug() {
        assert_eq!(frames_in_payload(288, 2, Encoding::L24), Ok(48));
        assert_eq!(frames_in_payload(192, 2, Encoding::L16), Ok(48));
        assert_eq!(
            frames_in_payload(287, 2, Encoding::L24),
            Err(ParseError::PartialFrame {
                payload: 287,
                frame: 6
            })
        );
        assert!(decode_samples(&[0, 0], Encoding::L24, &mut Vec::new()).is_err());
    }

    /// AES67's packet times at 48kHz: 1ms is 48 frames, 125us is 6.
    #[test]
    fn aes67_packet_times_frame_as_expected() {
        assert_eq!(frames_in_payload(48 * 2 * 3, 2, Encoding::L24), Ok(48));
        assert_eq!(frames_in_payload(6 * 2 * 3, 2, Encoding::L24), Ok(6));
    }
}
