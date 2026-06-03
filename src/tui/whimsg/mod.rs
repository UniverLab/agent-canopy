pub mod kaomojis;
pub mod rng;

pub use kaomojis::{WhimContext, TITLE};

use std::time::{Duration, Instant};

use kaomojis::{
    ACT_ERROR, ACT_LOADING, ACT_SUCCESS, ACT_THINKING, BLANK_MS, ERASE_MS, EVENT_DECAY_SECS,
    HOLD_MAX, HOLD_MIN, INTERVAL_MAX, INTERVAL_MIN, KAOMOJI_MS, KAO_ERROR, KAO_LOADING,
    KAO_SUCCESS, KAO_THINKING, OBJ_ABSURD, OBJ_AI, OBJ_DEV, OBJ_NATURE, OBJ_SCIENCE, OBJ_SPACE,
    PH_BUSY, PH_ERROR, PH_IDLE, PH_SCROLL, PH_SPAWN, PH_SUCCESS, TWIST_ADVICE, TWIST_CHILL,
    TWIST_FUNNY, TWIST_POETIC, TYPE_MS,
};
use rng::{pick_no_repeat, DedupRing, Rng};

pub struct WhimFrame {
    /// How many chars of TITLE are visible (0 = hidden, `TITLE.len()` = full).
    pub title_visible: usize,
    /// The kaomoji to display (empty when title is showing).
    pub kaomoji: String,
    /// The message text (may be partially visible).
    pub text: String,
    /// How many chars of `text` are visible.
    pub text_visible: usize,
}

impl WhimFrame {
    fn full_title() -> Self {
        Self {
            title_visible: TITLE.len(),
            kaomoji: String::new(),
            text: String::new(),
            text_visible: 0,
        }
    }
    fn empty_title() -> Self {
        Self {
            title_visible: 0,
            kaomoji: String::new(),
            text: String::new(),
            text_visible: 0,
        }
    }
}

fn tick_erasing_title(elapsed: u64) -> (WhimFrame, Option<Phase>) {
    let erased = (elapsed / ERASE_MS) as usize;
    if erased >= TITLE.len() {
        return (WhimFrame::empty_title(), Some(Phase::KaomojiFlash));
    }
    (
        WhimFrame {
            title_visible: TITLE.len() - erased,
            kaomoji: String::new(),
            text: String::new(),
            text_visible: 0,
        },
        None,
    )
}

#[derive(Clone, Copy)]
enum Intent {
    Loading,
    Success,
    Error,
    Thinking,
}

enum Phase {
    Idle,
    ErasingTitle,
    KaomojiFlash,
    TypingMsg,
    Holding,
    ErasingMsg,
    Blank,
}

// ── PRNG ──────────────────────────────────────────────────────────

pub struct Whimsg {
    rng: Rng,
    phase: Phase,
    phase_start: Instant,
    next_trigger: Instant,
    active_kaomoji: String,
    active_text: String,
    active_hold_ms: u64,
    event_context: Option<WhimContext>,
    event_at: Instant,
    ambient: WhimContext,
    seen_kaomoji: DedupRing,
    seen_action: DedupRing,
    seen_object: DedupRing,
    seen_twist: DedupRing,
    seen_phrase: DedupRing,
    mission_title: Option<String>,
}

impl Whimsg {
    pub fn new() -> Self {
        let mut rng = Rng::from_instant(Instant::now());
        let first = Duration::from_secs(rng.between(8, 20));
        Self {
            phase: Phase::Idle,
            phase_start: Instant::now(),
            next_trigger: Instant::now() + first,
            active_kaomoji: String::new(),
            active_text: String::new(),
            active_hold_ms: 0,
            event_context: None,
            event_at: Instant::now() - Duration::from_secs(999),
            ambient: WhimContext::Idle,
            seen_kaomoji: DedupRing::new(8),
            seen_action: DedupRing::new(8),
            seen_object: DedupRing::new(8),
            seen_twist: DedupRing::new(8),
            seen_phrase: DedupRing::new(8),
            mission_title: None,
            rng,
        }
    }

    /// Celebrate a newly unlocked mission (one-shot, high priority).
    pub fn notify_mission_unlocked(&mut self, title: &str) {
        self.mission_title = Some(title.to_string());
        self.notify_event(WhimContext::MissionUnlocked);
    }

    /// Set the ambient context (reflects ongoing state: idle, busy, etc.).
    pub fn set_ambient(&mut self, ctx: WhimContext) {
        self.ambient = ctx;
    }

