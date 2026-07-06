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

use crate::application::ports::AgentRepository;
use crate::db::Database;
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
    /// Track last execution time per schedulable to avoid double-firing.
    /// Agents are keyed by their id; loops by [`loop_key`] to avoid colliding
    /// with an agent that happens to share the same id.
    last_fired: Arc<Mutex<std::collections::HashMap<String, chrono::DateTime<Utc>>>>,
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

    /// Initialize the last_fired tracking from database to prevent duplicate
    /// executions after daemon restart.
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

    /// Fire all agents whose next cron time is now (within a 1-second tolerance).
    async fn fire_due_tasks(&self) -> anyhow::Result<()> {
        let agents = self.db.list_cron_agents()?;
        // Evaluate schedules in the user's local timezone. `now_utc` is only
        // used for the persisted `last_fired` comparison (which is stored in
        // UTC), and `now_local` for the cron-field match.
        let now_local = Local::now();
        let now_utc = Utc::now();

        for agent in &agents {
            self.try_fire_agent(agent, now_local, now_utc).await?;
        }

        // Cron-triggered loops are evaluated in the same local frame.
        if self.loop_engine.is_some() {
            let loops = self.db.list_cron_loops()?;
            for lp in &loops {
                self.try_fire_loop(lp, now_local, now_utc).await?;
            }
        }
        Ok(())
    }

    /// Evaluate a single cron loop and launch it via the loop engine if due.
    async fn try_fire_loop(
        &self,
        lp: &crate::domain::loops::Loop,
        now_local: chrono::DateTime<Local>,
        now_utc: chrono::DateTime<Utc>,
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

        if due_fire_local(&schedule, now_local).is_none() {
            return Ok(());
        }

        // De-dupe in UTC using a loop-namespaced key.
        let key = loop_key(&lp.id);
        let window_start_utc = now_utc - chrono::Duration::seconds(60);
        {
            let mut last_fired = self.last_fired.lock().await;
            if last_fired
                .get(&key)
                .is_some_and(|last| *last >= window_start_utc)
            {
                return Ok(());
            }
            last_fired.insert(key, now_utc);
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
        now_utc: chrono::DateTime<Utc>,
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
        if due_fire_local(&schedule, now_local).is_none() {
            return Ok(());
        }

        // Persist and de-dupe in UTC so the timestamps line up with the rest
        // of the system (DB schema, `last_run_at`, daemon JSON).
        let window_start_utc = now_utc - chrono::Duration::seconds(60);
        {
            let mut last_fired = self.last_fired.lock().await;
            if last_fired
                .get(&agent.id)
                .is_some_and(|last| *last >= window_start_utc)
            {
                return Ok(());
            }
            last_fired.insert(agent.id.clone(), now_utc);
        }

        let executor = Arc::clone(&self.executor);
        let agent = agent.clone();
        let retry = self.retry;
        let cancel = self.cancel.clone();
        tokio::spawn(async move {
            run_with_retry(executor, agent, retry, cancel).await;
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
}
