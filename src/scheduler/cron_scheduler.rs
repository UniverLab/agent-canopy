//! Internal cron scheduler — runs inside the daemon process.
//!
//! Instead of polling on a fixed interval, the scheduler computes the
//! nearest `next_fire_time` across all active cron agents and sleeps exactly
//! until that instant.  A `Notify` handle lets the daemon wake the
//! scheduler early when agents are added, updated, or re-enabled.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{Local, Utc};
use cron::Schedule;
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

use crate::application::ports::{AgentRepository, RunRepository};
use crate::db::Database;
use crate::domain::loops::{LoopResetOutcome, LoopStatus};
use crate::executor::Executor;
use crate::loop_engine::LoopEngine;

const RECONCILE_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// How a failed scheduled run is retried, independently of the cron slot.
///
/// A cron miss/failure (e.g. a CLI hitting its quota) used to wait for the
/// next scheduled slot — hours away. With retry enabled, a failing run is
/// re-attempted after `delay_minutes`, up to `max_retries` times, without
/// disturbing the regular cron schedule.
///
/// Configurable via environment (read once at scheduler construction):
/// - `CANOPY_RETRY_ENABLED`      (bool, default true)
/// - `CANOPY_RETRY_DELAY_MINUTES` (u64,  default 60)
/// - `CANOPY_RETRY_MAX`          (u32,  default 3)
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub enabled: bool,
    pub delay_minutes: u64,
    pub max_retries: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            delay_minutes: 60,
            max_retries: 3,
        }
    }
}

impl RetryPolicy {
    /// Build from environment variables, falling back to defaults.
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            enabled: std::env::var("CANOPY_RETRY_ENABLED")
                .ok()
                .and_then(|v| match v.trim().to_ascii_lowercase().as_str() {
                    "1" | "true" | "yes" | "on" => Some(true),
                    "0" | "false" | "no" | "off" => Some(false),
                    _ => None,
                })
                .unwrap_or(d.enabled),
            delay_minutes: std::env::var("CANOPY_RETRY_DELAY_MINUTES")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(d.delay_minutes),
            max_retries: std::env::var("CANOPY_RETRY_MAX")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(d.max_retries),
        }
    }
}

/// The internal cron scheduler that runs as a tokio background_agent.
pub struct CronScheduler {
    db: Arc<Database>,
    executor: Arc<Executor>,
    cancel: CancellationToken,
    /// Wakes the scheduler to recalculate the next fire time.
    notify: Arc<Notify>,
    /// Optional loop engine — when set, the scheduler also evaluates loops
    /// whose trigger is `Cron` and launches them alongside agents.
    loop_engine: Option<Arc<LoopEngine>>,
    /// Tick idempotency: the most recent *scheduled* tick already fired per
    /// schedulable (agents keyed by their id; loops by [`loop_key`] to avoid
    /// colliding with an agent that happens to share the same id).
    ///
    /// The stored value is the matched fire time itself (what
    /// [`due_fire_local`] returned), not the wall-clock instant the
    /// evaluation ran at. Two evaluations of the *same* tick — whether from
    /// the regular tick loop firing twice in a row or a second evaluation
    /// path racing it — compute the identical scheduled-tick value, so a
    /// plain equality/`>=` check dedupes them exactly. A wall-clock window
    /// (e.g. "fired within the last 60s") would instead depend on how long
    /// evaluation happened to take, which is what let two evaluations of one
    /// tick slip past a time-window check and both spawn.
    last_fired: Arc<Mutex<std::collections::HashMap<String, chrono::DateTime<Utc>>>>,
    /// Per-agent in-flight guard: agents currently spawned and not yet
    /// finalized. A fire for an agent already in this set is SKIPPED and
    /// logged at info — never queued behind, never spawned in parallel.
    /// This is the single choke point every firing path (scheduled, watch,
    /// manual) routes through at the scheduler level.
    in_flight: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Failure-retry policy applied to scheduled runs.
    retry: RetryPolicy,
}

/// Namespace a loop id in the shared `last_fired` map.
fn loop_key(loop_id: &str) -> String {
    format!("loop:{loop_id}")
}

impl CronScheduler {
    pub fn new(db: Arc<Database>, executor: Arc<Executor>) -> Self {
        Self {
            db,
            executor,
            cancel: CancellationToken::new(),
            notify: Arc::new(Notify::new()),
            loop_engine: None,
            last_fired: Arc::new(Mutex::new(std::collections::HashMap::new())),
            in_flight: Arc::new(Mutex::new(std::collections::HashSet::new())),
            retry: RetryPolicy::from_env(),
        }
    }

    /// Build a scheduler that also fires cron-triggered loops via `loop_engine`.
    pub fn with_loops(
        db: Arc<Database>,
        executor: Arc<Executor>,
        loop_engine: Arc<LoopEngine>,
    ) -> Self {
        Self {
            loop_engine: Some(loop_engine),
            ..Self::new(db, executor)
        }
    }

    /// Get a handle to wake the scheduler when agents change.
    pub fn notifier(&self) -> Arc<Notify> {
        Arc::clone(&self.notify)
    }

    /// Seed the tick-idempotency map from the database so a restart doesn't
    /// immediately re-fire a tick whose run is still recorded from before
    /// the restart. `last_run_at` is a completion timestamp, not a scheduled
    /// tick, so this is a conservative approximation: it only suppresses a
    /// re-fire when the next candidate tick is not newer than the last
    /// recorded run. The [`try_start_run`](crate::application::ports::RunRepository::try_start_run)
    /// in-flight guard is what actually prevents a concurrent duplicate
    /// execution; this seed is just about not re-spawning a tick that was
    /// already handled moments before the restart.
    async fn initialize_last_fired(&self) {
        let mut last_fired = self.last_fired.lock().await;
        if let Ok(agents) = self.db.list_cron_agents() {
            for agent in agents {
                if let Some(last_run_at) = agent.last_run_at {
                    last_fired.insert(agent.id, last_run_at);
                }
            }
        }
    }

    /// Start the scheduler loop as a background tokio task.
    ///
    /// Returns a `CancellationToken` that can be used to stop the scheduler.
    pub fn start(self: Arc<Self>) -> CancellationToken {
        let cancel = self.cancel.clone();
        let scheduler = Arc::clone(&self);

        tokio::spawn(async move {
            tracing::info!("Internal cron scheduler started");
            // Initialize from database to prevent duplicate executions after restart
            scheduler.initialize_last_fired().await;
            scheduler.run_loop().await;
            tracing::info!("Internal cron scheduler stopped");
        });

        cancel
    }

    /// The main scheduler loop. Sleeps until the next agent is due,
    /// or wakes early on cancel/notify.
    async fn run_loop(&self) {
        loop {
            let sleep_dur = self.next_sleep_duration();

            tokio::select! {
                _ = self.cancel.cancelled() => break,
                _ = self.notify.notified() => {
                    continue;
                }
                _ = tokio::time::sleep(RECONCILE_INTERVAL) => {
                    continue;
                }
                _ = tokio::time::sleep(sleep_dur) => {
                    if let Err(e) = self.fire_due_tasks().await {
                        tracing::error!("Scheduler fire failed: {}", e);
                    }
                }
            }
        }
    }

    /// Compute how long to sleep until the nearest agent fires.
    /// Falls back to 60 s if there are no active agents or on parse errors.
    fn next_sleep_duration(&self) -> std::time::Duration {
        const FALLBACK: std::time::Duration = std::time::Duration::from_secs(60);

        let Ok(agents) = self.db.list_cron_agents() else {
            return FALLBACK;
        };

        // Cron expressions are authored in the user's local timezone (a user
        // who types `0 9 * * *` expects 9 AM on their wall clock, not 9 AM
        // UTC). We feed the schedule iterator a `Local` "now" so it walks
        // fire times in the same frame the user wrote the expression in,
        // then convert the result to UTC for sleep-delta math (which uses
        // a wall-clock-independent `Duration`).
        let now_local = Local::now();
        let now_utc = Utc::now();
        let mut earliest: Option<chrono::DateTime<Utc>> = None;

        for agent in &agents {
            if !agent.enabled || agent.is_expired() {
                continue;
            }
            fold_earliest(&mut earliest, agent.schedule_expr(), now_local);
        }

        // Cron-triggered loops share the same sleep math as agents.
        if self.loop_engine.is_some() {
            if let Ok(loops) = self.db.list_cron_loops() {
                for lp in &loops {
                    if !lp.is_fireable() {
                        continue;
                    }
                    fold_earliest(&mut earliest, lp.schedule_expr(), now_local);
                }
            }
        }

        // One-shot `enable_at` schedules also need a wakeup, independent of
        // any cron expression on the agent.
        if let Ok(pending) = self.db.list_pending_enable_agents() {
            for agent in &pending {
                if let Some(enable_at) = agent.enable_at {
                    let nearer = match earliest {
                        Some(e) => enable_at < e,
                        None => true,
                    };
                    if nearer {
                        earliest = Some(enable_at);
                    }
                }
            }
        }

        // One-shot `autorun_at` loop schedules need a wakeup too, independent
        // of any cron trigger on the loop.
        if self.loop_engine.is_some() {
            if let Ok(pending) = self.db.list_pending_autorun_loops() {
                for lp in &pending {
                    if let Some(autorun_at) = lp.autorun_at {
                        let nearer = match earliest {
                            Some(e) => autorun_at < e,
                            None => true,
                        };
                        if nearer {
                            earliest = Some(autorun_at);
                        }
                    }
                }
            }
        }

        // One-shot `auto_continue_at` loop schedules (deferred resume of a
        // paused loop) need a wakeup too, same as `autorun_at` above.
        if self.loop_engine.is_some() {
            if let Ok(pending) = self.db.list_pending_auto_continue_loops() {
                for lp in &pending {
                    if let Some(auto_continue_at) = lp.auto_continue_at {
                        let nearer = match earliest {
                            Some(e) => auto_continue_at < e,
                            None => true,
                        };
                        if nearer {
                            earliest = Some(auto_continue_at);
                        }
                    }
                }
            }
        }

        match earliest {
            Some(t) => {
                let delta = t.signed_duration_since(now_utc);
                if delta.num_milliseconds() <= 0 {
                    std::time::Duration::ZERO
                } else {
                    std::time::Duration::from_millis(delta.num_milliseconds() as u64)
                }
            }
            None => FALLBACK,
        }
    }

