//! Dandelion Seed particle scene (day mode).
//!
//! Seeds spawn in probabilistic gusts, all sharing a global drift direction.
//! Mouse cursor acts as an **attractor** — seeds within range are pulled toward
//! it and conserve that new trajectory afterward (no restoration to wind_dir).

use std::f32::consts::PI;
use std::time::Instant;

use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::tui::atmosphere::{AtmosphereCtx, Particle, Scene};
use crate::tui::whimsg::rng::Rng;

const SHIMMER_FRAMES: [&str; 4] = ["¤", "*", "+", "·"];
const MAX_SEEDS: usize = 10;
const BASE_SPEED: f32 = 0.4;
const DRIFT_VX: f32 = 3.0;

/// Attraction radius (cells).
const ATTRACT_RADIUS: f32 = 25.0;
/// Attraction force — pulls seed toward cursor.
const ATTRACT_FORCE: f32 = 10.0;

/// Universal repulsion radius — pushes ALL particles away when very close.
const UNIVERSAL_REPEL_RADIUS: f32 = 4.0;
/// Universal repulsion force — strong push to keep particles off the cursor.
const UNIVERSAL_REPEL_FORCE: f32 = 30.0;

const GUST_PROB_PER_SEC: f32 = 0.3;
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
    fn tick(&mut self, delta_secs: f32, area: Rect, ctx: &mut AtmosphereCtx) {
        if area.width < 4 || area.height < 2 {
            return;
        }

        // Probabilistic spawn
        if self.seeds.is_empty() {
            if self.rng.chance(0.3) {
                self.wind_dir = -self.wind_dir;
            }
            if self
                .rng
                .chance((GUST_PROB_PER_SEC * delta_secs).clamp(0.0, 1.0) as f64)
            {
                let count = self.rng.between(2, 4) as usize;
                self.spawn_gust(area, count);
            }
        } else if self.seeds.len() < MAX_SEEDS
            && self
                .rng
                .chance((DRIFT_PROB_PER_SEC * delta_secs).clamp(0.0, 1.0) as f64)
        {
            self.spawn_gust(area, 1);
        }

        let x_exit_min = area.x as f32 - 3.0;
        let x_exit_max = (area.x + area.width) as f32 + 3.0;
        let y_exit_min = area.y as f32 - 2.0;
        let y_exit_max = (area.y + area.height) as f32 + 2.0;

        let mx = ctx.mouse_col as f32;
        let my = ctx.mouse_row as f32;

        self.seeds.retain_mut(|seed| {
            seed.life -= delta_secs;
            if seed.life <= 0.0 {
                return false;
            }

            seed.phase = (seed.phase
                + delta_secs * (BASE_SPEED + ctx.typing_speed * 0.5 + ctx.scroll_velocity * 0.5))
                % 1.0;

            // Cursor attraction — pull toward mouse, no restoration afterward
            let dx = mx - seed.x;
            let dy = my - seed.y;
            let dist = (dx * dx + dy * dy).sqrt().max(0.1);
            if dist < ATTRACT_RADIUS {
                let force = ATTRACT_FORCE * (1.0 - dist / ATTRACT_RADIUS) / dist;
                seed.vx += dx * force * delta_secs;
                seed.vy += dy * force * delta_secs;
            }

            // Universal repulsion — push away when very close to cursor
            if dist < UNIVERSAL_REPEL_RADIUS {
                let force = UNIVERSAL_REPEL_FORCE * (1.0 - dist / UNIVERSAL_REPEL_RADIUS) / dist;
                seed.vx -= dx * force * delta_secs;
                seed.vy -= dy * force * delta_secs;
            }

            // Soft speed cap only — trajectory is conserved
            let spd = (seed.vx * seed.vx + seed.vy * seed.vy).sqrt();
            if spd > DRIFT_VX * 3.0 {
                seed.vx = seed.vx / spd * DRIFT_VX * 3.0;
                seed.vy = seed.vy / spd * DRIFT_VX * 3.0;
            }

            seed.x += seed.vx * delta_secs;
            seed.y += seed.vy * delta_secs;

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
            assert!(DandelionScene::shimmer_frame(i as f32 / 100.0) < SHIMMER_FRAMES.len());
        }
    }

    #[test]
    fn test_seeds_respect_max() {
        let mut scene = DandelionScene::new();
        let area = Rect::new(0, 0, 80, 24);
        let mut ctx = AtmosphereCtx {
            scroll_velocity: 1.0,
            ..Default::default()
        };
        for _ in 0..200 {
            scene.tick(0.05, area, &mut ctx);
        }
        assert!(scene.seeds.len() <= MAX_SEEDS);
    }

    #[test]
    fn test_cursor_attracts_seed() {
        let mut scene = DandelionScene::new();
        let area = Rect::new(0, 0, 80, 24);
        // Seed on the left, mouse on the right
        scene.seeds.push(Seed {
            x: 10.0,
            y: 12.0,
            vx: 0.0,
            vy: 0.0,
            phase: 0.0,
            life: 60.0,
        });
        // Mouse 20 cells to the right — within ATTRACT_RADIUS (25)
        let mut ctx = AtmosphereCtx {
            mouse_col: 30,
            mouse_row: 12,
            ..Default::default()
        };
        scene.tick(0.1, area, &mut ctx);
        // vx should have increased toward the mouse (positive direction)
        assert!(scene.seeds[0].vx > 0.0);
    }

    #[test]
    fn test_universal_repulsion_pushes_seed_away() {
        let mut scene = DandelionScene::new();
        let area = Rect::new(0, 0, 80, 24);
        // Seed very close to cursor — universal repulsion should kick in
        scene.seeds.push(Seed {
            x: 10.0,
            y: 12.0,
            vx: 0.0,
            vy: 0.0,
            phase: 0.0,
            life: 60.0,
        });
        // Mouse 2 cells away — within UNIVERSAL_REPEL_RADIUS (4)
        let mut ctx = AtmosphereCtx {
            mouse_col: 12,
            mouse_row: 12,
            ..Default::default()
        };
        scene.tick(0.1, area, &mut ctx);
        // vx should be negative (pushed away from mouse)
        assert!(scene.seeds[0].vx < 0.0);
    }
}
