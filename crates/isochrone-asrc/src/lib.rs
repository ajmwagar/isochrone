//! Absorbing the difference between two clocks that will never agree.
//!
//! # The problem
//!
//! Audio crosses a network from a converter running on one crystal to a
//! converter running on another. Nominally both are 48kHz. Actually they
//! differ by a few parts per million, and neither can be told to stop.
//!
//! A few ppm sounds harmless and is not. At 10ppm the sender produces about
//! one extra sample every two seconds; over an hour that is eighteen hundred
//! samples the receiver must either invent or discard. The buffer between
//! them drains or overflows on a schedule, and the failure is audible: a
//! click, then silence, then a click.
//!
//! No amount of clock *measurement* fixes this. Knowing precisely how far
//! apart two free-running crystals are does not bring them together. PTP is
//! worth building when something downstream can be disciplined; when the
//! endpoint is a USB converter locked to its own oscillator or to a host
//! controller's frame timing, there is nothing to steer. What is left is to
//! change the number of samples, continuously and quietly, which is what
//! this crate governs.
//!
//! # What this is, and what it is not
//!
//! This is the governor, not the resampler. It answers one question -- given
//! how full the playout buffer is right now, what ratio should the resampler
//! run at -- and it answers it slowly on purpose. The arithmetic of turning
//! ratio into samples belongs to whatever resampler is chosen; the decision
//! of *which* ratio is where the audible mistakes live, so it is separated,
//! and it is testable without audio.
//!
//! # Why the correction has to be slow
//!
//! Resampling ratio is pitch. A correction applied abruptly is a pitch step,
//! and pitch steps are the one artifact ears are exceptionally good at: a
//! listener who would never notice 3ms of added latency will hear a 50ppm
//! jump as a flutter. So the servo is deliberately sluggish, the correction
//! is clamped well below audibility, and a buffer that is drifting is
//! allowed to keep drifting for a while rather than being yanked back.
//!
//! The cost of slowness is that a genuine step change -- a receiver that
//! restarts, a sender that reconnects -- takes a long time to settle. That
//! is not a tuning failure; it is a different situation, and callers should
//! reset rather than wait for the servo to crawl there. See [`Servo::reset`].

use core::time::Duration;

/// Parts per million, the unit clock error is actually discussed in.
///
/// Kept as a distinct type because the two numbers in play -- a ratio near
/// 1.0 and a correction near 0.00001 -- are extremely easy to confuse, and
/// confusing them produces audio that is wrong in a way that looks right.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Ppm(pub f64);

impl Ppm {
    /// The resampling ratio this correction implies.
    #[must_use]
    pub fn as_ratio(self) -> f64 {
        1.0 + self.0 / 1_000_000.0
    }
}

/// How the servo is allowed to behave.
#[derive(Debug, Clone, Copy)]
pub struct ServoConfig {
    /// Where the buffer should sit when nothing is wrong.
    ///
    /// Not a minimum. The servo corrects in both directions, so the target
    /// must leave room to absorb a burst from below and a gap from above --
    /// which is why the natural choice is half of what the buffer holds
    /// rather than as little as playback can survive.
    pub target_fill: Duration,

    /// The most the ratio may ever be bent, in ppm.
    ///
    /// This is an audibility bound, not a performance one. Around 100ppm is
    /// roughly a sixth of a cent -- inaudible on programme material, and far
    /// more than crystal drift between two commodity converters, which lands
    /// in the tens. A system needing more than this is not drifting; it has
    /// a different fault, and quietly pitch-shifting to hide it is the wrong
    /// response.
    pub max_correction: Ppm,

    /// Proportional gain: ppm of correction per second of fill error.
    ///
    /// Low. A tenth of a second of error earning a few ppm means the servo
    /// takes tens of seconds to walk out a disturbance, which is the
    /// intent -- drift arrives over minutes, so nothing is gained by
    /// reacting in milliseconds and pitch stability is lost.
    pub proportional: f64,

