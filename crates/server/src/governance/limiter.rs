//! Token buckets, one per caller.
//!
//! A bucket holds up to `burst` tokens and gains `rate` of them a second. A request spends its
//! cost in tokens, or is turned away with the time until it could be served. This is the usual
//! shape for an API limit because it allows a burst and bounds the average, and because it
//! needs no timer: a bucket is brought up to date when it is next looked at.
//!
//! The clock is an argument, in nanoseconds from any fixed point, so tests move time by hand.
//! Buckets are held in process memory. With several servers behind one balancer each keeps its
//! own, so a caller's effective rate is up to the number of servers times the configured one;
//! the quotas, which live in the database, are what is shared.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// The key every caller beyond the capacity shares. A table that grows without bound is a way
/// to exhaust memory with addresses, so past the cap new callers share one bucket instead.
const OVERFLOW: &str = "overflow";

#[derive(Clone, Copy, Debug)]
struct Bucket {
    tokens: f64,
    at: u64,
    rate: f64,
    burst: f64,
}

impl Bucket {
    fn refilled(&self, now: u64) -> f64 {
        let seconds = now.saturating_sub(self.at) as f64 / 1e9;
        (self.tokens + seconds * self.rate).min(self.burst)
    }
}

/// What the limiter decided, and what a caller should be told about it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Decision {
    pub allowed: bool,
    /// The size of the bucket.
    pub limit: u32,
    /// Whole tokens left.
    pub remaining: u32,
    /// For a refusal, how long until the request could be served.
    pub retry_after: Duration,
    /// How long until the bucket is full again.
    pub reset: Duration,
}

impl Decision {
    /// `Retry-After` is whole seconds, and never zero for a refusal.
    pub fn retry_after_secs(&self) -> u64 {
        (self.retry_after.as_secs_f64().ceil() as u64).max(1)
    }

    pub fn reset_secs(&self) -> u64 {
        self.reset.as_secs_f64().ceil() as u64
    }
}

/// The least time between two sweeps for room that a full table makes. Scanning every bucket
/// under the one lock for each caller that does not fit would make the table's being full the
/// thing that slows everyone down.
const SWEEP_EVERY: u64 = 1_000_000_000;

pub struct Limiter {
    buckets: Mutex<HashMap<String, Bucket>>,
    capacity: usize,
    last_sweep: AtomicU64,
}

impl Limiter {
    pub fn new(capacity: usize) -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
            capacity,
            last_sweep: AtomicU64::new(0),
        }
    }

    /// Spend `cost` from `key`'s bucket. A cost above `burst` is taken as `burst`, so a request
    /// can always be served by a full bucket.
    pub fn take(&self, key: &str, rate: f64, burst: u32, cost: u32, now: u64) -> Decision {
        let rate = rate.max(f64::MIN_POSITIVE);
        let burst_tokens = f64::from(burst.max(1));
        let cost = f64::from(cost).min(burst_tokens);
        let mut buckets = self.buckets.lock().expect("limiter");
        let key = if buckets.contains_key(key) || buckets.len() < self.capacity {
            key
        } else {
            if now.saturating_sub(self.last_sweep.load(Ordering::Relaxed)) >= SWEEP_EVERY {
                self.last_sweep.store(now, Ordering::Relaxed);
                sweep_idle(&mut buckets, now);
            }
            if buckets.len() < self.capacity {
                key
            } else {
                OVERFLOW
            }
        };
        // The key is only copied for a caller that is new: a lookup is all a known one needs.
        if !buckets.contains_key(key) {
            buckets.insert(
                key.to_string(),
                Bucket {
                    tokens: burst_tokens,
                    at: now,
                    rate,
                    burst: burst_tokens,
                },
            );
        }
        let bucket = buckets.get_mut(key).expect("the bucket was just made");
        // The configuration may have changed since the bucket was made.
        bucket.rate = rate;
        bucket.burst = burst_tokens;
        bucket.tokens = bucket.refilled(now);
        bucket.at = now;
        let allowed = bucket.tokens >= cost;
        let retry_after = if allowed {
            Duration::ZERO
        } else {
            span((cost - bucket.tokens) / rate)
        };
        if allowed {
            bucket.tokens -= cost;
        }
        Decision {
            allowed,
            limit: burst.max(1),
            remaining: bucket.tokens.floor() as u32,
            retry_after,
            reset: span((bucket.burst - bucket.tokens) / rate),
        }
    }

    /// Whether `key` has at least a token, without spending one. A key never seen has.
    pub fn has_token(&self, key: &str, rate: f64, burst: u32, now: u64) -> bool {
        let rate = rate.max(f64::MIN_POSITIVE);
        let buckets = self.buckets.lock().expect("limiter");
        match buckets.get(key) {
            Some(bucket) => {
                Bucket {
                    rate,
                    burst: f64::from(burst.max(1)),
                    ..*bucket
                }
                .refilled(now)
                    >= 1.0
            }
            None => true,
        }
    }

    /// Forget the buckets that have refilled completely: a missing bucket and a full one are the
    /// same, so this changes nothing but the memory in use.
    pub fn sweep(&self, now: u64) {
        sweep_idle(&mut self.buckets.lock().expect("limiter"), now);
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.buckets.lock().expect("limiter").len()
    }
}

