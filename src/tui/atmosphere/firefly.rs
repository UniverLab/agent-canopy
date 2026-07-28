//! Firefly particle scene (night mode).
//!
//! 3 cells per firefly: [left_wing] [¡] [right_wing]
//! Glow is independent of wing animation — probabilistic ignition
//! that builds up over time and lasts 500–2000ms randomly.

use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::tui::atmosphere::{AtmosphereCtx, Particle, Scene};
use crate::tui::whimsg::rng::Rng;
use std::time::Instant;

/// Wing pairs: (left, right).
const WINGS: [(&str, &str); 3] = [("≽", "≼"), ("=", "="), (">", "<")];
const FRAME_DURATION: f32 = 0.13;
const MAX_FIREFLIES: usize = 4;
const SPAWN_INTERVAL_SECS: f32 = 4.0;

const BODY_DIM: Color = Color::Rgb(80, 80, 80);
const WING_DIM: Color = Color::Rgb(70, 90, 70);
/// Bright glow fg when lit
const BODY_GLOW_FG: Color = Color::Rgb(255, 240, 120);
/// Glow bg — warm yellow
const GLOW_BG: Color = Color::Rgb(80, 65, 10);

const SPEED_MIN: f32 = 1.5;
const SPEED_MAX: f32 = 3.5;
const REPEL_RADIUS: f32 = 18.0;
const REPEL_FORCE: f32 = 35.0;

/// Universal repulsion radius — pushes ALL particles away when very close.
const UNIVERSAL_REPEL_RADIUS: f32 = 4.0;
/// Universal repulsion force — strong push to keep particles off the cursor.
const UNIVERSAL_REPEL_FORCE: f32 = 30.0;

/// Max seconds before a firefly is guaranteed to glow (probability ramps linearly).
const GLOW_RAMP_SECS: f32 = 20.0;

/// How much faster fireflies fade once the scene is outside its time window.
const WINDDOWN_FADE: f32 = 4.0;

#[derive(Clone, Copy, PartialEq)]
enum GlowState {
    /// Off — time_in_state tracks seconds dark (used to ramp ignition probability)
    Off,
    /// On — time_in_state tracks remaining glow duration
    On,
}

struct Firefly {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    frame: usize,
    frame_timer: f32,
    life: f32,
    glow: GlowState,
    /// Seconds spent in current glow state
    time_in_state: f32,
    /// Glow duration chosen at ignition (seconds)
    glow_duration: f32,
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
        if self.fireflies.len() >= MAX_FIREFLIES || area.width < 5 || area.height < 2 {
            return;
        }
        let x = self.rng.between(2, area.width.saturating_sub(3) as u64) as f32 + area.x as f32;
        let y = self.rng.between(1, area.height.saturating_sub(2) as u64) as f32 + area.y as f32;
        let speed = SPEED_MIN + self.rng.between(0, 100) as f32 / 100.0 * (SPEED_MAX - SPEED_MIN);
        let angle = self.rng.between(0, 628) as f32 / 100.0;
        // Start dark with random offset so they don't all ignite at once
        let initial_dark = self.rng.between(0, GLOW_RAMP_SECS as u64 / 2) as f32;
        self.fireflies.push(Firefly {
            x,
            y,
            vx: angle.cos() * speed,
            vy: angle.sin() * speed * 0.5,
            frame: self.rng.range(WINGS.len()),
            frame_timer: 0.0,
            life: self.rng.between(40, 90) as f32,
            glow: GlowState::Off,
            time_in_state: initial_dark,
            glow_duration: 0.0,
        });
    }
}

