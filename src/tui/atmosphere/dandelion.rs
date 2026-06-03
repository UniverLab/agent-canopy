//! Dandelion Seed particle scene (day mode).
//!
//! Seeds spawn in probabilistic gusts from one edge at a time,
//! all drifting the same global direction. Mouse acts as wind.
//! When no seeds are on screen, the scene is inactive.

use std::f32::consts::PI;
use std::time::Instant;

use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::tui::atmosphere::{AtmosphereCtx, Particle, Scene};
use crate::tui::whimsg::rng::Rng;

const SHIMMER_FRAMES: [&str; 4] = ["¤", "*", "+", "·"];
const MAX_SEEDS: usize = 10;
const BASE_SPEED: f32 = 0.4;
/// Base horizontal drift (cells/sec) — all seeds share same global direction.
const DRIFT_VX: f32 = 3.0;
/// Wind influence factor from mouse movement.
const WIND_INFLUENCE: f32 = 0.08;
const WIND_RADIUS: f32 = 12.0;
const REPULSION_DIST: f32 = 3.0;

/// Probability per-second of triggering a gust when no seeds are active.
const GUST_PROB_PER_SEC: f32 = 0.3;
/// Probability per-second of a small extra drift when seeds are already on screen.
const DRIFT_PROB_PER_SEC: f32 = 0.05;

const SEED_COLOR: Color = Color::Rgb(200, 220, 255);

struct Seed {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    phase: f32,
    life: f32,
}

pub struct DandelionScene {
    seeds: Vec<Seed>,
    rng: Rng,
    /// Global wind direction: +1 = left→right, -1 = right→left
    wind_dir: f32,
}

impl DandelionScene {
    pub fn new() -> Self {
        Self {
            seeds: Vec::with_capacity(MAX_SEEDS),
            rng: Rng::from_instant(Instant::now()),
            wind_dir: 1.0,
        }
    }

    fn spawn_gust(&mut self, area: Rect, count: usize) {
        if area.width < 4 || area.height < 2 {
            return;
        }
        let n = count.min(MAX_SEEDS.saturating_sub(self.seeds.len()));
        // Pick edge based on current wind direction
        let from_right = self.wind_dir < 0.0;
        let x_start = if from_right {
            (area.x + area.width) as f32 - 1.0
        } else {
            area.x as f32
        };
        let vx_base = self.wind_dir * DRIFT_VX;

        for _ in 0..n {
            let y = self
                .rng
                .between(area.y as u64, (area.y + area.height - 1) as u64)
                as f32;
            // Small individual variation around the global direction
            let vx = vx_base + (self.rng.between(0, 60) as f32 - 30.0) / 100.0;
            let vy = (self.rng.between(0, 80) as f32 - 40.0) / 100.0;
            let phase = self.rng.between(0, 100) as f32 / 100.0;
            let life = self.rng.between(20, 55) as f32;
            self.seeds.push(Seed {
                x: x_start,
                y,
                vx,
                vy,
                phase,
                life,
            });
        }
    }

    fn shimmer_frame(phase: f32) -> usize {
        let val = (phase * PI).sin().abs() * 4.0;
        (val as usize).min(SHIMMER_FRAMES.len() - 1)
    }
}