    /// Quarantine any agent whose row failed to decode (e.g. a `trigger_config`
    /// written directly to SQLite by an external tool, not the JSON shape
    /// canopy expects): disable it and warn once, then leave it alone.
    ///
    /// Once disabled, the row is excluded from `list_cron_agents` (which
    /// filters `enabled = 1`), so subsequent ticks never see it as still
    /// enabled and never re-warn — this is what keeps a single corrupt row
    /// from producing a repeating per-tick error. The row itself is never
    /// touched or reinterpreted; only `enabled` changes.
    fn quarantine_corrupt_agents(&self) -> anyhow::Result<()> {
        for corrupt in self.db.list_corrupt_agents()? {
            if !corrupt.enabled {
                continue;
            }
            tracing::warn!(
                "Agent '{}' has a corrupt trigger_config and cannot be scheduled ({}); \
                 quarantining (disabling) it",
                corrupt.id,
                corrupt.error
            );
            self.db.update_agent_enabled(&corrupt.id, false)?;
        }
        Ok(())
    }

    /// Fire all agents whose next cron time is now (within a 1-second tolerance).
    async fn fire_due_tasks(&self) -> anyhow::Result<()> {
        self.quarantine_corrupt_agents()?;
        let agents = self.db.list_cron_agents()?;
        // Evaluate schedules in the user's local timezone. `now_utc` is only
        // used for the persisted `last_fired` comparison (which is stored in
        // UTC), and `now_local` for the cron-field match.
        let now_local = Local::now();
        let now_utc = Utc::now();

        for agent in &agents {
            self.try_fire_agent(agent, now_local).await?;
        }

        // Cron-triggered loops are evaluated in the same local frame.
        if self.loop_engine.is_some() {
            let loops = self.db.list_cron_loops()?;
            for lp in &loops {
                self.try_fire_loop(lp, now_local).await?;
            }
        }

        self.fire_due_enable_at(now_utc)?;
        self.fire_due_autorun_loops(now_utc)?;
        self.fire_due_auto_continue_loops(now_utc)?;

        Ok(())
    }

    /// One-shot `enable_at`: for each disabled agent with a pending
    /// `enable_at` in the past, enable it and clear the schedule. Unlike
    /// cron agents (`list_cron_agents` filters `enabled = 1`), these are
    /// disabled by definition, hence the dedicated query.
    fn fire_due_enable_at(&self, now_utc: chrono::DateTime<Utc>) -> anyhow::Result<()> {
        let pending = self.db.list_pending_enable_agents()?;
        for agent in &pending {
            if agent.enable_at.is_some_and(|at| now_utc >= at) {
                tracing::info!("Agent '{}' reached its enable_at time; enabling", agent.id);
                self.db.activate_scheduled_enable(&agent.id)?;
            }
        }
        Ok(())
    }

    /// One-shot `autorun_at`: for each loop with a pending `autorun_at` in
    /// the past that is still fireable (not `Running`/`Paused`) — or `Paused`
    /// because `reconcile_orphaned_loops` put it there, see
    /// [`crate::domain::loops::Loop::is_autorun_due`] — clear the schedule
    /// and launch it once via the loop engine. Unlike cron loops, this never
    /// repeats.
    ///
    /// A `failed` loop is not launched as-is — `loop_run` refuses `failed`
    /// loops, so firing here performs an explicit auto-reset-and-resume
    /// first: the same transition [`Database::reset_loop`] that backs the
    /// `loop_reset` MCP tool, logged at INFO. That is the sanctioned,
    /// intentional way a quota-failed loop revives itself unattended (see
    /// [`crate::daemon::handler`] `loop_reset`/`loop_schedule_autorun` tool
    /// docs). A `completed` loop is deliberately left alone — re-running a
    /// finished loop is a human decision via `loop_reset` + `loop_run` — so
    /// firing on one only logs a WARN and clears the schedule.
    fn fire_due_autorun_loops(&self, now_utc: chrono::DateTime<Utc>) -> anyhow::Result<()> {
        let Some(loop_engine) = self.loop_engine.as_ref() else {
            return Ok(());
        };
        let pending = self.db.list_pending_autorun_loops()?;
        for lp in &pending {
            if !lp.is_autorun_due(now_utc) {
                // C1: a due schedule on a loop the *operator* paused
                // (`paused_by_reconciliation` is false) can never fire on its
                // own — `is_fireable()` deliberately keeps excluding `Paused`
                // for every caller but reconciliation's. That's correct, but
                // it must not be a second silent expiry: surface it every
                // tick it's still blocked, so an operator watching logs sees
                // why the resume never happened instead of concluding canopy
                // forgot.
                if lp.status == LoopStatus::Paused
                    && !lp.paused_by_reconciliation
                    && lp.autorun_at.is_some_and(|at| now_utc >= at)
                {
                    tracing::warn!(
                        "Loop '{}' autorun_at is due but the loop is paused (not by \
                         reconciliation); it will not fire until resumed manually via \
                         loop_continue or loop_run — the schedule remains pending.",
                        lp.id
                    );
                }
                continue;
            }
            self.db.clear_loop_autorun(&lp.id)?;

            if lp.status == LoopStatus::Completed {
                tracing::warn!(
                    "Loop '{}' autorun fired but the loop is already completed; clearing the \
                     schedule without re-running it (re-running a finished loop is a human \
                     decision via loop_reset + loop_run)",
                    lp.id
                );
                continue;
            }

            if lp.status == LoopStatus::Failed {
                match self.db.reset_loop(&lp.id, None)? {
                    LoopResetOutcome::Reset { spec_count } => {
                        tracing::info!(
                            "Loop '{}' was failed; auto-reset by its schedule ({} spec(s) reset) \
                             and resuming",
                            lp.id,
                            spec_count
                        );
                    }
                    other => {
                        tracing::warn!(
                            "Loop '{}' autorun could not auto-reset it ({:?}); skipping launch",
                            lp.id,
                            other
                        );
                        continue;
                    }
                }
            }

            tracing::info!("Loop '{}' reached its autorun_at time; launching", lp.id);
            // Resume with the loop's persisted run context (its queue, if any)
            // rather than a fresh `start_background`, which would fall back
            // to the loop's own bound specs — empty for a queue run, and
            // exactly how a resumed queue run used to be mistaken for
            // "nothing to do" and marked completed with members still
            // pending.
            Arc::clone(loop_engine).resume_background(lp.id.clone());
        }
        Ok(())
    }

    /// One-shot `auto_continue_at`: for each loop with a pending
    /// auto-continue schedule whose time has been reached, clear the
    /// schedule and — only if the loop is still `Paused` — fire the
    /// requested `loop_continue` action (`retry_current_node` by default) and
    /// resume it in place.
    ///
    /// Unlike [`Self::fire_due_autorun_loops`], a loop that is no longer
    /// `Paused` by the scheduled time (already continued manually, failed,
    /// completed, or running) is *not* waited on further — the schedule is
    /// cleared right away without firing, so a stale deferred-resume can
    /// never double-run a loop that moved on some other way. This never
    /// resets or relaunches the loop (that's `autorun_at`'s job): it goes
    /// straight through the same action-then-resume path the `loop_continue`
    /// MCP tool uses, preserving the paused cursor/context.
    fn fire_due_auto_continue_loops(&self, now_utc: chrono::DateTime<Utc>) -> anyhow::Result<()> {
        let Some(loop_engine) = self.loop_engine.as_ref() else {
            return Ok(());
        };
        let pending = self.db.list_pending_auto_continue_loops()?;
        for lp in &pending {
            if !lp.is_auto_continue_due(now_utc) {
                // Not firing this tick. If the time has passed but the loop
                // left `Paused` some other way (manual `loop_continue`,
                // failure) before the schedule fired, it's stale — clear it
                // now rather than leaving it to linger forever waiting for
                // `Paused` to recur (unlike `autorun_at`, which does wait,
                // since its target statuses don't otherwise repeat).
                if lp.is_auto_continue_time_reached(now_utc) {
                    self.db.clear_loop_auto_continue(&lp.id)?;
                    tracing::warn!(
                        "Loop '{}' auto-continue fired but the loop is no longer paused ({}); \
                         clearing the schedule without resuming it",
                        lp.id,
                        lp.status.as_str()
                    );
                }
                continue;
            }
            self.db.clear_loop_auto_continue(&lp.id)?;

            let action = lp
                .auto_continue_action
                .as_deref()
                .unwrap_or("retry_current_node");
            let applied = match action {
                "skip_next_spec" => crate::daemon::handler::handle_skip_next_spec(&self.db, &lp.id),
                _ => crate::daemon::handler::handle_retry_current_node(&self.db, &lp.id),
            };
            if let Err(error) = applied {
                tracing::warn!(
                    "Loop '{}' auto-continue could not apply action '{}' ({}); skipping resume",
                    lp.id,
                    action,
                    error.message
                );
                continue;
            }

            tracing::info!(
                "Loop '{}' reached its auto_continue_at time; resuming with action '{}'",
                lp.id,
                action
            );
            Arc::clone(loop_engine).resume_background(lp.id.clone());
        }
        Ok(())
    }