    /// Integral gain: ppm per second-of-error per second.
    ///
    /// This is the term that actually cancels drift. Proportional control
    /// alone settles at a standing offset -- it needs error to produce
    /// output, so it parks the buffer away from target forever. The
    /// integrator is what lets the ratio hold a steady non-zero value with
    /// the buffer sitting exactly where it belongs.
    pub integral: f64,
}

impl Default for ServoConfig {
    fn default() -> Self {
        Self {
            target_fill: Duration::from_millis(20),
            max_correction: Ppm(100.0),
            // Error is measured in SECONDS, so these look large and are not.
            // A 10ms deviation -- ordinary for a 20ms buffer -- earns 30ppm,
            // a third of the audibility bound. Saturation then needs roughly
            // 33ms of error, which for a 20ms target means the buffer has
            // nearly emptied or tripled: no longer drift, a fault.
            proportional: 3_000.0,
            // Paired with `proportional` for near-critical damping. The
            // plant is an integrator with a gain of 1e-6 -- a ppm of ratio
            // moves the buffer by a millionth of a second per second -- so
            // the loop's natural frequency is sqrt(integral * 1e-6) and its
            // damping is proportional * 1e-6 / (2 * that). At 200 the ratio
            // lands near 0.1: violently underdamped, overshooting to twice
            // the drift and ringing for an hour. 2.5 puts damping near 1.
            //
            // The resulting time constant is minutes, which is not slack
            // tuning but the arithmetic: correcting 10ms of error at 30ppm
            // takes 333 seconds no matter how the gains are arranged. Clock
            // drift is a slow problem and can only be answered slowly.
            integral: 2.5,
        }
    }
}

/// What the servo concluded, and why.
///
/// The reason travels with the number because a bare ratio is unreviewable:
/// a system quietly running at its correction limit for an hour looks
/// identical to a healthy one until someone asks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Correction {
    pub ppm: Ppm,
    /// True when the correction was clamped. Sustained clamping means the
    /// drift exceeds what resampling should hide, and something else is
    /// wrong -- a wrong nominal rate, a stalled sender, the wrong buffer.
    pub saturated: bool,
}

impl Correction {
    #[must_use]
    pub fn as_ratio(self) -> f64 {
        self.ppm.as_ratio()
    }
}

/// Steers a resampling ratio from how full the playout buffer is.
///
/// Fill is the only input on purpose. It is the one signal that already
/// contains everything the receiver needs to know -- clock drift, network
/// pacing, and its own consumption all show up there -- and unlike a clock
/// comparison it requires no agreement with the far end about time.
#[derive(Debug, Clone)]
pub struct Servo {
    config: ServoConfig,
    integrator: f64,
}

impl Servo {
    #[must_use]
    pub fn new(config: ServoConfig) -> Self {
        Self {
            config,
            integrator: 0.0,
        }
    }

    /// Forget accumulated state.
    ///
    /// For discontinuities rather than disturbances: a reconnect, a rate
    /// change, a buffer refilled from empty. The integrator holds a running
    /// account of past error, and after a discontinuity that account
    /// describes a system that no longer exists -- carrying it forward makes
    /// the servo spend the next minute correcting for history.
    pub fn reset(&mut self) {
        self.integrator = 0.0;
    }

