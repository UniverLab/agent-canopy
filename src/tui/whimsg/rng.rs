use std::time::Instant;

pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn from_instant(t: Instant) -> Self {
        let seed = t.elapsed().as_nanos() as u64 ^ 0xDEAD_BEEF_CAFE_BABE;
        Self(if seed == 0 { 1 } else { seed })
    }
    pub(crate) fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub(crate) fn range(&mut self, max: usize) -> usize {
        if max == 0 {
            return 0;
        }
        (self.next() % max as u64) as usize
    }
    pub(crate) fn between(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo + 1)
    }
    pub(crate) fn chance(&mut self, p: f64) -> bool {
        (self.next() % 1000) < (p * 1000.0) as u64
    }
}

// ── Dedup ring ────────────────────────────────────────────────────

#[derive(Clone)]
pub(crate) struct DedupRing {
    buf: Vec<usize>,
    cap: usize,
}

impl DedupRing {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
            cap,
        }
    }
    pub(crate) fn contains(&self, idx: usize) -> bool {
        self.buf.contains(&idx)
    }
    pub(crate) fn push(&mut self, idx: usize) {
        if self.buf.len() >= self.cap {
            self.buf.remove(0);
        }
        self.buf.push(idx);
    }
}

pub(crate) fn pick_no_repeat(rng: &mut Rng, len: usize, seen: &DedupRing) -> usize {
    for _ in 0..10 {
        let i = rng.range(len);
        if !seen.contains(i) {
            return i;
        }
    }
    rng.range(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rng can be constructed from an instant
    #[test]
    fn rng_from_instant_creates_nonzero_seed() {
        let rng = Rng::from_instant(Instant::now());
        assert_ne!(rng.0, 0);
    }

    /// Rng from two different instants creates different seeds
    #[test]
    fn rng_from_different_instants_differs() {
        let rng1 = Rng::from_instant(Instant::now());
        let rng2 = Rng::from_instant(Instant::now());
        // Very high probability they're different (not guaranteed but extremely likely)
        assert_ne!(rng1.0, rng2.0);
    }

    /// Rng::next produces values
    #[test]
    fn rng_next_produces_values() {
        let mut rng = Rng::from_instant(Instant::now());
        let v1 = rng.next();
        let v2 = rng.next();
        let v3 = rng.next();
        // Values should differ (with overwhelming probability)
        assert!(v1 != v2 || v2 != v3);
    }

    /// Rng::range with max=0 returns 0
    #[test]
    fn rng_range_zero_returns_zero() {
        let mut rng = Rng::from_instant(Instant::now());
        assert_eq!(rng.range(0), 0);
    }

    /// Rng::range with max=1 always returns 0
    #[test]
    fn rng_range_one_always_zero() {
        let mut rng = Rng::from_instant(Instant::now());
        for _ in 0..10 {
            assert_eq!(rng.range(1), 0);
        }
    }

    /// Rng::range respects upper bound
    #[test]
    fn rng_range_respects_upper_bound() {
        let mut rng = Rng::from_instant(Instant::now());
        for max in [2, 10, 100, 1000] {
            for _ in 0..100 {
                let val = rng.range(max);
                assert!(val < max, "range({max}) returned {val}, should be < {max}");
            }
        }
    }

    /// Rng::range produces variety of results within bounds
    #[test]
    fn rng_range_produces_variety() {
        let mut rng = Rng::from_instant(Instant::now());
        let mut seen = std::collections::HashSet::new();
        for _ in 0..100 {
            seen.insert(rng.range(10));
        }
        // Should see multiple different values in 100 samples
        assert!(
            seen.len() > 3,
            "Expected variety in range(10), got {}",
            seen.len()
        );
    }

    /// Rng::between lo equals hi returns lo
    #[test]
    fn rng_between_equal_bounds_returns_lo() {
        let mut rng = Rng::from_instant(Instant::now());
        for lo in [0u64, 5, 100] {
            for _ in 0..10 {
                assert_eq!(rng.between(lo, lo), lo);
            }
        }
    }

    /// Rng::between respects bounds
    #[test]
    fn rng_between_respects_bounds() {
        let mut rng = Rng::from_instant(Instant::now());
        for _ in 0..100 {
            let val = rng.between(10, 20);
            assert!((10..=20).contains(&val), "between(10,20) returned {val}");
        }
    }

    /// Rng::between produces variety within range
    #[test]
    fn rng_between_produces_variety() {
        let mut rng = Rng::from_instant(Instant::now());
        let mut seen = std::collections::HashSet::new();
        for _ in 0..50 {
            seen.insert(rng.between(0, 10));
        }
        // Should see multiple values
        assert!(seen.len() > 2, "Expected variety in between(0,10)");
    }

    /// Rng::chance with p=0.0 returns false
    #[test]
    fn rng_chance_zero_always_false() {
        let mut rng = Rng::from_instant(Instant::now());
        for _ in 0..20 {
            assert!(!rng.chance(0.0));
        }
    }

    /// Rng::chance with p=1.0 returns true
    #[test]
    fn rng_chance_one_always_true() {
        let mut rng = Rng::from_instant(Instant::now());
        for _ in 0..20 {
            assert!(rng.chance(1.0));
        }
    }

    /// Rng::chance with p=0.5 produces roughly half true/false
    #[test]
    fn rng_chance_half_probability() {
        let mut rng = Rng::from_instant(Instant::now());
        let mut true_count = 0;
        for _ in 0..1000 {
            if rng.chance(0.5) {
                true_count += 1;
            }
        }
        // Should be roughly 500, allow ±150 for randomness
        assert!(
            true_count > 350 && true_count < 650,
            "Expected ~500 trues in 1000 samples with p=0.5, got {true_count}"
        );
    }

    /// DedupRing new creates empty ring
    #[test]
    fn dedup_ring_new_is_empty() {
        let ring = DedupRing::new(5);
        assert_eq!(ring.buf.len(), 0);
        assert_eq!(ring.cap, 5);
    }

    /// DedupRing contains returns false for empty ring
    #[test]
    fn dedup_ring_empty_contains_nothing() {
        let ring = DedupRing::new(5);
        assert!(!ring.contains(0));
        assert!(!ring.contains(99));
    }

    /// DedupRing push and contains work
    #[test]
    fn dedup_ring_push_and_contains() {
        let mut ring = DedupRing::new(5);
        ring.push(3);
        assert!(ring.contains(3));
        assert!(!ring.contains(4));
    }

    /// DedupRing push multiple and all contained
    #[test]
    fn dedup_ring_push_multiple_values() {
        let mut ring = DedupRing::new(5);
        for i in 0..5 {
            ring.push(i);
        }
        for i in 0..5 {
            assert!(ring.contains(i));
        }
    }

    /// DedupRing respects capacity (FIFO eviction)
    #[test]
    fn dedup_ring_respects_capacity_fifo() {
        let mut ring = DedupRing::new(3);
        ring.push(1);
        ring.push(2);
        ring.push(3);
        assert!(ring.contains(1));
        assert!(ring.contains(2));
        assert!(ring.contains(3));

        ring.push(4); // Should evict 1
        assert!(!ring.contains(1));
        assert!(ring.contains(2));
        assert!(ring.contains(3));
        assert!(ring.contains(4));
    }

    /// DedupRing can be cloned
    #[test]
    fn dedup_ring_clone_preserves_contents() {
        let mut ring1 = DedupRing::new(5);
        ring1.push(10);
        ring1.push(20);

        let ring2 = ring1.clone();
        assert!(ring2.contains(10));
        assert!(ring2.contains(20));
    }

    /// DedupRing with capacity 1 still works
    #[test]
    fn dedup_ring_capacity_one() {
        let mut ring = DedupRing::new(1);
        ring.push(5);
        assert!(ring.contains(5));

        ring.push(10);
        assert!(!ring.contains(5));
        assert!(ring.contains(10));
    }

    /// pick_no_repeat prefers unseen values
    #[test]
    fn pick_no_repeat_avoids_seen() {
        let mut rng = Rng::from_instant(Instant::now());
        let mut seen = DedupRing::new(10);

        // Mark 0..5 as seen
        for i in 0..5 {
            seen.push(i);
        }

        // Pick from a set of 10 items where 0..5 are seen
        // Should usually pick from 5..10
        let mut picked = Vec::new();
        for _ in 0..20 {
            picked.push(pick_no_repeat(&mut rng, 10, &seen));
        }

        // Most picks should be >= 5
        let unseen_picks = picked.iter().filter(|i| **i >= 5).count();
        assert!(
            unseen_picks >= 15,
            "Expected most picks to be unseen (>=5), got only {unseen_picks}/20"
        );
    }

    /// pick_no_repeat with all items seen falls back to random
    #[test]
    fn pick_no_repeat_fallback_when_all_seen() {
        let mut rng = Rng::from_instant(Instant::now());
        let mut seen = DedupRing::new(3);

        // Mark all 3 items as seen
        for i in 0..3 {
            seen.push(i);
        }

        // Should still return a valid index
        let result = pick_no_repeat(&mut rng, 3, &seen);
        assert!(result < 3);
    }

    /// pick_no_repeat with empty seen ring always picks fresh
    #[test]
    fn pick_no_repeat_with_empty_dedup() {
        let mut rng = Rng::from_instant(Instant::now());
        let seen = DedupRing::new(10);

        let result = pick_no_repeat(&mut rng, 10, &seen);
        assert!(result < 10);
    }

    /// Rng::chance with boundary probabilities
    #[test]
    fn rng_chance_boundary_values() {
        let mut rng = Rng::from_instant(Instant::now());

        // Very low probability
        let mut low_true = 0;
        for _ in 0..100 {
            if rng.chance(0.01) {
                low_true += 1;
            }
        }
        assert!(
            low_true <= 10,
            "0.01 probability in 100 samples should give ~1 true"
        );

        // Very high probability
        let mut high_true = 0;
        for _ in 0..100 {
            if rng.chance(0.99) {
                high_true += 1;
            }
        }
        assert!(
            high_true >= 90,
            "0.99 probability in 100 samples should give ~99 trues"
        );
    }
}
