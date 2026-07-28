//! Brian's Brain cellular automaton.
//!
//! 3-state automaton: On → Dying → Off → On
//! Rule: Off cell turns On if exactly 2 neighbors are On.
//! Uses toroidal wrapping so patterns flow across edges.
//!
//! Includes automatic particle count validation and noise injection to prevent
//! the automaton from stabilizing with too few particles.
//!
//! Colors use the canopy banner green gradient palette.

use crate::shared::banner::BANNER_GRADIENT;
use std::time::Instant;

// ── Automaton tuning (tuned: slower, less intrusive noise) ───────

const MIN_PARTICLE_THRESHOLD: f64 = 0.004; // lower -> less frequent auto-noise
const LOW_ACTIVITY_THRESHOLD: f64 = 0.010; // lower -> noise triggers only on very low activity
const EDGE_NOISE_PROBABILITY: f64 = 0.08;
const EDGE_PULSE_PROBABILITY: f64 = 0.03;
const NOISE_PULSE_PROBABILITY: f64 = 0.05;
const EDGE_PULSE_BURST_MIN: usize = 1;
const EDGE_PULSE_BURST_MAX: usize = 6;

// Canopy banner green gradient indices for automaton coloring
const BANNER_GREEN_IDX: usize = 3; // Mid-range green
const NOISE_GREEN_IDX: usize = 2; // Brighter green for edge effects

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CellState {
    Off,
    On,
    Dying,
}

pub struct BriansBrain {
    pub grid: Vec<Vec<CellState>>,
    /// Per-cell green channel (0-255) for automaton color variation.
    pub green_grid: Vec<Vec<u8>>,
    pub rows: usize,
    pub cols: usize,
    /// Milliseconds between automaton steps. Throttles work per-frame to avoid UI freezes.
    pub step_interval_ms: u64,
    /// Timestamp of last step.
    pub last_step: Instant,
    /// Mouse movement temporarily speeds up the automaton until this time.
    mouse_boost_until: Instant,
}

/// How long mouse boost lasts (ms).
const MOUSE_BOOST_DURATION_MS: u64 = 1000;
/// Step interval during mouse boost.
const MOUSE_BOOST_STEP_MS: u64 = 30;

impl BriansBrain {
    pub fn new(rows: usize, cols: usize, step_interval_ms: u64) -> Self {
        let mut grid = vec![vec![CellState::Off; cols]; rows];
        let mut green_grid = vec![vec![0u8; cols]; rows];

        // Seed with random scattered On cells along edges and a few inside
        // Use canopy banner greens for the automaton colors
        for r in 0..rows {
            for c in 0..cols {
                if (r == 0 || r == rows - 1 || c == 0 || c == cols - 1)
                    && rand::random::<f64>() < 0.15
                {
                    grid[r][c] = CellState::On;
                    // Use a bright canopy green for edge cells
                    green_grid[r][c] = BANNER_GRADIENT[NOISE_GREEN_IDX].1;
                } else if rand::random::<f64>() < 0.02 {
                    grid[r][c] = CellState::On;
                    // Use mid-range canopy greens for interior cells
                    green_grid[r][c] = BANNER_GRADIENT[3 + (r % 3)].1;
                }
            }
        }

        Self {
            grid,
            green_grid,
            rows,
            cols,
            step_interval_ms,
            last_step: Instant::now(),
            mouse_boost_until: Instant::now(),
        }
    }

    pub fn notify_mouse(&mut self) {
        self.mouse_boost_until =
            Instant::now() + std::time::Duration::from_millis(MOUSE_BOOST_DURATION_MS);
    }

    pub fn step(&mut self) {
        // Throttle steps to avoid CPU spikes / UI freezes
        let effective_interval = if Instant::now() < self.mouse_boost_until {
            MOUSE_BOOST_STEP_MS
        } else {
            self.step_interval_ms
        };
        if effective_interval > 0 {
            if self.last_step.elapsed() < std::time::Duration::from_millis(effective_interval) {
                return;
            }
            self.last_step = Instant::now();
        }

        let mut next = vec![vec![CellState::Off; self.cols]; self.rows];
        let mut next_green = vec![vec![0u8; self.cols]; self.rows];

        for (r, row) in next.iter_mut().enumerate().take(self.rows) {
            for (c, cell) in row.iter_mut().enumerate().take(self.cols) {
                *cell = match self.grid[r][c] {
                    CellState::On => {
                        // Dying cells inherit their parent's green
                        next_green[r][c] = self.green_grid[r][c];
                        CellState::Dying
                    }
                    CellState::Dying => CellState::Off,
                    CellState::Off if self.count_on_neighbors(r, c) == 2 => {
                        // Newborn: average green of the 2 On neighbors
                        next_green[r][c] = self.avg_neighbor_green(r, c);
                        CellState::On
                    }
                    CellState::Off => CellState::Off,
                };
            }
        }
        self.grid = next;
        self.green_grid = next_green;
        self.validate_and_inject_noise();
    }