    /// Evaluate a single cron loop and launch it via the loop engine if due.
    async fn try_fire_loop(
        &self,
        lp: &crate::domain::loops::Loop,
        now_local: chrono::DateTime<Local>,
    ) -> anyhow::Result<()> {
        let Some(loop_engine) = self.loop_engine.as_ref() else {
            return Ok(());
        };
        // A loop already running/paused must not be relaunched by its trigger.
        if !lp.is_fireable() {
            return Ok(());
        }

        let Some(schedule_expr) = lp.schedule_expr() else {
            return Ok(());
        };
        let schedule = match Schedule::from_str(&to_7field_cron(schedule_expr)) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "Loop '{}' has invalid cron expression '{}': {}",
                    lp.id,
                    schedule_expr,
                    e
                );
                return Ok(());
            }
        };

        let Some(due_local) = due_fire_local(&schedule, now_local) else {
            return Ok(());
        };
        let scheduled_tick = due_local.with_timezone(&Utc);

        // Tick idempotency in a loop-namespaced key: dedupe by the scheduled
        // tick itself, not by when this evaluation happened to run.
        let key = loop_key(&lp.id);
        {
            let mut last_fired = self.last_fired.lock().await;
            if last_fired
                .get(&key)
                .is_some_and(|last| *last >= scheduled_tick)
            {
                tracing::info!(
                    "Loop '{}' tick {} already fired; skipping duplicate evaluation",
                    lp.id,
                    scheduled_tick
                );
                return Ok(());
            }
            last_fired.insert(key, scheduled_tick);
        }

        tracing::info!("Cron loop '{}' is due; launching", lp.id);
        // Loops are launched fire-and-forget: the loop engine drives the graph
        // and owns its own failure handling, so the agent RetryPolicy does not
        // apply here.
        Arc::clone(loop_engine).start_background(lp.id.clone());
        Ok(())
    }

    /// Evaluate a single agent and spawn it if it is due. Returns early on
    /// disabled/expired/parse-error conditions.
    async fn try_fire_agent(
        &self,
        agent: &crate::domain::models::Agent,
        now_local: chrono::DateTime<Local>,
    ) -> anyhow::Result<()> {
        if !agent.enabled {
            return Ok(());
        }

        if agent.is_expired() {
            tracing::info!("Agent '{}' has expired, disabling", agent.id);
            self.db.update_agent_enabled(&agent.id, false)?;
            return Ok(());
        }

        let Some(schedule_expr) = agent.schedule_expr() else {
            return Ok(());
        };

        let cron_7field = to_7field_cron(schedule_expr);
        let schedule = match Schedule::from_str(&cron_7field) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "Agent '{}' has invalid cron expression '{}': {}",
                    agent.id,
                    schedule_expr,
                    e
                );
                return Ok(());
            }
        };

        // 1-minute lookback so a scheduler hiccup doesn't skip a fire that
        // was scheduled to happen just before "now" (in local time).
        let Some(due_local) = due_fire_local(&schedule, now_local) else {
            return Ok(());
        };
        let scheduled_tick = due_local.with_timezone(&Utc);

        // Tick idempotency: dedupe by the scheduled tick itself (in UTC, so
        // it lines up with the rest of the system — DB schema, `last_run_at`,
        // daemon JSON), not by when this evaluation happened to run. Two
        // evaluations of the same tick — the regular tick loop firing twice,
        // or a second evaluation path racing it — compute the identical
        // `scheduled_tick`, so this is an exact key, not a time-window guess.
        {
            let mut last_fired = self.last_fired.lock().await;
            if last_fired
                .get(&agent.id)
                .is_some_and(|last| *last >= scheduled_tick)
            {
                tracing::info!(
                    "Agent '{}' tick {} already fired; skipping duplicate evaluation",
                    agent.id,
                    scheduled_tick
                );
                return Ok(());
            }
            last_fired.insert(agent.id.clone(), scheduled_tick);
        }

        // Per-agent in-flight guard: if this agent is already running
        // (spawned but not yet finalized), skip this fire. This is the
        // single choke point at the scheduler level — all firing paths
        // (scheduled tick, notify wake-up) go through it.
        {
            let mut in_flight = self.in_flight.lock().await;
            if !in_flight.insert(agent.id.clone()) {
                tracing::info!(
                    "Agent '{}' is already running; skipping this fire",
                    agent.id
                );
                // Record a Missed run so the skip is visible in execution
                // history, distinguishing "skipped: already running" from
                // "never fired".
                let now = Utc::now();
                let missed = crate::domain::models::RunLog {
                    id: uuid::Uuid::new_v4().to_string(),
                    background_agent_id: agent.id.clone(),
                    status: crate::domain::models::RunStatus::Missed,
                    trigger_type: crate::domain::models::TriggerType::Scheduled,
                    summary: Some(
                        "Skipped: already running (scheduler in-flight guard)".to_string(),
                    ),
                    started_at: now,
                    finished_at: Some(now),
                    exit_code: None,
                    timeout_at: None,
                };
                let _ = self.db.insert_run(&missed);
                return Ok(());
            }
        }

        let executor = Arc::clone(&self.executor);
        let agent = agent.clone();
        let retry = self.retry;
        let cancel = self.cancel.clone();
        let in_flight = Arc::clone(&self.in_flight);
        let agent_id = agent.id.clone();
        tokio::spawn(async move {
            run_with_retry(executor, agent, retry, cancel).await;
            // Remove from in-flight set when the run completes (including
            // retries). This must happen unconditionally so a failed run
            // doesn't permanently block future fires.
            in_flight.lock().await.remove(&agent_id);
        });

        Ok(())
    }

    /// Stop the scheduler.
    pub fn stop(&self) {
        self.cancel.cancel();
    }
}

/// Whether a run outcome counts as a failure eligible for retry.
///
/// An `Err` (spawn/IO failure) and any non-zero exit code are failures.
/// A clean exit (code 0) is a success. Callers treat lock-skips — which
/// surface as an `Ok` with the process's own code — like any other run.
fn run_outcome_is_failure(outcome: &anyhow::Result<i32>) -> bool {
    !matches!(outcome, Ok(0))
}

/// Given a failed run, decide whether another attempt is warranted.
/// `attempt` is the 0-based index of the attempt that just failed.
fn should_retry(retry: &RetryPolicy, attempt: u32) -> bool {
    retry.enabled && attempt < retry.max_retries
}

/// Run a scheduled agent, retrying on failure per [`RetryPolicy`].
///
/// The first attempt runs immediately (the cron slot fired). On failure,
/// waits `delay_minutes` and re-runs, up to `max_retries` extra attempts.
/// The wait is cancellation-aware, so a stopping daemon does not leave a
/// pending retry sleeping.
async fn run_with_retry(
    executor: Arc<Executor>,
    agent: crate::domain::models::Agent,
    retry: RetryPolicy,
    cancel: CancellationToken,
) {
    let mut attempt: u32 = 0;
    loop {
        let outcome = executor.execute_agent(&agent, false).await;
        if !run_outcome_is_failure(&outcome) {
            if let Ok(code) = outcome {
                tracing::info!(
                    "Scheduled agent '{}' completed (exit code: {})",
                    agent.id,
                    code
                );
            }
            return;
        }

        match &outcome {
            Ok(code) => tracing::warn!(
                "Scheduled agent '{}' failed (exit code: {}), attempt {}",
                agent.id,
                code,
                attempt + 1
            ),
            Err(e) => tracing::error!(
                "Scheduled agent '{}' failed: {}, attempt {}",
                agent.id,
                e,
                attempt + 1
            ),
        }

        if !should_retry(&retry, attempt) {
            if retry.enabled {
                tracing::warn!(
                    "Scheduled agent '{}' exhausted {} retries; waiting for next cron slot",
                    agent.id,
                    retry.max_retries
                );
            }
            return;
        }

        attempt += 1;
        tracing::info!(
            "Scheduled agent '{}' will retry ({}/{}) in {} min",
            agent.id,
            attempt,
            retry.max_retries,
            retry.delay_minutes
        );
        let wait = Duration::from_secs(retry.delay_minutes.saturating_mul(60));
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = cancel.cancelled() => {
                tracing::info!("Retry for agent '{}' cancelled (daemon stopping)", agent.id);
                return;
            }
        }
    }
}

/// Compute the next fire instant (in UTC) for a cron schedule, evaluated
/// against the user's local wall clock.
///
/// Cron expressions are authored in the user's local timezone (someone who
/// types `0 9 * * *` expects 9 AM on their wall clock, not 9 AM UTC), so we
/// feed the schedule iterator a `Local` reference and only convert the
/// resulting instant to UTC for wall-clock-independent delta math. Returns
/// `None` if the schedule has no future occurrence.
fn next_fire_utc(
    schedule: &Schedule,
    now_local: chrono::DateTime<Local>,
) -> Option<chrono::DateTime<Utc>> {
    schedule
        .after(&now_local)
        .next()
        .map(|next_local| next_local.with_timezone(&Utc))
}