    /// Push a one-shot event (spawn, exit, error). Triggers a sooner message.
    pub fn notify_event(&mut self, event: WhimContext) {
        self.event_context = Some(event);
        self.event_at = Instant::now();
        if matches!(self.phase, Phase::Idle) {
            let soon = self.rng.between(15, 30);
            let proposed = Instant::now() + Duration::from_secs(soon);
            if proposed < self.next_trigger {
                self.next_trigger = proposed;
            }
        }
    }

    /// Produce the current animation frame. Call every render tick.
    pub fn tick(&mut self) -> WhimFrame {
        loop {
            let elapsed = self.phase_start.elapsed().as_millis() as u64;
            let (frame, next_phase) = match self.phase {
                Phase::Idle => self.tick_idle(),
                Phase::ErasingTitle => tick_erasing_title(elapsed),
                Phase::KaomojiFlash => self.tick_kaomoji_flash(elapsed),
                Phase::TypingMsg => self.tick_typing_msg(elapsed),
                Phase::Holding => self.tick_holding(elapsed),
                Phase::ErasingMsg => self.tick_erasing_msg(elapsed),
                Phase::Blank => self.tick_blank(elapsed),
            };
            if let Some(next) = next_phase {
                self.advance(next);
                continue;
            }
            return frame;
        }
    }

    fn tick_idle(&mut self) -> (WhimFrame, Option<Phase>) {
        if Instant::now() >= self.next_trigger {
            self.generate();
            return (WhimFrame::empty_title(), Some(Phase::ErasingTitle));
        }
        (WhimFrame::full_title(), None)
    }

    fn tick_kaomoji_flash(&self, elapsed: u64) -> (WhimFrame, Option<Phase>) {
        if elapsed >= KAOMOJI_MS {
            return (WhimFrame::empty_title(), Some(Phase::TypingMsg));
        }
        (
            WhimFrame {
                title_visible: 0,
                kaomoji: self.active_kaomoji.clone(),
                text: String::new(),
                text_visible: 0,
            },
            None,
        )
    }

    fn tick_typing_msg(&self, elapsed: u64) -> (WhimFrame, Option<Phase>) {
        let total = self.active_text.chars().count();
        let typed = (elapsed / TYPE_MS) as usize;
        if typed >= total {
            return (WhimFrame::empty_title(), Some(Phase::Holding));
        }
        (
            WhimFrame {
                title_visible: 0,
                kaomoji: self.active_kaomoji.clone(),
                text: self.active_text.clone(),
                text_visible: typed,
            },
            None,
        )
    }

    fn tick_holding(&self, elapsed: u64) -> (WhimFrame, Option<Phase>) {
        if elapsed >= self.active_hold_ms {
            return (WhimFrame::empty_title(), Some(Phase::ErasingMsg));
        }
        (
            WhimFrame {
                title_visible: 0,
                kaomoji: self.active_kaomoji.clone(),
                text: self.active_text.clone(),
                text_visible: self.active_text.chars().count(),
            },
            None,
        )
    }

    fn tick_erasing_msg(&self, elapsed: u64) -> (WhimFrame, Option<Phase>) {
        let total = self.active_text.chars().count();
        let erased = (elapsed / ERASE_MS) as usize;
        if erased >= total {
            return (WhimFrame::empty_title(), Some(Phase::Blank));
        }
        (
            WhimFrame {
                title_visible: 0,
                kaomoji: self.active_kaomoji.clone(),
                text: self.active_text.clone(),
                text_visible: total - erased,
            },
            None,
        )
    }

    fn tick_blank(&mut self, elapsed: u64) -> (WhimFrame, Option<Phase>) {
        if elapsed >= BLANK_MS {
            let delay = self.rng.between(INTERVAL_MIN, INTERVAL_MAX);
            self.next_trigger = Instant::now() + Duration::from_secs(delay);
            self.advance(Phase::Idle);
            return (WhimFrame::full_title(), None);
        }
        (WhimFrame::empty_title(), None)
    }

    fn advance(&mut self, next: Phase) {
        self.phase = next;
        self.phase_start = Instant::now();
    }

    fn active_context(&self) -> WhimContext {
        if let Some(ctx) = self.event_context {
            if self.event_at.elapsed() < Duration::from_secs(EVENT_DECAY_SECS) {
                return ctx;
            }
        }
        self.ambient
    }

