//! Reconnect backoff.
//!
//! An OPC UA server that is down stays down for minutes, not milliseconds. The
//! spike retried every 5 s forever, which on a fleet of gateways turns into a
//! synchronised connection storm the moment the server comes back. Exponential
//! backoff with jitter spreads that out.
//!
//! The jitter source is injected as a plain `u32` seed so the schedule is
//! reproducible in tests.

/// Exponential backoff with full jitter, capped.
#[derive(Debug, Clone)]
pub struct Backoff {
    base_ms: u64,
    max_ms: u64,
    attempt: u32,
    rng: Lcg,
}

impl Backoff {
    /// Creates a backoff starting at `base_ms` and saturating at `max_ms`.
    pub fn new(base_ms: u64, max_ms: u64, seed: u32) -> Self {
        Self {
            base_ms: base_ms.max(1),
            max_ms: max_ms.max(base_ms.max(1)),
            attempt: 0,
            rng: Lcg::new(seed),
        }
    }

    /// The default schedule: 1 s doubling to 60 s.
    pub fn default_schedule(seed: u32) -> Self {
        Self::new(1_000, 60_000, seed)
    }

    /// Number of consecutive failures so far.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Returns the next delay and advances the schedule.
    ///
    /// Uses "full jitter": a uniform draw from `[base, ceiling]` rather than
    /// `ceiling ± something`. The lower bound keeps the first retry prompt
    /// while still de-correlating a fleet.
    pub fn next_delay_ms(&mut self) -> u64 {
        let ceiling = self
            .base_ms
            .saturating_mul(1u64 << self.attempt.min(20))
            .min(self.max_ms);
        self.attempt = self.attempt.saturating_add(1);

        let span = ceiling - self.base_ms;
        if span == 0 {
            return ceiling;
        }
        self.base_ms + (self.rng.next() as u64 % (span + 1))
    }

    /// Resets the schedule after a successful connection.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

/// Tiny linear congruential generator (Numerical Recipes constants).
///
/// Backoff jitter does not need cryptographic quality, and this avoids pulling
/// a random-number crate into the firmware image.
#[derive(Debug, Clone)]
struct Lcg(u32);

impl Lcg {
    fn new(seed: u32) -> Self {
        Lcg(seed ^ 0x9e37_79b9)
    }

    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_stay_within_the_configured_window() {
        let mut b = Backoff::default_schedule(1);
        for _ in 0..40 {
            let d = b.next_delay_ms();
            assert!((1_000..=60_000).contains(&d), "delay {d} out of range");
        }
    }

    #[test]
    fn the_ceiling_grows_and_then_saturates() {
        // Averaging over many seeds shows the trend without depending on the
        // exact jitter draw.
        let mean = |attempt: u32| -> u64 {
            let total: u64 = (0..64u32)
                .map(|seed| {
                    let mut b = Backoff::default_schedule(seed);
                    (0..attempt).for_each(|_| {
                        b.next_delay_ms();
                    });
                    b.next_delay_ms()
                })
                .sum();
            total / 64
        };
        assert!(mean(0) < mean(3), "backoff did not grow");
        assert!(mean(3) < mean(6), "backoff did not keep growing");
        // Once saturated the ceiling stops moving, so the two means differ only
        // by the jitter draw, not by the schedule.
        let (a, b) = (mean(20) as i64, mean(30) as i64);
        assert!(
            (a - b).abs() < 3_000,
            "backoff did not saturate: {a} vs {b}"
        );
    }

    #[test]
    fn first_delay_is_exactly_the_base() {
        // At attempt 0 the ceiling equals the base, so there is nothing to
        // jitter: the first retry is prompt and deterministic.
        assert_eq!(Backoff::default_schedule(7).next_delay_ms(), 1_000);
    }

    #[test]
    fn different_seeds_decorrelate_a_fleet() {
        let delays: Vec<u64> = (0..8)
            .map(|seed| {
                let mut b = Backoff::default_schedule(seed);
                b.next_delay_ms();
                b.next_delay_ms();
                b.next_delay_ms();
                b.next_delay_ms()
            })
            .collect();
        let unique: std::collections::HashSet<_> = delays.iter().collect();
        assert!(unique.len() > 4, "jitter is not spreading: {delays:?}");
    }

    #[test]
    fn reset_returns_to_the_start_of_the_schedule() {
        let mut b = Backoff::default_schedule(3);
        for _ in 0..10 {
            b.next_delay_ms();
        }
        assert_eq!(b.attempt(), 10);
        b.reset();
        assert_eq!(b.attempt(), 0);
        assert_eq!(b.next_delay_ms(), 1_000);
    }

    #[test]
    fn attempt_counter_does_not_overflow() {
        let mut b = Backoff::new(1_000, 60_000, 5);
        b.attempt = u32::MAX;
        let d = b.next_delay_ms();
        assert!((1_000..=60_000).contains(&d));
        assert_eq!(b.attempt(), u32::MAX);
    }
}