/// Parse `schedule_expr` and, if it yields a nearer next fire than `earliest`,
/// update `earliest`. A `None` expression or an unparseable one is skipped.
/// Shared by agents and cron loops so both walk fire times identically.
fn fold_earliest(
    earliest: &mut Option<chrono::DateTime<Utc>>,
    schedule_expr: Option<&str>,
    now_local: chrono::DateTime<Local>,
) {
    let Some(schedule_expr) = schedule_expr else {
        return;
    };
    let Ok(schedule) = Schedule::from_str(&to_7field_cron(schedule_expr)) else {
        return;
    };
    if let Some(next_utc) = next_fire_utc(&schedule, now_local) {
        let nearer = match earliest {
            Some(e) => next_utc < *e,
            None => true,
        };
        if nearer {
            *earliest = Some(next_utc);
        }
    }
}

/// Decide whether a cron schedule is due at `now_local`, using a 60-second
/// lookback so a scheduler hiccup doesn't skip a fire scheduled just before
/// "now". Returns the matched fire time (in local wall-clock time) when due,
/// or `None` otherwise.
///
/// Like [`next_fire_utc`], the schedule is evaluated in the local frame so
/// the cron fields mean local wall-clock times.
fn due_fire_local(
    schedule: &Schedule,
    now_local: chrono::DateTime<Local>,
) -> Option<chrono::DateTime<Local>> {
    let window_start = now_local - chrono::Duration::seconds(60);
    let candidate = schedule.after(&window_start).next()?;
    (candidate <= now_local).then_some(candidate)
}

