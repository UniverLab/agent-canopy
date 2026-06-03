//! Firefly particle scene (night mode).
//!
//! Each firefly = 3 terminal cells rendered separately:
//!   [left_wing] [!] [right_wing]
//!
//! Wing frames (fast cycle):  ≽/= />   and  ≼/= /<
//! Center `!` is always dim gray. On "glow" frames, its background
//! flashes a soft yellow — as if the light source pulses.
//!
//! Fireflies repel from the mouse cursor and move faster overall.

use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::tui::atmosphere::{AtmosphereCtx, Particle, Scene};
use crate::tui::whimsg::rng::Rng;
use std::time::Instant;

/// Wing pairs: (left, right). Index 0 = open, 1 = mid, 2 = closed.
const WINGS: [(&str, &str); 3] = [("≽", "≼"), ("=", "="), (">", "<")];
/// Frame 0 is the "glow" frame (wings fully open).
const FRAME_DURATION: f32 = 0.12; // fast wing flap
const MAX_FIREFLIES: usize = 7;
const SPAWN_INTERVAL_SECS: f32 = 3.0;

/// Center body color (dim gray when not glowing).
const BODY_DIM: Color = Color::Rgb(100, 100, 100);
/// Wing color.
const WING_COLOR: Color = Color::Rgb(90, 110, 90);
/// Glow background — soft warm yellow pulse on center cell.
const GLOW_BG: Color = Color::Rgb(60, 50, 10);

/// Speed range (cells/sec).
const SPEED_MIN: f32 = 1.5;
const SPEED_MAX: f32 = 3.5;
/// Mouse repulsion radius (cells).
const REPEL_RADIUS: f32 = 10.0;
/// Repulsion force multiplier.
const REPEL_FORCE: f32 = 18.0;

struct Firefly {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    frame: usize,
    frame_timer: f32,
    life: f32,
}

pub struct FireflyScene {
    fireflies: Vec<Firefly>,
    rng: Rng,
    spawn_timer: f32,
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
        if self.fireflies.len() >= MAX_FIREFLIES {
            return;
        }
        if area.width < 5 || area.height < 2 {
            return;
        }
        // Keep away from left edge (need col-1 for left wing)
        let x = self.rng.between(2, area.width.saturating_sub(3) as u64) as f32 + area.x as f32;
        let y = self.rng.between(1, area.height.saturating_sub(2) as u64) as f32 + area.y as f32;

        let speed = SPEED_MIN + self.rng.between(0, 100) as f32 / 100.0 * (SPEED_MAX - SPEED_MIN);
        let angle = self.rng.between(0, 628) as f32 / 100.0; // 0..2π
        let vx = angle.cos() * speed;
        let vy = angle.sin() * speed * 0.5; // flatter vertical movement

        // Random starting frame so not all in sync
        let frame = self.rng.range(WINGS.len());

        self.fireflies.push(Firefly {
            x,
            y,
            vx,
            vy,
            frame,
            frame_timer: 0.0,
            life: self.rng.between(30, 80) as f32,
        });
    }
}

