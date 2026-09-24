//! Holding audio still long enough to play it evenly.
//!
//! Packets leave a sender evenly spaced and do not arrive that way. They
//! bunch, reorder, duplicate, and occasionally never arrive at all, and the
//! sound card underneath asks for its next block on a schedule that does not
//! care. The buffer between them is what converts one into the other, and it
//! buys that with latency: it is a deliberate delay, sized to the worst
//! lateness worth tolerating.
//!
//! # Samples, not packets
//!
//! Packets are how the audio arrives, not what it is. A queue of packets has
//! to special-case reordering, duplicates, partial overlap and gaps
//! separately, and each case is a place to be wrong.
//!
//! So this addresses a ring of *samples* by RTP timestamp. Where a packet
//! lands is arithmetic; a reordered packet lands behind a later one that
//! already arrived, a duplicate lands on top of itself, and a gap is simply
//! a span nothing was written to. One mechanism, and the awkward cases stop
//! being cases.
//!
//! # What it refuses to do
//!
//! It never stalls to wait for a packet. A late packet has missed its
//! moment: the sound card has already been handed that instant, and holding
//! the stream to insert it converts one lost millisecond into a permanent
//! millisecond of added latency for every packet after it. Late audio is
//! discarded and the loss is reported.
//!
//! It also never plays a gap as though it were audio. A hole reads as
//! silence and says so, so the servo above is not fed a fill level that is
//! really absence — which would have it correct for drift that is not there.

use crate::{StreamFormat, sequence::Timestamp};

/// What happened to a packet offered to the buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accepted {
    /// Written at its timestamp.
    Written { frames: usize },
    /// Already had these samples. Networks duplicate; this is not an error.
    Duplicate,
    /// Its instant has already been played. Reported rather than silently
    /// dropped: sustained lateness means the buffer is too small for this
    /// path, which is a decision for the operator, not the buffer.
    TooLate { by_frames: u32 },
    /// So far from the playhead that it cannot be the same stream
    /// continuing — a restart, a rate change, or a stray sender. The caller
    /// resets rather than the buffer guessing which.
    Discontinuous,
}

/// What came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Played {
    /// Real audio.
    Audio { frames: usize },
    /// Nothing had been written for this span; silence was produced.
    Concealed { frames: usize },
}

/// A sample ring addressed by media timestamp.
pub struct Playout {
    format: StreamFormat,
    /// Interleaved samples, `capacity_frames * channels` long.
    ring: Vec<f32>,
    /// Which frames hold real audio. Without this a gap is indistinguishable
    /// from genuine digital silence, and concealment would either never fire
    /// or fire constantly on quiet passages.
    written: Vec<bool>,
    capacity_frames: usize,
    /// The next frame to hand to the device.
    playhead: Timestamp,
    started: bool,
}

impl Playout {
    /// Capacity is the widest lateness tolerable, not the target depth. The
    /// servo aims for a fraction of it so there is room to absorb a burst
    /// from above and a gap from below.
    #[must_use]
    pub fn new(format: StreamFormat, capacity_frames: usize) -> Self {
        Self {
            ring: vec![0.0; capacity_frames * format.channels as usize],
            written: vec![false; capacity_frames],
            capacity_frames,
            format,
            playhead: Timestamp(0),
            started: false,
        }
    }

    /// Frames of real audio queued ahead of the playhead.
    ///
    /// Counts only what was actually written, so a hole does not read as
    /// depth. Stops at the first gap, because what matters to the servo is
    /// how much can be played without concealment, not how much is scattered
    /// across the ring.
    #[must_use]
    pub fn fill_frames(&self) -> usize {
        (0..self.capacity_frames)
            .take_while(|offset| self.written[self.slot(self.playhead.advance(*offset as u32))])
            .count()
    }

    #[must_use]
    pub fn fill(&self) -> core::time::Duration {
        core::time::Duration::from_nanos(
            self.fill_frames() as u64 * 1_000_000_000 / u64::from(self.format.sample_rate_hz),
        )
    }

    fn slot(&self, timestamp: Timestamp) -> usize {
        timestamp.0 as usize % self.capacity_frames
    }

    /// Start (or restart) at this timestamp, discarding anything held.
    pub fn reset_to(&mut self, timestamp: Timestamp) {
        self.written.fill(false);
        self.playhead = timestamp;
        self.started = true;
    }

