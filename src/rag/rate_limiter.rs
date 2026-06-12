//! Per-agent rate limiter: max N calls per minute using a sliding window.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub struct RateLimiter {
    max_per_minute: usize,
    window: VecDeque<Instant>,
}

impl RateLimiter {
    pub fn new(max_per_minute: usize) -> Self {
        Self {
            max_per_minute,
            window: VecDeque::new(),
        }
    }

    /// Returns `Ok(())` if the call is allowed, or `Err(retry_after_secs)`.
    pub fn check(&mut self) -> Result<(), u64> {
        let now = Instant::now();
        let cutoff = now - Duration::from_secs(60);

        // Evict old entries
        while self.window.front().is_some_and(|t| *t < cutoff) {
            self.window.pop_front();
        }

        if self.window.len() >= self.max_per_minute {
            let oldest = self.window.front().copied().unwrap();
            let retry = 60u64.saturating_sub(now.duration_since(oldest).as_secs());
            return Err(retry.max(1));
        }

        self.window.push_back(now);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_limit() {
        let mut rl = RateLimiter::new(3);
        assert!(rl.check().is_ok());
        assert!(rl.check().is_ok());
        assert!(rl.check().is_ok());
        assert!(rl.check().is_err());
    }

    #[test]
    fn retry_after_is_positive() {
        let mut rl = RateLimiter::new(1);
        rl.check().unwrap();
        let err = rl.check().unwrap_err();
        assert!(err >= 1);
    }
}
