//! A bounded retry budget for the compare-and-swap loops that publish refs.
//!
//! Every ref-mutating operation in this crate re-reads a ref, rebuilds the
//! commit over it, and lands the result with a compare-and-swap. A lost race
//! is retryable, but the loop that retries it must terminate: sustained
//! contention, or a backend that reports a lost race for reasons of its own,
//! must not spin forever or leave a caller unable to bound latency. This
//! module is the one place that budget lives.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::Error;

/// How many compare-and-swap attempts one loop may make before giving up.
/// The first is the operation itself; the rest are retries, each preceded by
/// a jittered backoff.
pub(crate) const ATTEMPTS: u32 = 8;

/// The backoff before the second attempt; doubled on each further retry.
const BASE_BACKOFF: Duration = Duration::from_millis(1);

/// The largest backoff a single retry waits, however many attempts have lost.
const MAX_BACKOFF: Duration = Duration::from_millis(50);

/// A per-loop retry budget.
///
/// A caller takes one budget for a loop, makes its compare-and-swap attempt,
/// and on a loss calls [`Budget::retry`] to either sleep a jittered backoff and
/// go again, or stop and surface [`Budget::exhausted`]. A caller that never
/// loses never calls `retry`, so the uncontended path never sleeps.
pub(crate) struct Budget {
    /// Retries still allowed, not counting the attempt in flight.
    remaining: u32,
    /// Cheap local jitter state; seeded per budget, advanced per backoff.
    jitter: u64,
}

impl Budget {
    /// A fresh budget allowing [`ATTEMPTS`] attempts in total.
    pub(crate) fn new() -> Self {
        Self {
            remaining: ATTEMPTS - 1,
            jitter: seed(),
        }
    }

    /// Spend one retry. Returns `true` after sleeping a jittered backoff when
    /// another attempt is allowed, and `false` once the budget is spent — the
    /// caller must then stop and return [`Budget::exhausted`].
    pub(crate) fn retry(&mut self) -> bool {
        if self.remaining == 0 {
            return false;
        }
        // 1-based index of the attempt this backoff precedes.
        let attempt = ATTEMPTS - self.remaining;
        self.remaining -= 1;
        std::thread::sleep(self.backoff(attempt));
        true
    }

    /// The typed terminal error a spent budget produces.
    pub(crate) fn exhausted(&self) -> Error {
        Error::Contention { attempts: ATTEMPTS }
    }

    /// Full-jitter backoff for the attempt following `attempt` losses:
    /// uniform in `[0, min(MAX_BACKOFF, BASE_BACKOFF * 2^(attempt - 1))]`.
    fn backoff(&mut self, attempt: u32) -> Duration {
        let ceiling = BASE_BACKOFF
            .saturating_mul(1u32 << (attempt - 1).min(31))
            .min(MAX_BACKOFF);
        let span = ceiling.as_nanos().max(1) as u64;
        Duration::from_nanos(self.next_jitter() % span)
    }

    /// A cheap xorshift step, for jitter uncorrelated across threads and calls
    /// without pulling in a dependency.
    fn next_jitter(&mut self) -> u64 {
        let mut x = self.jitter;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.jitter = x;
        x
    }
}

/// Seed for a budget's jitter: wall-clock nanoseconds mixed with a
/// process-wide counter, so two budgets made in the same instant (on two
/// threads) still diverge. Nonzero, as xorshift requires.
fn seed() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let tick = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    (nanos ^ tick.wrapping_mul(0x9E37_79B9_7F4A_7C15)) | 1
}