impl Scene for FireflyScene {
    fn tick(&mut self, delta_secs: f32, area: Rect, ctx: &AtmosphereCtx) {
        if area.width < 5 || area.height < 2 {
            return;
        }

        if !self.initialized {
            for _ in 0..(MAX_FIREFLIES / 2).max(2) {
                self.spawn(area);
            }
            self.initialized = true;
        }

        self.spawn_timer += delta_secs;
        if self.spawn_timer >= SPAWN_INTERVAL_SECS {
            self.spawn_timer = 0.0;
            self.spawn(area);
        }

        let x_min = area.x as f32 + 1.0; // +1 for left wing cell
        let x_max = (area.x + area.width) as f32 - 2.0; // -1 for right wing cell
        let y_min = area.y as f32;
        let y_max = (area.y + area.height) as f32 - 1.0;

        let mx = ctx.mouse_col as f32;
        let my = ctx.mouse_row as f32;

        self.fireflies.retain_mut(|fly| {
            fly.life -= delta_secs;
            if fly.life <= 0.0 {
                return false;
            }

            // Mouse repulsion
            let dx = fly.x - mx;
            let dy = fly.y - my;
            let dist = (dx * dx + dy * dy).sqrt().max(0.1);
            if dist < REPEL_RADIUS {
                let force = REPEL_FORCE * (1.0 - dist / REPEL_RADIUS) / dist;
                fly.vx += dx * force * delta_secs;
                fly.vy += dy * force * delta_secs;
            }

            // Speed cap
            let spd = (fly.vx * fly.vx + fly.vy * fly.vy).sqrt();
            if spd > SPEED_MAX * 2.0 {
                fly.vx = fly.vx / spd * SPEED_MAX * 2.0;
                fly.vy = fly.vy / spd * SPEED_MAX * 2.0;
            }

            fly.x += fly.vx * delta_secs;
            fly.y += fly.vy * delta_secs;

            // Bounce off edges
            if fly.x < x_min {
                fly.vx = fly.vx.abs();
                fly.x = x_min;
            } else if fly.x > x_max {
                fly.vx = -fly.vx.abs();
                fly.x = x_max;
            }
            if fly.y < y_min {
                fly.vy = fly.vy.abs();
                fly.y = y_min;
            } else if fly.y > y_max {
                fly.vy = -fly.vy.abs();
                fly.y = y_max;
            }

            // Wing animation
            fly.frame_timer += delta_secs;
            if fly.frame_timer >= FRAME_DURATION {
                fly.frame_timer -= FRAME_DURATION;
                fly.frame = (fly.frame + 1) % WINGS.len();
            }

            true
        });
    }

    fn particles(&self) -> Vec<Particle> {
        let mut out = Vec::with_capacity(self.fireflies.len() * 3);
        for fly in &self.fireflies {
            let col = fly.x as u16;
            let row = fly.y as u16;
            let (lw, rw) = WINGS[fly.frame];
            let glowing = fly.frame == 0; // open wings = glow frame

            // Left wing
            out.push(Particle {
                col: col.saturating_sub(1),
                row,
                symbol: lw,
                color: WING_COLOR,
                bg: None,
            });
            // Center body — always `!`, bg flashes on glow frame
            out.push(Particle {
                col,
                row,
                symbol: "!",
                color: BODY_DIM,
                bg: if glowing { Some(GLOW_BG) } else { None },
            });
            // Right wing
            out.push(Particle {
                col: col + 1,
                row,
                symbol: rw,
                color: WING_COLOR,
                bg: None,
            });
        }
        out
    }

    fn is_active(&self) -> bool {
        self.initialized && !self.fireflies.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_firefly_emits_3_particles_per_fly() {
        let mut scene = FireflyScene::new();
        let area = Rect::new(0, 0, 80, 24);
        scene.tick(0.1, area, &AtmosphereCtx::default());
        let n = scene.fireflies.len();
        assert_eq!(scene.particles().len(), n * 3);
    }

    #[test]
    fn test_firefly_mouse_repulsion_moves_fly() {
        let mut scene = FireflyScene::new();
        let area = Rect::new(0, 0, 80, 24);
        scene.fireflies.push(Firefly {
            x: 5.0,
            y: 5.0,
            vx: 0.0,
            vy: 0.0,
            frame: 0,
            frame_timer: 0.0,
            life: 60.0,
        });
        // Mouse 2 cells away — within REPEL_RADIUS
        let ctx = AtmosphereCtx {
            mouse_col: 7,
            mouse_row: 5,
            ..Default::default()
        };
        scene.tick(0.1, area, &ctx);
        let fly = &scene.fireflies[0];
        let moved = (fly.x - 5.0).abs() > 0.01 || (fly.y - 5.0).abs() > 0.01;
        assert!(moved);
    }
}