    /// Average green channel of On neighbors (used for newborn inheritance).
    fn avg_neighbor_green(&self, row: usize, col: usize) -> u8 {
        let mut sum = 0u32;
        let mut count = 0u32;
        for dr in [-1i32, 0, 1] {
            for dc in [-1i32, 0, 1] {
                if dr == 0 && dc == 0 {
                    continue;
                }
                let r = (row as i32 + dr).rem_euclid(self.rows as i32) as usize;
                let c = (col as i32 + dc).rem_euclid(self.cols as i32) as usize;
                if self.grid[r][c] == CellState::On {
                    sum += self.green_grid[r][c] as u32;
                    count += 1;
                }
            }
        }
        if let Some(avg) = sum.checked_div(count).map(|value| value as i16) {
            // Small random drift ±5 to create gradual color variation
            let drift = (rand::random::<i16>() % 11) - 5;
            (avg + drift).clamp(100, 255) as u8
        } else {
            BANNER_GRADIENT[BANNER_GREEN_IDX].1
        }
    }

    fn count_particles(&self) -> usize {
        self.grid
            .iter()
            .flatten()
            .filter(|&&cell| cell == CellState::On)
            .count()
    }

    fn is_edge_cell(&self, row: usize, col: usize) -> bool {
        row == 0 || row == self.rows - 1 || col == 0 || col == self.cols - 1
    }

    fn validate_and_inject_noise(&mut self) {
        let total_cells = self.rows * self.cols;
        let particle_ratio = self.count_particles() as f64 / total_cells as f64;
        if particle_ratio < MIN_PARTICLE_THRESHOLD {
            self.inject_edge_noise(EDGE_NOISE_PROBABILITY);
        } else if particle_ratio < LOW_ACTIVITY_THRESHOLD {
            self.inject_noise_pulse(EDGE_NOISE_PROBABILITY * 0.65);
        } else if rand::random::<f64>() < NOISE_PULSE_PROBABILITY {
            self.inject_noise_pulse(EDGE_PULSE_PROBABILITY);
        }
    }

    fn inject_noise_pulse(&mut self, probability: f64) {
        let burst_span = EDGE_PULSE_BURST_MAX - EDGE_PULSE_BURST_MIN + 1;
        let burst_limit = EDGE_PULSE_BURST_MIN + rand::random::<u32>() as usize % burst_span.max(1);
        let mut injected = 0usize;
        for _ in 0..2 {
            injected += self.inject_edge_noise_until(probability, burst_limit - injected);
            if injected >= burst_limit {
                break;
            }
        }
    }

    fn inject_edge_noise(&mut self, probability: f64) {
        let _ = self.inject_edge_noise_until(probability, usize::MAX);
    }

    fn inject_edge_noise_until(&mut self, probability: f64, max_injections: usize) -> usize {
        let mut injected = 0usize;
        for r in 0..self.rows {
            for c in 0..self.cols {
                if self.is_edge_cell(r, c)
                    && self.grid[r][c] == CellState::Off
                    && rand::random::<f64>() < probability
                {
                    self.grid[r][c] = CellState::On;
                    self.green_grid[r][c] = BANNER_GRADIENT[NOISE_GREEN_IDX].1;
                    injected += 1;
                    if injected >= max_injections {
                        return injected;
                    }
                }
            }
        }
        injected
    }

    fn count_on_neighbors(&self, row: usize, col: usize) -> usize {
        let mut count = 0;
        for dr in [-1i32, 0, 1] {
            for dc in [-1i32, 0, 1] {
                if dr == 0 && dc == 0 {
                    continue;
                }
                let r = (row as i32 + dr).rem_euclid(self.rows as i32) as usize;
                let c = (col as i32 + dc).rem_euclid(self.cols as i32) as usize;
                if self.grid[r][c] == CellState::On {
                    count += 1;
                }
            }
        }
        count
    }
}