    fn generate(&mut self) {
        let ctx = self.active_context();
        let intent = self.pick_intent(ctx);

        // Always pick kaomoji (100%)
        let kaomojis = match intent {
            Intent::Loading => KAO_LOADING,
            Intent::Success => KAO_SUCCESS,
            Intent::Error => KAO_ERROR,
            Intent::Thinking => KAO_THINKING,
        };
        let ki = pick_no_repeat(&mut self.rng, kaomojis.len(), &self.seen_kaomoji);
        self.seen_kaomoji.push(ki);
        self.active_kaomoji = kaomojis[ki].to_string();

        // 30% chance of a direct context-driven phrase
        if let Some(title) = self.mission_title.take() {
            self.active_text = format!("( ^ω^) Achieved: {title}!");
        } else if self.rng.chance(0.30) {
            let phrases = match ctx {
                WhimContext::Idle => PH_IDLE,
                WhimContext::AgentSpawned => PH_SPAWN,
                WhimContext::AgentDone => PH_SUCCESS,
                WhimContext::AgentFailed => PH_ERROR,
                WhimContext::TaskRunning => PH_BUSY,
                WhimContext::Scrolling => PH_SCROLL,
                WhimContext::Busy => PH_BUSY,
                WhimContext::MissionUnlocked => kaomojis::PH_MISSION,
            };
            let pi = pick_no_repeat(&mut self.rng, phrases.len(), &self.seen_phrase);
            self.seen_phrase.push(pi);
            self.active_text = phrases[pi].to_string();
        } else {
            // Template: action + object + twist
            let actions = match intent {
                Intent::Loading => ACT_LOADING,
                Intent::Success => ACT_SUCCESS,
                Intent::Error => ACT_ERROR,
                Intent::Thinking => ACT_THINKING,
            };
            let domain = self.rng.range(6);
            let objects = match domain {
                0 => OBJ_DEV,
                1 => OBJ_SPACE,
                2 => OBJ_SCIENCE,
                3 => OBJ_NATURE,
                4 => OBJ_AI,
                _ => OBJ_ABSURD,
            };
            let style = self.rng.range(5);
            let twists: &[&str] = match style {
                0 => TWIST_FUNNY,
                1 => TWIST_POETIC,
                2 => TWIST_ADVICE,
                3 => TWIST_CHILL,
                _ => &["..."],
            };

            let ai = pick_no_repeat(&mut self.rng, actions.len(), &self.seen_action);
            self.seen_action.push(ai);
            let oi = pick_no_repeat(&mut self.rng, objects.len(), &self.seen_object);
            self.seen_object.push(oi);
            let ti = pick_no_repeat(&mut self.rng, twists.len(), &self.seen_twist);
            self.seen_twist.push(ti);
            self.active_text = format!("{} {} {}", actions[ai], objects[oi], twists[ti]);
        }

        self.active_hold_ms = self.rng.between(HOLD_MIN, HOLD_MAX) * 1000;
    }

    fn pick_intent(&mut self, ctx: WhimContext) -> Intent {
        match ctx {
            WhimContext::Idle => match self.rng.range(10) {
                0..=3 => Intent::Thinking,
                4..=6 => Intent::Loading,
                _ => Intent::Success,
            },
            WhimContext::AgentSpawned => {
                if self.rng.chance(0.8) {
                    Intent::Success
                } else {
                    Intent::Loading
                }
            }
            WhimContext::AgentDone => {
                if self.rng.chance(0.9) {
                    Intent::Success
                } else {
                    Intent::Thinking
                }
            }
            WhimContext::AgentFailed => {
                // Balance errors: 40% error, 40% thinking (pondering), 20% hopeful/success
                match self.rng.range(10) {
                    0..=3 => Intent::Error,
                    4..=7 => Intent::Thinking,
                    _ => Intent::Success,
                }
            }
            WhimContext::TaskRunning => {
                if self.rng.chance(0.7) {
                    Intent::Loading
                } else {
                    Intent::Thinking
                }
            }
            WhimContext::Scrolling => {
                if self.rng.chance(0.7) {
                    Intent::Thinking
                } else {
                    Intent::Loading
                }
            }
            WhimContext::Busy => {
                if self.rng.chance(0.7) {
                    Intent::Loading
                } else {
                    Intent::Thinking
                }
            }
            WhimContext::MissionUnlocked => Intent::Success,
        }
    }
}
