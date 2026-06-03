//! Atmosphere Engine — ephemeral particle overlays for the Canopy TUI.
//!
//! Renders after all other UI panels so particles appear above content.
//! All state is in-memory and re-derived every frame; nothing persists.

mod dandelion;
mod firefly;

pub use dandelion::DandelionScene;
pub use firefly::FireflyScene;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use std::time::Instant;

// ── Particle render primitive ─────────────────────────────────────

/// A single cell to paint on the terminal buffer as an overlay.
#[derive(Clone)]
pub struct Particle {
    pub col: u16,
    pub row: u16,
    pub symbol: &'static str,
    pub color: ratatui::style::Color,
    /// Optional background color for this cell.
    pub bg: Option<ratatui::style::Color>,
}

// ── Scene trait ───────────────────────────────────────────────────

/// A self-contained particle animation.
pub trait Scene: Send {
    /// Advance the simulation by `delta_secs` seconds.
    fn tick(&mut self, delta_secs: f32, area: Rect, ctx: &AtmosphereCtx);
    /// Collect the particles for this frame.
    fn particles(&self) -> Vec<Particle>;
    /// Whether the scene has anything to render right now.
    fn is_active(&self) -> bool;
}

// ── Context passed to scenes each tick ───────────────────────────

#[derive(Clone, Default)]
pub struct AtmosphereCtx {
    /// Current hour of day (0–23).
    pub hour: u8,
    /// Mouse position, updated from events.
    pub mouse_col: u16,
    pub mouse_row: u16,
    /// Delta mouse movement since last tick (for wind).
    pub mouse_delta_col: i16,
    pub mouse_delta_row: i16,
    /// Scroll velocity hint (0.0 = idle, 1.0 = fast).
    pub scroll_velocity: f32,
    /// Typing speed hint (0.0 = idle, 1.0 = fast).
    pub typing_speed: f32,
}

// ── Prerequisites ─────────────────────────────────────────────────

#[derive(Clone)]
pub struct EventPrerequisites {
    /// Inclusive hour range [start, end]. Wraps around midnight (e.g. 22–4).
    pub time_range: Option<(u8, u8)>,
}

impl EventPrerequisites {
    fn matches(&self, ctx: &AtmosphereCtx) -> bool {
        let Some((start, end)) = self.time_range else {
            return true;
        };
        if start <= end {
            ctx.hour >= start && ctx.hour <= end
        } else {
            // Wraps midnight (e.g. 22–4): active if hour >= start OR hour <= end
            ctx.hour >= start || ctx.hour <= end
        }
    }
}

// ── AtmosphereEvent ───────────────────────────────────────────────

#[allow(dead_code)]
pub struct AtmosphereEvent {
    pub id: &'static str,
    pub prerequisites: EventPrerequisites,
    pub scene: Box<dyn Scene>,
}

// ── SceneManager ──────────────────────────────────────────────────

/// Owns all registered events and drives the active scene.
pub struct SceneManager {
    events: Vec<AtmosphereEvent>,
    last_tick: Instant,
}

impl SceneManager {
    pub fn new() -> Self {
        let events = vec![
            AtmosphereEvent {
                id: "fireflies",
                prerequisites: EventPrerequisites {
                    // time_range: Some((22, 4)), // night 22:00–04:00 — uncomment for prod
                    time_range: None,
                },
                scene: Box::new(FireflyScene::new()),
            },
            AtmosphereEvent {
                id: "dandelion",
                prerequisites: EventPrerequisites {
                    time_range: Some((6, 18)), // 06:00 – 18:00
                },
                scene: Box::new(DandelionScene::new()),
            },
        ];
        Self {
            events,
            last_tick: Instant::now(),
        }
    }

    /// Advance all matching scenes and collect their particles.
    pub fn tick(&mut self, area: Rect, ctx: &AtmosphereCtx) {
        let delta = self.last_tick.elapsed().as_secs_f32().min(0.1);
        self.last_tick = Instant::now();

        for event in &mut self.events {
            if event.prerequisites.matches(ctx) {
                event.scene.tick(delta, area, ctx);
            }
        }
    }

    /// Render all active particles onto `buf` as a top-layer overlay.
    pub fn render(&self, buf: &mut Buffer, area: Rect) {
        for event in &self.events {
            if !event.scene.is_active() {
                continue;
            }
            for p in event.scene.particles() {
                if p.col < area.x
                    || p.row < area.y
                    || p.col >= area.x + area.width
                    || p.row >= area.y + area.height
                {
                    continue;
                }
                let cell = buf.cell_mut((p.col, p.row));
                if let Some(cell) = cell {
                    cell.set_symbol(p.symbol);
                    cell.set_fg(p.color);
                    if let Some(bg) = p.bg {
                        cell.set_bg(bg);
                    }
                }
            }
        }
    }

    /// Notify the atmosphere about mouse movement (forwarded via ctx on next tick).
    pub fn notify_mouse(&mut self, _col: u16, _row: u16, _delta_col: i16, _delta_row: i16) {}
}

// ── Renderer (top-level draw call) ───────────────────────────────

/// Overlay the atmosphere particles on top of the fully-rendered frame.
/// Call this as the very last step in `ui::draw`.
pub fn render_atmosphere(manager: &SceneManager, buf: &mut Buffer, area: Rect) {
    manager.render(buf, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prerequisites_day_range() {
        let p = EventPrerequisites {
            time_range: Some((6, 18)),
        };
        let mut ctx = AtmosphereCtx::default();
        ctx.hour = 12;
        assert!(p.matches(&ctx));
        ctx.hour = 5;
        assert!(!p.matches(&ctx));
        ctx.hour = 19;
        assert!(!p.matches(&ctx));
    }

    #[test]
    fn test_prerequisites_night_range_wraps_midnight() {
        let p = EventPrerequisites {
            time_range: Some((22, 4)),
        };
        let mut ctx = AtmosphereCtx::default();
        ctx.hour = 23;
        assert!(p.matches(&ctx));
        ctx.hour = 0;
        assert!(p.matches(&ctx));
        ctx.hour = 4;
        assert!(p.matches(&ctx));
        ctx.hour = 5;
        assert!(!p.matches(&ctx));
        ctx.hour = 21;
        assert!(!p.matches(&ctx));
    }

    #[test]
    fn test_scene_manager_tick_does_not_panic() {
        let mut mgr = SceneManager::new();
        let area = Rect::new(0, 0, 80, 24);
        let ctx = AtmosphereCtx {
            hour: 23,
            ..Default::default()
        };
        mgr.tick(area, &ctx);
    }
}
