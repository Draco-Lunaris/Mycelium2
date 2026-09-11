//! In-memory login throttling with exponential backoff (per user + IP).

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Decision returned by the throttle after recording a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThrottleDecision {
    /// Authentication may proceed.
    Allow,
    /// Too many recent failures; retry after the backoff delay.
    Deny { retry_after: Duration },
}

/// Exponential backoff throttle: delay doubles per consecutive failure,
/// capped, and decays after a success.
///
/// Backoff schedule (seconds): 1, 2, 4, 8, 16, cap.
#[derive(Debug, Clone)]
pub struct AuthThrottle {
    failures: HashMap<String, u32>,
    last_failure: HashMap<String, Instant>,
    /// Failures before the first deny (default 3).
    max_failures: u32,
    /// Backoff cap (default 30s).
    cap: Duration,
}

/// Default failures before deny.
const DEFAULT_MAX_FAILURES: u32 = 3;
/// Default backoff cap.
const DEFAULT_CAP: Duration = Duration::from_secs(30);

/// Exponential backoff delay for a failure count (free function so record
/// paths can compute it without borrowing self).
fn delay_for(failures: u32, max_failures: u32, cap: Duration) -> Duration {
    let exp = failures.saturating_sub(max_failures).min(5);
    let secs = 1u64 << exp; // 1, 2, 4, 8, 16
    Duration::from_secs(secs).min(cap)
}

impl Default for AuthThrottle {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthThrottle {
    pub fn new() -> Self {
        Self {
            failures: HashMap::new(),
            last_failure: HashMap::new(),
            max_failures: DEFAULT_MAX_FAILURES,
            cap: DEFAULT_CAP,
        }
    }

    /// Configure the deny threshold and backoff cap (admin-configurable
    /// security settings; callers clamp to sane bounds).
    pub fn with_limits(mut self, max_failures: u32, cap: Duration) -> Self {
        self.max_failures = max_failures.max(1);
        self.cap = cap;
        self
    }

    /// Update the deny threshold and backoff cap in place (runtime
    /// reconfiguration; callers clamp to sane bounds).
    pub fn set_limits(&mut self, max_failures: u32, cap: Duration) {
        self.max_failures = max_failures.max(1);
        self.cap = cap;
    }

    /// Check whether an attempt is allowed WITHOUT recording anything.
    pub fn check(&self, key: &str) -> ThrottleDecision {
        let Some(&n) = self.failures.get(key) else {
            return ThrottleDecision::Allow;
        };
        if n < self.max_failures {
            return ThrottleDecision::Allow;
        }
        let Some(last) = self.last_failure.get(key) else {
            return ThrottleDecision::Allow;
        };
        let delay = delay_for(n, self.max_failures, self.cap);
        let elapsed = last.elapsed();
        if elapsed >= delay {
            // The backoff window has passed: allow (failure count persists
            // until a success resets it, but the delay keeps growing).
            return ThrottleDecision::Allow;
        }
        ThrottleDecision::Deny {
            retry_after: delay - elapsed,
        }
    }

    /// Record a failed attempt; returns the decision for the NEXT attempt.
    pub fn record_failure(&mut self, key: &str) -> ThrottleDecision {
        let n = self.failures.entry(key.to_string()).or_insert(0);
        *n += 1;
        self.last_failure.insert(key.to_string(), Instant::now());
        if *n < self.max_failures {
            return ThrottleDecision::Allow;
        }
        let delay = delay_for(*n, self.max_failures, self.cap);
        ThrottleDecision::Deny { retry_after: delay }
    }

    /// Record a success: reset the failure count.
    pub fn record_success(&mut self, key: &str) {
        self.failures.remove(key);
        self.last_failure.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_first_failures() {
        let mut t = AuthThrottle::new();
        assert_eq!(t.check("u"), ThrottleDecision::Allow);
        assert_eq!(t.record_failure("u"), ThrottleDecision::Allow);
        assert_eq!(t.record_failure("u"), ThrottleDecision::Allow);
    }

    #[test]
    fn denies_after_threshold() {
        let mut t = AuthThrottle::new();
        for _ in 0..3 {
            t.record_failure("u");
        }
        assert!(matches!(
            t.record_failure("u"),
            ThrottleDecision::Deny { .. }
        ));
        assert!(matches!(t.check("u"), ThrottleDecision::Deny { .. }));
    }

    #[test]
    fn success_resets() {
        let mut t = AuthThrottle::new();
        for _ in 0..5 {
            t.record_failure("u");
        }
        assert!(matches!(t.check("u"), ThrottleDecision::Deny { .. }));
        t.record_success("u");
        assert_eq!(t.check("u"), ThrottleDecision::Allow);
    }

    #[test]
    fn keys_are_independent() {
        let mut t = AuthThrottle::new();
        for _ in 0..5 {
            t.record_failure("a");
        }
        assert_eq!(t.check("b"), ThrottleDecision::Allow);
    }

    #[test]
    fn backoff_is_capped() {
        let mut t = AuthThrottle::new();
        for _ in 0..20 {
            t.record_failure("u");
        }
        if let ThrottleDecision::Deny { retry_after } = t.check("u") {
            assert!(retry_after <= Duration::from_secs(30));
        } else {
            panic!("expected deny");
        }
    }
}
