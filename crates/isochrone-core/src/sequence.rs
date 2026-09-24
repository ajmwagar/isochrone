//! Counters that wrap, and comparing them without lying.
//!
//! RTP numbers packets with 16 bits and stamps them with 32. Both run out:
//! at 48kHz with 1ms packets the sequence wraps every 65 seconds and the
//! timestamp every 24 hours. Neither is an error — the protocol expects it,
//! and a receiver is supposed to keep working straight through.
//!
//! Which means `<` is wrong. Sequence 0 arriving after 65535 is the next
//! packet, not the oldest one in history, and a buffer that orders with
//! ordinary comparison will hold the entire stream back for a minute while
//! it waits for sixty-five thousand packets that already played. The failure
//! is not subtle when it happens; it is just rare enough to ship.
//!
//! So the ordering here is relative: RFC 1982 serial-number arithmetic, where
//! "newer" means "within half the space, forwards". That has a sharp edge
//! worth knowing about — exactly half a space apart, neither value is newer,
//! because the question genuinely has no answer. A stream that has drifted
//! that far apart is not out of order, it is a different stream.

/// A 16-bit RTP sequence number.
///
/// Ordering is deliberately not `Ord`: these do not have a total order, and
/// a type that pretends otherwise invites `sort()`, which is wrong here in a
/// way that looks fine in tests that never cross a wrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sequence(pub u16);

impl Sequence {
    /// Packets between `self` and `later`, forwards, across any wrap.
    ///
    /// Answers "how far ahead" rather than "which is bigger". A gap of one
    /// is consecutive; zero is a duplicate.
    #[must_use]
    pub fn distance_to(self, later: Self) -> u16 {
        later.0.wrapping_sub(self.0)
    }

    /// Is `other` newer than `self`?
    ///
    /// Newer means forwards by less than half the sequence space. Beyond
    /// that the answer is genuinely ambiguous and this reports `false`,
    /// which pushes the caller toward its reset path rather than
    /// letting it act on a guess.
    #[must_use]
    pub fn precedes(self, other: Self) -> bool {
        let forward = self.distance_to(other);
        forward != 0 && forward < 0x8000
    }

    #[must_use]
    pub fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

/// An RTP timestamp, counted in samples at the media clock rate.
///
/// Samples, not seconds. Everything a receiver needs to decide — is this
/// packet contiguous with the last, how much silence does a gap need — is
/// exact in samples and approximate in seconds, and the approximation is
/// what accumulates into drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Timestamp(pub u32);

impl Timestamp {
    /// Samples from `self` forward to `later`, across any wrap.
    #[must_use]
    pub fn distance_to(self, later: Self) -> u32 {
        later.0.wrapping_sub(self.0)
    }

    #[must_use]
    pub fn precedes(self, other: Self) -> bool {
        let forward = self.distance_to(other);
        forward != 0 && forward < 0x8000_0000
    }

    #[must_use]
    pub fn advance(self, samples: u32) -> Self {
        Self(self.0.wrapping_add(samples))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_ordering_works() {
        assert!(Sequence(1).precedes(Sequence(2)));
        assert!(!Sequence(2).precedes(Sequence(1)));
    }

    /// The bug this module exists for. Naive comparison says 0 is older than
    /// 65535 and a receiver stalls for a minute waiting for packets that
    /// already played.
    #[test]
    fn a_wrap_is_forward_motion_not_sixty_five_thousand_packets_backwards() {
        assert!(Sequence(65_535).precedes(Sequence(0)));
        assert!(Sequence(65_530).precedes(Sequence(4)));
        assert!(!Sequence(0).precedes(Sequence(65_535)));
        assert_eq!(Sequence(65_535).distance_to(Sequence(0)), 1);
        assert_eq!(Sequence(65_530).distance_to(Sequence(4)), 10);
    }

    /// A duplicate is not newer. Reporting it as newer makes a receiver
    /// play the same audio twice rather than discard it.
    #[test]
    fn a_packet_does_not_precede_itself() {
        assert!(!Sequence(42).precedes(Sequence(42)));
        assert_eq!(Sequence(42).distance_to(Sequence(42)), 0);
    }

    /// Exactly half the space apart, "newer" has no answer. Reporting false
    /// in both directions is what surfaces the ambiguity to the caller
    /// instead of resolving it by coin flip.
    #[test]
    fn half_a_space_apart_is_ambiguous_in_both_directions() {
        assert!(!Sequence(0).precedes(Sequence(0x8000)));
        assert!(!Sequence(0x8000).precedes(Sequence(0)));
    }

    /// One short of half is still ordered, which is where the boundary has
    /// to be for the ambiguity above to be the only gap.
    #[test]
    fn just_inside_half_a_space_is_still_ordered() {
        assert!(Sequence(0).precedes(Sequence(0x7fff)));
        assert!(!Sequence(0x7fff).precedes(Sequence(0)));
    }

    /// 32 bits at 48kHz wraps about once a day. A stream left running
    /// overnight crosses it, which is exactly when nobody is watching.
    #[test]
    fn timestamps_wrap_the_same_way() {
        let before = Timestamp(u32::MAX - 47);
        let after = before.advance(48);
        assert_eq!(after, Timestamp(0));
        assert!(before.precedes(after));
        assert_eq!(before.distance_to(after), 48);
    }

    /// Walking the whole space must stay consistent: every step forward is
    /// one packet, including the step over the boundary.
    #[test]
    fn stepping_through_a_wrap_stays_consecutive() {
        let mut sequence = Sequence(65_530);
        for _ in 0..12 {
            let next = sequence.next();
            assert_eq!(sequence.distance_to(next), 1, "at {sequence:?}");
            assert!(sequence.precedes(next));
            sequence = next;
        }
        assert_eq!(sequence, Sequence(6));
    }
}