    /// The correction currently implied by the buffer.
    ///
    /// `elapsed` is the time since the previous observation, and it must be
    /// real rather than assumed: observations arrive on audio callbacks,
    /// which jitter, and an integrator fed a nominal interval accumulates
    /// error proportional to how wrong that nominal is.
    pub fn observe(&mut self, fill: Duration, elapsed: Duration) -> Correction {
        // Positive error means the buffer is fuller than wanted, which means
        // samples are arriving faster than they leave, which is corrected by
        // consuming faster: a positive ratio correction.
        let error = fill.as_secs_f64() - self.config.target_fill.as_secs_f64();
        let seconds = elapsed.as_secs_f64();

        let limit = self.config.max_correction.0;

        let candidate = self.integrator + error * self.config.integral * seconds;
        let raw = error * self.config.proportional + candidate;
        let clamped = raw.clamp(-limit, limit);
        let saturated = raw != clamped;

        // Conditional integration. Clamping the integrator is not enough on
        // its own: an integrator pinned at the limit still commands the limit
        // by itself, so when the error finally clears it keeps pulling and
        // drives the buffer straight past target -- here, all the way to
        // empty. While the output is already saturated, more error buys
        // nothing except a debt repaid as overshoot.
        //
        // Unwinding is always allowed. Refusing to integrate at all while
        // saturated would leave the integrator stuck at the limit forever,
        // which is the same failure wearing a different hat.
        let unwinding = candidate.abs() < self.integrator.abs();
        if !saturated || unwinding {
            self.integrator = candidate.clamp(-limit, limit);
        }

        Correction {
            ppm: Ppm(clamped),
            saturated: raw != clamped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    /// A buffer sitting exactly where it should implies no correction, and
    /// no amount of sitting there changes that. Sounds trivial; it is the
    /// property a mis-signed integrator breaks.
    #[test]
    fn a_buffer_at_target_is_left_alone() {
        let mut servo = Servo::new(ServoConfig::default());
        for _ in 0..1_000 {
            let correction = servo.observe(ms(20), ms(10));
            assert_eq!(correction.ppm, Ppm(0.0));
            assert!(!correction.saturated);
        }
    }

    /// Fuller than target means samples arrive faster than they leave, so the
    /// receiver must consume faster -- a positive ratio. Getting this
    /// backwards produces a servo that drives the buffer to whichever end it
    /// started nearest, confidently.
    #[test]
    fn correction_runs_toward_the_target_from_both_sides() {
        let mut servo = Servo::new(ServoConfig::default());
        assert!(servo.observe(ms(30), ms(10)).ppm.0 > 0.0);

        let mut servo = Servo::new(ServoConfig::default());
        assert!(servo.observe(ms(10), ms(10)).ppm.0 < 0.0);
    }

    /// The reason the integrator exists. Under constant drift a proportional
    /// controller settles wherever error happens to produce the matching
    /// output, leaving the buffer permanently off target; the integrator
    /// holds the ratio while the error returns to zero.
    #[test]
    fn a_steady_drift_is_cancelled_with_the_buffer_back_at_target() {
        let mut servo = Servo::new(ServoConfig::default());
        // Sender runs 12ppm fast: the buffer gains 12us per second.
        let drift_ppm = 12.0;
        let mut fill = 0.020_f64;

        // 6000 simulated seconds. Settling takes roughly four time
        // constants and the time constant here is ~11 minutes; a test that
        // ran for 200 seconds was sampling the first swing and calling it
        // the answer.
        for _ in 0..600_000 {
            let correction = servo.observe(Duration::from_secs_f64(fill), ms(10));
            // Over one 10ms tick the buffer gains the drift and loses
            // whatever the correction consumes.
            fill += (drift_ppm - correction.ppm.0) / 1_000_000.0 * 0.010;
        }

        // Settled: the ratio holds the drift, and the buffer is back where
        // it belongs rather than parked at an offset.
        let settled = servo.observe(Duration::from_secs_f64(fill), ms(10));
        assert!(
            (settled.ppm.0 - drift_ppm).abs() < 1.0,
            "ratio should hold ~{drift_ppm}ppm, got {}",
            settled.ppm.0
        );
        assert!(
            (fill - 0.020).abs() < 0.001,
            "buffer should return to target, sat at {fill}s"
        );
    }

    /// Ratio is pitch, and pitch steps are the one artifact ears catch
    /// immediately. A large sudden error must not produce a large sudden
    /// correction.
    #[test]
    fn a_sudden_large_error_is_not_answered_with_a_sudden_large_correction() {
        let mut servo = Servo::new(ServoConfig::default());
        let correction = servo.observe(ms(120), ms(10));
        assert!(
            correction.ppm.0 <= ServoConfig::default().max_correction.0,
            "correction escaped the audibility bound: {}",
            correction.ppm.0
        );
        // Around a sixth of a cent. Inaudible on programme material.
        assert!(correction.ppm.0 <= 100.0);
    }

    /// Saturation is reported rather than hidden, because a system that has
    /// been at its limit for an hour is not drifting -- it is broken, and it
    /// looks healthy from the outside.
    #[test]
    fn persistent_saturation_is_visible_to_the_caller() {
        let mut servo = Servo::new(ServoConfig::default());
        // 60ms past a 20ms target: the buffer has quadrupled. That is not
        // drift, and the servo says so rather than quietly pitching harder.
        assert!(servo.observe(ms(80), ms(10)).saturated);
        // Ordinary deviation is not saturation, or the signal means nothing.
        assert!(!Servo::new(ServoConfig::default()).observe(ms(30), ms(10)).saturated);
    }

    /// Windup: while clamped, an unbounded integrator keeps climbing and
    /// then refuses to come down, so the buffer sails past target and the
    /// ratio stays wrong for as long as it took to wind up.
    ///
    /// Tested against a buffer that actually responds. Holding fill at target
    /// by hand would prove nothing -- a buffer cannot sit still while the
    /// ratio is pulling it, and a controller cannot unwind against an error
    /// that never appears.
    #[test]
    fn the_integrator_does_not_wind_up_while_saturated() {
        let mut servo = Servo::new(ServoConfig::default());
        let target = 0.020_f64;
        // Start badly overfull and let the loop bring it home.
        let mut fill = 0.200_f64;
        let mut undershoot = 0.0_f64;
        let mut settled_at = None;

        for tick in 0..400_000 {
            let correction = servo.observe(Duration::from_secs_f64(fill), ms(10));
            fill -= correction.ppm.0 / 1_000_000.0 * 0.010;
            fill = fill.max(0.0);
            if settled_at.is_none() && (fill - target).abs() < 0.001 {
                settled_at = Some(tick);
            }
            if settled_at.is_some() {
                undershoot = undershoot.max(target - fill);
            }
        }

        let settled_at = settled_at.expect("never reached target");
        // Recovery from a 180ms excess is bounded by the correction limit:
        // 100ppm removes 0.1us per second, so this cannot be fast. What it
        // must not be is unbounded.
        // 180ms of excess removed at no more than 100ppm is 1800 seconds of
        // arithmetic. The bound is that it is bounded, not that it is quick.
        assert!(settled_at < 400_000, "settled only at tick {settled_at}");
        // The windup symptom: overshooting far past target and staying there.
        assert!(
            undershoot < 0.010,
            "wound up and overshot {undershoot}s past target"
        );
    }

    /// A reconnect is a discontinuity, not a disturbance: the accumulated
    /// account describes a stream that no longer exists.
    #[test]
    fn reset_discards_history_from_a_stream_that_ended() {
        let mut servo = Servo::new(ServoConfig::default());
        for _ in 0..1_000 {
            servo.observe(ms(60), ms(10));
        }
        servo.reset();
        assert_eq!(servo.observe(ms(20), ms(10)).ppm, Ppm(0.0));
    }

    /// Observations arrive on audio callbacks, which jitter. An integrator
    /// that assumes a nominal interval accumulates error in proportion to
    /// how wrong the assumption is, so the elapsed time has to be real.
    #[test]
    fn integration_follows_real_elapsed_time_not_a_nominal_tick() {
        let mut fast = Servo::new(ServoConfig::default());
        let mut slow = Servo::new(ServoConfig::default());
        for _ in 0..100 {
            fast.observe(ms(30), ms(5));
        }
        for _ in 0..50 {
            slow.observe(ms(30), ms(10));
        }
        // Same total elapsed time at the same error: same accumulation.
        let a = fast.observe(ms(30), ms(5)).ppm.0;
        let b = slow.observe(ms(30), ms(10)).ppm.0;
        assert!((a - b).abs() < 1.0, "{a} vs {b}");
    }

    #[test]
    fn ppm_converts_to_a_ratio_near_one() {
        assert!((Ppm(0.0).as_ratio() - 1.0).abs() < f64::EPSILON);
        assert!((Ppm(10.0).as_ratio() - 1.000_01).abs() < 1e-12);
        assert!((Ppm(-10.0).as_ratio() - 0.999_99).abs() < 1e-12);
    }
}
