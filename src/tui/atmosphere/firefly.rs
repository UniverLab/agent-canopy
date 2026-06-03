//! Firefly particle scene (night mode).
//!
//! 3-frame "living light" pulse cycling through:
//!   Open : `≽¡≼`  (wings extended, top-dot)
//!   Mid  : `=|=`   (wings neutral)
//!   Closed: `>!<`  (wings tucked, bottom-dot)

use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::tui::atmosphere::{AtmosphereCtx, Particle, Scene};
use crate::tui::whimsg::rng::Rng;
use std::time::Instant;

const FRAMES: [&str; 3] = ["≽¡≼", "=|=", ">!<"];
const FRAME_DURATION: f32 = 0.35; // seconds per frame
const MAX_FIREFLIES: usize = 8;
const FIREFLY_COLOR: Color = Color::Rgb(255, 230, 80); // warm yellow
const SPAWN_INTERVAL_SECS: f32 = 2.5;

struct Firefly {
    /// Sub-cell position (fractional for smooth movement)
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    /// Current animation frame index (0-2)
    frame: usize,
    /// Time accumulator for frame cycling
    frame_timer: f32,
    /// Remaining lifetime in seconds
    life: f32,
}

pub struct FireflyScene {
    fireflies: Vec<Firefly>,
    rng: Rng,
    spawn_timer: f32,
    /// Whether we've ever been ticked with a valid area
    initialized: bool,
}

impl FireflyScene {
    pub fn new() -> Self {
        Self {
            fireflies: Vec::with_capacity(MAX_FIREFLIES),
            rng: Rng::from_instant(Instant::now()),
            spawn_timer: 0.0,
            initialized: false,
        }
    }

    fn spawn(&mut self, area: Rect) {
        if area.width < 4 || area.height < 2 {
            return;
        }
        if self.fireflies.len() >= MAX_FIREFLIES {
            return;
        }

        let x = self.rng.between(2, (area.width.saturating_sub(4)) as u64) as f32 + area.x as f32;
        let y = self.rng.between(1, (area.height.saturating_sub(2)) as u64) as f32 + area.y as f32;

        // Small random velocity: ±0.5 cells/sec
        let vx = (self.rng.between(0, 100) as f32 - 50.0) / 100.0;
        let vy = (self.rng.between(0, 60) as f32 - 30.0) / 100.0;
        let life = self.rng.between(60, 120) as f32; // 60–120 seconds

        self.fireflies.push(Firefly {
            x,
            y,
            vx,
            vy,
            frame: 0,
            frame_timer: 0.0,
            life,
        });
    }
}

impl Scene for FireflyScene {
    fn tick(&mut self, delta_secs: f32, area: Rect, _ctx: &AtmosphereCtx) {
        if area.width < 4 || area.height < 2 {
            return;
        }

        if !self.initialized {
            // Seed a few fireflies immediately so the scene looks alive from the start
            let initial = (MAX_FIREFLIES / 2).max(1);
            for _ in 0..initial {
                self.spawn(area);
            }
            self.initialized = true;
        }

        // Spawn timer
        self.spawn_timer += delta_secs;
        if self.spawn_timer >= SPAWN_INTERVAL_SECS {
            self.spawn_timer = 0.0;
            self.spawn(area);
        }

        let x_min = area.x as f32;
        let x_max = (area.x + area.width) as f32 - 3.0; // leave room for 3-char glyph
        let y_min = area.y as f32;
        let y_max = (area.y + area.height) as f32 - 1.0;

        self.fireflies.retain_mut(|fly| {
            fly.life -= delta_secs;
            if fly.life <= 0.0 {
                return false;
            }

            // Move
            fly.x += fly.vx * delta_secs;
            fly.y += fly.vy * delta_secs;

            // Bounce softly off edges
            if fly.x < x_min || fly.x > x_max {
                fly.vx = -fly.vx;
                fly.x = fly.x.clamp(x_min, x_max);
            }
            if fly.y < y_min || fly.y > y_max {
                fly.vy = -fly.vy;
                fly.y = fly.y.clamp(y_min, y_max);
            }

            // Advance animation frame
            fly.frame_timer += delta_secs;
            if fly.frame_timer >= FRAME_DURATION {
                fly.frame_timer -= FRAME_DURATION;
                fly.frame = (fly.frame + 1) % FRAMES.len();
            }

            true
        });
    }

    fn particles(&self) -> Vec<Particle> {
        self.fireflies
            .iter()
            .map(|fly| Particle {
                col: fly.x as u16,
                row: fly.y as u16,
                symbol: FRAMES[fly.frame],
                color: FIREFLY_COLOR,
            })
            .collect()
    }

    fn is_active(&self) -> bool {
        self.initialized && !self.fireflies.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_firefly_spawn_and_tick() {
        let mut scene = FireflyScene::new();
        let area = Rect::new(0, 0, 80, 24);
        let ctx = AtmosphereCtx::default();
        scene.tick(0.1, area, &ctx);
        assert!(scene.is_active());
        assert!(!scene.particles().is_empty());
    }

    #[test]
    fn test_firefly_particles_within_area() {
        let mut scene = FireflyScene::new();
        let area = Rect::new(0, 0, 80, 24);
        let ctx = AtmosphereCtx::default();
        // Tick enough to initialize
        for _ in 0..5 {
            scene.tick(0.05, area, &ctx);
        }
        for p in scene.particles() {
            assert!(p.col < 80, "col {} out of bounds", p.col);
            assert!(p.row < 24, "row {} out of bounds", p.row);
        }
    }
}