/// Convert a standard 5-field cron expression to the 7-field format
/// expected by the `cron` crate: `sec min hour day month dow year`.
///
/// Input:  `*/5 * * * *`       (min hour day month dow)
/// Output: `0 */5 * * * * *`   (sec min hour day month dow year)
fn to_7field_cron(expr: &str) -> String {
    format!("0 {} *", expr.trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::notification_service::DefaultNotificationService;
    use crate::application::ports::{AgentRepository, RunRepository};
    use crate::domain::models::{Agent, Cli, Trigger};

    fn manual_agent(id: &str, enabled: bool) -> Agent {
        Agent {
            id: id.to_string(),
            prompt: "do nothing".to_string(),
            trigger: None,
            cli: Cli::new("opencode"),
            model: None,
            working_dir: None,
            enabled,
            enable_at: None,
            created_at: Utc::now(),
            log_path: "/tmp/enable-at-test.log".to_string(),
            timeout_minutes: 15,
            expires_at: None,
            last_run_at: None,
            last_run_ok: None,
            last_triggered_at: None,
            trigger_count: 0,
        }
    }

    fn test_scheduler() -> (Arc<Database>, CronScheduler) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::new(&dir.path().join("test.db")).unwrap());
        std::mem::forget(dir);
        let executor = Arc::new(Executor::new(
            db.clone(),
            Arc::new(DefaultNotificationService),
        ));
        let scheduler = CronScheduler::new(db.clone(), executor);
        (db, scheduler)
    }

    fn test_scheduler_with_loops() -> (Arc<Database>, CronScheduler) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::new(&dir.path().join("test.db")).unwrap());
        std::mem::forget(dir);
        let executor = Arc::new(Executor::new(
            db.clone(),
            Arc::new(DefaultNotificationService),
        ));
        let loop_engine = Arc::new(crate::loop_engine::LoopEngine::new(
            db.clone(),
            Arc::new(DefaultNotificationService),
        ));
        let scheduler = CronScheduler::with_loops(db.clone(), executor, loop_engine);
        (db, scheduler)
    }

    fn sample_loop(
        id: &str,
        status: crate::domain::loops::LoopStatus,
    ) -> crate::domain::loops::Loop {
        crate::domain::loops::Loop {
            archived: false,
            paused_by_reconciliation: false,
            id: id.to_string(),
            name: "Autorun test loop".to_string(),
            description: None,
            workdir: "/tmp/loop-autorun-test".to_string(),
            status,
            trigger: None,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_queue_id: None,
            on_completed: None,
        }
    }

    /// A future `autorun_at` must not launch the loop — it's a schedule, not
    /// an immediate action.
    #[tokio::test]
    async fn fire_due_autorun_loops_ignores_future_schedule() {
        use crate::domain::loops::LoopStatus;

        let (db, scheduler) = test_scheduler_with_loops();
        db.insert_loop(&sample_loop("future-autorun", LoopStatus::Failed))
            .unwrap();
        db.schedule_loop_autorun("future-autorun", Utc::now() + chrono::Duration::hours(1))
            .unwrap();

        scheduler.fire_due_autorun_loops(Utc::now()).unwrap();

        let lp = db.get_loop("future-autorun").unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Failed, "must not launch yet");
        assert!(
            lp.autorun_at.is_some(),
            "future autorun_at must remain pending"
        );
    }

    /// A past-due `autorun_at` on a fireable (`failed`) loop must launch it
    /// once and clear the schedule — the one-shot semantics from the spec.
    #[tokio::test]
    async fn fire_due_autorun_loops_fires_past_schedule_once_and_clears_it() {
        use crate::domain::loops::LoopStatus;

        let (db, scheduler) = test_scheduler_with_loops();
        db.insert_loop(&sample_loop("past-autorun", LoopStatus::Failed))
            .unwrap();
        db.schedule_loop_autorun("past-autorun", Utc::now() - chrono::Duration::minutes(1))
            .unwrap();

        scheduler.fire_due_autorun_loops(Utc::now()).unwrap();

        let lp = db.get_loop("past-autorun").unwrap().unwrap();
        assert!(
            lp.autorun_at.is_none(),
            "firing must clear autorun_at (one-shot)"
        );

        // Firing again must be a no-op: the schedule is already cleared, so
        // it must not appear among pending autorun loops anymore.
        let pending = db.list_pending_autorun_loops().unwrap();
        assert!(
            pending.iter().all(|l| l.id != "past-autorun"),
            "loop must not remain pending after firing once"
        );
    }

    /// B41: a schedule cancelled via `clear_loop_autorun` (the path
    /// `loop_schedule_autorun` with `at` omitted takes) must not fire later,
    /// even past its original due time — cancelling must actually prevent
    /// the wake-up, not just delay it.
    #[tokio::test]
    async fn fire_due_autorun_loops_never_fires_a_cancelled_schedule() {
        use crate::domain::loops::LoopStatus;

        let (db, scheduler) = test_scheduler_with_loops();
        db.insert_loop(&sample_loop("cancelled-autorun", LoopStatus::Failed))
            .unwrap();
        db.schedule_loop_autorun(
            "cancelled-autorun",
            Utc::now() - chrono::Duration::minutes(1),
        )
        .unwrap();
        db.clear_loop_autorun("cancelled-autorun").unwrap();

        scheduler.fire_due_autorun_loops(Utc::now()).unwrap();

        let lp = db.get_loop("cancelled-autorun").unwrap().unwrap();
        assert_eq!(
            lp.status,
            LoopStatus::Failed,
            "a cancelled schedule must not auto-reset/resume the loop"
        );
        assert!(lp.autorun_at.is_none());
    }

    /// A `Running`/`Paused` loop must not be relaunched by its own
    /// `autorun_at`, even if it's past due — that would spawn a duplicate
    /// execution over the same graph.
    #[tokio::test]
    async fn fire_due_autorun_loops_skips_running_and_paused_loops() {
        use crate::domain::loops::LoopStatus;

        for status in [LoopStatus::Running, LoopStatus::Paused] {
            let (db, scheduler) = test_scheduler_with_loops();
            let id = format!("busy-autorun-{}", status.as_str());
            db.insert_loop(&sample_loop(&id, status)).unwrap();
            db.schedule_loop_autorun(&id, Utc::now() - chrono::Duration::minutes(1))
                .unwrap();

            scheduler.fire_due_autorun_loops(Utc::now()).unwrap();

            let lp = db.get_loop(&id).unwrap().unwrap();
            assert_eq!(lp.status, status, "status must be untouched");
            assert!(
                lp.autorun_at.is_some(),
                "{status:?} loop must not have its autorun_at cleared"
            );
        }
    }

    /// `loop_run` refuses a `failed` loop directly, so firing autorun on one
    /// must not call `start_background` on it as-is. Instead it must go
    /// through the same reset transition as `loop_reset`
    /// ([`Database::reset_loop`]) and then resume — the resilience pattern a
    /// quota-failed loop relies on to revive itself unattended.
    #[tokio::test]
    async fn fire_due_autorun_loops_resets_failed_loop_through_shared_path_and_resumes() {
        use crate::domain::loops::{
            Loop, LoopNode, LoopNodeKind, LoopSpec, LoopSpecStatus, LoopStatus,
        };

        let (db, scheduler) = test_scheduler_with_loops();
        // A real, existing workdir: the resumed run's check node actually
        // spawns a shell in it, unlike the other autorun tests which only
        // assert on synchronous state and never let the loop engine run.
        let workdir = tempfile::tempdir().unwrap();
        let loop_id = "failed-autorun".to_string();
        db.insert_loop(&Loop {
            archived: false,
            paused_by_reconciliation: false,
            id: loop_id.clone(),
            name: "Autorun test loop".to_string(),
            description: None,
            workdir: workdir.path().to_string_lossy().to_string(),
            status: LoopStatus::Failed,
            trigger: None,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_queue_id: None,
            on_completed: None,
        })
        .unwrap();
        db.insert_loop_spec(&LoopSpec {
            id: "spec-1".to_string(),
            loop_id: Some(loop_id.clone()),
            name: "Spec 1".to_string(),
            description: Some(
                "Functional Requirements:\n- A\n\nNon-Functional Requirements:\n- B\n\nObjective:\n- C\n\nConstraints:\n- D\n\nGuidelines:\n- E\n\nIn Scope:\n- F\n\nOut of Scope:\n- G".to_string(),
            ),
            position: 1,
            parallelizable: false,
            status: LoopSpecStatus::Failed,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        })
        .unwrap();
        db.insert_loop_node(&LoopNode {
            id: "node-check".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: Utc::now(),
        })
        .unwrap();
        db.schedule_loop_autorun(&loop_id, Utc::now() - chrono::Duration::minutes(1))
            .unwrap();

        scheduler.fire_due_autorun_loops(Utc::now()).unwrap();

        // The reset happens synchronously, before the resumed run is spawned
        // in the background.
        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert!(lp.autorun_at.is_none(), "firing must clear autorun_at");
        assert_ne!(
            lp.status,
            LoopStatus::Failed,
            "the loop must be reset off `failed` before resuming"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let lp = db.get_loop(&loop_id).unwrap().unwrap();
            if lp.status == LoopStatus::Completed {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "resumed run did not complete in time; loop status is {:?}",
                lp.status
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let spec = db.get_loop_spec("spec-1").unwrap().unwrap();
        assert_eq!(
            spec.status,
            LoopSpecStatus::Completed,
            "the auto-resumed run must have actually executed the spec's graph"
        );
    }

    /// C1's regression test — the real sequence: a loop is genuinely
    /// `Running` when the daemon dies uncleanly; `reconcile_orphaned_loops`
    /// (as it would run at the next boot) pauses it and marks its dangling
    /// run interrupted; the resilience node's `loop_schedule_autorun` is
    /// already due by the time the scheduler next evaluates it. Before this
    /// fix, `is_autorun_due` excluded every `Paused` loop unconditionally, so
    /// this schedule would sit forever and never fire. It must fire now,
    /// through the same `resume_background` path a manual `loop_continue`
    /// takes, and actually finish the interrupted spec.
    #[tokio::test]
    async fn fire_due_autorun_loops_fires_reconciliation_paused_loop() {
        use crate::domain::loops::{LoopNodeRun, LoopRunStatus, LoopSpecStatus};

        let (db, scheduler) = test_scheduler_with_loops();
        let workdir = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        let loop_id = "reconciled-autorun".to_string();

        let mut lp = sample_loop(&loop_id, LoopStatus::Running);
        lp.workdir = workdir.path().to_string_lossy().to_string();
        db.insert_loop(&lp).unwrap();

        let spec = crate::domain::loops::LoopSpec {
            id: "spec-reconciled".to_string(),
            loop_id: Some(loop_id.clone()),
            name: "Spec 1".to_string(),
            description: Some(
                "Functional Requirements:\n- A\n\nNon-Functional Requirements:\n- B\n\nObjective:\n- C\n\nConstraints:\n- D\n\nGuidelines:\n- E\n\nIn Scope:\n- F\n\nOut of Scope:\n- G".to_string(),
            ),
            position: 1,
            parallelizable: false,
            status: LoopSpecStatus::Running,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        };
        db.insert_loop_spec(&spec).unwrap();
        db.update_loop_spec_status(&spec.id, LoopSpecStatus::Running, Some(Utc::now()), None)
            .unwrap();

        let node = crate::domain::loops::LoopNode {
            id: "node-reconciled".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "check".to_string(),
            kind: crate::domain::loops::LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: Utc::now(),
        };
        db.insert_loop_node(&node).unwrap();

        // The run left dangling by the daemon that died uncleanly.
        db.insert_loop_run(&LoopNodeRun {
            id: "run-reconciled".to_string(),
            loop_id: loop_id.clone(),
            spec_id: spec.id.clone(),
            node_id: node.id.clone(),
            status: LoopRunStatus::Running,
            input: None,
            output: None,
            started_at: Utc::now(),
            completed_at: None,
            iteration: 1,
            pid: None,
            boot_id: None,
            session_id: None,
        })
        .unwrap();

        // The next boot's reconciliation pass — this is what must leave the
        // loop resumable, not the test hand-setting `paused_by_reconciliation`.
        assert_eq!(db.reconcile_orphaned_loops(data_dir.path()).unwrap(), 1);
        let lp_after_reconcile = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp_after_reconcile.status, LoopStatus::Paused);
        assert!(
            lp_after_reconcile.paused_by_reconciliation,
            "reconciliation must flag its own pause"
        );

        // The resilience node's scheduled resume, already due.
        db.schedule_loop_autorun(&loop_id, Utc::now() - chrono::Duration::minutes(1))
            .unwrap();

        scheduler.fire_due_autorun_loops(Utc::now()).unwrap();

        let lp_fired = db.get_loop(&loop_id).unwrap().unwrap();
        assert!(
            lp_fired.autorun_at.is_none(),
            "firing must clear autorun_at"
        );

        // Unlike the `failed` case, there's no synchronous reset step here —
        // `resume_background`'s spawned task is what actually claims the
        // loop off `Paused` (via `claim_loop_for_run`). Prove it left
        // `Paused` by waiting for the resumed run to actually complete.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let lp = db.get_loop(&loop_id).unwrap().unwrap();
            if lp.status == LoopStatus::Completed {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "resumed run did not complete in time; loop status is {:?}",
                lp.status
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let spec_after = db.get_loop_spec(&spec.id).unwrap().unwrap();
        assert_eq!(
            spec_after.status,
            LoopSpecStatus::Completed,
            "the auto-resumed run must have actually executed the interrupted spec"
        );
    }

    /// C1: firing on a reconciliation-paused loop must clear `autorun_at`
    /// exactly like every other autorun fire — a second scheduler tick must
    /// not resume it again.
    #[tokio::test]
    async fn fire_due_autorun_loops_on_reconciliation_paused_loop_is_one_shot() {
        let (db, scheduler) = test_scheduler_with_loops();
        let loop_id = "reconciled-once".to_string();
        let mut lp = sample_loop(&loop_id, LoopStatus::Paused);
        lp.paused_by_reconciliation = true;
        db.insert_loop(&lp).unwrap();
        db.schedule_loop_autorun(&loop_id, Utc::now() - chrono::Duration::minutes(1))
            .unwrap();

        scheduler.fire_due_autorun_loops(Utc::now()).unwrap();
        let pending = db.list_pending_autorun_loops().unwrap();
        assert!(
            pending.iter().all(|l| l.id != loop_id),
            "firing must clear autorun_at so a second tick can't resume it again"
        );
    }

    /// C1: a due `autorun_at` on a loop the operator paused (not
    /// reconciliation) must never fire — `loop_pause`/`loop_report_blocker`
    /// both route through `update_loop_status`, which clears
    /// `paused_by_reconciliation`, so this is the default state of any
    /// `Paused` loop that didn't come through reconciliation.
    #[tokio::test]
    async fn fire_due_autorun_loops_never_fires_on_an_operator_paused_loop() {
        let (db, scheduler) = test_scheduler_with_loops();
        let loop_id = "operator-paused-autorun".to_string();
        let lp = sample_loop(&loop_id, LoopStatus::Paused);
        assert!(!lp.paused_by_reconciliation);
        db.insert_loop(&lp).unwrap();
        db.schedule_loop_autorun(&loop_id, Utc::now() - chrono::Duration::minutes(1))
            .unwrap();

        scheduler.fire_due_autorun_loops(Utc::now()).unwrap();

        let lp_after = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp_after.status, LoopStatus::Paused, "must stay paused");
        assert!(
            lp_after.autorun_at.is_some(),
            "operator pause must not be silently discarded either — the schedule stays pending"
        );
    }

    /// The exact incident this spec fixes: a loop was launched with
    /// `loop_run { queue_id }`, failed mid-queue (e.g. a quota error), and its
    /// `loop_schedule_autorun` fired to revive it. Before this fix, autorun
    /// resumed the loop with its own bound specs — empty for a queue run — so
    /// the engine found nothing to do and marked the loop `completed` with
    /// queue members still pending. Firing autorun now must reset and resume
    /// against the *same queue*, in queue order, until it's genuinely done.
    #[tokio::test]
    async fn fire_due_autorun_loops_resumes_same_queue_after_failed_run() {
        use crate::domain::loops::{
            Loop, LoopNode, LoopNodeKind, LoopSpec, LoopSpecStatus, LoopStatus,
        };
        use crate::domain::queues::Queue;

        let (db, scheduler) = test_scheduler_with_loops();
        let workdir = tempfile::tempdir().unwrap();
        let loop_id = "failed-queue-autorun".to_string();
        db.insert_loop(&Loop {
            archived: false,
            paused_by_reconciliation: false,
            id: loop_id.clone(),
            name: "Autorun queue test loop".to_string(),
            description: None,
            workdir: workdir.path().to_string_lossy().to_string(),
            status: LoopStatus::Failed,
            trigger: None,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_queue_id: Some("queue-1".to_string()),
            on_completed: None,
        })
        .unwrap();

        let standalone = |id: &str, position: i64, status: LoopSpecStatus| LoopSpec {
            id: id.to_string(),
            loop_id: None,
            name: id.to_string(),
            description: None,
            position,
            parallelizable: false,
            status,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        };
        db.insert_loop_spec(&standalone("queue-done", 1, LoopSpecStatus::Completed))
            .unwrap();
        // Left `failed` by the run that hit quota mid-queue — never explicitly
        // reset, unlike the loop's own status.
        db.insert_loop_spec(&standalone("queue-failed", 2, LoopSpecStatus::Failed))
            .unwrap();
        db.insert_loop_spec(&standalone("queue-pending", 3, LoopSpecStatus::Pending))
            .unwrap();
        db.insert_queue(&Queue {
            id: "queue-1".to_string(),
            name: "queue-1".to_string(),
            created_at: Utc::now(),
        })
        .unwrap();
        for spec_id in ["queue-done", "queue-failed", "queue-pending"] {
            db.append_queue_member("queue-1", spec_id, None).unwrap();
        }
        // No bound specs on the loop itself — this is what the real incident
        // hit: a `loop_run { queue_id }` launch never binds specs to the loop.
        db.insert_loop_node(&LoopNode {
            id: "node-check".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: Utc::now(),
        })
        .unwrap();
        db.schedule_loop_autorun(&loop_id, Utc::now() - chrono::Duration::minutes(1))
            .unwrap();

        scheduler.fire_due_autorun_loops(Utc::now()).unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert!(lp.autorun_at.is_none(), "firing must clear autorun_at");
        assert_ne!(
            lp.status,
            LoopStatus::Failed,
            "the loop must be reset off `failed` before resuming"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let lp = db.get_loop(&loop_id).unwrap().unwrap();
            if lp.status == LoopStatus::Completed {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "resumed queue run did not complete in time; loop status is {:?}",
                lp.status
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // All three queue members ran to completion via the *same* queue — not
        // a false completion with them left pending.
        for spec_id in ["queue-done", "queue-failed", "queue-pending"] {
            let spec = db.get_loop_spec(spec_id).unwrap().unwrap();
            assert_eq!(
                spec.status,
                LoopSpecStatus::Completed,
                "spec '{spec_id}' should have completed via the resumed queue run"
            );
        }
        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(
            lp.status,
            LoopStatus::Completed,
            "the resumed queue run must reach genuine completion"
        );
        // B31: the run context survives genuine completion as last-run data
        // so `loop list` / `loop info` keep rendering the finished loop's
        // real n/n queue progress. B8's anti-pollution guarantee is upheld at
        // launch time (every path re-persists this before the first spec),
        // not by clearing it on completion.
        assert_eq!(
            lp.active_run_queue_id.as_deref(),
            Some("queue-1"),
            "a genuinely finished queue run keeps the persisted run context for progress display"
        );
    }

    /// Firing autorun on an already-`completed` loop must not silently
    /// re-run it — that's a human decision via `loop_reset` + `loop_run`.
    /// The scheduler should warn and clear the schedule instead.
    #[tokio::test]
    async fn fire_due_autorun_loops_on_completed_loop_warns_and_does_not_run() {
        use crate::domain::loops::LoopStatus;

        let (db, scheduler) = test_scheduler_with_loops();
        let loop_id = "completed-autorun".to_string();
        db.insert_loop(&sample_loop(&loop_id, LoopStatus::Completed))
            .unwrap();
        db.schedule_loop_autorun(&loop_id, Utc::now() - chrono::Duration::minutes(1))
            .unwrap();

        scheduler.fire_due_autorun_loops(Utc::now()).unwrap();

        // Give any (unexpected) spawned background run a chance to run.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert!(
            lp.autorun_at.is_none(),
            "the one-shot schedule must still be cleared"
        );
        assert_eq!(
            lp.status,
            LoopStatus::Completed,
            "a completed loop must not be re-run by its own autorun"
        );
    }

    /// A future `auto_continue_at` must not resume the loop — it's a
    /// schedule, not an immediate action.
    #[tokio::test]
    async fn fire_due_auto_continue_loops_ignores_future_schedule() {
        use crate::domain::loops::LoopStatus;

        let (db, scheduler) = test_scheduler_with_loops();
        db.insert_loop(&sample_loop("future-auto-continue", LoopStatus::Paused))
            .unwrap();
        db.schedule_loop_auto_continue(
            "future-auto-continue",
            Utc::now() + chrono::Duration::hours(1),
            None,
        )
        .unwrap();

        scheduler.fire_due_auto_continue_loops(Utc::now()).unwrap();

        let lp = db.get_loop("future-auto-continue").unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Paused, "must not resume yet");
        assert!(
            lp.auto_continue_at.is_some(),
            "future auto_continue_at must remain pending"
        );
    }

    /// A schedule cancelled via `clear_loop_auto_continue` must not fire
    /// later, even past its original due time.
    #[tokio::test]
    async fn fire_due_auto_continue_loops_never_fires_a_cancelled_schedule() {
        use crate::domain::loops::LoopStatus;

        let (db, scheduler) = test_scheduler_with_loops();
        db.insert_loop(&sample_loop("cancelled-auto-continue", LoopStatus::Paused))
            .unwrap();
        db.schedule_loop_auto_continue(
            "cancelled-auto-continue",
            Utc::now() - chrono::Duration::minutes(1),
            None,
        )
        .unwrap();
        db.clear_loop_auto_continue("cancelled-auto-continue")
            .unwrap();

        scheduler.fire_due_auto_continue_loops(Utc::now()).unwrap();

        let lp = db.get_loop("cancelled-auto-continue").unwrap().unwrap();
        assert_eq!(
            lp.status,
            LoopStatus::Paused,
            "a cancelled schedule must not resume the loop"
        );
        assert!(lp.auto_continue_at.is_none());
    }

    /// If the loop is no longer `Paused` by the scheduled time (already
    /// continued manually, failed, completed, or running), the schedule must
    /// be cleared without resuming it — never double-run.
    #[tokio::test]
    async fn fire_due_auto_continue_loops_clears_without_firing_when_not_paused() {
        use crate::domain::loops::LoopStatus;

        for status in [
            LoopStatus::Draft,
            LoopStatus::Running,
            LoopStatus::Completed,
            LoopStatus::Failed,
        ] {
            let (db, scheduler) = test_scheduler_with_loops();
            let id = format!("not-paused-auto-continue-{}", status.as_str());
            db.insert_loop(&sample_loop(&id, status)).unwrap();
            db.schedule_loop_auto_continue(&id, Utc::now() - chrono::Duration::minutes(1), None)
                .unwrap();

            scheduler.fire_due_auto_continue_loops(Utc::now()).unwrap();

            let lp = db.get_loop(&id).unwrap().unwrap();
            assert_eq!(
                lp.status, status,
                "{status:?} loop's status must be untouched"
            );
            assert!(
                lp.auto_continue_at.is_none(),
                "{status:?} loop's stale schedule must still be cleared, not left pending forever"
            );
        }
    }

    /// The functional core of this feature: a `Paused` loop's
    /// `auto_continue_at` firing must go straight through the
    /// `loop_continue`/`resume_background` path — never `loop_reset` and
    /// never a fresh dispatch — preserving the paused cursor. Verified by
    /// checking the in-flight spec's status is untouched *synchronously*,
    /// right after firing (a reset would flip it to `Pending` immediately,
    /// before any background dispatch runs), then letting the real resumed
    /// dispatch run to completion.
    #[tokio::test]
    async fn fire_due_auto_continue_loops_resumes_paused_loop_without_reset_or_relaunch() {
        use crate::domain::loops::{
            Loop, LoopNode, LoopNodeKind, LoopSpec, LoopSpecStatus, LoopStatus,
        };

        let (db, scheduler) = test_scheduler_with_loops();
        let workdir = tempfile::tempdir().unwrap();
        let loop_id = "paused-auto-continue".to_string();
        db.insert_loop(&Loop {
            archived: false,
            paused_by_reconciliation: false,
            id: loop_id.clone(),
            name: "Auto-continue test loop".to_string(),
            description: None,
            workdir: workdir.path().to_string_lossy().to_string(),
            status: LoopStatus::Paused,
            trigger: None,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_queue_id: None,
            on_completed: None,
        })
        .unwrap();
        db.insert_loop_spec(&LoopSpec {
            id: "spec-1".to_string(),
            loop_id: Some(loop_id.clone()),
            name: "Spec 1".to_string(),
            description: Some(
                "Functional Requirements:\n- A\n\nNon-Functional Requirements:\n- B\n\nObjective:\n- C\n\nConstraints:\n- D\n\nGuidelines:\n- E\n\nIn Scope:\n- F\n\nOut of Scope:\n- G".to_string(),
            ),
            position: 1,
            parallelizable: false,
            // A loop paused mid-spec leaves that spec `running` — `loop_pause`
            // never touches spec status, only the loop's own.
            status: LoopSpecStatus::Running,
            started_at: Some(Utc::now()),
            completed_at: None,
            spec_start_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        })
        .unwrap();
        db.insert_loop_node(&LoopNode {
            id: "node-check".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: Utc::now(),
        })
        .unwrap();
        db.schedule_loop_auto_continue(
            &loop_id,
            Utc::now() - chrono::Duration::minutes(1),
            Some("retry_current_node"),
        )
        .unwrap();

        scheduler.fire_due_auto_continue_loops(Utc::now()).unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert!(
            lp.auto_continue_at.is_none(),
            "firing must clear auto_continue_at"
        );

        // `retry_current_node` never mutates the spec, and a reset would have
        // flipped it to `Pending` synchronously, before the background
        // dispatch even starts — so `Running` here proves no reset happened.
        let spec = db.get_loop_spec("spec-1").unwrap().unwrap();
        assert_eq!(
            spec.status,
            LoopSpecStatus::Running,
            "auto-continue must not reset the in-flight spec"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let lp = db.get_loop(&loop_id).unwrap().unwrap();
            if lp.status == LoopStatus::Completed {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "resumed run did not complete in time; loop status is {:?}",
                lp.status
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let spec = db.get_loop_spec("spec-1").unwrap().unwrap();
        assert_eq!(
            spec.status,
            LoopSpecStatus::Completed,
            "the auto-continued run must have actually executed the spec's graph"
        );
    }

    /// `skip_next_spec` must be honored too, not just the default
    /// `retry_current_node` — the scheduler must pass the configured action
    /// through exactly like the `loop_continue` MCP tool would.
    #[tokio::test]
    async fn fire_due_auto_continue_loops_applies_skip_next_spec_action() {
        use crate::domain::loops::{Loop, LoopSpec, LoopSpecStatus, LoopStatus};

        let (db, scheduler) = test_scheduler_with_loops();
        let workdir = tempfile::tempdir().unwrap();
        let loop_id = "paused-auto-continue-skip".to_string();
        db.insert_loop(&Loop {
            archived: false,
            paused_by_reconciliation: false,
            id: loop_id.clone(),
            name: "Auto-continue skip test loop".to_string(),
            description: None,
            workdir: workdir.path().to_string_lossy().to_string(),
            status: LoopStatus::Paused,
            trigger: None,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_queue_id: None,
            on_completed: None,
        })
        .unwrap();
        db.insert_loop_spec(&LoopSpec {
            id: "spec-skip".to_string(),
            loop_id: Some(loop_id.clone()),
            name: "Spec skip".to_string(),
            description: Some("desc".to_string()),
            position: 1,
            parallelizable: false,
            status: LoopSpecStatus::Running,
            started_at: Some(Utc::now()),
            completed_at: None,
            spec_start_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        })
        .unwrap();
        db.schedule_loop_auto_continue(
            &loop_id,
            Utc::now() - chrono::Duration::minutes(1),
            Some("skip_next_spec"),
        )
        .unwrap();

        scheduler.fire_due_auto_continue_loops(Utc::now()).unwrap();

        let spec = db.get_loop_spec("spec-skip").unwrap().unwrap();
        assert_eq!(
            spec.status,
            LoopSpecStatus::Skipped,
            "skip_next_spec must mark the in-flight spec skipped, synchronously"
        );
        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert!(lp.auto_continue_at.is_none(), "firing must be one-shot");
    }

    /// A future `enable_at` must not flip the agent to enabled — it's a
    /// schedule, not an immediate action.
    #[test]
    fn fire_due_enable_at_ignores_future_schedule() {
        let (db, scheduler) = test_scheduler();
        db.upsert_agent(&manual_agent("future-wake", false))
            .unwrap();
        db.schedule_agent_enable("future-wake", Utc::now() + chrono::Duration::hours(1))
            .unwrap();

        scheduler.fire_due_enable_at(Utc::now()).unwrap();

        let agent = db.get_agent("future-wake").unwrap().unwrap();
        assert!(!agent.enabled, "future enable_at must not enable yet");
        assert!(
            agent.enable_at.is_some(),
            "future enable_at must remain pending"
        );
    }

    /// A past-due `enable_at` must enable the agent and clear the field —
    /// the one-shot semantics from the spec.
    #[test]
    fn fire_due_enable_at_activates_past_schedule_and_clears_it() {
        let (db, scheduler) = test_scheduler();
        db.upsert_agent(&manual_agent("past-wake", false)).unwrap();
        db.schedule_agent_enable("past-wake", Utc::now() - chrono::Duration::minutes(1))
            .unwrap();

        scheduler.fire_due_enable_at(Utc::now()).unwrap();

        let agent = db.get_agent("past-wake").unwrap().unwrap();
        assert!(agent.enabled, "past-due enable_at must enable the agent");
        assert!(
            agent.enable_at.is_none(),
            "activation must clear enable_at (one-shot)"
        );
    }

    /// `next_sleep_duration` must wake the scheduler for a pending
    /// `enable_at`, not just cron expressions — otherwise the one-shot
    /// enable would rely on the 60s reconcile fallback instead of firing on
    /// time.
    #[test]
    fn next_sleep_duration_wakes_for_pending_enable_at() {
        let (db, scheduler) = test_scheduler();
        db.upsert_agent(&manual_agent("soon-wake", false)).unwrap();
        db.schedule_agent_enable("soon-wake", Utc::now() + chrono::Duration::seconds(5))
            .unwrap();

        let dur = scheduler.next_sleep_duration();
        assert!(
            dur <= std::time::Duration::from_secs(6),
            "expected to wake in ~5s for the pending enable_at, got {:?}",
            dur
        );
    }

    /// `next_sleep_duration` must also wake for a pending `auto_continue_at`,
    /// same as `autorun_at` — otherwise a deferred paused-loop resume would
    /// rely on the reconcile fallback instead of firing on time.
    #[test]
    fn next_sleep_duration_wakes_for_pending_auto_continue_at() {
        use crate::domain::loops::LoopStatus;

        let (db, scheduler) = test_scheduler_with_loops();
        db.insert_loop(&sample_loop("soon-auto-continue", LoopStatus::Paused))
            .unwrap();
        db.schedule_loop_auto_continue(
            "soon-auto-continue",
            Utc::now() + chrono::Duration::seconds(5),
            None,
        )
        .unwrap();

        let dur = scheduler.next_sleep_duration();
        assert!(
            dur <= std::time::Duration::from_secs(6),
            "expected to wake in ~5s for the pending auto_continue_at, got {:?}",
            dur
        );
    }

    /// B7: a single agent row with a malformed `trigger_config` (e.g. a raw
    /// cron string `11 3 11 7 *` written directly to SQLite by an external
    /// tool, where JSON `{"type":"cron","schedule_expr":"..."}` is expected)
    /// must be quarantined — disabled with one WARN — not left to bail
    /// `list_cron_agents` and repeat "Scheduler fire failed" every tick.
    #[test]
    fn quarantine_corrupt_agents_disables_corrupt_row_and_leaves_healthy_untouched() {
        let (db, scheduler) = test_scheduler();
        db.insert_corrupt_agent_for_test("corrupt-1", true).unwrap();
        db.upsert_agent(&manual_agent("healthy-1", true)).unwrap();

        scheduler
            .quarantine_corrupt_agents()
            .expect("must not bail on a corrupt row");

        let corrupt = db.list_corrupt_agents().unwrap();
        assert_eq!(corrupt.len(), 1);
        assert_eq!(corrupt[0].id, "corrupt-1");
        assert!(!corrupt[0].enabled, "corrupt row must be quarantined");

        let healthy = db.get_agent("healthy-1").unwrap().unwrap();
        assert!(healthy.enabled, "healthy agent must be left untouched");
    }

    /// Quarantining an already-disabled corrupt row must be a no-op — this is
    /// what keeps a repeated tick from re-warning about the same row forever.
    #[test]
    fn quarantine_corrupt_agents_is_idempotent_for_an_already_disabled_row() {
        let (db, scheduler) = test_scheduler();
        db.insert_corrupt_agent_for_test("corrupt-1", false)
            .unwrap();

        scheduler.quarantine_corrupt_agents().unwrap();
        scheduler.quarantine_corrupt_agents().unwrap();

        let corrupt = db.list_corrupt_agents().unwrap();
        assert_eq!(corrupt.len(), 1, "the row itself is untouched, not deleted");
        assert!(!corrupt[0].enabled);
    }

    /// The actual incident: `fire_due_tasks` (the scheduler tick) must not
    /// bail when a corrupt cron row is present — it must quarantine that row
    /// and still evaluate/fire the remaining, healthy cron agents.
    #[tokio::test]
    async fn fire_due_tasks_quarantines_corrupt_row_and_keeps_scheduling_others() {
        let (db, scheduler) = test_scheduler();
        db.insert_corrupt_agent_for_test("corrupt-1", true).unwrap();

        let mut healthy = manual_agent("healthy-1", true);
        healthy.cli = Cli::new("definitely-not-a-real-cli-binary-xyz");
        healthy.trigger = Some(Trigger::Cron {
            schedule_expr: "* * * * *".to_string(),
        });
        db.upsert_agent(&healthy).unwrap();

        scheduler
            .fire_due_tasks()
            .await
            .expect("a corrupt row must not fail the whole tick");

        let corrupt = db.list_corrupt_agents().unwrap();
        assert_eq!(corrupt.len(), 1);
        assert!(!corrupt[0].enabled, "corrupt row must be quarantined");

        {
            let last_fired = scheduler.last_fired.lock().await;
            assert!(
                last_fired.contains_key("healthy-1"),
                "the healthy cron agent must still have been evaluated and fired \
                 despite the corrupt row"
            );
        }

        // A second tick must not re-touch the now-disabled corrupt row (no
        // repeated per-tick warning/write for the same row).
        scheduler.fire_due_tasks().await.unwrap();
        let corrupt_again = db.list_corrupt_agents().unwrap();
        assert_eq!(corrupt_again.len(), 1);
        assert!(!corrupt_again[0].enabled);
    }

    #[test]
    fn test_to_7field_cron() {
        assert_eq!(to_7field_cron("*/5 * * * *"), "0 */5 * * * * *");
        assert_eq!(to_7field_cron("0 9 * * *"), "0 0 9 * * * *");
        assert_eq!(to_7field_cron("0 9 * * 1-5"), "0 0 9 * * 1-5 *");
    }

    #[test]
    fn test_retry_policy_defaults() {
        let d = RetryPolicy::default();
        assert!(d.enabled);
        assert_eq!(d.delay_minutes, 60);
        assert_eq!(d.max_retries, 3);
    }

    #[test]
    fn test_run_outcome_is_failure() {
        assert!(!run_outcome_is_failure(&Ok(0)), "clean exit is success");
        assert!(run_outcome_is_failure(&Ok(1)), "non-zero exit is failure");
        assert!(
            run_outcome_is_failure(&Err(anyhow::anyhow!("spawn failed"))),
            "spawn error is failure"
        );
    }

    #[test]
    fn test_should_retry_respects_enabled_and_cap() {
        let on = RetryPolicy {
            enabled: true,
            delay_minutes: 60,
            max_retries: 3,
        };
        // attempts 0,1,2 retry; the 3rd failed attempt (index 3) does not.
        assert!(should_retry(&on, 0));
        assert!(should_retry(&on, 2));
        assert!(!should_retry(&on, 3));

        let off = RetryPolicy {
            enabled: false,
            ..on
        };
        assert!(!should_retry(&off, 0), "disabled policy never retries");
    }

    #[test]
    fn test_cron_parse_after_conversion() {
        let cases = vec![
            "*/5 * * * *",    // every 5 min
            "0 9 * * *",      // daily at 9am
            "0 9 * * 1-5",    // weekdays at 9am
            "30 14 1,15 * *", // 1st and 15th at 2:30pm
        ];

        for expr in cases {
            let converted = to_7field_cron(expr);
            let result = Schedule::from_str(&converted);
            assert!(
                result.is_ok(),
                "Failed to parse '{}' -> '{}': {:?}",
                expr,
                converted,
                result.err()
            );
        }
    }

    #[test]
    fn test_to_7field_cron_trims_whitespace() {
        assert_eq!(to_7field_cron("  */5 * * * *  "), "0 */5 * * * * *");
        assert_eq!(to_7field_cron("\t0 9 * * *\t"), "0 0 9 * * * *");
    }

    #[test]
    fn test_cron_schedule_next_fire_time() {
        let converted = to_7field_cron("* * * * *");
        let schedule = Schedule::from_str(&converted).unwrap();
        let now = chrono::Utc::now();
        let next = schedule.after(&now).next();
        assert!(next.is_some());
    }

    /// The schedule iterator interprets cron fields in the timezone of the
    /// "now" reference. We feed it a `Local` "now" so user-authored cron
    /// expressions like `0 9 * * *` mean "9 AM on the user's wall clock",
    /// not 9 AM UTC. This test verifies the field interpretation by
    /// comparing local vs UTC.
    #[test]
    fn test_cron_field_uses_local_timezone() {
        use chrono::Timelike;
        let converted = to_7field_cron("0 9 * * *");
        let schedule = Schedule::from_str(&converted).unwrap();
        let now_local = chrono::Local::now();
        let next_local = schedule.after(&now_local).next().expect("next fire time");
        // The hour field of the *local* fire time must be 9 — that's the
        // whole point of evaluating against a Local reference.
        assert_eq!(next_local.hour(), 9);
        // And the local hour must differ from the UTC hour whenever the
        // system isn't in UTC, otherwise the test isn't proving anything.
        // (Skip the assertion in the rare case the test runs in UTC, e.g.
        // CI on a server with TZ=UTC.)
        let next_utc = next_local.with_timezone(&chrono::Utc);
        if chrono::Local::now().offset().local_minus_utc() != 0 {
            assert_ne!(
                next_local.hour(),
                next_utc.hour(),
                "local hour and UTC hour are equal — the scheduler would be \
                 treating cron fields as UTC, which is the bug we are guarding against"
            );
        }
    }

    /// `next_fire_utc` must land the fire at the local wall-clock time named
    /// in the cron expression. For `30 8 * * *` the next fire, converted back
    /// to local, must read 08:30 — regardless of the machine's UTC offset.
    #[test]
    fn test_next_fire_utc_lands_at_local_wall_clock() {
        use chrono::Timelike;
        let schedule = Schedule::from_str(&to_7field_cron("30 8 * * *")).unwrap();
        let next_utc = next_fire_utc(&schedule, chrono::Local::now()).expect("next fire time");
        let next_local = next_utc.with_timezone(&chrono::Local);
        assert_eq!(next_local.hour(), 8, "fire must be at 08:xx local");
        assert_eq!(next_local.minute(), 30, "fire must be at xx:30 local");
    }

    /// A cron loop shares the agents' fire math: `fold_earliest` on a loop's
    /// schedule lands the next fire at the local wall-clock time the expression
    /// names (08:30 local for `30 8 * * *`), not 08:30 UTC.
    #[test]
    fn fold_earliest_lands_loop_cron_at_local_wall_clock() {
        use chrono::Timelike;
        let mut earliest: Option<chrono::DateTime<Utc>> = None;
        fold_earliest(&mut earliest, Some("30 8 * * *"), chrono::Local::now());
        let next_local = earliest
            .expect("cron loop yields a fire time")
            .with_timezone(&Local);
        assert_eq!(next_local.hour(), 8, "loop fire must be at 08:xx local");
        assert_eq!(next_local.minute(), 30, "loop fire must be at xx:30 local");
    }

    /// A manual loop (no schedule) never contributes a fire time, so the
    /// scheduler never launches it on its own — it only runs via `loop_run`.
    #[test]
    fn fold_earliest_ignores_manual_loop() {
        let mut earliest: Option<chrono::DateTime<Utc>> = Some(Utc::now());
        let before = earliest;
        fold_earliest(&mut earliest, None, chrono::Local::now());
        assert_eq!(
            earliest, before,
            "a manual loop must not change the nearest fire time"
        );
    }

    #[test]
    fn loop_key_namespaces_ids() {
        assert_eq!(loop_key("abc"), "loop:abc");
    }

    /// `due_fire_local` fires within the local minute the cron field names and
    /// only within the 60-second lookback window — never for a future minute.
    #[test]
    fn test_due_fire_local_window() {
        use chrono::{TimeZone, Timelike};
        let schedule = Schedule::from_str(&to_7field_cron("30 8 * * *")).unwrap();

        // A few seconds past 08:30 local → due (matched fire is 08:30 local).
        let just_after = Local.with_ymd_and_hms(2026, 7, 3, 8, 30, 20).unwrap();
        let fired = due_fire_local(&schedule, just_after).expect("should be due");
        assert_eq!(
            (fired.hour(), fired.minute()),
            (8, 30),
            "matched fire must be the 08:30 local occurrence"
        );

        // 08:00 local → the next occurrence after 07:59 is 08:30, which is in
        // the future, so it must not be due yet.
        let before = Local.with_ymd_and_hms(2026, 7, 3, 8, 0, 0).unwrap();
        assert!(
            due_fire_local(&schedule, before).is_none(),
            "08:00 must not fire the 08:30 schedule"
        );

        // Just over a minute past 08:30 → outside the lookback window; the
        // next occurrence after 08:30:30 is tomorrow's 08:30, in the future.
        let stale = Local.with_ymd_and_hms(2026, 7, 3, 8, 31, 30).unwrap();
        assert!(
            due_fire_local(&schedule, stale).is_none(),
            "08:31:30 is past the 60s lookback and must not re-fire"
        );
    }

    /// B15: booting the scheduler and then evaluating the same cron tick a
    /// second time (simulating a second firing path racing the regular tick
    /// loop) must record exactly one execution for that tick, never two.
    ///
    /// Uses an every-minute schedule: thanks to `due_fire_local`'s 60-second
    /// lookback, the most recent minute boundary is *always* due at the
    /// instant this test runs, so there's no need to align to (or wait for)
    /// a real cron boundary. `start_paused` means the scheduler's internal
    /// `tokio::time::sleep` for its first tick is advanced virtually — the
    /// test performs no real wall-clock sleep.
    #[tokio::test(start_paused = true)]
    async fn scheduler_fires_a_cron_agent_at_most_once_per_tick() {
        let (db, scheduler) = test_scheduler();
        let mut agent = manual_agent("b15-once-per-tick", true);
        agent.trigger = Some(Trigger::Cron {
            schedule_expr: "* * * * *".to_string(),
        });
        // A binary that can't resolve: `run_cli_process` fails fast without
        // depending on any real external CLI, but the executor still runs
        // its full start-run/finalize-run path and records the run row —
        // all this test needs to count fires.
        agent.cli = Cli::new("definitely-not-a-real-cli-binary-b15");
        db.upsert_agent(&agent).unwrap();

        let scheduler = Arc::new(scheduler);
        let _cancel = Arc::clone(&scheduler).start();

        // Advance the virtual clock past the scheduler's first computed
        // sleep (at most ~60s until the next minute boundary) in small
        // steps, yielding between each so the background `run_loop` task
        // actually gets polled and reaches its own `fire_due_tasks` call —
        // a single large `advance` only fast-forwards the clock, it doesn't
        // by itself guarantee the woken task has run before this test task
        // continues.
        for _ in 0..200 {
            tokio::time::advance(Duration::from_millis(500)).await;
            tokio::task::yield_now().await;
            if !db.list_runs("b15-once-per-tick", 10).unwrap().is_empty() {
                break;
            }
        }
        assert!(
            !db.list_runs("b15-once-per-tick", 10).unwrap().is_empty(),
            "the scheduler's own tick loop never fired the due agent"
        );

        // Simulate a second evaluation path for the same tick (e.g. a
        // concurrent reconcile) landing right on top of the regular tick.
        scheduler.fire_due_tasks().await.unwrap();

        // Let the spawned executions (detached tokio tasks) record their run
        // rows on the paused-clock executor. Waits for the recorded count to
        // go quiet rather than stopping at the first row seen — stopping
        // early would miss a second spawn still in flight and turn this
        // into a false-negative regression test.
        let mut runs = db.list_runs("b15-once-per-tick", 10).unwrap();
        let mut quiet_iters = 0;
        for _ in 0..500 {
            tokio::task::yield_now().await;
            let current = db.list_runs("b15-once-per-tick", 10).unwrap();
            if current.len() == runs.len() {
                quiet_iters += 1;
                if quiet_iters >= 20 {
                    break;
                }
            } else {
                quiet_iters = 0;
            }
            runs = current;
        }

        assert_eq!(
            runs.len(),
            1,
            "the same tick must fire at most once, got {} run(s): {:?}",
            runs.len(),
            runs
        );
    }
}