/// A span of seconds as a `Duration`. A refill so slow that the wait overflows one is as good
/// as forever, and `Duration::from_secs_f64` would panic on it, in the request path.
fn span(seconds: f64) -> Duration {
    Duration::try_from_secs_f64(seconds.max(0.0))
        .unwrap_or(Duration::from_secs(u64::from(u32::MAX)))
}

fn sweep_idle(buckets: &mut HashMap<String, Bucket>, now: u64) {
    buckets.retain(|_, bucket| bucket.refilled(now) < bucket.burst);
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: u64 = 1_000_000_000;

    #[test]
    fn a_burst_is_allowed_and_then_the_rate_holds() {
        let limiter = Limiter::new(100);
        // 2 a second, bucket of 4.
        for left in (0..4).rev() {
            let decision = limiter.take("a", 2.0, 4, 1, 0);
            assert!(decision.allowed);
            assert_eq!((decision.limit, decision.remaining), (4, left));
        }
        let refused = limiter.take("a", 2.0, 4, 1, 0);
        assert!(!refused.allowed);
        assert_eq!(refused.remaining, 0);
        assert_eq!(refused.retry_after, Duration::from_millis(500));
        assert_eq!(refused.retry_after_secs(), 1, "never zero");
        // Half a second later there is one token, and only one.
        assert!(limiter.take("a", 2.0, 4, 1, SECOND / 2).allowed);
        assert!(!limiter.take("a", 2.0, 4, 1, SECOND / 2).allowed);
        // A long idle refills to the size of the bucket and no further.
        let later = limiter.take("a", 2.0, 4, 1, 100 * SECOND);
        assert_eq!(later.remaining, 3);
    }

    #[test]
    fn an_absurdly_slow_refill_is_a_long_wait_and_not_a_panic() {
        let limiter = Limiter::new(10);
        assert!(limiter.take("a", 1e-300, 5, 5, 0).allowed);
        let refused = limiter.take("a", 1e-300, 5, 1, 0);
        assert!(!refused.allowed);
        assert!(refused.retry_after_secs() >= u64::from(u32::MAX));
        assert!(refused.reset_secs() >= u64::from(u32::MAX));
        assert!(!limiter.take("a", f64::MIN_POSITIVE, 5, 1, 0).allowed);
    }

    #[test]
    fn a_refused_request_spends_nothing() {
        let limiter = Limiter::new(100);
        assert!(limiter.take("a", 1.0, 10, 8, 0).allowed);
        for _ in 0..50 {
            assert!(!limiter.take("a", 1.0, 10, 8, 0).allowed);
        }
        // Two tokens were left and refused requests did not take them.
        assert_eq!(limiter.take("a", 1.0, 10, 2, 0).remaining, 0);
    }

    #[test]
    fn costs_are_charged_and_retry_after_covers_the_shortfall() {
        let limiter = Limiter::new(100);
        assert!(limiter.take("a", 5.0, 20, 15, 0).allowed);
        let refused = limiter.take("a", 5.0, 20, 20, 0);
        assert!(!refused.allowed);
        // 5 tokens left, 20 wanted, 5 a second.
        assert_eq!(refused.retry_after, Duration::from_secs(3));
        assert_eq!(refused.retry_after_secs(), 3);
        assert!(limiter.take("a", 5.0, 20, 20, 3 * SECOND).allowed);
        // A cost larger than the bucket is a full bucket's worth, not an impossible request.
        assert!(limiter.take("b", 5.0, 20, 500, 0).allowed);
    }

    #[test]
    fn callers_do_not_share_buckets() {
        let limiter = Limiter::new(100);
        assert!(limiter.take("a", 1.0, 1, 1, 0).allowed);
        assert!(!limiter.take("a", 1.0, 1, 1, 0).allowed);
        assert!(limiter.take("b", 1.0, 1, 1, 0).allowed);
    }

    #[test]
    fn reset_says_when_the_bucket_is_full_again() {
        let limiter = Limiter::new(100);
        let decision = limiter.take("a", 2.0, 10, 4, 0);
        assert_eq!(decision.reset, Duration::from_secs(2));
        assert_eq!(decision.reset_secs(), 2);
        assert_eq!(limiter.take("b", 2.0, 10, 0, 0).reset, Duration::ZERO);
    }

    #[test]
    fn a_changed_configuration_applies_to_a_bucket_that_exists() {
        let limiter = Limiter::new(100);
        assert!(limiter.take("a", 1.0, 100, 100, 0).allowed);
        // The bucket is made smaller while it is empty: it refills to the new size, not the old.
        let decision = limiter.take("a", 1.0, 5, 1, 1_000 * SECOND);
        assert_eq!((decision.allowed, decision.remaining), (true, 4));
    }

    #[test]
    fn peeking_does_not_spend_and_an_unknown_key_has_a_token() {
        let limiter = Limiter::new(100);
        assert!(limiter.has_token("new", 1.0, 3, 0));
        assert_eq!(limiter.len(), 0, "peeking makes no bucket");
        for _ in 0..3 {
            assert!(limiter.take("a", 1.0, 3, 1, 0).allowed);
        }
        assert!(!limiter.has_token("a", 1.0, 3, 0));
        assert!(!limiter.has_token("a", 1.0, 3, SECOND / 2));
        assert!(limiter.has_token("a", 1.0, 3, SECOND));
        // Looking did not change what a take sees.
        assert!(!limiter.take("a", 1.0, 3, 1, 0).allowed);
    }

    #[test]
    fn a_full_table_is_swept_for_room_once_a_second_and_not_once_a_caller() {
        // Ten a second and one to a bucket: a bucket is idle again a tenth of a second on.
        let limiter = Limiter::new(2);
        assert!(limiter.take("a", 10.0, 1, 1, 0).allowed);
        assert!(limiter.take("b", 10.0, 1, 1, 0).allowed);
        // The first caller that does not fit finds that a and b are idle and makes room.
        let at = 2 * SECOND;
        assert!(limiter.take("c", 10.0, 1, 1, at).allowed);
        assert_eq!(limiter.len(), 1, "a and b were swept to make room");
        assert!(limiter.take("d", 10.0, 1, 1, at).allowed);
        // c and d are idle again soon, but the table was scanned a moment ago and is not
        // scanned again for each caller that does not fit: this one shares the overflow bucket.
        assert!(limiter.take("e", 10.0, 1, 1, at + SECOND / 5).allowed);
        assert_eq!(
            limiter.len(),
            3,
            "c, d, and the overflow bucket e landed in"
        );
        // A second after the last scan the next caller to find the table full makes room.
        assert!(limiter.take("f", 10.0, 1, 1, at + 2 * SECOND).allowed);
        assert_eq!(limiter.len(), 1, "everything idle was swept, f has its own");
    }

    #[test]
    fn idle_buckets_are_forgotten_and_the_table_cannot_grow_without_bound() {
        let limiter = Limiter::new(3);
        for key in ["a", "b", "c"] {
            assert!(limiter.take(key, 1.0, 2, 1, 0).allowed);
        }
        assert_eq!(limiter.len(), 3);
        // All three are still short of full, so a fourth caller shares the overflow bucket.
        assert!(limiter.take("d", 1.0, 2, 1, 0).allowed);
        assert!(limiter.take("e", 1.0, 2, 1, 0).allowed);
        assert!(
            !limiter.take("f", 1.0, 2, 1, 0).allowed,
            "the overflow is shared"
        );
        assert_eq!(limiter.len(), 4);
        // Once they have refilled they are the same as no bucket, and are dropped.
        limiter.sweep(10 * SECOND);
        assert_eq!(limiter.len(), 0);
        assert!(limiter.take("d", 1.0, 2, 1, 10 * SECOND).allowed);
        assert_eq!(limiter.len(), 1);
    }
}
