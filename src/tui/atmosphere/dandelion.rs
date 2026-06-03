//! Dandelion Seed particle scene (day mode).
//!
//! Single-cell shimmer particles that drift from terminal edges.
//! Shimmer frames: `¤` `*` `+` `·`
//! Mouse cursor acts as a directional wind source.

use std::f32::consts::PI;
use std::time::Instant;

use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::tui::atmosphere::{AtmosphereCtx, Particle, Scene};
use crate::tui::whimsg::rng::Rng;

const SHIMMER_FRAMES: [&str; 4] = ["¤", "*", "+", "·"];
const MAX_SEEDS: usize = 10;
const BASE_SPEED: f32 = 0.4; // phase units per second
const DRIFT_VX: f32 = 2.5; // cells per second (horizontal drift)
const DRIFT_VY: f32 = 0.3; // small vertical drift
const WIND_RADIUS: f32 = 8.0; // cells of wind influence
const WIND_INFLUENCE: f32 = 0.3;
const REPULSION_DIST: f32 = 3.0; // cells for mouse repulsion when stationary
const MAX_LIFE_SECS: f32 = 60.0;
const PERIODIC_SPAWN_SECS: f32 = 30.0; // 3-7 minute range mapped to shorter demo interval

/// Seed color — soft white/silver airy look
const SEED_COLOR: Color = Color::Rgb(200, 220, 255);

struct Seed {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    /// Shimmer phase [0.0, 1.0)
    phase: f32,
    life: f32,
}

pub struct DandelionScene {
    seeds: Vec<Seed>,
    rng: Rng,
    spawn_timer: f32,
    initialized: bool,
}

impl DandelionScene {
    pub fn new() -> Self {
        Self {
            seeds: Vec::with_capacity(MAX_SEEDS),
            rng: Rng::from_instant(Instant::now()),
            spawn_timer: 0.0,
            initialized: false,
        }
    }

    fn spawn_from_edge(&mut self, area: Rect, from_right: bool, count: usize) {
        if area.width < 4 || area.height < 2 {
            return;
        }
        let available = MAX_SEEDS.saturating_sub(self.seeds.len());
        let n = count.min(available);
        for _ in 0..n {
            let x = if from_right {
                (area.x + area.width) as f32 - 1.0
            } else {
                area.x as f32
            };
            let y = self
                .rng
                .between(area.y as u64, (area.y + area.height - 1) as u64)
                as f32;

            // Drift direction: from left goes right, from right goes left
            let vx = if from_right { -DRIFT_VX } else { DRIFT_VX };
            let vy = (self.rng.between(0, 100) as f32 - 50.0) / 100.0 * DRIFT_VY * 4.0;
            let phase = self.rng.between(0, 100) as f32 / 100.0;
            let life = self.rng.between(15, MAX_LIFE_SECS as u64) as f32;

            self.seeds.push(Seed {
                x,
                y,
                vx,
                vy,
                phase,
                life,
            });
        }
    }

    fn shimmer_frame(phase: f32) -> usize {
        // floor(abs(sin(phase * PI)) * 4) clamped to 0..3
        let val = (phase * PI).sin().abs() * 4.0;
        (val as usize).min(SHIMMER_FRAMES.len() - 1)
    }
}