    /// Offer one packet's interleaved samples, stamped at `timestamp`.
    pub fn insert(&mut self, timestamp: Timestamp, samples: &[f32]) -> Accepted {
        let channels = self.format.channels as usize;
        let frames = samples.len() / channels;
        if frames == 0 {
            return Accepted::Written { frames: 0 };
        }

        // The first packet defines where the stream starts; there is nothing
        // to be late for yet.
        if !self.started {
            self.reset_to(timestamp);
        }

        let ahead = self.playhead.distance_to(timestamp);
        // distance_to is forwards-only, so lateness shows up as an enormous
        // forward distance. Anything past half the timestamp space is behind
        // us, and the true lateness is the complement.
        if ahead >= 0x8000_0000 {
            return Accepted::TooLate {
                by_frames: self.playhead.0.wrapping_sub(timestamp.0),
            };
        }
        if ahead as usize + frames > self.capacity_frames {
            return Accepted::Discontinuous;
        }

        // A duplicate is a packet whose every frame is already present.
        // Checked before writing so a retransmission cannot overwrite audio
        // the device is about to read.
        let already = (0..frames).all(|frame| {
            self.written[self.slot(timestamp.advance(frame as u32))]
        });
        if already {
            return Accepted::Duplicate;
        }

        for frame in 0..frames {
            let slot = self.slot(timestamp.advance(frame as u32));
            self.written[slot] = true;
            let dst = slot * channels;
            let src = frame * channels;
            self.ring[dst..dst + channels].copy_from_slice(&samples[src..src + channels]);
        }
        Accepted::Written { frames }
    }

