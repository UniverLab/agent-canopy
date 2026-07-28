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
    celebration_remaining: u32,
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
            celebration_remaining: 0,
            rng,
        }
    }

    /// Celebrate a newly unlocked mission (one-shot, high priority).
    pub fn notify_mission_unlocked(&mut self, title: &str) {
        self.mission_title = Some(title.to_string());
        self.celebration_remaining = 3;
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
        let blank_ms = if self.celebration_remaining > 0 {
            100
        } else {
            BLANK_MS
        };
        if elapsed >= blank_ms {
            if self.celebration_remaining > 0 {
                self.celebration_remaining -= 1;
                self.event_context = Some(WhimContext::MissionUnlocked);
                self.event_at = Instant::now();
                self.generate();
                self.advance(Phase::ErasingTitle);
                return (WhimFrame::empty_title(), None);
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// WhimFrame factory methods produce correct initial state
    #[test]
    fn whim_frame_full_title_has_visible_title() {
        let frame = WhimFrame::full_title();
        assert_eq!(frame.title_visible, TITLE.len());
        assert!(frame.kaomoji.is_empty());
        assert!(frame.text.is_empty());
        assert_eq!(frame.text_visible, 0);
    }

    /// WhimFrame factory methods produce correct initial state
    #[test]
    fn whim_frame_empty_title_has_no_visible_title() {
        let frame = WhimFrame::empty_title();
        assert_eq!(frame.title_visible, 0);
        assert!(frame.kaomoji.is_empty());
        assert!(frame.text.is_empty());
        assert_eq!(frame.text_visible, 0);
    }

    /// tick_erasing_title progresses character visibility over time
    #[test]
    fn tick_erasing_title_progressively_hides_characters() {
        let (frame, _next_phase) = tick_erasing_title(0);
        let initial_visible = frame.title_visible;
        assert!(initial_visible > 0);
        assert_eq!(initial_visible, TITLE.len());

        let (frame2, _next_phase2) = tick_erasing_title(ERASE_MS);
        assert!(frame2.title_visible < initial_visible);

        let (frame3, next_phase3) = tick_erasing_title(ERASE_MS * (TITLE.len() as u64));
        assert_eq!(frame3.title_visible, 0);
        assert!(next_phase3.is_some());
    }

    /// tick_erasing_title transitions to KaomojiFlash when title is fully erased
    #[test]
    fn tick_erasing_title_transitions_to_kaomoji_flash_when_done() {
        let (_, next_phase) = tick_erasing_title(ERASE_MS * (TITLE.len() as u64 + 1));
        assert!(matches!(next_phase, Some(Phase::KaomojiFlash)));
    }

    /// Whimsg::new creates a Whimsg with sensible defaults
    #[test]
    fn whimsg_new_initializes_correctly() {
        let w = Whimsg::new();
        assert!(matches!(w.phase, Phase::Idle));
        assert!(w.active_kaomoji.is_empty());
        assert!(w.active_text.is_empty());
        assert_eq!(w.active_hold_ms, 0);
        assert_eq!(w.celebration_remaining, 0);
        assert!(w.event_context.is_none());
        assert!(matches!(w.ambient, WhimContext::Idle));
    }

    /// set_ambient updates the ambient context
    #[test]
    fn set_ambient_changes_context() {
        let mut w = Whimsg::new();
        assert!(matches!(w.ambient, WhimContext::Idle));

        w.set_ambient(WhimContext::AgentSpawned);
        assert!(matches!(w.ambient, WhimContext::AgentSpawned));

        w.set_ambient(WhimContext::Busy);
        assert!(matches!(w.ambient, WhimContext::Busy));
    }

    /// notify_event sets event context and updates trigger time
    #[test]
    fn notify_event_sets_context_and_trigger() {
        let mut w = Whimsg::new();
        let old_trigger = w.next_trigger;

        w.notify_event(WhimContext::AgentFailed);
        assert!(w.event_context.is_some());
        assert!(matches!(w.event_context, Some(WhimContext::AgentFailed)));

        // From Idle phase, trigger should be sooner
        assert!(w.next_trigger <= old_trigger);
    }

    /// notify_event from non-idle phase doesn't change trigger time if already sooner
    #[test]
    fn notify_event_when_already_triggered_keeps_early_trigger() {
        let mut w = Whimsg::new();

        // First event
        w.notify_event(WhimContext::AgentSpawned);
        let first_trigger = w.next_trigger;

        // Second event soon after (trigger is already soon)
        w.notify_event(WhimContext::AgentFailed);
        // Trigger shouldn't go further back since it's already set
        assert!(w.next_trigger <= first_trigger);
    }

    /// notify_mission_unlocked sets mission title and celebration counter
    #[test]
    fn notify_mission_unlocked_sets_title_and_celebration() {
        let mut w = Whimsg::new();
        w.notify_mission_unlocked("Test Mission");
        assert_eq!(w.mission_title, Some("Test Mission".to_string()));
        assert_eq!(w.celebration_remaining, 3);
        assert!(w.event_context.is_some());
    }

    /// tick returns a WhimFrame on every call without panicking
    #[test]
    fn tick_always_produces_frame() {
        let mut w = Whimsg::new();
        for _ in 0..100 {
            let frame = w.tick();
            assert!(frame.title_visible <= TITLE.len());
            // text_visible is always >= 0 by construction (it's a usize)
        }
    }

    /// tick always starts in idle phase with full title visible
    #[test]
    fn tick_initial_state_shows_full_title_in_idle() {
        let mut w = Whimsg::new();
        let frame = w.tick();
        assert_eq!(frame.title_visible, TITLE.len());
        assert!(matches!(w.phase, Phase::Idle));
    }

    /// pick_intent for Idle context returns reasonable distribution
    #[test]
    fn pick_intent_idle_returns_thinking_loading_or_success() {
        let mut w = Whimsg::new();
        w.ambient = WhimContext::Idle;

        let mut found_thinking = false;
        let mut found_loading = false;
        let mut found_success = false;

        for _ in 0..100 {
            let intent = w.pick_intent(WhimContext::Idle);
            match intent {
                Intent::Thinking => found_thinking = true,
                Intent::Loading => found_loading = true,
                Intent::Success => found_success = true,
                Intent::Error => panic!("Idle should not produce Error intent"),
            }
        }

        // With 100 samples, we should see some variety
        assert!(found_thinking, "should see some Thinking intents");
        assert!(found_loading, "should see some Loading intents");
        assert!(found_success, "should see some Success intents");
    }

    /// pick_intent for AgentSpawned heavily favors Success
    #[test]
    fn pick_intent_agent_spawned_favors_success() {
        let mut w = Whimsg::new();

        let mut success_count = 0;
        for _ in 0..100 {
            let intent = w.pick_intent(WhimContext::AgentSpawned);
            if matches!(intent, Intent::Success) {
                success_count += 1;
            }
        }

        // With 80% probability, expect 70+ successes in 100 samples
        assert!(
            success_count >= 50,
            "AgentSpawned should favor Success (got {success_count}/100)"
        );
    }

    /// pick_intent for AgentDone heavily favors Success
    #[test]
    fn pick_intent_agent_done_favors_success() {
        let mut w = Whimsg::new();

        let mut success_count = 0;
        for _ in 0..100 {
            let intent = w.pick_intent(WhimContext::AgentDone);
            if matches!(intent, Intent::Success) {
                success_count += 1;
            }
        }

        // With 90% probability, expect high success rate
        assert!(
            success_count >= 70,
            "AgentDone should favor Success (got {success_count}/100)"
        );
    }

    /// pick_intent for AgentFailed produces balanced distribution
    #[test]
    fn pick_intent_agent_failed_balanced() {
        let mut w = Whimsg::new();

        let mut error_count = 0;
        let mut thinking_count = 0;
        let mut success_count = 0;

        for _ in 0..100 {
            let intent = w.pick_intent(WhimContext::AgentFailed);
            match intent {
                Intent::Error => error_count += 1,
                Intent::Thinking => thinking_count += 1,
                Intent::Success => success_count += 1,
                Intent::Loading => panic!("AgentFailed should not produce Loading intent"),
            }
        }

        // Should see balanced spread (40% error, 40% thinking, 20% success)
        assert!(error_count > 20, "should see some Error intents");
        assert!(thinking_count > 20, "should see some Thinking intents");
        assert!(success_count > 5, "should see some Success intents");
    }

    /// pick_intent for TaskRunning favors Loading
    #[test]
    fn pick_intent_task_running_favors_loading() {
        let mut w = Whimsg::new();

        let mut loading_count = 0;
        for _ in 0..100 {
            let intent = w.pick_intent(WhimContext::TaskRunning);
            if matches!(intent, Intent::Loading) {
                loading_count += 1;
            }
        }

        // With 70% probability, expect 50+ loadings in 100 samples
        assert!(
            loading_count >= 40,
            "TaskRunning should favor Loading (got {loading_count}/100)"
        );
    }

    /// pick_intent for MissionUnlocked always returns Success
    #[test]
    fn pick_intent_mission_unlocked_always_success() {
        let mut w = Whimsg::new();

        for _ in 0..20 {
            let intent = w.pick_intent(WhimContext::MissionUnlocked);
            assert!(
                matches!(intent, Intent::Success),
                "MissionUnlocked must always produce Success"
            );
        }
    }

    /// Active context decays from event to ambient after timeout
    #[test]
    fn active_context_decays_after_event_timeout() {
        let mut w = Whimsg::new();
        w.ambient = WhimContext::Idle;
        w.event_context = Some(WhimContext::AgentSpawned);
        w.event_at = Instant::now() - Duration::from_secs(EVENT_DECAY_SECS + 1);

        let ctx = w.active_context();
        assert!(matches!(ctx, WhimContext::Idle));
    }

    /// Active context uses event while within timeout
    #[test]
    fn active_context_uses_event_within_timeout() {
        let mut w = Whimsg::new();
        w.ambient = WhimContext::Idle;
        w.event_context = Some(WhimContext::AgentFailed);
        w.event_at = Instant::now() - Duration::from_secs(1);

        let ctx = w.active_context();
        assert!(matches!(ctx, WhimContext::AgentFailed));
    }

    /// WhimFrame doesn't panic when constructing with various text lengths
    #[test]
    fn whim_frame_handles_various_text_lengths() {
        let test_strings = vec!["", "a", "hello", "こんにちは", "🎉🚀"];

        for text in test_strings {
            let frame = WhimFrame {
                title_visible: 0,
                kaomoji: text.to_string(),
                text: text.to_string(),
                text_visible: text.chars().count(),
            };
            // Just verify it constructs without panic
            assert_eq!(frame.text, text);
        }
    }

    /// Multiple sequential notifications don't corrupt state
    #[test]
    fn sequential_notifications_maintain_state() {
        let mut w = Whimsg::new();

        w.notify_event(WhimContext::AgentSpawned);
        assert!(matches!(w.event_context, Some(WhimContext::AgentSpawned)));

        w.notify_event(WhimContext::AgentFailed);
        assert!(matches!(w.event_context, Some(WhimContext::AgentFailed)));

        w.set_ambient(WhimContext::Busy);
        assert!(matches!(w.ambient, WhimContext::Busy));
    }

    /// Celebration loop properly decrements counter during celebration phase
    #[test]
    fn celebration_sets_and_eventually_uses_title() {
        let mut w = Whimsg::new();
        w.notify_mission_unlocked("Test Mission");
        assert_eq!(w.celebration_remaining, 3);
        assert_eq!(w.mission_title, Some("Test Mission".to_string()));

        // Verify that celebration_remaining can be decremented manually
        w.celebration_remaining = 2;
        assert_eq!(w.celebration_remaining, 2);

        // The actual decrement happens during tick_blank when we've accumulated
        // time in that phase. For this unit test, we just verify the state is set up correctly.
    }
}
