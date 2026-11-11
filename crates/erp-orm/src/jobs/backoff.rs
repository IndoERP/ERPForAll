//! Retry delays (1): capped exponential with half jitter, or an explicit schedule.

use std::time::Duration;

use erp_fields::{Error, Result};

/// Smallest and largest base of an exponential backoff, and largest cap.
const BASE_MIN: Duration = Duration::from_secs(1);
const BASE_MAX: Duration = Duration::from_secs(3_600);
const CAP_MAX: Duration = Duration::from_secs(86_400);
/// Most entries of an explicit schedule, and the range of each.
const SCHEDULE_MAX_LEN: usize = 16;

/// How long a failed job waits before its next attempt.
///
/// ```
/// use std::time::Duration;
/// use erp_orm::jobs::Backoff;
/// let backoff = Backoff::Exponential { base: Duration::from_secs(10), cap: Duration::from_secs(3600) };
/// // after the 3rd failed attempt the nominal delay is 10 s * 2^2, jitter keeps at least half
/// assert_eq!(backoff.delay(3, 1.0), Duration::from_secs(40));
/// assert_eq!(backoff.delay(3, 0.0), Duration::from_secs(20));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Backoff {
    /// `min(cap, base * 2^(n-1))` after the `n`th failed attempt, times a factor in `0.5..=1.0`.
    /// `base` is 1 s..=1 h, `cap` is `base`..=24 h.
    Exponential {
        /// Delay after the first failure.
        base: Duration,
        /// Longest delay.
        cap: Duration,
    },
    /// The `n`th entry after the `n`th failure, the last one repeating. 1 to 16 entries, each
    /// 1 s..=24 h. Exact: no jitter.
    Schedule(&'static [Duration]),
}

impl Backoff {
    /// The delay after `attempt` failed attempts (1 is the first), with `unit` in `0.0..=1.0`
    /// choosing the jitter (`0.0` gives half the nominal delay, `1.0` all of it). A `unit`
    /// outside the range or NaN is treated as `1.0`; the arithmetic saturates, so attempt
    /// numbers up to `u32::MAX` never overflow.
    #[must_use]
    pub fn delay(&self, attempt: u32, unit: f64) -> Duration {
        match *self {
            Self::Exponential { base, cap } => {
                // 2^31 seconds already exceeds every legal cap, so the exponent stops there
                let exponent = attempt.saturating_sub(1).min(31);
                let nominal = base.saturating_mul(1u32 << exponent).min(cap);
                let unit = if (0.0..=1.0).contains(&unit) {
                    unit
                } else {
                    1.0
                };
                nominal.mul_f64(0.5 + 0.5 * unit)
            }
            Self::Schedule(entries) => {
                let index = usize::try_from(attempt.saturating_sub(1)).unwrap_or(usize::MAX);
                entries
                    .get(index)
                    .or_else(|| entries.last())
                    .copied()
                    .unwrap_or(BASE_MIN)
            }
        }
    }

    /// The delay with a random jitter from the operating system's entropy.
    ///
    /// # Errors
    ///
    /// `Internal` when no entropy is available (never substituted by a fixed factor).
    pub fn delay_random(&self, attempt: u32) -> Result<Duration> {
        Ok(self.delay(attempt, random_unit()?))
    }

    /// Check the declaration's bounds.
    ///
    /// # Errors
    ///
    /// `Programming` naming the violated bound.
    pub fn validate(&self) -> Result<()> {
        match *self {
            Self::Exponential { base, cap } => {
                if !(BASE_MIN..=BASE_MAX).contains(&base) {
                    return Err(Error::programming(format!(
                        "backoff base {} s is outside 1..=3600 s",
                        base.as_secs_f64()
                    )));
                }
                if cap < base || cap > CAP_MAX {
                    return Err(Error::programming(format!(
                        "backoff cap {} s is outside the base ({} s)..=86400 s",
                        cap.as_secs_f64(),
                        base.as_secs_f64()
                    )));
                }
                Ok(())
            }
            Self::Schedule(entries) => {
                if entries.is_empty() || entries.len() > SCHEDULE_MAX_LEN {
                    return Err(Error::programming(format!(
                        "a backoff schedule has {} entries; it needs 1..=16",
                        entries.len()
                    )));
                }
                match entries
                    .iter()
                    .copied()
                    .find(|d| !(BASE_MIN..=CAP_MAX).contains(d))
                {
                    Some(bad) => Err(Error::programming(format!(
                        "backoff schedule entry {} s is outside 1..=86400 s",
                        bad.as_secs_f64()
                    ))),
                    None => Ok(()),
                }
            }
        }
    }
}