impl Scene for FireflyScene {
    fn tick(&mut self, delta_secs: f32, area: Rect, ctx: &mut AtmosphereCtx) {
        if area.width < 5 || area.height < 2 {
            return;
        }
        // Only seed new fireflies inside the time window. When winding down
        // (`!ctx.spawning`) the existing swarm keeps flying and fades out.
        if ctx.spawning {
            if !self.initialized {
                for _ in 0..2 {
                    self.spawn(area);
                }
                self.initialized = true;
            }

            self.spawn_timer += delta_secs;
            if self.spawn_timer >= SPAWN_INTERVAL_SECS {
                self.spawn_timer = 0.0;
                self.spawn(area);
            }
        }

        let fade = if ctx.spawning {
            delta_secs
        } else {
            delta_secs * WINDDOWN_FADE
        };

        let x_min = area.x as f32 + 1.0;
        let x_max = (area.x + area.width) as f32 - 2.0;
        let y_min = area.y as f32;
        let y_max = (area.y + area.height) as f32 - 1.0;
        let mx = ctx.mouse_col as f32;
        let my = ctx.mouse_row as f32;

        if ctx.mouse_clicked {
            for fly in &self.fireflies {
                let dx = fly.x - mx;
                let dy = fly.y - my;
                let dist = (dx * dx + dy * dy).sqrt();
                if dist <= 1.5 {
                    ctx.firefly_caught = true;
                    break;
                }
            }
        }

        self.fireflies.retain_mut(|fly| {
            fly.life -= fade;
            if fly.life <= 0.0 {
                return false;
            }

            // Glow state machine
            fly.time_in_state += delta_secs;
            match fly.glow {
                GlowState::Off => {
                    // Probability ramps from 0% at t=0 to 100% at GLOW_RAMP_SECS
                    let prob_per_sec = (fly.time_in_state / GLOW_RAMP_SECS).clamp(0.0, 1.0);
                    if self
                        .rng
                        .chance((prob_per_sec * delta_secs * 5.0).min(1.0) as f64)
                    {
                        fly.glow = GlowState::On;
                        fly.time_in_state = 0.0;
                        // 500–2000 ms
                        fly.glow_duration = 0.5 + self.rng.between(0, 150) as f32 / 100.0;
                    }
                }
                GlowState::On => {
                    if fly.time_in_state >= fly.glow_duration {
                        fly.glow = GlowState::Off;
                        fly.time_in_state = 0.0;
                    }
                }
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

            // Universal repulsion — push away when very close to cursor
            if dist < UNIVERSAL_REPEL_RADIUS {
                let force = UNIVERSAL_REPEL_FORCE * (1.0 - dist / UNIVERSAL_REPEL_RADIUS) / dist;
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
            let lit = fly.glow == GlowState::On;

            out.push(Particle {
                col: col.saturating_sub(1),
                row,
                symbol: lw,
                color: WING_DIM,
                bg: None,
            });
            out.push(Particle {
                col,
                row,
                symbol: "¡",
                color: if lit { BODY_GLOW_FG } else { BODY_DIM },
                bg: if lit { Some(GLOW_BG) } else { None },
            });
            out.push(Particle {
                col: col + 1,
                row,
                symbol: rw,
                color: WING_DIM,
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
        let mut ctx = AtmosphereCtx {
            spawning: true,
            ..Default::default()
        };
        scene.tick(0.1, area, &mut ctx);
        assert_eq!(scene.particles().len(), scene.fireflies.len() * 3);
    }

    #[test]
    fn test_glow_ignites_within_ramp() {
        let mut scene = FireflyScene::new();
        scene.fireflies.push(Firefly {
            x: 20.0,
            y: 10.0,
            vx: 0.0,
            vy: 0.0,
            frame: 0,
            frame_timer: 0.0,
            life: 999.0,
            glow: GlowState::Off,
            // Already at max ramp — should ignite quickly
            time_in_state: GLOW_RAMP_SECS,
            glow_duration: 0.0,
        });
        let area = Rect::new(0, 0, 80, 24);
        let mut ctx = AtmosphereCtx::default();
        // Tick many times at max probability; should ignite within ~50 ticks
        let lit = (0..200).any(|_| {
            scene.tick(0.05, area, &mut ctx);
            scene
                .fireflies
                .first()
                .is_some_and(|f| f.glow == GlowState::On)
        });
        assert!(lit);
    }

    #[test]
    fn test_repulsion_moves_fly() {
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
            glow: GlowState::Off,
            time_in_state: 0.0,
            glow_duration: 0.0,
        });
        // Mouse 10 cells away — within REPEL_RADIUS (18)
        let mut ctx = AtmosphereCtx {
            mouse_col: 15,
            mouse_row: 5,
            ..Default::default()
        };
        scene.tick(0.1, area, &mut ctx);
        let fly = &scene.fireflies[0];
        assert!((fly.x - 5.0).abs() > 0.01 || (fly.y - 5.0).abs() > 0.01);
    }

    #[test]
    fn test_firefly_catch_on_click_within_range() {
        let mut scene = FireflyScene::new();
        let area = Rect::new(0, 0, 80, 24);
        scene.fireflies.push(Firefly {
            x: 10.0,
            y: 12.0,
            vx: 0.0,
            vy: 0.0,
            frame: 0,
            frame_timer: 0.0,
            life: 60.0,
            glow: GlowState::Off,
            time_in_state: 0.0,
            glow_duration: 0.0,
        });
        let mut ctx = AtmosphereCtx {
            mouse_col: 11,
            mouse_row: 12,
            mouse_clicked: true,
            ..Default::default()
        };
        scene.tick(0.1, area, &mut ctx);
        assert!(ctx.firefly_caught);
    }

    #[test]
    fn test_universal_repulsion_pushes_fly_away() {
        let mut scene = FireflyScene::new();
        let area = Rect::new(0, 0, 80, 24);
        scene.fireflies.push(Firefly {
            x: 10.0,
            y: 12.0,
            vx: 0.0,
            vy: 0.0,
            frame: 0,
            frame_timer: 0.0,
            life: 60.0,
            glow: GlowState::Off,
            time_in_state: 0.0,
            glow_duration: 0.0,
        });
        // Mouse 2 cells away — within UNIVERSAL_REPEL_RADIUS (4)
        let mut ctx = AtmosphereCtx {
            mouse_col: 12,
            mouse_row: 12,
            ..Default::default()
        };
        scene.tick(0.1, area, &mut ctx);
        let fly = &scene.fireflies[0];
        // Should be pushed away (x < 10 or vx negative)
        assert!(fly.x < 10.0 || fly.vx < 0.0);
    }
}
