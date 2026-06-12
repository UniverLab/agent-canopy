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