    /// Take the next `frames` for the device, concealing anything missing.
    ///
    /// Always fills the output. A device asking for a block gets a block:
    /// returning short would leave the callback to invent something, and it
    /// has less context to invent well.
    pub fn play(&mut self, frames: usize, out: &mut Vec<f32>) -> Played {
        let channels = self.format.channels as usize;
        out.clear();
        out.reserve(frames * channels);

        let mut concealed = 0usize;
        for frame in 0..frames {
            let timestamp = self.playhead.advance(frame as u32);
            let slot = self.slot(timestamp);
            if self.written[slot] {
                let src = slot * channels;
                out.extend_from_slice(&self.ring[src..src + channels]);
                // Consumed: leaving it marked would let a wrap replay it.
                self.written[slot] = false;
            } else {
                out.extend(core::iter::repeat_n(0.0, channels));
                concealed += 1;
            }
        }
        self.playhead = self.playhead.advance(frames as u32);

        if concealed == 0 {
            Played::Audio { frames }
        } else {
            Played::Concealed { frames: concealed }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::Encoding;

    fn format() -> StreamFormat {
        StreamFormat {
            sample_rate_hz: 48_000,
            channels: 2,
            encoding: Encoding::L24,
            frames_per_packet: 48,
        }
    }

    /// Interleaved stereo ramp, distinct per frame so ordering is visible.
    fn packet(first: u32, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|frame| {
                let value = (first + frame as u32) as f32;
                [value, -value]
            })
            .collect()
    }

    fn playout() -> Playout {
        Playout::new(format(), 960) // 20ms at 48kHz
    }

    #[test]
    fn packets_in_order_play_back_in_order() {
        let mut buffer = playout();
        buffer.insert(Timestamp(1_000), &packet(0, 48));
        buffer.insert(Timestamp(1_048), &packet(48, 48));

        let mut out = Vec::new();
        assert_eq!(buffer.play(96, &mut out), Played::Audio { frames: 96 });
        assert_eq!(out[0], 0.0);
        assert_eq!(out[2], 1.0);
        assert_eq!(out[190], 95.0);
    }

    /// The reason for addressing samples rather than queueing packets: a
    /// packet arriving behind one already held simply lands in its place.
    #[test]
    fn a_reordered_packet_lands_where_it_belongs() {
        let mut buffer = playout();
        buffer.insert(Timestamp(1_000), &packet(0, 48));
        // Third arrives before the second.
        buffer.insert(Timestamp(1_096), &packet(96, 48));
        buffer.insert(Timestamp(1_048), &packet(48, 48));

        let mut out = Vec::new();
        assert_eq!(buffer.play(144, &mut out), Played::Audio { frames: 144 });
        for frame in 0..144 {
            assert_eq!(out[frame * 2], frame as f32, "frame {frame}");
        }
    }

    /// Networks duplicate. Overwriting would be harmless here and is not in
    /// general -- a retransmission must not land on audio the device is
    /// about to read.
    #[test]
    fn a_duplicate_is_recognised_rather_than_rewritten() {
        let mut buffer = playout();
        buffer.insert(Timestamp(1_000), &packet(0, 48));
        assert_eq!(
            buffer.insert(Timestamp(1_000), &packet(500, 48)),
            Accepted::Duplicate
        );
        let mut out = Vec::new();
        buffer.play(48, &mut out);
        assert_eq!(out[0], 0.0, "the duplicate overwrote live audio");
    }

    /// Waiting for a late packet converts one lost millisecond into a
    /// permanent millisecond of added latency for everything after it.
    #[test]
    fn a_late_packet_is_dropped_and_the_loss_reported() {
        let mut buffer = playout();
        buffer.insert(Timestamp(1_000), &packet(0, 48));
        let mut out = Vec::new();
        buffer.play(48, &mut out);

        // Now arrives, 48 frames after its moment passed.
        assert_eq!(
            buffer.insert(Timestamp(1_000), &packet(0, 48)),
            Accepted::TooLate { by_frames: 48 }
        );
    }

    /// A hole must read as silence and say so. If it read as audio the servo
    /// above would see depth that is not there and correct for drift that is
    /// not happening.
    #[test]
    fn a_gap_is_concealed_and_not_counted_as_depth() {
        let mut buffer = playout();
        buffer.insert(Timestamp(1_000), &packet(0, 48));
        // 1_048 never arrives.
        buffer.insert(Timestamp(1_096), &packet(96, 48));

        assert_eq!(buffer.fill_frames(), 48, "the gap must stop the count");

        let mut out = Vec::new();
        assert_eq!(buffer.play(96, &mut out), Played::Concealed { frames: 48 });
        assert_eq!(out[0], 0.0);
        assert_eq!(out[96], 0.0, "concealed span is silence");
    }

    /// Genuine digital silence is audio and must not be mistaken for a hole,
    /// or concealment fires constantly on quiet passages.
    #[test]
    fn real_silence_is_not_mistaken_for_a_gap() {
        let mut buffer = playout();
        buffer.insert(Timestamp(1_000), &vec![0.0; 48 * 2]);
        let mut out = Vec::new();
        assert_eq!(buffer.play(48, &mut out), Played::Audio { frames: 48 });
    }

    /// 32-bit timestamps wrap about once a day, and a stream left running
    /// overnight crosses it while nobody is watching.
    #[test]
    fn playback_continues_across_a_timestamp_wrap() {
        let mut buffer = playout();
        let before = Timestamp(u32::MAX - 23);
        buffer.insert(before, &packet(0, 24));
        buffer.insert(Timestamp(0), &packet(24, 24));

        let mut out = Vec::new();
        assert_eq!(buffer.play(48, &mut out), Played::Audio { frames: 48 });
        for frame in 0..48 {
            assert_eq!(out[frame * 2], frame as f32, "frame {frame}");
        }
    }

    /// Too far ahead is not a very early packet; it is a different stream.
    /// The buffer says so rather than writing it somewhere plausible.
    #[test]
    fn a_wildly_future_packet_is_reported_as_discontinuous() {
        let mut buffer = playout();
        buffer.insert(Timestamp(1_000), &packet(0, 48));
        assert_eq!(
            buffer.insert(Timestamp(500_000), &packet(0, 48)),
            Accepted::Discontinuous
        );
    }

    /// Consumed samples must not be replayed when the ring wraps around to
    /// their slots again.
    #[test]
    fn played_samples_are_not_replayed_after_the_ring_wraps() {
        let mut buffer = playout();
        let mut out = Vec::new();
        buffer.insert(Timestamp(0), &packet(0, 48));
        buffer.play(48, &mut out);

        // A full lap of the 960-frame ring later, nothing was written.
        buffer.playhead = Timestamp(960);
        assert_eq!(buffer.play(48, &mut out), Played::Concealed { frames: 48 });
    }

    #[test]
    fn fill_is_reported_in_time_as_well_as_frames() {
        let mut buffer = playout();
        buffer.insert(Timestamp(1_000), &packet(0, 480));
        assert_eq!(buffer.fill_frames(), 480);
        assert_eq!(buffer.fill(), core::time::Duration::from_millis(10));
    }
}