impl Scene for DandelionScene {
    fn tick(&mut self, delta_secs: f32, area: Rect, ctx: &AtmosphereCtx) {
        if area.width < 4 || area.height < 2 {
            return;
        }

        if !self.initialized {
            // Seed a couple of seeds from each side
            self.spawn_from_edge(area, false, 2);
            self.spawn_from_edge(area, true, 1);
            self.initialized = true;
        }

        // Periodic spawn
        self.spawn_timer += delta_secs;
        if self.spawn_timer >= PERIODIC_SPAWN_SECS {
            self.spawn_timer = 0.0;
            let from_right = self.rng.chance(0.5);
            let n = self.rng.between(1, 2) as usize;
            self.spawn_from_edge(area, from_right, n);
        }

        // Gust: high activity triggers extra seeds
        let activity = ctx.scroll_velocity + ctx.typing_speed;
        if activity > 0.5 && self.seeds.len() < MAX_SEEDS {
            let n = self.rng.between(1, 3) as usize;
            let from_right = self.rng.chance(0.5);
            self.spawn_from_edge(area, from_right, n);
        }

        let mouse_stationary = ctx.mouse_delta_col == 0 && ctx.mouse_delta_row == 0;

        let x_min = area.x as f32;
        let x_max = (area.x + area.width) as f32;
        let y_min = area.y as f32;
        let y_max = (area.y + area.height) as f32;

        self.seeds.retain_mut(|seed| {
            seed.life -= delta_secs;
            if seed.life <= 0.0 {
                return false;
            }

            // Shimmer phase update
            let speed = BASE_SPEED + ctx.typing_speed * 0.5 + ctx.scroll_velocity * 0.5;
            seed.phase = (seed.phase + delta_secs * speed) % 1.0;

            // Mouse wind interaction
            let dx = seed.x - ctx.mouse_col as f32;
            let dy = seed.y - ctx.mouse_row as f32;
            let dist = (dx * dx + dy * dy).sqrt();

            if !mouse_stationary && dist < WIND_RADIUS {
                // Directional wind boost from mouse movement
                let influence = (1.0 - dist / WIND_RADIUS) * WIND_INFLUENCE;
                seed.vx += ctx.mouse_delta_col as f32 * influence;
                seed.vy += ctx.mouse_delta_row as f32 * influence;
            } else if mouse_stationary && dist < REPULSION_DIST && dist > 0.01 {
                // Gentle repulsion when mouse is stationary and close
                let repulse = 0.5 * delta_secs / dist;
                seed.vx += dx * repulse;
                seed.vy += dy * repulse;
            }

            // Clamp velocity to prevent runaway
            seed.vx = seed.vx.clamp(-8.0, 8.0);
            seed.vy = seed.vy.clamp(-4.0, 4.0);

            // Move
            seed.x += seed.vx * delta_secs;
            seed.y += seed.vy * delta_secs;

            // Remove if exited from the opposite edge or wandered out
            if seed.x < x_min - 2.0
                || seed.x > x_max + 2.0
                || seed.y < y_min - 1.0
                || seed.y > y_max + 1.0
            {
                return false;
            }

            true
        });
    }

    fn particles(&self) -> Vec<Particle> {
        self.seeds
            .iter()
            .map(|seed| Particle {
                col: seed.x as u16,
                row: seed.y as u16,
                symbol: SHIMMER_FRAMES[Self::shimmer_frame(seed.phase)],
                color: SEED_COLOR,
            })
            .collect()
    }

    fn is_active(&self) -> bool {
        self.initialized && !self.seeds.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shimmer_frame_range() {
        for i in 0..=100 {
            let phase = i as f32 / 100.0;
            let frame = DandelionScene::shimmer_frame(phase);
            assert!(frame < SHIMMER_FRAMES.len(), "frame {frame} out of range");
        }
    }

    #[test]
    fn test_dandelion_spawn_and_tick() {
        let mut scene = DandelionScene::new();
        let area = Rect::new(0, 0, 80, 24);
        let ctx = AtmosphereCtx::default();
        scene.tick(0.1, area, &ctx);
        assert!(scene.is_active());
        assert!(!scene.particles().is_empty());
    }

    #[test]
    fn test_dandelion_respects_max_seeds() {
        let mut scene = DandelionScene::new();
        let area = Rect::new(0, 0, 80, 24);
        let ctx = AtmosphereCtx {
            scroll_velocity: 1.0,
            typing_speed: 1.0,
            ..Default::default()
        };
        for _ in 0..50 {
            scene.tick(0.016, area, &ctx);
        }
        assert!(scene.seeds.len() <= MAX_SEEDS);
    }
}