/// A uniform value in `0.0..=1.0` from the system CSPRNG.
pub(crate) fn random_unit() -> Result<f64> {
    let mut bytes = [0_u8; 4];
    getrandom::fill(&mut bytes)
        .map_err(|e| Error::internal("job backoff: entropy source unavailable").with_source(e))?;
    Ok(f64::from(u32::from_le_bytes(bytes)) / f64::from(u32::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXP: Backoff = Backoff::Exponential {
        base: Duration::from_secs(10),
        cap: Duration::from_secs(3600),
    };

    #[test]
    fn the_nominal_delay_doubles_per_attempt_up_to_the_cap() {
        let nominal: Vec<u64> = (1..=10).map(|n| EXP.delay(n, 1.0).as_secs()).collect();
        assert_eq!(nominal, [10, 20, 40, 80, 160, 320, 640, 1280, 2560, 3600]);
    }

    #[test]
    fn jitter_keeps_between_half_and_all_of_the_nominal_delay() {
        for attempt in 1..=12 {
            let nominal = EXP.delay(attempt, 1.0);
            let low = EXP.delay(attempt, 0.0);
            assert_eq!(low, nominal / 2, "attempt {attempt}");
            for step in 0..=20 {
                let d = EXP.delay(attempt, f64::from(step) / 20.0);
                assert!(
                    d >= low && d <= nominal,
                    "attempt {attempt} unit {step}/20: {d:?}"
                );
            }
        }
    }

    #[test]
    fn attempt_one_hundred_and_beyond_do_not_overflow() {
        assert_eq!(EXP.delay(100, 1.0), Duration::from_secs(3600));
        assert_eq!(EXP.delay(u32::MAX, 1.0), Duration::from_secs(3600));
        assert_eq!(EXP.delay(0, 1.0), Duration::from_secs(10));
        let wide = Backoff::Exponential {
            base: Duration::from_secs(3600),
            cap: Duration::from_secs(86_400),
        };
        assert_eq!(wide.delay(u32::MAX, 1.0), Duration::from_secs(86_400));
    }

    #[test]
    fn an_out_of_range_unit_is_treated_as_full_delay() {
        assert_eq!(EXP.delay(1, f64::NAN), Duration::from_secs(10));
        assert_eq!(EXP.delay(1, 2.0), Duration::from_secs(10));
        assert_eq!(EXP.delay(1, -1.0), Duration::from_secs(10));
    }

    #[test]
    fn an_explicit_schedule_is_exact_and_its_last_entry_repeats() {
        static STEPS: [Duration; 3] = [
            Duration::from_secs(60),
            Duration::from_secs(300),
            Duration::from_secs(1800),
        ];
        let schedule = Backoff::Schedule(&STEPS);
        let seen: Vec<u64> = (1..=6).map(|n| schedule.delay(n, 0.0).as_secs()).collect();
        assert_eq!(seen, [60, 300, 1800, 1800, 1800, 1800]);
        assert_eq!(schedule.delay(u32::MAX, 0.3), Duration::from_secs(1800));
    }

    #[test]
    fn declarations_outside_their_bounds_are_refused() {
        let small = Backoff::Exponential {
            base: Duration::from_millis(500),
            cap: Duration::from_secs(10),
        };
        assert_eq!(
            small.validate().unwrap_err().message(),
            "backoff base 0.5 s is outside 1..=3600 s"
        );
        let capped = Backoff::Exponential {
            base: Duration::from_secs(60),
            cap: Duration::from_secs(30),
        };
        assert_eq!(
            capped.validate().unwrap_err().message(),
            "backoff cap 30 s is outside the base (60 s)..=86400 s"
        );
        static EMPTY: [Duration; 0] = [];
        assert_eq!(
            Backoff::Schedule(&EMPTY).validate().unwrap_err().message(),
            "a backoff schedule has 0 entries; it needs 1..=16"
        );
        static LONG: [Duration; 17] = [Duration::from_secs(1); 17];
        assert_eq!(
            Backoff::Schedule(&LONG).validate().unwrap_err().message(),
            "a backoff schedule has 17 entries; it needs 1..=16"
        );
        static ZERO: [Duration; 1] = [Duration::ZERO];
        assert_eq!(
            Backoff::Schedule(&ZERO).validate().unwrap_err().message(),
            "backoff schedule entry 0 s is outside 1..=86400 s"
        );
        assert!(EXP.validate().is_ok());
    }

    #[test]
    fn sequence_jitter_caps_and_schedules_hold_together() {
        // nominal sequence with the cap, jitter between half and all, an explicit schedule whose
        // last entry repeats, and no overflow for any attempt number
        let nominal: Vec<u64> = (1..=10).map(|n| EXP.delay(n, 1.0).as_secs()).collect();
        assert_eq!(nominal, [10, 20, 40, 80, 160, 320, 640, 1280, 2560, 3600]);
        for attempt in [1, 5, 12, 100, u32::MAX] {
            let nominal = EXP.delay(attempt, 1.0);
            assert_eq!(EXP.delay(attempt, 0.0), nominal / 2);
        }
        static STEPS: [Duration; 2] = [Duration::from_secs(60), Duration::from_secs(300)];
        let schedule = Backoff::Schedule(&STEPS);
        assert_eq!(schedule.delay(1, 0.3), Duration::from_secs(60));
        assert_eq!(schedule.delay(9, 0.3), Duration::from_secs(300));
    }

    #[test]
    fn the_random_unit_is_in_range() {
        for _ in 0..100 {
            let unit = random_unit().unwrap();
            assert!((0.0..=1.0).contains(&unit));
        }
    }
}