impl Scene for DandelionScene {
    fn tick(&mut self, delta_secs: f32, area: Rect, ctx: &AtmosphereCtx) {
        if area.width < 4 || area.height < 2 {
            return;
        }

        // Probabilistic spawn logic
        if self.seeds.is_empty() {
            // Flip wind direction occasionally
            if self.rng.chance(0.25) {
                self.wind_dir = -self.wind_dir;
            }
            if self
                .rng
                .chance((GUST_PROB_PER_SEC * delta_secs).min(1.0) as f64)
            {
                let count = self.rng.between(2, 4) as usize;
                self.spawn_gust(area, count);
            }
        } else if self.seeds.len() < MAX_SEEDS {
            // Occasional extra seed while gust is in progress
            if self
                .rng
                .chance((DRIFT_PROB_PER_SEC * delta_secs).min(1.0) as f64)
            {
                self.spawn_gust(area, 1);
            }
        }

        let mouse_moving = ctx.mouse_delta_col != 0 || ctx.mouse_delta_row != 0;

        let x_exit_min = area.x as f32 - 3.0;
        let x_exit_max = (area.x + area.width) as f32 + 3.0;
        let y_exit_min = area.y as f32 - 2.0;
        let y_exit_max = (area.y + area.height) as f32 + 2.0;

        self.seeds.retain_mut(|seed| {
            seed.life -= delta_secs;
            if seed.life <= 0.0 {
                return false;
            }

            // Shimmer phase
            let speed = BASE_SPEED + ctx.typing_speed * 0.5 + ctx.scroll_velocity * 0.5;
            seed.phase = (seed.phase + delta_secs * speed) % 1.0;

            // Mouse wind — applied to vx/vy as an impulse, then decays
            let dx = seed.x - ctx.mouse_col as f32;
            let dy = seed.y - ctx.mouse_row as f32;
            let dist = (dx * dx + dy * dy).sqrt();

            if mouse_moving && dist < WIND_RADIUS {
                let influence = (1.0 - dist / WIND_RADIUS) * WIND_INFLUENCE;
                seed.vx += ctx.mouse_delta_col as f32 * influence;
                seed.vy += ctx.mouse_delta_row as f32 * influence;
            } else if !mouse_moving && dist < REPULSION_DIST && dist > 0.1 {
                let repulse = 0.3 * delta_secs / dist;
                seed.vx += dx * repulse;
                seed.vy += dy * repulse;
            }

            // Gently restore drift direction so wind boost doesn't make them go backwards
            let target_vx = self.wind_dir * DRIFT_VX;
            seed.vx += (target_vx - seed.vx) * delta_secs * 0.5;
            seed.vx = seed.vx.clamp(-10.0, 10.0);
            seed.vy = seed.vy.clamp(-3.0, 3.0);

            seed.x += seed.vx * delta_secs;
            seed.y += seed.vy * delta_secs;

            // Exit when they cross to the opposite side
            seed.x > x_exit_min && seed.x < x_exit_max && seed.y > y_exit_min && seed.y < y_exit_max
        });
    }

    fn particles(&self) -> Vec<Particle> {
        self.seeds
            .iter()
            .filter(|s| s.x >= 0.0 && s.y >= 0.0)
            .map(|seed| Particle {
                col: seed.x as u16,
                row: seed.y as u16,
                symbol: SHIMMER_FRAMES[Self::shimmer_frame(seed.phase)],
                color: SEED_COLOR,
                bg: None,
            })
            .collect()
    }

    fn is_active(&self) -> bool {
        !self.seeds.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shimmer_frame_range() {
        for i in 0..=100 {
            let frame = DandelionScene::shimmer_frame(i as f32 / 100.0);
            assert!(frame < SHIMMER_FRAMES.len());
        }
    }

    #[test]
    fn test_seeds_respect_max() {
        let mut scene = DandelionScene::new();
        let area = Rect::new(0, 0, 80, 24);
        let ctx = AtmosphereCtx {
            scroll_velocity: 1.0,
            ..Default::default()
        };
        for _ in 0..200 {
            scene.tick(0.05, area, &ctx);
        }
        assert!(scene.seeds.len() <= MAX_SEEDS);
    }

    #[test]
    fn test_all_seeds_roughly_same_direction() {
        let mut scene = DandelionScene::new();
        let area = Rect::new(0, 0, 80, 24);
        let ctx = AtmosphereCtx::default();
        // force a gust
        scene.spawn_gust(area, 5);
        scene.tick(0.05, area, &ctx);
        let dirs: Vec<f32> = scene.seeds.iter().map(|s| s.vx.signum()).collect();
        if dirs.len() > 1 {
            // All should share the same sign
            assert!(dirs.iter().all(|&d| d == dirs[0]));
        }
    }
}
