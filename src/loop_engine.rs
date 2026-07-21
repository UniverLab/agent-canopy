use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::Semaphore;

use crate::application::notification_service::{LoopFinishOutcome, NotificationService};
use crate::daemon::process::KILL_GRACE;
use crate::db::Database;
use crate::domain::loops::{
    EnsembleDetails, EnsembleMember, LoopEdge, LoopEdgeCondition, LoopNode, LoopNodeKind,
    LoopNodeRun, LoopRunStatus, LoopSpec, LoopSpecStatus, LoopStatus,
};
use crate::domain::models::Cli;

// Five bounces of the same (spec,node) pair is enough signal that a spec
// needs a human or a redesign; ten burned entire quota windows ping-ponging.
const DEFAULT_MAX_ITERATIONS_PER_NODE: usize = 5;
const DEFAULT_INFRA_RETRY_LIMIT: u32 = 2;
const DEFAULT_INFRA_CRASH_MAX_SECONDS: u64 = 60;
const DEFAULT_INFRA_BACKOFF_SECONDS: u64 = 30;

/// Default cap (F1) on ensemble members actually executing at once, across
/// every loop run this engine drives — an 8-member ensemble queues past this
/// many rather than fork-bombing the host. Overridable via
/// [`LoopEngine::with_ensemble_concurrency_cap`]
/// (`CanopyConfig::ensemble_concurrency_cap`).
const DEFAULT_ENSEMBLE_CONCURRENCY_CAP: usize = 4;

#[derive(Clone)]
pub struct LoopEngine {
    db: Arc<Database>,
    notification_service: Arc<dyn NotificationService>,
    /// Global semaphore (F1) bounding how many ensemble members run
    /// concurrently across every loop this engine drives. Shared (not
    /// per-run) so an 8-member ensemble in one loop can't starve another
    /// loop's ensemble running at the same time — they queue for the same
    /// pool of permits.
    ensemble_concurrency: Arc<Semaphore>,
}

/// Where a spec's sequential graph cursor currently is: at a single ordinary
/// node, or about to fan out into (or having just fanned out into) an
/// ensemble's members. The cursor is a single value at all times — a spec
/// never has two of these in flight — which is what lets `run_spec`'s loop
/// stay a plain `loop { ... }` even though an `Ensemble` step internally runs
/// N member nodes concurrently before it resolves to a single result.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SpecCursor {
    Node(String),
    /// Ensemble id — resolves to the join's own [`NodeExecution`] once every
    /// member has terminated (F1's wait-all join).
    Ensemble(String),
}

enum SpecExecutionOutcome {
    /// `summary` is the completing node's own summary text — the "one-line
    /// summary" [`render_completion_hook_prompt`]'s `{{completed_specs}}`
    /// placeholder reports for this spec.
    Completed {
        summary: String,
    },
    Paused,
    Failed(String),
    /// This spec's in-flight node run was terminated because a newer attempt at
    /// the same node superseded it (B42). Pure engine bookkeeping, not a node
    /// failure: the dispatch that owned the superseded run stops silently —
    /// it routes down no edge, fails nothing, and completes nothing. The newer
    /// attempt (or, for a duplicate resume, the dispatch that won the loop
    /// claim) is what now drives the loop.
    Superseded,
}

struct NodeExecution {
    status: LoopRunStatus,
    output: Value,
    summary: String,
}

/// (B17) Distinct failure mode for [`LoopEngine::run_loop_dispatch`]'s launch
/// guard: the loop's effective spec set (bound specs, or the given pool's
/// pending members) was empty, so the run never actually launched. Unlike
/// every other error `run_loop_dispatch` can return, this one must never flip
/// the loop to `Failed` — [`LoopEngine::start_background_run`] and
/// [`LoopEngine::resume_background`] downcast for it and skip `fail_loop`,
/// leaving the loop's status exactly as it was before the call.
#[derive(Debug)]
struct EmptySpecSetError(String);

impl std::fmt::Display for EmptySpecSetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EmptySpecSetError {}

impl LoopEngine {
    pub fn new(db: Arc<Database>, notification_service: Arc<dyn NotificationService>) -> Self {
        Self {
            db,
            notification_service,
            ensemble_concurrency: Arc::new(Semaphore::new(DEFAULT_ENSEMBLE_CONCURRENCY_CAP)),
        }
    }

    /// Override the default ensemble concurrency cap (F1) — e.g. from
    /// `CanopyConfig::ensemble_concurrency_cap` at daemon startup. `cap` is
    /// floored at 1 so a misconfigured `0` can't wedge every ensemble join
    /// forever.
    pub fn with_ensemble_concurrency_cap(mut self, cap: usize) -> Self {
        self.ensemble_concurrency = Arc::new(Semaphore::new(cap.max(1)));
        self
    }

    pub fn start_background(self: Arc<Self>, loop_id: String) {
        Arc::clone(&self).start_background_run(loop_id, None, None);
    }

    /// Same as [`Self::start_background`], but optionally drives the loop's
    /// pending pool specs (see [`Self::run_loop`]) and/or overrides the
    /// workdir for this run only.
    ///
    /// This is a fresh dispatch, not a resume — it backs `loop_run`, the tool
    /// a human/scheduler calls to launch or *relaunch* a loop (including
    /// directly relaunching a `paused` loop instead of going through
    /// `loop_continue`). Every spec it reaches is treated as newly entered
    /// for `{{spec_start_head}}` purposes (B10): even a spec left `running`
    /// from a stale, never-reset prior attempt gets a fresh baseline here,
    /// rather than silently inheriting one captured under a previous
    /// run/launch. See [`Self::resume_background`] for the one path that is
    /// allowed to reuse a persisted baseline.
    pub fn start_background_run(
        self: Arc<Self>,
        loop_id: String,
        pool_id: Option<String>,
        workdir_override: Option<String>,
    ) {
        tokio::spawn(async move {
            if let Err(error) = self
                .run_loop(loop_id.clone(), pool_id, workdir_override)
                .await
            {
                if error.downcast_ref::<EmptySpecSetError>().is_some() {
                    // (B17) The loop never launched — its status is already
                    // untouched, and it must stay that way, so don't
                    // `fail_loop` it.
                    tracing::error!("Loop '{}' launch refused: {error:#}", loop_id);
                } else {
                    tracing::error!("Loop '{}' failed to run: {error:#}", loop_id);
                    let _ = self.fail_loop(&loop_id, None, &error.to_string());
                }
            }
        });
    }

    pub fn request_pause(&self, loop_id: &str) -> Result<bool> {
        let Some(lp) = self.db.get_loop(loop_id)? else {
            return Ok(false);
        };

        match lp.status {
            LoopStatus::Running => {
                let result = self
                    .db
                    .update_loop_status(loop_id, LoopStatus::Paused, None, None);
                // B12: don't wait for the sequential run_spec loop to notice
                // the pause between node executions — that could be up to a
                // full node timeout away. Kill whatever's actually running
                // for this loop right now, so the in-flight `wait()` inside
                // `run_agent_process`/`execute_check_node` unblocks promptly
                // and `run_spec`'s existing post-execution pause check (which
                // already tolerates a run whose status was changed out from
                // under it) takes it from there.
                for run in self.db.list_running_loop_runs(loop_id).unwrap_or_default() {
                    self.terminate_run(&run, "loop paused");
                }
                result
            }
            LoopStatus::Paused => Ok(true),
            _ => Ok(false),
        }
    }

    /// Run `loop_id`'s specs through its graph (R2).
    ///
    /// With `pool_id`: runs the pool's pending members, in the pool's queue
    /// order, instead of the loop's own bound specs. Pool membership never
    /// mutates the specs themselves — they stay standalone (`loop_id: None`)
    /// so the same pool can be run by different loops over time.
    ///
    /// A pool run is *live* (R6): the "next pending" spec is re-queried from
    /// the pool at every spec boundary via
    /// [`Database::pool_next_pending_spec_id`], never off a list captured at
    /// launch. That's what lets `pool_add_spec`/`pool_reorder` calls made
    /// while the run is in flight actually change what runs next — the run
    /// ends only when a pick finds no pending member left. A bound run (no
    /// `pool_id`) keeps the pre-pool behavior below: its spec list is fixed
    /// at launch.
    ///
    /// `workdir_override`, when set, wins over `loop.workdir` for this run
    /// only — the loop's own `workdir` is left untouched.
    ///
    /// Without `pool_id`: identical to the pre-pool behavior (bound specs,
    /// `loop.workdir`).
    ///
    /// Equivalent to a fresh (non-resumed) dispatch — see
    /// [`Self::run_loop_dispatch`] for the `is_resume` distinction that
    /// matters for `{{spec_start_head}}` (B10).
    pub async fn run_loop(
        &self,
        loop_id: String,
        pool_id: Option<String>,
        workdir_override: Option<String>,
    ) -> Result<()> {
        self.run_loop_dispatch(loop_id, pool_id, workdir_override, false)
            .await
    }

    /// Core of [`Self::run_loop`], plus the one bit `run_loop`'s public
    /// signature can't carry: whether this call is *resuming* an
    /// already-in-flight run ([`Self::resume_background`], the sole path
    /// behind `loop_continue` and interrupted-pool/autorun resumption) or a
    /// fresh dispatch (`loop_run`, including relaunching a `paused` loop
    /// directly, and the loop's initial launch).
    ///
    /// That distinction is exactly what `{{spec_start_head}}` (B10) needs: a
    /// spec can be left `running` in the DB either because this exact run is
    /// paused mid-node-graph (daemon restart, explicit `loop_pause`) — where
    /// the previously captured baseline is still correct and must be kept —
    /// or because a *prior, distinct* run/launch died without ever being
    /// reset — where reusing that baseline would silently compare against a
    /// HEAD from a different attempt entirely. Only `is_resume = true`
    /// (i.e. only [`Self::resume_background`]) is allowed to reuse it; every
    /// other entry point re-captures, per spec.
    async fn run_loop_dispatch(
        &self,
        loop_id: String,
        pool_id: Option<String>,
        workdir_override: Option<String>,
        is_resume: bool,
    ) -> Result<()> {
        let Some(lp) = self.db.get_loop(&loop_id)? else {
            bail!("Loop '{}' not found.", loop_id);
        };

        // (B17) Compute the effective spec set BEFORE flipping the loop to
        // `Running` — the single choke point every launch path (fresh
        // `loop_run`, cron/watch triggers, scheduled autorun, and
        // `loop_continue`'s resume) funnels through. An empty set is a launch
        // error, not a successful no-op run: it must leave the loop's status
        // untouched and record no run, so monitoring never sees a false
        // `completed` over a backlog the caller simply failed to point this
        // launch at (the 2026-07-14T14:16:31Z incident).
        if let Some(message) = self.empty_launch_check(&loop_id, pool_id.as_deref())? {
            return Err(EmptySpecSetError(message).into());
        }

        // (B42) Claim the loop for this dispatch by flipping it to `Running`,
        // but ONLY if it isn't already `Running`. This is the single guarded
        // entry point every launch path — fresh `loop_run`, cron/watch
        // triggers, scheduled autorun, and `loop_continue`'s resume — funnels
        // through, so two dispatches racing to launch the same loop (the
        // autorun-vs-resume check-then-act race: one read the loop as `failed`,
        // the other hadn't written `running` yet) can't both proceed. The loser
        // of the atomic claim finds the loop already `Running` and returns a
        // silent no-op rather than starting a duplicate run that would
        // supersede the winner's in-flight node the moment it reached the same
        // node. It touches nothing (no status flip, no pool context, no
        // notification), leaving the loop exactly as the winning dispatch left it.
        if !self.db.claim_loop_for_run(&loop_id, chrono::Utc::now())? {
            tracing::info!(
                "Loop '{}' is already running; this launch is a duplicate and was refused \
                 (another dispatch owns the run).",
                loop_id
            );
            return Ok(());
        }
        // Persist which pool (if any) this run is drawing from *before* the
        // first spec executes, so an interruption (quota failure, daemon
        // crash) leaves behind the context every resume path needs — a
        // resumed run must never fall back to the loop's own (often empty)
        // bound specs. `None` for a bound-spec run, overwriting whatever a
        // previous run against this loop may have left behind.
        self.db
            .set_loop_active_run_pool(&loop_id, pool_id.as_deref())?;

        // The run's `workdir` param wins over `loop.workdir` — a pool run can
        // point the same loop's graph at a different checkout without
        // mutating the loop itself.
        let workdir = workdir_override.unwrap_or_else(|| lp.workdir.clone());

        // A single fire per dispatch: covers a fresh launch (manual
        // `loop_run`, a cron/watch trigger) and a resume (`loop_continue`,
        // autorun's auto-reset-and-resume) alike — every path that reaches
        // this function is a run actually starting to execute.
        let (done, total_specs) = self.spec_progress(&loop_id, pool_id.as_deref())?;
        // "Resumed" vs "Started": a resume of an in-flight run (autorun /
        // loop_continue), or any dispatch where prior specs already completed,
        // shouldn't read as the loop starting over from scratch. The first
        // spec this dispatch will work is surfaced so the toast says what's
        // next, not just a count.
        let resumed = is_resume || done > 0;
        let first_pending = self.first_pending_spec_name(&loop_id, pool_id.as_deref())?;
        self.notification_service.notify_loop_started(
            &lp.name,
            total_specs,
            resumed,
            first_pending.as_deref(),
        );

        // Specs this dispatch itself completes — never specs that were
        // already `completed`/`skipped` before this run started (those are
        // skipped below without ever reaching `run_spec`). Feeds
        // `{{completed_specs}}` in the `on_completed` hook's prompt (N2) —
        // see `render_completion_hook_prompt`.
        let mut completed_specs: Vec<(String, String)> = Vec::new();

        match &pool_id {
            Some(pool_id) => {
                // R3 (B18): a pool member can be left `running` with no live
                // node run behind it by a path G2 boot reconcile never
                // touches (reconcile only reconciles a loop that was itself
                // `Running` at boot — see `reconcile_orphaned_loops`). Surface
                // it here, before the live pick loop starts scanning, so an
                // operator can see it — but never auto-reset it: a spec can
                // legitimately sit `running` with no matching `loop_runs` row
                // for a moment (between two node executions), and this check
                // can't tell that race apart from a genuine crash-orphan.
                // Auto-resetting would risk yanking a spec out from under a
                // dispatch that's still actively working it. Recovery stays
                // the documented manual path: `loop_reset` (see
                // `pool_has_incomplete_members`'s own guard below, which
                // leaves the loop `running` rather than completing out from
                // under a member stuck like this).
                for spec_id in self
                    .db
                    .pool_stale_running_members(pool_id, crate::system::boot_id().as_deref())?
                {
                    tracing::warn!(
                        "Loop '{}' pool run against '{}': member spec '{}' is 'running' with no \
                         live node run in this daemon's lifetime; leaving it as-is. Reset it via \
                         loop_reset to resume if it's genuinely stuck.",
                        loop_id,
                        pool_id,
                        spec_id
                    );
                }
                // B35: When resuming (retry_current_node), re-dispatch the
                // spec that was already `running` before falling through to
                // the pending-picker. Without this, pool_next_pending_spec_id
                // skips the running spec (it only picks `pending`) and the
                // loop advances to the next queue member, stranding the
                // original spec in `running` with no active run.
                if is_resume {
                    if let Some(running_spec_id) = self.db.pool_running_spec_id(pool_id)? {
                        if let Some(spec) = self.db.get_loop_spec(&running_spec_id)? {
                            match self
                                .run_spec(&lp, &spec, &workdir, is_resume, Some(pool_id.as_str()))
                                .await?
                            {
                                SpecExecutionOutcome::Completed { summary } => {
                                    completed_specs.push((spec.name.clone(), summary));
                                }
                                // B42: a superseded run is silent — a newer
                                // dispatch now owns this loop, so stop without
                                // failing or completing anything.
                                SpecExecutionOutcome::Paused | SpecExecutionOutcome::Superseded => {
                                    return Ok(())
                                }
                                SpecExecutionOutcome::Failed(summary) => {
                                    self.fail_loop(&loop_id, Some(&spec.name), &summary)?;
                                    return Ok(());
                                }
                            }
                        }
                    }
                }
                loop {
                    if self.is_paused(&loop_id)? {
                        return Ok(());
                    }
                    // Live pick: fresh query, not a frozen list. Only ever
                    // returns a spec whose status is `pending` (defense in
                    // depth — even if the pool's stored order were ever
                    // corrupted to place a running/completed member where a
                    // pending one belongs, this filter still won't pick it).
                    let Some(spec_id) = self.db.pool_next_pending_spec_id(pool_id)? else {
                        break;
                    };
                    let Some(spec) = self.db.get_loop_spec(&spec_id)? else {
                        continue;
                    };

                    match self
                        .run_spec(&lp, &spec, &workdir, is_resume, Some(pool_id.as_str()))
                        .await?
                    {
                        SpecExecutionOutcome::Completed { summary } => {
                            completed_specs.push((spec.name.clone(), summary));
                            continue;
                        }
                        SpecExecutionOutcome::Paused | SpecExecutionOutcome::Superseded => {
                            return Ok(())
                        }
                        SpecExecutionOutcome::Failed(summary) => {
                            self.fail_loop(&loop_id, Some(&spec.name), &summary)?;
                            return Ok(());
                        }
                    }
                }
            }
            None => {
                for spec in self.db.list_loop_specs(&loop_id)? {
                    if self.is_paused(&loop_id)? {
                        return Ok(());
                    }
                    if matches!(
                        spec.status,
                        LoopSpecStatus::Completed | LoopSpecStatus::Skipped
                    ) {
                        continue;
                    }

                    match self.run_spec(&lp, &spec, &workdir, is_resume, None).await? {
                        SpecExecutionOutcome::Completed { summary } => {
                            completed_specs.push((spec.name.clone(), summary));
                            continue;
                        }
                        SpecExecutionOutcome::Paused | SpecExecutionOutcome::Superseded => {
                            return Ok(())
                        }
                        SpecExecutionOutcome::Failed(summary) => {
                            self.fail_loop(&loop_id, Some(&spec.name), &summary)?;
                            return Ok(());
                        }
                    }
                }
            }
        }

        // A pool run's live-pick loop above only ever breaks when no
        // `pending` member remains — but a member can still be stuck
        // `running`/`failed` from a prior interrupted run that was never
        // reset. That isn't a genuinely finished pool, so the loop must not
        // be marked `completed` out from under it (it would silently strand
        // those members forever, exactly the false-completion this guards
        // against).
        if let Some(pool_id) = &pool_id {
            if self.db.pool_has_incomplete_members(pool_id)? {
                tracing::warn!(
                    "Loop '{}' pool run against '{}' found no pending member to pick, but the \
                     pool still has incomplete (non completed/skipped) member(s); leaving the \
                     loop as-is rather than marking it completed. Reset the stuck member(s) via \
                     loop_reset to resume.",
                    loop_id,
                    pool_id
                );
                return Ok(());
            }
        }

        // The run is genuinely finished, but keep `active_run_pool_id` as
        // last-run context rather than clearing it (B31): a finished
        // pool-driven loop with no bound specs of its own would otherwise
        // lose the only link back to the queue it ran, so `loop list` /
        // `loop info` render a misleading `0/0` instead of its real `n/n`
        // (`loop_progress` in `daemon/loop_cli.rs` reads this field). B8's
        // anti-pollution guarantee is unaffected: every launch path
        // re-persists this field before the first spec runs (the
        // unconditional `set_loop_active_run_pool` above), so a later fresh
        // `loop_run` against a different pool — or a bound-spec run (`None`)
        // — overwrites this value rather than inheriting it.
        self.db.update_loop_status(
            &loop_id,
            LoopStatus::Completed,
            None,
            Some(chrono::Utc::now()),
        )?;
        let (done, total) = self.spec_progress(&loop_id, pool_id.as_deref())?;
        // (B17) This dispatch's own completed-spec count is what makes a
        // completion "real": a run that never actually executed a spec this
        // dispatch (every bound spec was already completed/skipped, or —
        // resuming a pool — the last pending member got skipped out from
        // under it) still legitimately transitions to `Completed`, but must
        // never fire `on_completed` for work it didn't do.
        let executed_any_spec = !completed_specs.is_empty();
        let hook_launched = executed_any_spec && lp.on_completed.is_some();
        self.notification_service.notify_loop_finished(
            &lp.name,
            LoopFinishOutcome::Completed {
                done,
                total,
                hook_launched,
            },
        );

        // N2: fire the loop's `on_completed` hook exactly once, right here —
        // the sole place a run transitions to `Completed`. Awaited (not
        // fire-and-forget) so its outcome is recorded before this dispatch
        // returns, but its own pass/fail never feeds back into `loop_id`'s
        // status above: the run is already finished.
        if executed_any_spec {
            self.fire_completion_hook(&lp, &workdir, &completed_specs)
                .await;
        }

        Ok(())
    }

    /// Fire `lp`'s `on_completed` hook (N2), if configured — a no-op
    /// otherwise. Runs through the same spawn path as a loop agent node
    /// ([`run_agent_process`]/[`spawn_and_wait_cli_process`]), records the
    /// firing in `loop_completion_hook_runs` (visible via `loop_get`/`canopy
    /// loop info`), and on failure logs a WARN plus a "post-completion hook
    /// failed" notification. Never returns an `Err` — a malformed hook
    /// config or a failed process must never propagate past the run that
    /// already finished successfully.
    async fn fire_completion_hook(
        &self,
        lp: &crate::domain::loops::Loop,
        workdir: &str,
        completed_specs: &[(String, String)],
    ) {
        let Some(hook) = lp.on_completed.as_ref() else {
            return;
        };

        // Recorded the moment the hook fires — even a platform that fails to
        // resolve below still shows up in `loop_get`/`canopy loop info` as a
        // failed firing, exactly like a spawn failure would, rather than
        // silently vanishing.
        let run_id = uuid::Uuid::new_v4().to_string();
        if let Err(error) =
            self.db
                .insert_loop_completion_hook_run(&crate::domain::loops::LoopCompletionHookRun {
                    id: run_id.clone(),
                    loop_id: lp.id.clone(),
                    status: LoopRunStatus::Running,
                    output: None,
                    summary: None,
                    started_at: chrono::Utc::now(),
                    completed_at: None,
                    pid: None,
                    boot_id: None,
                })
        {
            tracing::warn!(
                "Loop '{}' failed to record on_completed hook run: {:#}",
                lp.name,
                error
            );
        }

        let execution = match Cli::resolve(Some(&hook.platform)) {
            Ok(cli) => {
                let mut strategy = cli.strategy();
                let prompt =
                    render_completion_hook_prompt(lp, workdir, completed_specs, &hook.prompt);
                // Same E2BIG safety net as an agent node (see
                // `execute_agent_node`): an oversized `{{completed_specs}}`
                // list must not crash the spawn.
                if prompt.len() > ARGV_SAFETY_THRESHOLD && !strategy.prompt_via_stdin {
                    *strategy = strategy.with_stdin_forced();
                }
                let timeout_minutes = hook.timeout_minutes.unwrap_or(30);

                run_completion_hook_process(
                    &self.db,
                    &run_id,
                    &cli,
                    &strategy,
                    &prompt,
                    hook.model.as_deref(),
                    workdir,
                    timeout_minutes,
                )
                .await
            }
            Err(error) => HookExecution {
                status: LoopRunStatus::Fail,
                output: serde_json::json!({ "platform": hook.platform, "error": error }),
                summary: format!(
                    "on_completed hook has an invalid platform '{}': {error}",
                    hook.platform
                ),
            },
        };

        let _ = self.db.update_loop_completion_hook_run_result(
            &run_id,
            execution.status,
            Some(&execution.output),
            Some(&execution.summary),
            Some(chrono::Utc::now()),
        );

        if execution.status != LoopRunStatus::Pass {
            tracing::warn!(
                "Loop '{}' on_completed hook failed: {}",
                lp.name,
                execution.summary
            );
            self.notification_service
                .notify_loop_completion_hook_failed(&lp.name, &execution.summary);
        }
    }

    /// (B17) `Ok(Some(message))` if launching `loop_id` (optionally against
    /// `pool_id`) would find no effective spec to run — `message` is the
    /// actionable, human/LLM-readable error to surface. `Ok(None)` means the
    /// launch may proceed.
    ///
    /// Exposed (not just inlined in [`Self::run_loop_dispatch`]) so the
    /// synchronous `loop_run` MCP handler can hand this straight back to its
    /// caller instead of the caller only finding out via a log line once the
    /// fire-and-forget background dispatch fails — every other launch path
    /// (autorun, cron/watch triggers, `loop_continue`) still gets the same
    /// check from `run_loop_dispatch` itself.
    ///
    /// Emptiness is defined per launch mode:
    /// - Bound specs (`pool_id` is `None`): the loop has *zero* specs bound
    ///   to it at all — mirrors the incident exactly (a loop whose specs all
    ///   live in a pool has no bound specs). Deliberately not "every bound
    ///   spec is already completed/skipped" — a loop's own bound specs
    ///   belong to it 1:1, so if they're all done the loop genuinely is
    ///   finished (see the zero-execution completion path in
    ///   `run_loop_dispatch`, which still completes but never fires the
    ///   hook).
    /// - A pool (`pool_id` is `Some`): the pool has no `pending` member *and*
    ///   no other non-terminal (`running`/`failed`) member left either — i.e.
    ///   [`Database::pool_has_incomplete_members`] is false. Unlike bound
    ///   specs, a pool is a shared queue another loop or a stale relaunch can
    ///   easily point at by mistake, so "every member already done" is
    ///   treated as an error here rather than a silent, do-nothing
    ///   completion (regression (b): a pool run where every member is
    ///   already completed).
    pub fn empty_launch_check(
        &self,
        loop_id: &str,
        pool_id: Option<&str>,
    ) -> Result<Option<String>> {
        let Some(lp) = self.db.get_loop(loop_id)? else {
            // Not-found is handled by the caller (`run_loop_dispatch` bails
            // on it above this check runs; the MCP handler checks it before
            // calling this at all) — nothing to report here.
            return Ok(None);
        };

        let is_empty = match pool_id {
            Some(pool_id) => !self.db.pool_has_incomplete_members(pool_id)?,
            None => self.db.list_loop_specs(loop_id)?.is_empty(),
        };
        if !is_empty {
            return Ok(None);
        }

        Ok(Some(self.empty_spec_set_message(&lp, pool_id)?))
    }

    /// Build the actionable error text for [`Self::empty_launch_check`].
    ///
    /// Pool membership doesn't record which loop(s) normally draw from it
    /// (pool specs stay standalone — see [`Self::run_loop`]'s doc), so the
    /// one concrete, discoverable link back to "which pool should this loop
    /// use?" is the loop's own [`crate::domain::loops::Loop::active_run_pool_id`]
    /// — the pool its last real run drew from. This is exactly requirement 3's
    /// guard rail: a pool-less relaunch of a loop that was last pool-driven
    /// names that pool so a recovery agent can retry correctly instead of
    /// the launch silently discarding the pool context.
    fn empty_spec_set_message(
        &self,
        lp: &crate::domain::loops::Loop,
        pool_id: Option<&str>,
    ) -> Result<String> {
        match pool_id {
            Some(pool_id) => {
                let total = self.db.list_pool_member_spec_ids(pool_id)?.len();
                Ok(format!(
                    "Loop '{}' has no specs to run: queue '{}' has {} member(s), none pending \
                     (all already completed/skipped, or the queue is empty). Add pending specs \
                     to the queue, or pass a different queue_id.",
                    lp.name, pool_id, total
                ))
            }
            None => {
                let mut message = format!(
                    "Loop '{}' has no specs to run: it has 0 bound specs and no queue_id was \
                     given.",
                    lp.name
                );
                match &lp.active_run_pool_id {
                    Some(last_pool) => {
                        message.push_str(&format!(
                            " Its last run drew from queue '{last_pool}' — pass queue_id: \
                             \"{last_pool}\" to relaunch against it."
                        ));
                    }
                    None => {
                        message.push_str(" Pass queue_id to run it against a queue instead.");
                    }
                }
                Ok(message)
            }
        }
    }

    /// Resume `loop_id` in the background using whatever run context (pool
    /// or bound-spec) it last persisted via [`Database::set_loop_active_run_pool`].
    /// The one path every "continue where this loop left off" entry point —
    /// the scheduler's autorun auto-reset-and-resume, `loop_continue` — must
    /// go through, so a pool run is never silently swapped for the loop's own
    /// (typically empty) bound specs.
    ///
    /// This is the *only* entry point allowed to carry `is_resume = true`
    /// into [`Self::run_loop_dispatch`] — see that function's doc for why the
    /// distinction matters for `{{spec_start_head}}` (B10). A loop relaunched
    /// via `loop_run` directly (even a `paused` one) goes through
    /// [`Self::start_background_run`] instead and always gets a fresh
    /// baseline.
    pub fn resume_background(self: Arc<Self>, loop_id: String) {
        let pool_id = self
            .db
            .get_loop(&loop_id)
            .ok()
            .flatten()
            .and_then(|lp| lp.active_run_pool_id);
        tokio::spawn(async move {
            if let Err(error) = self
                .run_loop_dispatch(loop_id.clone(), pool_id, None, true)
                .await
            {
                if error.downcast_ref::<EmptySpecSetError>().is_some() {
                    tracing::error!("Loop '{}' launch refused: {error:#}", loop_id);
                } else {
                    tracing::error!("Loop '{}' failed to run: {error:#}", loop_id);
                    let _ = self.fail_loop(&loop_id, None, &error.to_string());
                }
            }
        });
    }

    /// Run one spec's node graph to completion, pause, or failure.
    ///
    /// `is_resume` (see [`Self::run_loop_dispatch`]) governs whether
    /// `{{spec_start_head}}` may be inherited from a value this spec already
    /// persisted (only valid when this call is genuinely continuing the same
    /// in-flight attempt) or must be captured fresh (every other case,
    /// including a spec that is stuck `running` from an unrelated, never-reset
    /// prior attempt).
    async fn run_spec(
        &self,
        lp: &crate::domain::loops::Loop,
        spec: &LoopSpec,
        workdir: &str,
        is_resume: bool,
        pool_id: Option<&str>,
    ) -> Result<SpecExecutionOutcome> {
        let spec_details = self
            .db
            .get_loop_spec_details(&spec.id)?
            .ok_or_else(|| anyhow!("Loop spec '{}' not found.", spec.id))?;
        let graph_nodes = self.db.list_loop_nodes_for_loop(&lp.id)?;
        let graph_edges = self.db.list_loop_edges_for_loop(&lp.id)?;

        // A spec with its own graph always uses it (full backwards
        // compatibility). Only a spec with no nodes of its own falls back to
        // the loop-level graph, so the same graph can drive every spec in
        // the loop without repeating it per spec. Ensembles (F1) follow the
        // exact same precedence — a spec-level ensemble only exists when the
        // spec has its own graph, so it's fetched alongside it.
        let (nodes, edges, ensembles): (&[LoopNode], &[LoopEdge], Vec<EnsembleDetails>) =
            if !spec_details.nodes.is_empty() {
                let ensembles = self.db.list_ensembles_for_spec(&spec.id)?;
                (&spec_details.nodes, &spec_details.edges, ensembles)
            } else if !graph_nodes.is_empty() {
                let ensembles = self.db.list_ensembles_for_loop(&lp.id)?;
                (&graph_nodes, &graph_edges, ensembles)
            } else {
                let summary = format!(
                "Spec '{}' has no nodes of its own and loop '{}' has no loop-level graph to fall back to.",
                spec.name, lp.name
            );
                self.db.update_loop_spec_status(
                    &spec.id,
                    LoopSpecStatus::Failed,
                    Some(chrono::Utc::now()),
                    Some(chrono::Utc::now()),
                )?;
                return Ok(SpecExecutionOutcome::Failed(summary));
            };

        let nodes_by_id = nodes
            .iter()
            .map(|node| (node.id.as_str(), node))
            .collect::<HashMap<_, _>>();
        // B37: whether this graph designates a committer at all. Resolved
        // once from whichever graph won the precedence above, so a spec-level
        // graph and the loop-level fallback each answer for themselves.
        let enforce_commit_rights = graph_enforces_commit_rights(nodes);
        let existing_runs = self.db.list_loop_runs_for_spec(&spec.id)?;
        let (mut cursor, mut previous_output, mut iterations) =
            resolve_spec_start(nodes, edges, spec, &existing_runs, &ensembles)?;

        // Capture the workdir's git HEAD once, at the moment the engine
        // starts executing this spec in the *current run attempt* — never
        // re-resolved at node-exec time (B10). The persisted value is only
        // ever reused, never re-derived, and only when both of these hold:
        //
        // - `is_resume` — this call is genuinely continuing the same
        //   in-flight attempt (daemon restart mid-node-graph, explicit
        //   `loop_pause`/`loop_continue`), not a fresh dispatch. Only
        //   `resume_background` sets this; `start_background_run`/`loop_run`
        //   — including relaunching a `paused` loop directly — never do, so
        //   a relaunch always re-captures even if it finds a spec still
        //   marked `running` from a stale, never-reset earlier attempt. That
        //   stale-`running` case is exactly the 2026-07-11 incident: a
        //   spec's baseline from a launch two relaunches earlier kept getting
        //   silently reused because status alone couldn't distinguish "same
        //   attempt, paused" from "different, abandoned attempt".
        // - `spec.status == Running` — this spec itself has already started
        //   (as opposed to a pending/failed spec a resumed pool/loop run is
        //   only now reaching for the first time, which must capture fresh
        //   like any other new entry).
        //
        // Whenever a fresh capture happens, it happens strictly before any
        // node of this attempt executes (right here, before the node loop
        // below and before `spec.status` is even flipped to `running`), so
        // it can never observe a commit this attempt's own agent node is
        // about to make — only commits that landed before this attempt
        // started (e.g. a prior spec's work, or a concurrent spec sharing
        // this workdir) are visible in it. Once captured, the value is fixed
        // for every node execution and every review/check retry of this
        // attempt, amend or no amend — it is never touched again until the
        // next spec attempt captures its own.
        let spec_start_head = if is_resume && spec.status == LoopSpecStatus::Running {
            spec_details.spec.spec_start_head.clone()
        } else {
            let head = capture_workdir_head(workdir).await;
            self.db
                .set_loop_spec_start_head(&spec.id, head.as_deref())?;
            head
        };

        self.db.update_loop_spec_status(
            &spec.id,
            LoopSpecStatus::Running,
            Some(chrono::Utc::now()),
            None,
        )?;

        // RS2: session ids captured for each node during THIS dispatch, so a
        // fail-edge bounce back to a node can resume its prior session instead
        // of cold-starting. In-memory only and local to this call — a restart,
        // reset, or fresh dispatch starts with an empty map and therefore cold,
        // which is exactly the freshness guarantee we want. Keyed by node_id;
        // being local to one spec's run also means a session never leaks across
        // specs.
        let mut resumable_sessions: HashMap<String, String> = HashMap::new();

        // RS3: the context group this spec belongs to within the running
        // queue, if any. Only pool/queue runs carry a group (a loop's own
        // bound specs never do — `pool_id` is `None` there), so ungrouped and
        // non-queue specs never cross-resume. This is the ONE deliberate
        // exception to RS2's "first visit is cold" rule: the first visit of a
        // grouped spec to a node resumes the session captured by the previous
        // successfully-completed grouped sibling on that same node (see the
        // seed below and [`Database::group_session_for_node`]).
        let spec_group = match pool_id {
            Some(pid) => self.db.pool_member_group(pid, &spec.id)?,
            None => None,
        };

        loop {
            if self.is_paused(&lp.id)? {
                return Ok(SpecExecutionOutcome::Paused);
            }

            let budget_key = match &cursor {
                SpecCursor::Node(node_id) => node_id.clone(),
                SpecCursor::Ensemble(ensemble_id) => format!("ensemble:{ensemble_id}"),
            };

            // Reap a `running` row left by a prior attempt at this exact
            // node/ensemble that was never finalized (crashed mid-execution,
            // daemon restarted before its own timeout handler ran, etc.) —
            // B12. Execution here is strictly sequential (one cursor step in
            // flight per spec at a time — an `Ensemble` step's own members
            // run concurrently with each other, but never alongside another
            // step), so anything still `running` at this point can only be a
            // leftover, never the legitimately active run: this iteration's
            // own rows don't exist yet.
            for node_id in cursor_node_ids(&cursor, &ensembles) {
                if let Some(stale) = self.db.get_active_loop_run_for_node(&node_id)? {
                    self.terminate_run(&stale, SUPERSEDE_REASON);
                }
            }

            let iteration = iterations.entry(budget_key).or_insert(0);
            *iteration += 1;
            if *iteration > DEFAULT_MAX_ITERATIONS_PER_NODE {
                // B12: the process from the last execution at this node
                // (or any other node still running) must not survive the
                // spec failure — otherwise it burns quota, holds locks,
                // and could call loop_complete_node late with a stale
                // report. The process from the *previous* iteration is
                // the most likely survivor: the budget check fires before
                // any new execution starts, so the in-flight child is
                // always from a prior run at this node.
                for run in self.db.list_running_loop_runs(&lp.id).unwrap_or_default() {
                    self.terminate_run(&run, "iteration budget exhausted");
                }
                let summary = format!(
                    "Spec '{}' exceeded max iterations for {}.",
                    spec.name,
                    cursor_label(&cursor, &ensembles)
                );
                self.db.update_loop_spec_status(
                    &spec.id,
                    LoopSpecStatus::Failed,
                    None,
                    Some(chrono::Utc::now()),
                )?;
                return Ok(SpecExecutionOutcome::Failed(summary));
            }
            let iteration_value = *iteration;

            let (final_execution, from_node_id, run_id) = match &cursor {
                SpecCursor::Node(node_id) => {
                    let node = nodes_by_id
                        .get(node_id.as_str())
                        .ok_or_else(|| anyhow!("Loop node '{}' not found.", node_id))?;
                    let (retry_limit, crash_max_secs, backoff_secs) = read_infra_config(node);
                    let mut attempt: u32 = 0;
                    // RS2: resume candidate for this attempt. On the first
                    // visit to a node the map is empty → `None` → cold start.
                    // On a fail-edge bounce it holds the session captured on the
                    // node's previous run in this dispatch → resume.
                    let mut resume_candidate = resumable_sessions.get(node_id.as_str()).cloned();

                    // RS3 group-session handoff: a grouped spec's FIRST visit to
                    // this node (nothing yet in `resumable_sessions` for it)
                    // resumes the group's live session for this node — the
                    // session captured by the previous successfully-completed
                    // grouped sibling on the same node. Derived from the DB so a
                    // daemon restart mid-queue keeps group context. Taint is
                    // enforced inside `group_session_for_node`: a failed nearest
                    // sibling yields `None` here, so this spec cold-starts and
                    // its fresh session becomes the group's new session. Bounces
                    // (map already populated) keep RS2's in-dispatch session and
                    // never re-consult the group.
                    if resume_candidate.is_none() {
                        if let (Some(group), Some(pid)) = (spec_group.as_deref(), pool_id) {
                            resume_candidate = self.db.group_session_for_node(
                                pid,
                                group,
                                &spec.id,
                                node_id.as_str(),
                            )?;
                        }
                    }
                    let mut run_id = uuid::Uuid::new_v4().to_string();
                    self.db.insert_loop_run(&LoopNodeRun {
                        id: run_id.clone(),
                        loop_id: lp.id.clone(),
                        spec_id: spec.id.clone(),
                        node_id: node.id.clone(),
                        status: LoopRunStatus::Running,
                        input: previous_output.clone(),
                        output: None,
                        started_at: chrono::Utc::now(),
                        completed_at: None,
                        iteration: iteration_value as i64,
                        pid: None,
                        boot_id: crate::system::boot_id(),
                        session_id: None,
                    })?;

                    {
                        let node_platform = node
                            .config
                            .get("platform")
                            .or_else(|| node.config.get("cli"))
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|v| !v.is_empty());
                        let node_model = node.config.get("model").and_then(Value::as_str);
                        tracing::info!(
                            loop_id = %lp.id,
                            spec_id = %spec.id,
                            node_id = %node.id,
                            node = %node.name,
                            run_id = %run_id,
                            platform = node_platform.unwrap_or(""),
                            model = node_model.unwrap_or(""),
                            "node run launched"
                        );
                    }

                    // B37: baseline HEAD for this node's whole visit, infra
                    // retries included — a retry that commits is as much a
                    // violation as a first attempt that does.
                    let commit_watch = CommitRightsWatch::begin(
                        enforce_commit_rights,
                        node_has_commit_rights(node),
                        workdir,
                    )
                    .await;

                    let (final_execution, run) = loop {
                        let execution = self
                            .execute_node(
                                lp,
                                spec,
                                node,
                                previous_output.as_ref(),
                                spec_start_head.as_deref(),
                                &run_id,
                                workdir,
                                resume_candidate.as_deref(),
                            )
                            .await?;
                        let run = self.db.get_loop_run(&run_id)?.ok_or_else(|| {
                            anyhow!("Loop run '{}' not found after execution.", run_id)
                        })?;

                        if is_infra_crash(
                            node,
                            &execution,
                            &run,
                            attempt,
                            retry_limit,
                            crash_max_secs,
                        ) {
                            // B19: retry resuming the crashed attempt's own
                            // session if it managed to create one before dying;
                            // an infra crash at spawn usually created none, so
                            // this is normally `None` → the retry cold-starts.
                            resume_candidate = run.session_id.clone();
                            run_id = begin_infra_retry(
                                &self.db,
                                lp,
                                spec,
                                node,
                                previous_output.as_ref(),
                                iteration_value as i64,
                                &run_id,
                                &execution.output,
                                attempt,
                                backoff_secs,
                            )
                            .await?;
                            attempt += 1;
                            continue;
                        }

                        break (execution, run);
                    };

                    tracing::info!(
                        run_id = %run_id,
                        status = ?run.status,
                        node = %node.name,
                        "node run completed"
                    );

                    // B42: a newer attempt at this node terminated this run out
                    // from under us (see `terminate_run`/`SUPERSEDE_REASON`).
                    // That is engine bookkeeping — the run's `Fail` row is a
                    // reclaim, not a node failure — so this dispatch stops here:
                    // it evaluates NO edge (never the fail edge to a resilience
                    // node), fails nothing, and leaves the loop to whichever
                    // dispatch now owns it. Checked before any routing so the
                    // supersede can never be routed as a fail (the runaway that
                    // manufactured a resilience run per killed implementer).
                    if run_was_superseded(&run) {
                        return Ok(SpecExecutionOutcome::Superseded);
                    }

                    // RS2: remember this node's captured session so a later
                    // fail-edge bounce back to it resumes instead of cold
                    // starting. A resumed run recorded the same id it continued;
                    // a cold run recorded whatever it captured (or nothing).
                    if let Some(sid) = run.session_id.clone() {
                        resumable_sessions.insert(node.id.clone(), sid);
                    }

                    let final_execution = if run.status == LoopRunStatus::Running {
                        self.db.update_loop_run_result(
                            &run_id,
                            final_execution.status,
                            Some(&final_execution.output),
                            Some(chrono::Utc::now()),
                        )?;
                        final_execution
                    } else {
                        NodeExecution {
                            status: run.status,
                            output: run.output.unwrap_or_else(|| serde_json::json!({})),
                            summary: final_execution.summary,
                        }
                    };

                    // B37: applied AFTER the node's own verdict is settled,
                    // so it overrides every way a node can report success —
                    // a clean exit code, or a `loop_complete_node` self-report
                    // of `pass`. A node that moved history without the right
                    // to fails, and the fail is persisted on the run row so
                    // `canopy loop info` shows it.
                    let final_execution = match &commit_watch {
                        Some(watch) => match watch.violation(workdir).await {
                            Some(head_after) => {
                                let violation = commit_rights_failure(
                                    &format!("Node '{}'", node.name),
                                    &node.id,
                                    &watch.head_before,
                                    &head_after,
                                    final_execution.output,
                                );
                                tracing::warn!(
                                    node = %node.name,
                                    head_before = %watch.head_before,
                                    head_after = %head_after,
                                    "node committed but has no commit rights"
                                );
                                self.db.update_loop_run_result(
                                    &run_id,
                                    LoopRunStatus::Fail,
                                    Some(&violation.output),
                                    Some(chrono::Utc::now()),
                                )?;
                                violation
                            }
                            None => final_execution,
                        },
                        None => final_execution,
                    };

                    if self.is_paused(&lp.id)? {
                        return Ok(SpecExecutionOutcome::Paused);
                    }

                    if should_advance_to_next_spec(node, final_execution.status) {
                        self.db.update_loop_spec_status(
                            &spec.id,
                            LoopSpecStatus::Completed,
                            None,
                            Some(chrono::Utc::now()),
                        )?;
                        self.notify_spec_completed(lp, spec, pool_id)?;
                        return Ok(SpecExecutionOutcome::Completed {
                            summary: final_execution.summary,
                        });
                    }

                    (final_execution, node.id.clone(), Some(run_id))
                }
                SpecCursor::Ensemble(ensemble_id) => {
                    let details = ensembles
                        .iter()
                        .find(|details| &details.ensemble.id == ensemble_id)
                        .ok_or_else(|| anyhow!("Ensemble '{}' not found in graph.", ensemble_id))?;
                    let final_execution = self
                        .execute_ensemble(
                            lp,
                            spec,
                            details,
                            &nodes_by_id,
                            previous_output.as_ref(),
                            iteration_value,
                            workdir,
                            enforce_commit_rights,
                        )
                        .await?;

                    if self.is_paused(&lp.id)? {
                        return Ok(SpecExecutionOutcome::Paused);
                    }

                    (final_execution, details.ensemble.join_node_id.clone(), None)
                }
            };

            let step_selection =
                select_next_step(edges, &ensembles, &from_node_id, final_execution.status)?;

            let run_id_field = run_id.as_deref().unwrap_or("");
            match &step_selection {
                Some(sel) => match &sel.cursor {
                    SpecCursor::Node(target) => {
                        tracing::info!(
                            run_id = run_id_field,
                            from_node = %from_node_id,
                            to_node = %target,
                            status = ?final_execution.status,
                            edge_condition = sel.edge_condition.as_str(),
                            "edge traversed"
                        );
                    }
                    SpecCursor::Ensemble(eid) => {
                        tracing::info!(
                            run_id = run_id_field,
                            from_node = %from_node_id,
                            to_ensemble = %eid,
                            status = ?final_execution.status,
                            edge_condition = sel.edge_condition.as_str(),
                            "edge traversed to ensemble"
                        );
                    }
                },
                None => {
                    tracing::info!(
                        run_id = run_id_field,
                        from_node = %from_node_id,
                        status = ?final_execution.status,
                        "no outgoing edge matched; spec terminating"
                    );
                }
            }

            let next_step = step_selection.map(|sel| sel.cursor);

            match next_step {
                Some(step) => {
                    previous_output = Some(final_execution.output);
                    cursor = step;
                }
                None if final_execution.status == LoopRunStatus::Pass => {
                    self.db.update_loop_spec_status(
                        &spec.id,
                        LoopSpecStatus::Completed,
                        None,
                        Some(chrono::Utc::now()),
                    )?;
                    self.notify_spec_completed(lp, spec, pool_id)?;
                    return Ok(SpecExecutionOutcome::Completed {
                        summary: final_execution.summary,
                    });
                }
                None => {
                    self.db.update_loop_spec_status(
                        &spec.id,
                        LoopSpecStatus::Failed,
                        None,
                        Some(chrono::Utc::now()),
                    )?;
                    return Ok(SpecExecutionOutcome::Failed(final_execution.summary));
                }
            }
        }
    }

    /// Run an ensemble's members concurrently (F1), wait for every one of
    /// them to terminate (pass, fail, or straggler timeout — never early),
    /// and consolidate their outputs into the join's own [`NodeExecution`].
    ///
    /// Every member receives the exact same `previous_output` (the same
    /// input, in parallel — the defining shape of an ensemble). Concurrency
    /// is bounded by `self.ensemble_concurrency`, a semaphore shared across
    /// every loop this engine drives, so an 8-member ensemble queues past the
    /// cap rather than spawning all 8 processes at once.
    #[allow(clippy::too_many_arguments)]
    async fn execute_ensemble(
        &self,
        lp: &crate::domain::loops::Loop,
        spec: &LoopSpec,
        details: &EnsembleDetails,
        nodes_by_id: &HashMap<&str, &LoopNode>,
        previous_output: Option<&Value>,
        iteration: usize,
        workdir: &str,
        enforce_commit_rights: bool,
    ) -> Result<NodeExecution> {
        let ensemble = &details.ensemble;
        // 0 is a legitimate value (mirrors `run_agent_process`'s own
        // `timeout_minutes`) — minute-granular timeouts otherwise have no way
        // to force an immediate one in a fast test.
        let straggler_minutes = ensemble.effective_straggler_timeout_minutes().max(0) as u64;

        // B37: members run concurrently against one workdir, so a moved HEAD
        // cannot be attributed to a single member — enforcement is therefore
        // at ensemble granularity, and the quorum fails as a whole. Skipped
        // if any member is itself a designated committer.
        let any_member_may_commit = details.members.iter().any(|member| {
            nodes_by_id
                .get(member.node_id.as_str())
                .is_some_and(|node| node_has_commit_rights(node))
        });
        let commit_watch =
            CommitRightsWatch::begin(enforce_commit_rights, any_member_may_commit, workdir).await;

        let mut set = tokio::task::JoinSet::new();
        for member in &details.members {
            let node = (*nodes_by_id
                .get(member.node_id.as_str())
                .ok_or_else(|| anyhow!("Ensemble member node '{}' not found.", member.node_id))?)
            .clone();
            let run_id = uuid::Uuid::new_v4().to_string();
            self.db.insert_loop_run(&LoopNodeRun {
                id: run_id.clone(),
                loop_id: lp.id.clone(),
                spec_id: spec.id.clone(),
                node_id: node.id.clone(),
                status: LoopRunStatus::Running,
                input: previous_output.cloned(),
                output: None,
                started_at: chrono::Utc::now(),
                completed_at: None,
                iteration: iteration as i64,
                pid: None,
                boot_id: crate::system::boot_id(),
                session_id: None,
            })?;

            {
                let member_platform = member.platform.as_str();
                let member_model = member.model.as_deref().unwrap_or("");
                tracing::info!(
                    loop_id = %lp.id,
                    spec_id = %spec.id,
                    node_id = %node.id,
                    node = %node.name,
                    run_id = %run_id,
                    platform = %member_platform,
                    model = %member_model,
                    ensemble_id = %ensemble.id,
                    "ensemble member run launched"
                );
            }

            let db = Arc::clone(&self.db);
            let lp = lp.clone();
            let spec = spec.clone();
            let previous_output = previous_output.cloned();
            let workdir = workdir.to_string();
            let semaphore = Arc::clone(&self.ensemble_concurrency);
            let label = member_label(member);
            let ensemble_id = ensemble.id.clone();

            set.spawn(async move {
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .expect("ensemble concurrency semaphore is never closed");
                // B26: give the member the same B19 infra-crash retry as a
                // lone agent node — a quick, non-self-reported crash retries
                // the SAME member in place (doubling backoff, fresh
                // marker-carrying run rows) up to the limit, without touching
                // join/quorum semantics. The whole retry sequence runs inside
                // the ONE straggler timeout below, so a member still crashing
                // and backing off when the straggler window expires is counted
                // as failed deterministically (never silently abandoned) and
                // whatever attempt is live is killed. Members cold-start on
                // their first attempt (RS2 is scoped to the sequential bounce
                // path); a crashed attempt that captured a session is resumed
                // on retry, exactly like B19+RS2 for a lone node.
                let outcome = tokio::time::timeout(
                    std::time::Duration::from_secs(straggler_minutes * 60),
                    async {
                        let (retry_limit, crash_max_secs, backoff_secs) = read_infra_config(&node);
                        let mut attempt: u32 = 0;
                        let mut member_run_id = run_id.clone();
                        let mut resume_candidate: Option<String> = None;
                        loop {
                            let execution = execute_agent_node(
                                &db,
                                &lp,
                                &spec,
                                &node,
                                previous_output.as_ref(),
                                &member_run_id,
                                &workdir,
                                resume_candidate.as_deref(),
                            )
                            .await?;
                            let run = db.get_loop_run(&member_run_id)?.ok_or_else(|| {
                                anyhow!("Loop run '{}' not found after execution.", member_run_id)
                            })?;
                            if is_infra_crash(
                                &node,
                                &execution,
                                &run,
                                attempt,
                                retry_limit,
                                crash_max_secs,
                            ) {
                                resume_candidate = run.session_id.clone();
                                member_run_id = begin_infra_retry(
                                    &db,
                                    &lp,
                                    &spec,
                                    &node,
                                    previous_output.as_ref(),
                                    iteration as i64,
                                    &member_run_id,
                                    &execution.output,
                                    attempt,
                                    backoff_secs,
                                )
                                .await?;
                                attempt += 1;
                                continue;
                            }
                            break Ok::<_, anyhow::Error>((execution, run, member_run_id.clone()));
                        }
                    },
                )
                .await;

                let execution = match outcome {
                    Ok(Ok((execution, run, final_run_id))) => {
                        tracing::info!(
                            run_id = %final_run_id,
                            status = ?execution.status,
                            node = %node.name,
                            ensemble_id = %ensemble_id,
                            "ensemble member run completed"
                        );
                        if run.status == LoopRunStatus::Running {
                            let _ = db.update_loop_run_result(
                                &final_run_id,
                                execution.status,
                                Some(&execution.output),
                                Some(chrono::Utc::now()),
                            );
                            execution
                        } else {
                            NodeExecution {
                                status: run.status,
                                output: run.output.unwrap_or_else(|| serde_json::json!({})),
                                summary: execution.summary,
                            }
                        }
                    }
                    // A DB error (or other hard error) from within the retry
                    // loop — reuse the finalized row output if there is one.
                    Ok(Err(error)) => {
                        let output = db
                            .get_active_loop_run_for_node(&node.id)
                            .ok()
                            .flatten()
                            .and_then(|run| run.output)
                            .unwrap_or_else(|| serde_json::json!({ "error": error.to_string() }));
                        NodeExecution {
                            status: LoopRunStatus::Fail,
                            output,
                            summary: format!("Ensemble member '{}' failed: {error}", node.name),
                        }
                    }
                    // This ensemble's own straggler timeout elapsed before the
                    // member resolved (still executing, or still retrying/
                    // backing off). Kill whichever attempt is live now (B12) —
                    // located by node id, since retries advance the run id —
                    // and count the member as failed deterministically. A
                    // member caught mid-backoff has no live run and is simply
                    // recorded as failed.
                    Err(_elapsed) => {
                        if let Ok(Some(run)) = db.get_active_loop_run_for_node(&node.id) {
                            terminate_run_row(&db, &run, "ensemble straggler timeout");
                        }
                        NodeExecution {
                            status: LoopRunStatus::Fail,
                            output: serde_json::json!({
                                "kind": "agent",
                                "node_id": node.id,
                                "error": "straggler timeout",
                                "straggler_timeout_minutes": straggler_minutes,
                            }),
                            summary: format!(
                                "Ensemble member '{}' killed: straggler timeout after {straggler_minutes}m.",
                                node.name
                            ),
                        }
                    }
                };
                (node.id, label, execution)
            });
        }

        // Wait-all (F1): drain every task before consolidating, regardless
        // of arrival order, so the join can never fire while a member is
        // still in flight.
        let mut results: HashMap<String, (String, NodeExecution)> = HashMap::new();
        while let Some(joined) = set.join_next().await {
            let (node_id, label, execution) =
                joined.map_err(|error| anyhow!("Ensemble member task panicked: {error}"))?;
            results.insert(node_id, (label, execution));
        }

        let mut passed = 0i64;
        let mut consolidated_doc = String::new();
        let mut member_summaries = Vec::with_capacity(details.members.len());
        for member in &details.members {
            let (label, execution) = results.remove(&member.node_id).ok_or_else(|| {
                anyhow!(
                    "Ensemble member '{}' produced no result after wait-all.",
                    member.node_id
                )
            })?;
            let status_label = if execution.status == LoopRunStatus::Pass {
                passed += 1;
                "pass"
            } else {
                "fail"
            };
            consolidated_doc.push_str(&format!(
                "## {label} [{status_label}]\n\n{}\n\n",
                member_output_text(&execution.output)
            ));
            member_summaries.push(serde_json::json!({
                "node_id": member.node_id,
                "platform": member.platform,
                "model": member.model,
                "status": status_label,
                "output": execution.output,
            }));
        }

        let join_status = if passed >= ensemble.min_pass {
            LoopRunStatus::Pass
        } else {
            LoopRunStatus::Fail
        };
        let join_output = serde_json::json!({
            "kind": "quorum",
            "ensemble_id": ensemble.id,
            "members": member_summaries,
            "passed": passed,
            "min_pass": ensemble.min_pass,
            "consolidated_doc": consolidated_doc,
        });

        let mut execution = NodeExecution {
            status: join_status,
            output: join_output,
            summary: format!(
                "Ensemble '{}' {} ({}/{} passed).",
                ensemble.name,
                if join_status == LoopRunStatus::Pass {
                    "passed"
                } else {
                    "failed"
                },
                passed,
                details.members.len(),
            ),
        };

        // B37: a quorum that moved HEAD fails regardless of how its members
        // voted — the work already landed, so whatever the members reviewed
        // is no longer the diff under review.
        if let Some(watch) = &commit_watch {
            if let Some(head_after) = watch.violation(workdir).await {
                tracing::warn!(
                    ensemble = %ensemble.name,
                    head_before = %watch.head_before,
                    head_after = %head_after,
                    "ensemble member committed but has no commit rights"
                );
                execution = commit_rights_failure(
                    &format!("Ensemble '{}' (one of its members)", ensemble.name),
                    &ensemble.join_node_id,
                    &watch.head_before,
                    &head_after,
                    execution.output,
                );
            }
        }

        self.db.insert_loop_run(&LoopNodeRun {
            id: uuid::Uuid::new_v4().to_string(),
            loop_id: lp.id.clone(),
            spec_id: spec.id.clone(),
            node_id: ensemble.join_node_id.clone(),
            status: execution.status,
            input: previous_output.cloned(),
            output: Some(execution.output.clone()),
            started_at: chrono::Utc::now(),
            completed_at: Some(chrono::Utc::now()),
            iteration: iteration as i64,
            pid: None,
            boot_id: crate::system::boot_id(),
            session_id: None,
        })?;

        Ok(execution)
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    async fn execute_node(
        &self,
        lp: &crate::domain::loops::Loop,
        spec: &LoopSpec,
        node: &LoopNode,
        previous_output: Option<&Value>,
        spec_start_head: Option<&str>,
        run_id: &str,
        workdir: &str,
        resume_session_id: Option<&str>,
    ) -> Result<NodeExecution> {
        match node.kind {
            LoopNodeKind::Check => {
                execute_check_node(&self.db, run_id, lp, spec, node, spec_start_head, workdir).await
            }
            LoopNodeKind::Gate => execute_gate_node(node, previous_output),
            LoopNodeKind::Agent => {
                execute_agent_node(
                    &self.db,
                    lp,
                    spec,
                    node,
                    previous_output,
                    run_id,
                    workdir,
                    resume_session_id,
                )
                .await
            }
            // A quorum node (F1) never reaches the single-node path: `run_spec`
            // detects the fan-out into its ensemble before this would ever be
            // called and runs `execute_ensemble` instead. This arm exists
            // only so the match stays exhaustive against future callers.
            LoopNodeKind::Join => bail!(
                "Quorum node '{}' cannot execute directly; it only runs as part of ensemble fan-out.",
                node.name
            ),
        }
    }

    /// Best-effort termination (B12) of `run`'s OS process, if it still has
    /// one recorded, and finalization of its DB row as `Fail` so it stops
    /// showing up as `running`. Every abnormal end that abandons a node run
    /// without letting it finish on its own — a stale row from a crashed
    /// prior attempt, `loop_pause`, `loop_reset` of a running spec, or this
    /// run failing elsewhere — goes through here. A no-op beyond the
    /// status/summary update if `run` never got a pid recorded (e.g. a gate
    /// node, or an agent/check node that hadn't finished spawning yet).
    fn terminate_run(&self, run: &LoopNodeRun, reason: &str) {
        terminate_run_row(&self.db, run, reason);
    }

    fn is_paused(&self, loop_id: &str) -> Result<bool> {
        Ok(self
            .db
            .get_loop(loop_id)?
            .is_some_and(|lp| lp.status == LoopStatus::Paused))
    }

    fn fail_loop(&self, loop_id: &str, spec_name: Option<&str>, summary: &str) -> Result<()> {
        self.db
            .update_loop_status(loop_id, LoopStatus::Failed, None, Some(chrono::Utc::now()))?;
        // B12 catch-all: whatever hard-error path got us here (a node
        // timeout already kills its own process before bubbling up, but a
        // DB error or any other error class reaching this point wouldn't
        // have), make sure nothing is left running under this now-failed
        // loop.
        for run in self.db.list_running_loop_runs(loop_id).unwrap_or_default() {
            self.terminate_run(&run, "loop run failed");
        }
        let loop_name = self
            .db
            .get_loop(loop_id)?
            .map(|lp| lp.name)
            .unwrap_or_else(|| loop_id.to_string());
        self.notification_service.notify_loop_finished(
            &loop_name,
            LoopFinishOutcome::Failed {
                spec_name: spec_name.unwrap_or(summary),
            },
        );
        Ok(())
    }

    /// Notify that `loop_id` has become blocked on a node needing human
    /// intervention (`loop_report_blocker`). This is the only "loop
    /// finished" case not driven from within [`Self::run_loop_dispatch`] —
    /// the daemon's `loop_report_blocker` tool owns the actual state
    /// transition (pausing the loop, recording the blocker on the run) and
    /// calls this to fire the matching notification.
    pub fn notify_blocked(&self, loop_id: &str, summary: &str) -> Result<()> {
        let loop_name = self
            .db
            .get_loop(loop_id)?
            .map(|lp| lp.name)
            .unwrap_or_else(|| loop_id.to_string());
        self.notification_service
            .notify_loop_finished(&loop_name, LoopFinishOutcome::Blocked { summary });
        Ok(())
    }

    /// `(done, total)` specs for `loop_id`'s current run — the loop's bound
    /// specs, or `pool_id`'s members when this run is drawing from a pool.
    /// `done` counts specs already `completed`; skipped/pending/running/failed
    /// specs count toward `total` but not `done`.
    fn spec_progress(&self, loop_id: &str, pool_id: Option<&str>) -> Result<(usize, usize)> {
        match pool_id {
            Some(pool_id) => {
                let ids = self.db.list_pool_member_spec_ids(pool_id)?;
                let mut done = 0;
                for id in &ids {
                    if let Some(spec) = self.db.get_loop_spec(id)? {
                        if spec.status == LoopSpecStatus::Completed {
                            done += 1;
                        }
                    }
                }
                Ok((done, ids.len()))
            }
            None => {
                let specs = self.db.list_loop_specs(loop_id)?;
                let done = specs
                    .iter()
                    .filter(|spec| spec.status == LoopSpecStatus::Completed)
                    .count();
                Ok((done, specs.len()))
            }
        }
    }

    /// Fire the spec-completed notification for `spec`, which the caller has
    /// already marked `completed` in the database. Carries the next spec this
    /// run will pick up (if any) so the toast leads with progress *and* what's
    /// coming next.
    fn notify_spec_completed(
        &self,
        lp: &crate::domain::loops::Loop,
        spec: &LoopSpec,
        pool_id: Option<&str>,
    ) -> Result<()> {
        let (done, total) = self.spec_progress(&lp.id, pool_id)?;
        let next_pending = self.first_pending_spec_name(&lp.id, pool_id)?;
        self.notification_service.notify_spec_completed(
            &lp.name,
            &spec.name,
            done,
            total,
            next_pending.as_deref(),
        );
        Ok(())
    }

    /// Name of the next spec this run will work: the pool's next pending
    /// member for a pool run, else the loop's first `running`-or-`pending`
    /// bound spec in position order. `None` when nothing is left to do.
    fn first_pending_spec_name(
        &self,
        loop_id: &str,
        pool_id: Option<&str>,
    ) -> Result<Option<String>> {
        match pool_id {
            Some(pool_id) => {
                let Some(spec_id) = self.db.pool_next_pending_spec_id(pool_id)? else {
                    return Ok(None);
                };
                Ok(self.db.get_loop_spec(&spec_id)?.map(|spec| spec.name))
            }
            None => {
                let specs = self.db.list_loop_specs(loop_id)?;
                let next = specs
                    .iter()
                    .find(|spec| spec.status == LoopSpecStatus::Running)
                    .or_else(|| {
                        specs
                            .iter()
                            .find(|spec| spec.status == LoopSpecStatus::Pending)
                    });
                Ok(next.map(|spec| spec.name.clone()))
            }
        }
    }
}

fn read_infra_config(node: &LoopNode) -> (u32, u64, u64) {
    let retry_limit = node
        .config
        .get("infra_retry_limit")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_INFRA_RETRY_LIMIT as u64) as u32;
    let crash_max_secs = node
        .config
        .get("infra_crash_max_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_INFRA_CRASH_MAX_SECONDS);
    let backoff_secs = node
        .config
        .get("infra_backoff_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_INFRA_BACKOFF_SECONDS);
    (retry_limit, crash_max_secs, backoff_secs)
}

fn merge_attempt_marker(output: &Value, attempt: u32, is_crash: bool) -> Value {
    let mut obj = output.clone();
    if let serde_json::Value::Object(ref mut map) = obj {
        map.insert("infra_attempt".to_string(), Value::from(attempt));
        map.insert("infra_crash".to_string(), Value::from(is_crash));
    }
    obj
}

/// B19/B39 infra-crash decision for one agent attempt: a non-self-reported,
/// quick (within `crash_max_secs`) nonzero-exit failure of an AGENT node with
/// retry budget still left. A self-reported result, a Check/Gate node, a
/// semantic pass, a failure past the crash window, or a permanent spawn
/// failure (binary not found, permission denied) is never an infra crash.
///
/// Shared by the sequential node path ([`LoopEngine::run_spec`]) and, since
/// B26, by ensemble members ([`LoopEngine::execute_ensemble`]) — both use the
/// identical rule so a crashed member is retried exactly like a lone node and
/// only counts as failed for the join once its retries are exhausted.
fn is_infra_crash(
    node: &LoopNode,
    execution: &NodeExecution,
    run: &LoopNodeRun,
    attempt: u32,
    retry_limit: u32,
    crash_max_secs: u64,
) -> bool {
    let self_reported = run.status != LoopRunStatus::Running;
    let permanent = execution
        .output
        .get("spawn_permanent")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    !self_reported
        && !permanent
        && node.kind == LoopNodeKind::Agent
        && execution.status == LoopRunStatus::Fail
        && (chrono::Utc::now() - run.started_at).num_seconds() < crash_max_secs as i64
        && attempt < retry_limit
}

/// Persist a crashed agent attempt with B19 `infra_attempt`/`infra_crash`
/// markers, wait the doubling backoff (`backoff_secs * 2^attempt`), then
/// insert a fresh `Running` run row for the retry and return its id. `attempt`
/// is the zero-based index of the attempt that just crashed. Shared by the
/// sequential node path and ensemble members (B26) so every infra retry — no
/// matter which path — leaves the same distinct, marker-carrying run rows.
#[allow(clippy::too_many_arguments)]
async fn begin_infra_retry(
    db: &Database,
    lp: &crate::domain::loops::Loop,
    spec: &LoopSpec,
    node: &LoopNode,
    previous_output: Option<&Value>,
    iteration: i64,
    crashed_run_id: &str,
    crashed_output: &Value,
    attempt: u32,
    backoff_secs: u64,
) -> Result<String> {
    db.update_loop_run_result(
        crashed_run_id,
        LoopRunStatus::Fail,
        Some(&merge_attempt_marker(crashed_output, attempt, true)),
        Some(chrono::Utc::now()),
    )?;
    tokio::time::sleep(std::time::Duration::from_secs(
        backoff_secs * 2u64.pow(attempt),
    ))
    .await;
    let run_id = uuid::Uuid::new_v4().to_string();
    db.insert_loop_run(&LoopNodeRun {
        id: run_id.clone(),
        loop_id: lp.id.clone(),
        spec_id: spec.id.clone(),
        node_id: node.id.clone(),
        status: LoopRunStatus::Running,
        input: previous_output.cloned(),
        output: None,
        started_at: chrono::Utc::now(),
        completed_at: None,
        iteration,
        pid: None,
        boot_id: crate::system::boot_id(),
        session_id: None,
    })?;
    Ok(run_id)
}

async fn execute_check_node(
    db: &Database,
    run_id: &str,
    lp: &crate::domain::loops::Loop,
    spec: &LoopSpec,
    node: &LoopNode,
    spec_start_head: Option<&str>,
    workdir: &str,
) -> Result<NodeExecution> {
    let raw_command = node
        .config
        .get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("Check node '{}' is missing a command.", node.name))?;
    let command = raw_command.replace("{{spec_start_head}}", spec_start_head.unwrap_or(""));
    let success_condition = node
        .config
        .get("success_condition")
        .and_then(Value::as_str)
        .unwrap_or("exit_code_0");
    let timeout_seconds = node
        .config
        .get("timeout_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(120);

    let mut process = shell_command(&command);
    process.current_dir(workdir);
    let child = process
        .spawn()
        .with_context(|| format!("Check node '{}' failed to spawn.", node.name))?;
    let pid = child.id();
    if let Some(pid) = pid {
        let _ = db.set_loop_run_pid(run_id, pid as i64, crate::system::boot_id().as_deref());
    }

    let timeout_result = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_seconds),
        child.wait_with_output(),
    )
    .await;

    let output = match timeout_result {
        Ok(result) => result?,
        Err(_elapsed) => {
            if let Some(pid) = pid {
                crate::daemon::process::terminate_process_group_async(pid as i64, KILL_GRACE);
            }
            let output = serde_json::json!({
                "kind": "check",
                "loop_id": lp.id,
                "spec_id": spec.id,
                "node_id": node.id,
                "command": command,
                "error": "timed out",
                "timeout_seconds": timeout_seconds,
            });
            let _ = db.update_loop_run_result(
                run_id,
                LoopRunStatus::Fail,
                Some(&output),
                Some(chrono::Utc::now()),
            );
            // B28: a timeout is a check fail, not a hard error — it must
            // route through the fail edge like any other check failure,
            // never abort the whole spec.
            return Ok(NodeExecution {
                status: LoopRunStatus::Fail,
                output,
                summary: format!(
                    "Check node '{}' timed out after {timeout_seconds}s.",
                    node.name
                ),
            });
        }
    };

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let combined = if stderr.is_empty() {
        stdout.clone()
    } else if stdout.is_empty() {
        stderr.clone()
    } else {
        format!("{stdout}\n{stderr}")
    };
    let passed = evaluate_success_condition(success_condition, exit_code, &combined)?;

    Ok(NodeExecution {
        status: if passed {
            LoopRunStatus::Pass
        } else {
            LoopRunStatus::Fail
        },
        output: serde_json::json!({
            "kind": "check",
            "loop_id": lp.id,
            "spec_id": spec.id,
            "node_id": node.id,
            "command": command,
            "success_condition": success_condition,
            "exit_code": exit_code,
            "stdout": stdout,
            "stderr": stderr,
            "passed": passed,
        }),
        summary: format!(
            "Check node '{}' {}.",
            node.name,
            if passed { "passed" } else { "failed" }
        ),
    })
}

/// Force stdin prompt delivery for an oversized prompt. Node outputs are
/// arbitrarily large (e.g. a full `cargo test` log), and the composed prompt
/// embeds previous_output via `{{previous_feedback}}`. Even after elision the
/// total can exceed Linux's MAX_ARG_STRLEN (128KiB), crashing the spawn with
/// E2BIG — the temp-file + stdin transport has no size cliff.
fn sized_strategy(
    base: &crate::domain::cli_strategy::CliStrategy,
    prompt: &str,
) -> crate::domain::cli_strategy::CliStrategy {
    if prompt.len() > ARGV_SAFETY_THRESHOLD && !base.prompt_via_stdin {
        base.with_stdin_forced()
    } else {
        base.clone()
    }
}

/// If the agent finalized its own run row (called `loop_complete_node` /
/// `loop_report_blocker`), turn that self-reported status into the node's
/// result; otherwise `None` so the caller uses the process-derived execution.
fn self_reported_execution(run: Option<&LoopNodeRun>, node: &LoopNode) -> Option<NodeExecution> {
    let run = run?;
    if run.status == LoopRunStatus::Running {
        return None;
    }
    Some(NodeExecution {
        status: run.status,
        output: run.output.clone().unwrap_or_else(|| serde_json::json!({})),
        summary: format!("Agent node '{}' reported its own result.", node.name),
    })
}

/// Execute an agent node, RESUMING its captured session (RS2) when the engine
/// hands down a `resume_session_id` for a re-run of this node (a fail-edge
/// bounce, or a B19 infra retry of an attempt that had created a session) and
/// the node opts in and the platform supports headless resume-by-id.
///
/// A resumed spawn gets only the incremental prompt (new feedback + the
/// report contract), continues the same session (recorded on the new run row,
/// capture skipped), and — if the resume flag is rejected / crashes at spawn —
/// falls back to a byte-identical cold start whose verdict the node then uses.
/// Every other case cold-starts exactly as before.
#[allow(clippy::too_many_arguments)]
async fn execute_agent_node(
    db: &Arc<Database>,
    lp: &crate::domain::loops::Loop,
    spec: &LoopSpec,
    node: &LoopNode,
    previous_output: Option<&Value>,
    run_id: &str,
    workdir: &str,
    resume_session_id: Option<&str>,
) -> Result<NodeExecution> {
    let cli_name = node
        .config
        .get("platform")
        .or_else(|| node.config.get("cli"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("Agent node '{}' is missing a platform/cli.", node.name))?;
    let cli = Cli::resolve(Some(cli_name)).map_err(anyhow::Error::msg)?;
    let model = node.config.get("model").and_then(Value::as_str);
    let timeout_minutes = node
        .config
        .get("timeout_minutes")
        .and_then(Value::as_u64)
        .unwrap_or(30);
    let base_strategy = cli.strategy();

    // Per-node opt-out: `resume: false` forces cold starts. Default is to
    // resume whenever the engine offers a session and the platform supports it.
    let node_allows_resume = node.config.get("resume").and_then(Value::as_bool) != Some(false);

    // ── RS2 resume attempt ──────────────────────────────────────────────
    if let Some(sid) = resume_session_id {
        if node_allows_resume && base_strategy.supports_resume_by_id() {
            let resume_template = node
                .config
                .get("resume_prompt")
                .and_then(Value::as_str)
                .unwrap_or(RESUME_PROMPT_DEFAULT);
            let resume_prompt = render_resume_prompt(
                lp,
                spec,
                node,
                resume_template,
                previous_output,
                workdir,
                run_id,
            );
            let strategy = sized_strategy(&base_strategy, &resume_prompt);
            let execution = run_agent_process(
                db,
                run_id,
                &cli,
                &strategy,
                node,
                &resume_prompt,
                model,
                workdir,
                timeout_minutes,
                Some(sid),
            )
            .await?;

            let run = db.get_loop_run(run_id)?;
            // The resumed agent self-reported → route its verdict normally.
            if let Some(reported) = self_reported_execution(run.as_ref(), node) {
                return Ok(reported);
            }
            // Resume flag rejected / crashed at spawn (quick, non-self-reported
            // failure)? Fall back to a cold start whose result the node uses.
            // A resumed run that did real work and then failed (slow, or a
            // timeout) is a genuine fail and routes normally — never redone.
            let (_, crash_max_secs, _) = read_infra_config(node);
            let elapsed = run
                .as_ref()
                .map(|r| (chrono::Utc::now() - r.started_at).num_seconds())
                .unwrap_or(i64::MAX);
            let resume_failed_at_spawn =
                execution.status == LoopRunStatus::Fail && elapsed < crash_max_secs as i64;
            if !resume_failed_at_spawn {
                return Ok(execution);
            }
            tracing::warn!(
                run_id,
                node = %node.name,
                "resume failed at spawn; falling back to a cold start"
            );
            // fall through to the cold path below
        }
    }

    // ── Cold start (byte-identical to the pre-RS2 path) ─────────────────
    let prompt_template = node
        .config
        .get("prompt_template")
        .and_then(Value::as_str)
        .unwrap_or("{{spec_content}}\n\n{{previous_feedback}}");
    let prompt = render_agent_prompt(
        lp,
        spec,
        node,
        prompt_template,
        previous_output,
        workdir,
        run_id,
    );
    let strategy = sized_strategy(&base_strategy, &prompt);
    let execution = run_agent_process(
        db,
        run_id,
        &cli,
        &strategy,
        node,
        &prompt,
        model,
        workdir,
        timeout_minutes,
        None,
    )
    .await?;

    if let Some(reported) = self_reported_execution(db.get_loop_run(run_id)?.as_ref(), node) {
        return Ok(reported);
    }
    Ok(execution)
}

/// A failure to build or spawn the child process, classified by whether it
/// can plausibly resolve itself between attempts (B39).
///
/// Permanent failures — an unresolvable binary, a non-executable file — are
/// deterministic: retrying spends the infra-retry budget and its doubling
/// backoff on a state that cannot change. `permanent_reason` carries the
/// operator-facing "why we did not retry", set from the *kind* of the
/// underlying error (typed [`BinaryResolutionError`][ce], `io::ErrorKind`) and
/// never from matching the rendered message per CLI.
///
/// [ce]: crate::domain::cli_strategy::BinaryResolutionError
struct SpawnError {
    message: String,
    permanent_reason: Option<&'static str>,
}

impl SpawnError {
    /// A failure while building the command — chiefly resolving the CLI's
    /// configured binary, which is where a missing CLI surfaces.
    fn from_build(error: &anyhow::Error) -> Self {
        let permanent_reason = error
            .downcast_ref::<crate::domain::cli_strategy::BinaryResolutionError>()
            .map(|_| "cli binary could not be resolved");
        Self {
            message: error.to_string(),
            permanent_reason,
        }
    }

    /// A failure from the spawn/wait syscalls themselves.
    fn from_io(error: &std::io::Error) -> Self {
        let permanent_reason = match error.kind() {
            std::io::ErrorKind::NotFound => Some("cli binary not found at its resolved path"),
            std::io::ErrorKind::PermissionDenied => Some("cli binary is not executable"),
            _ => None,
        };
        Self {
            message: error.to_string(),
            permanent_reason,
        }
    }

    /// A failure with no reason to believe a retry would land differently is
    /// transient by default, so the B19/B26 retry path is unchanged.
    fn transient(message: String) -> Self {
        Self {
            message,
            permanent_reason: None,
        }
    }
}

/// Outcome of actually running the child process to completion, as opposed
/// to failing to build/spawn it (see [`spawn_and_wait_cli_process`]'s `Err`).
enum CliProcessOutcome {
    Finished {
        exit_code: i32,
        stdout: String,
        stderr: String,
    },
    /// The process started but didn't finish within `timeout_minutes`. Its
    /// process group has already been killed (B12) by the time this variant
    /// is returned — callers only need to decide how to record the failure.
    TimedOut,
}

/// Build the CLI command, spawn it, and wait for it (or a timeout) — the
/// one spawn path shared by every detached single-agent execution the
/// engine runs, node or hook alike: a loop agent node
/// ([`run_agent_process`]) and the `on_completed` hook
/// ([`run_completion_hook_process`]).
///
/// Returns `Err` only for a failure to build/spawn the process itself (e.g.
/// `E2BIG` from an oversized argv, binary not found, permission denied) —
/// callers turn that into their own kind of "failed" record rather than a
/// hard error, since a spawn failure must never abort anything wider (the
/// whole loop run, for a node; the loop's already-finalized status, for the
/// hook).
///
/// A timeout is a different failure class (the process started; it just
/// didn't finish in time). Unlike a build/spawn failure, the spawned process
/// group is actually killed here (B12) before returning
/// [`CliProcessOutcome::TimedOut`] — dropping the timed-out future used to
/// leave it running indefinitely (`Command::output` gives the caller no
/// handle to kill), which is exactly what let a `mimo run` child outlive its
/// node run by 42+ minutes in the 2026-07-12 incident.
#[allow(clippy::too_many_arguments)]
async fn spawn_and_wait_cli_process(
    strategy: &crate::domain::cli_strategy::CliStrategy,
    prompt: &str,
    model: Option<&str>,
    workdir: &str,
    timeout_minutes: u64,
    session_id: Option<&str>,
    resume_session_id: Option<&str>,
    on_pid: impl FnOnce(u32),
) -> Result<CliProcessOutcome, SpawnError> {
    // A resume (RS2) uses the by-id resume flag and continues an existing
    // session; a cold start uses the set-at-spawn flag (if any). The two are
    // mutually exclusive — the caller passes at most one.
    let mut command = match resume_session_id {
        Some(sid) => strategy
            .build_resume_command(sid, prompt, model, Some(workdir))
            .map_err(|error| SpawnError::from_build(&error))?,
        None => strategy
            .build_command_with_session(prompt, model, Some(workdir), session_id)
            .map_err(|error| SpawnError::from_build(&error))?,
    };
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    let child = command
        .spawn()
        .map_err(|error| SpawnError::from_io(&error))?;
    // Captured before `wait_with_output` below takes ownership of `child`.
    let pid = child.id();
    if let Some(pid) = pid {
        on_pid(pid);
    }

    let timeout_result = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_minutes * 60),
        child.wait_with_output(),
    )
    .await;

    match timeout_result {
        Ok(Ok(output)) => {
            let exit_code = output.status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Ok(CliProcessOutcome::Finished {
                exit_code,
                stdout,
                stderr,
            })
        }
        Ok(Err(error)) => Err(SpawnError::transient(error.to_string())),
        Err(_elapsed) => {
            if let Some(pid) = pid {
                crate::daemon::process::terminate_process_group_async(pid as i64, KILL_GRACE);
            }
            Ok(CliProcessOutcome::TimedOut)
        }
    }
}

/// Run an agent node's process via [`spawn_and_wait_cli_process`], turning
/// any failure to build or spawn the process into a failed `NodeExecution`
/// rather than propagating a hard error — routed through the graph's fail
/// edge for resilience triage, never aborting the whole loop run.
///
/// A timeout (B28) is likewise a failed `NodeExecution`, not a hard error:
/// it exceeds `infra_crash_max_seconds` by definition, so it's always a
/// semantic fail routed through the fail edge like any other, never an
/// infra-crash retry.
#[allow(clippy::too_many_arguments)]
async fn run_agent_process(
    db: &Database,
    run_id: &str,
    cli: &Cli,
    strategy: &crate::domain::cli_strategy::CliStrategy,
    node: &LoopNode,
    prompt: &str,
    model: Option<&str>,
    workdir: &str,
    timeout_minutes: u64,
    resume_session_id: Option<&str>,
) -> Result<NodeExecution> {
    // Resume (RS2): the run continues an existing session. Record that same
    // id on this run row and SKIP capture entirely — set-at-spawn must not
    // mint a new UUID and list-after-run must not diff, because a resume
    // creates no new session to find. When resuming, `session_id`/
    // `pre_session_ids` stay `None` so neither capture path runs.
    if let Some(sid) = resume_session_id {
        let _ = db.set_loop_run_session_id(run_id, sid);
    }

    // Set-at-spawn session id capture (RS1): when the platform accepts a
    // caller-chosen session id, mint one and record it on the run row
    // before spawning — the id is known without parsing any output, and
    // stays valid for resume however the run ends. Never on a resumed spawn.
    let session_id = if resume_session_id.is_none() {
        strategy
            .session_id_set_flag
            .as_ref()
            .map(|_| uuid::Uuid::new_v4().to_string())
    } else {
        None
    };
    if let Some(sid) = session_id.as_deref() {
        let _ = db.set_loop_run_session_id(run_id, sid);
    }

    // List-after-run session id capture (RS1 phase 2): for platforms that
    // can't set the id at spawn but do expose a session-list command, snapshot
    // the set of session ids BEFORE spawning so the new one can be diffed out
    // after the run. Skipped entirely when set-at-spawn already applied
    // (`session_id.is_some()`), which takes strict precedence, or when this is
    // a resumed spawn. Best-effort: a failed snapshot (`None`) just disables
    // capture for this run.
    let pre_session_ids = if resume_session_id.is_none()
        && session_id.is_none()
        && strategy.can_capture_session_after_run()
    {
        list_session_ids(strategy, workdir).await
    } else {
        None
    };

    let outcome = spawn_and_wait_cli_process(
        strategy,
        prompt,
        model,
        workdir,
        timeout_minutes,
        session_id.as_deref(),
        resume_session_id,
        |pid| {
            let _ = db.set_loop_run_pid(run_id, pid as i64, crate::system::boot_id().as_deref());
        },
    )
    .await;

    // Attribute the session the run just created (RS1 phase 2). Only when a
    // pre-snapshot was taken AND the process actually started — a spawn `Err`
    // means nothing ran, so there's nothing new to attribute. Never affects
    // the verdict: capture only ever writes `session_id`, and any failure
    // leaves it NULL.
    if let Some(pre) = pre_session_ids {
        if outcome.is_ok() {
            capture_session_id_after_run(db, run_id, strategy, workdir, &pre).await;
        }
    }

    match outcome {
        Err(error) => Ok(agent_spawn_failure(node, cli, model, &error)),
        Ok(CliProcessOutcome::TimedOut) => {
            let output = serde_json::json!({
                "kind": "agent",
                "node_id": node.id,
                "cli": cli.as_str(),
                "model": model,
                "error": "timed out",
                "timeout_minutes": timeout_minutes,
            });
            let _ = db.update_loop_run_result(
                run_id,
                LoopRunStatus::Fail,
                Some(&output),
                Some(chrono::Utc::now()),
            );
            Ok(NodeExecution {
                status: LoopRunStatus::Fail,
                output,
                summary: format!(
                    "Agent node '{}' timed out after {timeout_minutes}m.",
                    node.name
                ),
            })
        }
        Ok(CliProcessOutcome::Finished {
            exit_code,
            stdout,
            stderr,
        }) => Ok(NodeExecution {
            status: if exit_code == 0 {
                LoopRunStatus::Pass
            } else {
                LoopRunStatus::Fail
            },
            output: serde_json::json!({
                "kind": "agent",
                "node_id": node.id,
                "cli": cli.as_str(),
                "model": model,
                "exit_code": exit_code,
                "stdout": stdout,
                "stderr": stderr,
            }),
            summary: format!("Agent node '{}' exited with code {}.", node.name, exit_code),
        }),
    }
}

fn agent_spawn_failure(
    node: &LoopNode,
    cli: &Cli,
    model: Option<&str>,
    error: &SpawnError,
) -> NodeExecution {
    let mut output = serde_json::json!({
        "kind": "agent",
        "node_id": node.id,
        "cli": cli.as_str(),
        "model": model,
        "error": error.message,
    });
    // A permanent failure is recorded with its reason so the run reads as
    // "failed fast on purpose" rather than "retried and gave up" — the two
    // are otherwise indistinguishable in a persisted run row.
    if let (Some(reason), serde_json::Value::Object(map)) = (error.permanent_reason, &mut output) {
        map.insert("spawn_permanent".to_string(), Value::Bool(true));
        map.insert(
            "infra_retry_skipped".to_string(),
            Value::String(reason.to_string()),
        );
    }
    NodeExecution {
        status: LoopRunStatus::Fail,
        output,
        summary: format!(
            "Agent node '{}' failed to spawn: {}",
            node.name, error.message
        ),
    }
}

/// Hard cap on how long a session-list invocation may run during
/// list-after-run capture (RS1 phase 2). Capture is best-effort and must
/// never stall a run's bookkeeping, so a slow/hung list command is abandoned
/// (its process group killed via `kill_on_drop`) and the id left NULL.
const SESSION_LIST_TIMEOUT_SECS: u64 = 10;

/// Run the platform's session-list command (cwd = node workdir) with a short
/// timeout and return the extracted set of session ids. `None` means the
/// capability isn't configured, or the command failed / timed out — capture
/// is best-effort, so callers treat `None` as "leave the id NULL", never an
/// error. Registry-driven end to end: the subcommand, the machine-readable
/// args, and the id regex all come from the platform config.
async fn list_session_ids(
    strategy: &crate::domain::cli_strategy::CliStrategy,
    workdir: &str,
) -> Option<std::collections::HashSet<String>> {
    let mut cmd = match strategy.build_session_list_command(workdir) {
        Ok(Some(cmd)) => cmd,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(%error, "session id capture: could not build session-list command");
            return None;
        }
    };
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            tracing::warn!(%error, "session id capture: session-list command failed to spawn");
            return None;
        }
    };

    match tokio::time::timeout(
        std::time::Duration::from_secs(SESSION_LIST_TIMEOUT_SECS),
        child.wait_with_output(),
    )
    .await
    {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            Some(strategy.extract_session_ids(&stdout))
        }
        Ok(Err(error)) => {
            tracing::warn!(%error, "session id capture: session-list command errored");
            None
        }
        Err(_elapsed) => {
            // Dropping the future drops the Child; `kill_on_drop` (set in
            // `build_session_list_command`) reaps the process group.
            tracing::warn!(
                timeout_secs = SESSION_LIST_TIMEOUT_SECS,
                "session id capture: session-list command timed out"
            );
            None
        }
    }
}

/// After a run finishes, list the platform's sessions again and attribute the
/// single id that wasn't in `pre` to this run via `set_loop_run_session_id`.
/// A diff of exactly one is recorded; zero or many is logged and the id left
/// NULL (non-fatal). Same-platform parallel runs can race and produce
/// multiple new ids — that's logged, not solved. Never touches the verdict.
async fn capture_session_id_after_run(
    db: &Database,
    run_id: &str,
    strategy: &crate::domain::cli_strategy::CliStrategy,
    workdir: &str,
    pre: &std::collections::HashSet<String>,
) {
    let Some(post) = list_session_ids(strategy, workdir).await else {
        tracing::warn!(
            run_id,
            "session id capture: post-run session list unavailable; leaving session_id NULL"
        );
        return;
    };
    let new: Vec<&String> = post.difference(pre).collect();
    match new.as_slice() {
        [only] => {
            if let Err(error) = db.set_loop_run_session_id(run_id, only) {
                tracing::warn!(run_id, %error, "session id capture: failed to persist session id");
            }
        }
        [] => tracing::warn!(
            run_id,
            "session id capture: no new session appeared; leaving session_id NULL"
        ),
        many => tracing::warn!(
            run_id,
            candidates = many.len(),
            "session id capture: multiple new sessions (same-platform parallel runs?); \
             cannot attribute, leaving session_id NULL"
        ),
    }
}

/// Result of one `on_completed` hook firing (N2) — deliberately not a
/// [`NodeExecution`]: the hook belongs to no node, and unlike a node's
/// result, this one must never feed back into the run's routing or final
/// status (the run is already `Completed` by the time this fires).
struct HookExecution {
    status: LoopRunStatus,
    output: Value,
    summary: String,
}

/// Run the `on_completed` hook's process via [`spawn_and_wait_cli_process`] —
/// the same spawn path as [`run_agent_process`], minus the parts that are
/// specific to a graph node run (no `LoopNodeRun` id to route a late report
/// against, no hard-error timeout: a hook failure is always recorded and
/// reported to the caller as data, never propagated as an `Err`, since it
/// must never affect the already-finalized loop run that spawned it).
#[allow(clippy::too_many_arguments)]
async fn run_completion_hook_process(
    db: &Database,
    hook_run_id: &str,
    cli: &Cli,
    strategy: &crate::domain::cli_strategy::CliStrategy,
    prompt: &str,
    model: Option<&str>,
    workdir: &str,
    timeout_minutes: u64,
) -> HookExecution {
    let outcome = spawn_and_wait_cli_process(
        strategy,
        prompt,
        model,
        workdir,
        timeout_minutes,
        None,
        None,
        |pid| {
            let _ = db.set_loop_completion_hook_run_pid(
                hook_run_id,
                pid as i64,
                crate::system::boot_id().as_deref(),
            );
        },
    )
    .await;

    match outcome {
        Err(error) => HookExecution {
            status: LoopRunStatus::Fail,
            output: serde_json::json!({
                "cli": cli.as_str(),
                "model": model,
                "error": error.message,
            }),
            summary: format!("on_completed hook failed to spawn: {}", error.message),
        },
        Ok(CliProcessOutcome::TimedOut) => HookExecution {
            status: LoopRunStatus::Fail,
            output: serde_json::json!({
                "cli": cli.as_str(),
                "model": model,
                "error": "timed out",
                "timeout_minutes": timeout_minutes,
            }),
            summary: format!("on_completed hook timed out after {timeout_minutes}m."),
        },
        Ok(CliProcessOutcome::Finished {
            exit_code,
            stdout,
            stderr,
        }) => HookExecution {
            status: if exit_code == 0 {
                LoopRunStatus::Pass
            } else {
                LoopRunStatus::Fail
            },
            output: serde_json::json!({
                "cli": cli.as_str(),
                "model": model,
                "exit_code": exit_code,
                "stdout": stdout,
                "stderr": stderr,
            }),
            summary: format!("on_completed hook exited with code {exit_code}."),
        },
    }
}

fn execute_gate_node(node: &LoopNode, previous_output: Option<&Value>) -> Result<NodeExecution> {
    let previous_output = previous_output
        .ok_or_else(|| anyhow!("Gate node '{}' requires previous node output.", node.name))?;
    let evaluate = node
        .config
        .get("evaluate")
        .and_then(Value::as_str)
        .unwrap_or("output_contains");
    let expected = node
        .config
        .get("value")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let haystack = serde_json::to_string(previous_output)?;

    let passed = match evaluate {
        "output_contains" => haystack.contains(expected),
        other => bail!("Unsupported gate evaluate mode '{}'.", other),
    };

    Ok(NodeExecution {
        status: if passed {
            LoopRunStatus::Pass
        } else {
            LoopRunStatus::Fail
        },
        output: serde_json::json!({
            "kind": "gate",
            "node_id": node.id,
            "evaluate": evaluate,
            "value": expected,
            "passed": passed,
        }),
        summary: format!(
            "Gate node '{}' {}.",
            node.name,
            if passed { "passed" } else { "failed" }
        ),
    })
}

fn evaluate_success_condition(condition: &str, exit_code: i32, output: &str) -> Result<bool> {
    if condition == "exit_code_0" {
        return Ok(exit_code == 0);
    }

    if let Some(expected) = condition.strip_prefix("exit_code_0_and_output_contains:") {
        return Ok(exit_code == 0 && output.contains(expected.trim().trim_matches('"')));
    }

    if let Some(expected) = condition.strip_prefix("output_not_contains:") {
        return Ok(!output.contains(expected.trim().trim_matches('"')));
    }

    bail!("Unsupported success condition '{}'.", condition)
}

fn find_entry_node(nodes: &[LoopNode], edges: &[LoopEdge], spec_name: &str) -> Result<String> {
    let incoming = edges
        .iter()
        .map(|edge| edge.to_node.as_str())
        .collect::<HashSet<_>>();
    let entry_nodes = nodes
        .iter()
        .filter(|node| !incoming.contains(node.id.as_str()))
        .collect::<Vec<_>>();

    match entry_nodes.as_slice() {
        [entry] => Ok(entry.id.clone()),
        // Every node has an incoming edge: the graph is a retry cycle (e.g.
        // implement <-> review). There is no source node, so fall back to the
        // designated start — the node with the lowest position.
        [] => nodes
            .iter()
            .min_by_key(|node| node.position)
            .map(|node| node.id.clone())
            .ok_or_else(|| anyhow!("Spec '{}' has no nodes.", spec_name)),
        _ => bail!("Spec '{}' has multiple entry nodes.", spec_name),
    }
}

/// Result of [`select_next_step`]: the next cursor plus the edge condition
/// that matched (needed for B43 lifecycle logging).
#[derive(Debug)]
struct StepSelection {
    cursor: SpecCursor,
    edge_condition: LoopEdgeCondition,
}

/// Resolve the next graph step from `from_node`'s outgoing edges matching
/// `status`. Ordinarily a single matching edge (or several identical-target
/// edges) resolves to [`SpecCursor::Node`]. Multiple *distinct* targets are
/// ambiguous — unless they are exactly one ensemble's full member set, in
/// which case this is F1's fan-out point and resolves to
/// [`SpecCursor::Ensemble`] instead of erroring.
fn select_next_step(
    edges: &[LoopEdge],
    ensembles: &[EnsembleDetails],
    from_node: &str,
    status: LoopRunStatus,
) -> Result<Option<StepSelection>> {
    let matching = edges
        .iter()
        .filter(|edge| edge.from_node == from_node)
        .filter(|edge| match status {
            LoopRunStatus::Pass => {
                edge.condition == LoopEdgeCondition::Pass
                    || edge.condition == LoopEdgeCondition::Always
            }
            LoopRunStatus::Fail => {
                edge.condition == LoopEdgeCondition::Fail
                    || edge.condition == LoopEdgeCondition::Always
            }
            LoopRunStatus::Running => false,
        })
        .collect::<Vec<_>>();

    match matching.as_slice() {
        [] => Ok(None),
        [edge] => Ok(Some(StepSelection {
            cursor: SpecCursor::Node(edge.to_node.clone()),
            edge_condition: edge.condition,
        })),
        _ => {
            let distinct_targets = matching
                .iter()
                .map(|edge| edge.to_node.as_str())
                .collect::<HashSet<_>>();
            if distinct_targets.len() == 1 {
                let to_node = *distinct_targets.iter().next().expect("len == 1");
                return Ok(Some(StepSelection {
                    cursor: SpecCursor::Node(to_node.to_string()),
                    edge_condition: matching[0].condition,
                }));
            }
            for details in ensembles {
                let member_ids: HashSet<&str> = details
                    .members
                    .iter()
                    .map(|member| member.node_id.as_str())
                    .collect();
                if member_ids == distinct_targets {
                    return Ok(Some(StepSelection {
                        cursor: SpecCursor::Ensemble(details.ensemble.id.clone()),
                        edge_condition: matching[0].condition,
                    }));
                }
            }
            bail!("Node '{}' has ambiguous outgoing edges.", from_node)
        }
    }
}

/// Every node id belonging to a cursor step — one for [`SpecCursor::Node`],
/// or every member plus the join for [`SpecCursor::Ensemble`]. Used to reap
/// stale `running` rows (B12) across a whole ensemble fan-out, not just one
/// node.
fn cursor_node_ids(cursor: &SpecCursor, ensembles: &[EnsembleDetails]) -> Vec<String> {
    match cursor {
        SpecCursor::Node(node_id) => vec![node_id.clone()],
        SpecCursor::Ensemble(ensemble_id) => ensembles
            .iter()
            .find(|details| &details.ensemble.id == ensemble_id)
            .map(|details| {
                details
                    .members
                    .iter()
                    .map(|member| member.node_id.clone())
                    .chain(std::iter::once(details.ensemble.join_node_id.clone()))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// Human-readable label for a cursor step, for failure summaries.
fn cursor_label(cursor: &SpecCursor, ensembles: &[EnsembleDetails]) -> String {
    match cursor {
        SpecCursor::Node(node_id) => format!("node '{node_id}'"),
        SpecCursor::Ensemble(ensemble_id) => ensembles
            .iter()
            .find(|details| &details.ensemble.id == ensemble_id)
            .map(|details| format!("ensemble '{}'", details.ensemble.name))
            .unwrap_or_else(|| format!("ensemble '{ensemble_id}'")),
    }
}

/// Reason recorded on a node run terminated because a newer attempt at the
/// same node superseded it (B42). Unlike every other termination reason, a
/// superseded run is pure engine bookkeeping rather than a node failure: the
/// dispatch that owned it must recognise the marker and stop silently, routing
/// it down no edge (see [`run_was_superseded`] and its use in
/// [`LoopEngine::run_spec`]).
const SUPERSEDE_REASON: &str = "superseded by a new attempt at this node";

/// Whether `run` was terminated by the supersede path ([`SUPERSEDE_REASON`]) —
/// i.e. its `Fail` row is a newer attempt reclaiming the node, not a real node
/// failure. Recognised by the exact `{ "terminated": true, "reason": … }`
/// marker [`terminate_run_row`] writes, so a genuine agent output that merely
/// mentions the phrase can never be mistaken for one.
fn run_was_superseded(run: &LoopNodeRun) -> bool {
    let Some(output) = run.output.as_ref() else {
        return false;
    };
    output.get("terminated").and_then(Value::as_bool) == Some(true)
        && output.get("reason").and_then(Value::as_str) == Some(SUPERSEDE_REASON)
}

/// Best-effort termination (B12) of `run`'s OS process, if it still has one
/// recorded, and finalization of its DB row as `Fail`. Free-function core of
/// [`LoopEngine::terminate_run`] — also used by ensemble member tasks, which
/// don't have a `&LoopEngine` to call the method on.
fn terminate_run_row(db: &Database, run: &LoopNodeRun, reason: &str) {
    tracing::info!(
        run_id = %run.id,
        node_id = %run.node_id,
        reason,
        "node run terminated"
    );
    if let Some(pid) = run.pid {
        crate::daemon::process::terminate_process_group_async(pid, KILL_GRACE);
    }
    let _ = db.update_loop_run_result(
        &run.id,
        LoopRunStatus::Fail,
        Some(&serde_json::json!({ "terminated": true, "reason": reason })),
        Some(chrono::Utc::now()),
    );
}

/// `"platform"` or `"platform/model"` — the label used in an ensemble's
/// consolidated `"## <label> [pass|fail]"` sections and in the TUI's
/// collapsed ensemble view.
fn member_label(member: &EnsembleMember) -> String {
    match member.model.as_deref().map(str::trim) {
        Some(model) if !model.is_empty() => format!("{}/{}", member.platform, model),
        _ => member.platform.clone(),
    }
}

/// The human-readable text to carry into an ensemble's consolidated doc for
/// one member's output — its stdout when it produced any, the recorded
/// error when it didn't, else the raw output JSON.
fn member_output_text(output: &Value) -> String {
    if let Some(stdout) = output.get("stdout").and_then(Value::as_str) {
        if !stdout.trim().is_empty() {
            return stdout.to_string();
        }
    }
    if let Some(error) = output.get("error").and_then(Value::as_str) {
        return format!("(error: {error})");
    }
    serde_json::to_string_pretty(output).unwrap_or_default()
}

fn should_advance_to_next_spec(node: &LoopNode, status: LoopRunStatus) -> bool {
    let route_key = match status {
        LoopRunStatus::Pass => "pass_route",
        LoopRunStatus::Fail => "fail_route",
        LoopRunStatus::Running => return false,
    };

    node.kind == LoopNodeKind::Gate
        && node
            .config
            .get(route_key)
            .and_then(Value::as_str)
            .is_some_and(|route| route == "next_spec")
}

/// Above this many bytes, `{{previous_feedback}}` is elided to head+tail with
/// a marker instead of interpolated in full. This is a defensive bound that
/// applies regardless of prompt transport (argv or stdin): a prior node can
/// emit an arbitrarily large output (e.g. a full `cargo test` log), and
/// nothing about interpolating it whole into the next prompt is actually
/// useful past a point. The full output is never lost — it stays in
/// `loop_runs.output` for humans to inspect.
const PREVIOUS_FEEDBACK_ELISION_THRESHOLD: usize = 16 * 1024;

/// Maximum prompt size (in bytes) that is safe to pass via argv. Linux's
/// `MAX_ARG_STRLEN` is 128KiB; we leave headroom for other argv elements
/// (headless flags, model flag, working dir flag) by using 100KiB. When
/// the composed prompt exceeds this, the loop engine forces stdin delivery
/// regardless of the CLI's `prompt_via_stdin` registry setting.
const ARGV_SAFETY_THRESHOLD: usize = 100 * 1024;

/// Elide the middle of `text` with a marker once it exceeds
/// `PREVIOUS_FEEDBACK_ELISION_THRESHOLD`, keeping head and tail (each half
/// the threshold) intact. Slices on char boundaries so it never panics on
/// multi-byte UTF-8 content.
fn bound_previous_feedback(text: String) -> String {
    if text.len() <= PREVIOUS_FEEDBACK_ELISION_THRESHOLD {
        return text;
    }

    let half = PREVIOUS_FEEDBACK_ELISION_THRESHOLD / 2;
    let head_end = floor_char_boundary(&text, half);
    let tail_start = ceil_char_boundary(&text, text.len() - half);
    let elided_bytes = tail_start - head_end;

    format!(
        "{}\n[...{} bytes elided...]\n{}",
        &text[..head_end],
        elided_bytes,
        &text[tail_start..]
    )
}

fn floor_char_boundary(s: &str, index: usize) -> usize {
    let mut i = index.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, index: usize) -> usize {
    let mut i = index.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn render_agent_prompt(
    lp: &crate::domain::loops::Loop,
    spec: &LoopSpec,
    node: &LoopNode,
    prompt_template: &str,
    previous_output: Option<&Value>,
    workdir: &str,
    run_id: &str,
) -> String {
    let previous_feedback = previous_output
        .map(|value| serde_json::to_string_pretty(value).unwrap_or_default())
        .unwrap_or_else(|| "(none)".to_string());
    let previous_feedback = bound_previous_feedback(previous_feedback);
    let spec_content = spec.description.as_deref().unwrap_or(&spec.name);
    let prompt = prompt_template
        .replace("{{loop_name}}", &lp.name)
        .replace("{{workdir}}", workdir)
        .replace("{{spec_id}}", &spec.id)
        .replace("{{spec_name}}", &spec.name)
        .replace("{{spec_content}}", spec_content)
        .replace("{{node_id}}", &node.id)
        .replace("{{previous_feedback}}", &previous_feedback);

    // `run_id` (not just `node_id`) must round-trip through the report tools
    // (B12): a node can be retried, so more than one run can exist for the
    // same `node_id` over a spec's lifetime. Without the exact run_id, a
    // report arriving late from a killed/superseded attempt (e.g. a timed-out
    // agent that ignores its own termination and calls the tool anyway) would
    // otherwise be matched to "whatever's currently active for this node_id"
    // and silently corrupt a newer, unrelated run.
    format!(
        "# [LOOP CONTEXT]\n<loop>\n  <name>{}</name>\n  <spec>{}</spec>\n  <node>{}</node>\n  <workdir>{}</workdir>\n</loop>\n\n# [SPEC]\n{}\n\n# [PREVIOUS FEEDBACK]\n{}\n\n# [REPORTING]\nWhen you finish this node, call loop_complete_node with run_id=\"{}\", node_id=\"{}\", status=\"pass\"|\"fail\", a concise summary, and your output.\nIf you are blocked and need human intervention, call loop_report_blocker with run_id=\"{}\", node_id=\"{}\" and the blocker description.\n",
        lp.name,
        spec.name,
        node.name,
        workdir,
        prompt,
        previous_feedback,
        run_id,
        node.id,
        run_id,
        node.id
    )
}

/// Default incremental prompt for a RESUMED agent run (RS2). Deliberately
/// omits the full `[LOOP CONTEXT]`/`[SPEC]` block that a cold start renders:
/// the resumed session already holds all of that in its own history, so
/// re-sending it wastes tokens and can confuse the model into re-reading the
/// whole task. Only the new feedback and a one-line reminder of the reporting
/// contract are sent. Overridable per node via the `resume_prompt` config key.
const RESUME_PROMPT_DEFAULT: &str = "# [CONTINUE]\nYou are resuming your existing session for this task. The full task context is already in your session history — only the new feedback is included below. Address it, then report.\n\n# [PREVIOUS FEEDBACK]\n{{previous_feedback}}\n\n# [REPORTING]\nWhen you finish, call loop_complete_node with run_id=\"{{run_id}}\", node_id=\"{{node_id}}\", status=\"pass\"|\"fail\", a concise summary, and your output.\nIf you are blocked and need human intervention, call loop_report_blocker with run_id=\"{{run_id}}\", node_id=\"{{node_id}}\" and the blocker description.\n";

/// Render a resumed run's incremental prompt (RS2) from `template` (the node's
/// `resume_prompt` or [`RESUME_PROMPT_DEFAULT`]). Same `{{previous_feedback}}`
/// bounding as [`render_agent_prompt`], plus the run/node/spec placeholders the
/// reporting contract needs — but never `{{spec_content}}`, since a resume must
/// not re-render the spec block the session already has.
fn render_resume_prompt(
    lp: &crate::domain::loops::Loop,
    spec: &LoopSpec,
    node: &LoopNode,
    template: &str,
    previous_output: Option<&Value>,
    workdir: &str,
    run_id: &str,
) -> String {
    let previous_feedback = previous_output
        .map(|value| serde_json::to_string_pretty(value).unwrap_or_default())
        .unwrap_or_else(|| "(none)".to_string());
    let previous_feedback = bound_previous_feedback(previous_feedback);
    template
        .replace("{{loop_name}}", &lp.name)
        .replace("{{workdir}}", workdir)
        .replace("{{spec_id}}", &spec.id)
        .replace("{{spec_name}}", &spec.name)
        .replace("{{node}}", &node.name)
        .replace("{{node_id}}", &node.id)
        .replace("{{run_id}}", run_id)
        .replace("{{previous_feedback}}", &previous_feedback)
}

/// Render the `on_completed` hook's prompt template (N2). The hook has no
/// spec/node graph context to template against (it fires once per whole run,
/// not per spec), so it supports a smaller, hook-specific placeholder set
/// rather than [`render_agent_prompt`]'s full one:
///
/// - `{{loop_name}}` / `{{workdir}}` — same meaning as the node-prompt
///   placeholders of the same name.
/// - `{{completed_specs}}` — name + one-line summary of each spec completed
///   *in this run* (the final node's own summary text), one per line;
///   `(none)` if this run completed zero specs (e.g. every spec was already
///   `completed`/`skipped` before this run started).
fn render_completion_hook_prompt(
    lp: &crate::domain::loops::Loop,
    workdir: &str,
    completed_specs: &[(String, String)],
    prompt_template: &str,
) -> String {
    let completed_specs_text = if completed_specs.is_empty() {
        "(none)".to_string()
    } else {
        completed_specs
            .iter()
            .map(|(name, summary)| format!("- {name}: {summary}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    prompt_template
        .replace("{{loop_name}}", &lp.name)
        .replace("{{workdir}}", workdir)
        .replace("{{completed_specs}}", &completed_specs_text)
}

/// The workdir's current `git rev-parse HEAD`, or `None` if it isn't a git
/// repo (or the command otherwise fails). Never errors the caller — a check
/// node that references `{{spec_start_head}}` in a non-git workdir just sees
/// an empty string and decides for itself, per `execute_check_node`.
async fn capture_workdir_head(workdir: &str) -> Option<String> {
    let output = Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(workdir)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!head.is_empty()).then_some(head)
}

/// Whether this node is a designated committer (B37): explicit graph
/// configuration, `commit_rights: true`, never inferred from the node's name,
/// kind, or prompt. Absent the key, a node has no commit rights.
fn node_has_commit_rights(node: &LoopNode) -> bool {
    node.config.get("commit_rights").and_then(Value::as_bool) == Some(true)
}

/// Whether a graph opts into commit-rights enforcement (B37) — i.e. whether
/// any of its nodes declares `commit_rights: true`.
///
/// Enforcement is per-graph opt-in on purpose. A graph that designates nobody
/// cannot be told apart from one whose committer simply predates this key, so
/// enforcing there would fail exactly the node the graph relies on to land
/// work. Once ONE node declares the right, the graph's intent is unambiguous
/// and every other node in it is held to it.
fn graph_enforces_commit_rights(nodes: &[LoopNode]) -> bool {
    nodes.iter().any(node_has_commit_rights)
}

/// A pre/post `git rev-parse HEAD` comparison around one node's execution
/// (B37). Deterministic and cheap — two `git rev-parse` calls, no LLM — and
/// entirely absent (`begin` yields `None`) for the cases that must not change
/// behavior: graphs that designate no committer, the designated committer
/// itself, and non-git workdirs.
///
/// A prompt-level "you have no commit rights" rule has been broken by three
/// different models (a haiku implementer on 2026-07-16, an
/// opencode/mimo-v2.5-free implementer committing `21109a5` on 2026-07-18
/// against a caps-locked HARD RULE). The cascade is what makes it costly: the
/// work lands in history, the reviewer ensemble then reviews an empty or
/// formatting-only working diff, and the graph's quality gate silently becomes
/// a no-op.
struct CommitRightsWatch {
    head_before: String,
}

impl CommitRightsWatch {
    /// Start watching, or `None` if there is nothing to watch.
    async fn begin(enforced: bool, may_commit: bool, workdir: &str) -> Option<Self> {
        if !enforced || may_commit {
            return None;
        }
        capture_workdir_head(workdir)
            .await
            .map(|head_before| Self { head_before })
    }

    /// The HEAD the watched node left behind, if it moved history. `None`
    /// when HEAD is unchanged — including a node that edited files without
    /// committing, which is the normal, unaffected case.
    async fn violation(&self, workdir: &str) -> Option<String> {
        let head_after = capture_workdir_head(workdir).await?;
        (head_after != self.head_before).then_some(head_after)
    }
}

/// Turn a detected commit-rights violation into the node's actual result: a
/// deterministic FAIL carrying the reason, routed through the fail edge like
/// any other failure.
///
/// Deliberately reports and routes only. The engine never reverts, resets, or
/// otherwise rewrites the user's history — an automatic `git reset` on a
/// misbehaving agent risks destroying real work (the commit is frequently the
/// *correct* work, made by the wrong node), and history rewriting is not
/// something an unattended daemon should ever do on its own. Undoing is left
/// to the operator, who now has both hashes in the run output.
fn commit_rights_failure(
    label: &str,
    node_id: &str,
    head_before: &str,
    head_after: &str,
    node_output: Value,
) -> NodeExecution {
    let message = format!(
        "{label} committed but has no commit rights: HEAD moved {head_before} -> {head_after}. \
         Only a node configured with `commit_rights: true` may move git history. \
         The commit was left in place — undo it yourself if it does not belong there."
    );
    // Built by hand rather than with `json!` so the node's own output moves
    // in whole — it can be a full review document, and this runs on a path
    // that is already reporting a failure.
    let mut output = serde_json::Map::new();
    output.insert(
        "commit_rights_violation".to_string(),
        serde_json::json!({
            "node": label,
            "node_id": node_id,
            "head_before": head_before,
            "head_after": head_after,
            "message": message,
        }),
    );
    output.insert("node_output".to_string(), node_output);

    NodeExecution {
        output: Value::Object(output),
        summary: message,
        status: LoopRunStatus::Fail,
    }
}

/// The ensemble id `node_id` belongs to, whether as a member or as the join
/// itself — used both to resume onto [`SpecCursor::Ensemble`] (rather than a
/// single member node) and to key the iteration budget per-ensemble instead
/// of per-member.
fn ensemble_owning_node(node_id: &str, ensembles: &[EnsembleDetails]) -> Option<String> {
    ensembles
        .iter()
        .find(|details| {
            details.ensemble.join_node_id == node_id
                || details
                    .members
                    .iter()
                    .any(|member| member.node_id == node_id)
        })
        .map(|details| details.ensemble.id.clone())
}

fn resolve_spec_start(
    nodes: &[LoopNode],
    edges: &[LoopEdge],
    spec: &LoopSpec,
    existing_runs: &[LoopNodeRun],
    ensembles: &[EnsembleDetails],
) -> Result<(SpecCursor, Option<Value>, HashMap<String, usize>)> {
    if spec.status == LoopSpecStatus::Running {
        if let Some(last_run) = existing_runs.last() {
            // An ensemble's N members (+ its join) each get their own
            // `loop_runs` row sharing the same `iteration` number — dedupe
            // on (budget key, iteration) so a bounce into the ensemble
            // still counts as exactly one iteration (F1), not N+1.
            let mut iterations = HashMap::<String, usize>::new();
            let mut seen = HashSet::<(String, i64)>::new();
            for run in existing_runs {
                let key = match ensemble_owning_node(&run.node_id, ensembles) {
                    Some(ensemble_id) => format!("ensemble:{ensemble_id}"),
                    None => run.node_id.clone(),
                };
                if seen.insert((key.clone(), run.iteration)) {
                    *iterations.entry(key).or_insert(0) += 1;
                }
            }
            let cursor = match ensemble_owning_node(&last_run.node_id, ensembles) {
                Some(ensemble_id) => SpecCursor::Ensemble(ensemble_id),
                None => SpecCursor::Node(last_run.node_id.clone()),
            };
            return Ok((cursor, last_run.input.clone(), iterations));
        }
    }

    Ok((
        SpecCursor::Node(find_entry_node(nodes, edges, &spec.name)?),
        None,
        HashMap::new(),
    ))
}

/// Spawns `command` under a non-login POSIX `sh`. Using `-c` (not `-l`) means
/// no `/etc/profile` or `~/.profile` is sourced, so the process sees exactly
/// the daemon's own environment plus whatever env vars the engine explicitly
/// sets on the `Command` before spawning — never a user's shell-startup PATH
/// overrides or side effects.
#[cfg(unix)]
fn shell_command(command: &str) -> Command {
    let mut process = Command::new("sh");
    process.arg("-c").arg(command);
    // Own process-group leader so a hung/timed-out check can be `killpg`'d
    // along with anything it forks (B12) — see `CliStrategy::build_command`
    // for the same treatment on agent nodes.
    process.process_group(0);
    process.kill_on_drop(true);
    process
}

#[cfg(windows)]
fn shell_command(command: &str) -> Command {
    let mut process = Command::new("cmd");
    process.arg("/C").arg(command);
    process.kill_on_drop(true);
    process
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::notification_service::DefaultNotificationService;
    use tempfile::{tempdir, TempDir};

    fn loop_fixture() -> Result<(TempDir, Arc<Database>, LoopEngine, String, String)> {
        let dir = tempdir()?;
        let db = Arc::new(Database::new(&dir.path().join("test.db"))?);
        let lp = crate::domain::loops::Loop {
            id: "wf-test".to_string(),
            name: "Loop".to_string(),
            description: None,
            workdir: dir.path().to_string_lossy().to_string(),
            status: LoopStatus::Draft,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            active_run_pool_id: None,
            on_completed: None,
        };
        let spec = crate::domain::loops::LoopSpec {
            id: "spec-test".to_string(),
            loop_id: Some(lp.id.clone()),
            name: "Spec".to_string(),
            description: Some(
                "Functional Requirements:\n- A\n\nNon-Functional Requirements:\n- B\n\nObjective:\n- C\n\nConstraints:\n- D\n\nGuidelines:\n- E\n\nIn Scope:\n- F\n\nOut of Scope:\n- G".to_string(),
            ),
            position: 1,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        };

        db.insert_loop(&lp)?;
        db.insert_loop_spec(&spec)?;

        Ok((
            dir,
            Arc::clone(&db),
            LoopEngine::new(db, Arc::new(DefaultNotificationService)),
            lp.id,
            spec.id,
        ))
    }

    #[derive(Debug, Clone, PartialEq)]
    enum RecordedNotification {
        LoopStarted {
            loop_name: String,
            spec_count: usize,
            resumed: bool,
            first_pending: Option<String>,
        },
        SpecCompleted {
            loop_name: String,
            spec_name: String,
            done: usize,
            total: usize,
            next_pending: Option<String>,
        },
        LoopFinishedCompleted {
            loop_name: String,
            done: usize,
            total: usize,
            hook_launched: bool,
        },
        LoopFinishedFailed {
            loop_name: String,
            spec_name: String,
        },
        LoopFinishedBlocked {
            loop_name: String,
            summary: String,
        },
        CompletionHookFailed {
            loop_name: String,
            error: String,
        },
    }

    #[derive(Default)]
    struct MockNotificationService {
        events: std::sync::Mutex<Vec<RecordedNotification>>,
    }

    impl MockNotificationService {
        fn events(&self) -> Vec<RecordedNotification> {
            self.events.lock().unwrap().clone()
        }
    }

    impl NotificationService for MockNotificationService {
        fn notify_task_completed(&self, _task_id: &str, _success: bool, _exit_code: Option<i32>) {}
        fn notify_task_failed(&self, _task_id: &str, _exit_code: i32, _error_msg: &str) {}
        fn notify_watcher_triggered(&self, _watcher_id: &str, _path: &str, _event: &str) {}
        fn notify_agent_failed(&self, _agent_id: &str, _cli: &str, _exit_code: i32, _output: &str) {
        }
        fn notify_nursery_failed(&self, _error_msg: &str) {}

        fn notify_loop_started(
            &self,
            loop_name: &str,
            spec_count: usize,
            resumed: bool,
            first_pending: Option<&str>,
        ) {
            self.events
                .lock()
                .unwrap()
                .push(RecordedNotification::LoopStarted {
                    loop_name: loop_name.to_string(),
                    spec_count,
                    resumed,
                    first_pending: first_pending.map(str::to_string),
                });
        }

        fn notify_spec_completed(
            &self,
            loop_name: &str,
            spec_name: &str,
            done: usize,
            total: usize,
            next_pending: Option<&str>,
        ) {
            self.events
                .lock()
                .unwrap()
                .push(RecordedNotification::SpecCompleted {
                    loop_name: loop_name.to_string(),
                    spec_name: spec_name.to_string(),
                    done,
                    total,
                    next_pending: next_pending.map(str::to_string),
                });
        }

        fn notify_loop_finished(&self, loop_name: &str, outcome: LoopFinishOutcome<'_>) {
            let event = match outcome {
                LoopFinishOutcome::Completed {
                    done,
                    total,
                    hook_launched,
                } => RecordedNotification::LoopFinishedCompleted {
                    loop_name: loop_name.to_string(),
                    done,
                    total,
                    hook_launched,
                },
                LoopFinishOutcome::Failed { spec_name } => {
                    RecordedNotification::LoopFinishedFailed {
                        loop_name: loop_name.to_string(),
                        spec_name: spec_name.to_string(),
                    }
                }
                LoopFinishOutcome::Blocked { summary } => {
                    RecordedNotification::LoopFinishedBlocked {
                        loop_name: loop_name.to_string(),
                        summary: summary.to_string(),
                    }
                }
            };
            self.events.lock().unwrap().push(event);
        }

        fn notify_loop_completion_hook_failed(&self, loop_name: &str, error: &str) {
            self.events
                .lock()
                .unwrap()
                .push(RecordedNotification::CompletionHookFailed {
                    loop_name: loop_name.to_string(),
                    error: error.to_string(),
                });
        }
    }

    type MockLoopFixture = (
        TempDir,
        Arc<Database>,
        LoopEngine,
        Arc<MockNotificationService>,
        String,
        String,
    );

    fn loop_fixture_with_mock() -> Result<MockLoopFixture> {
        let dir = tempdir()?;
        let db = Arc::new(Database::new(&dir.path().join("test.db"))?);
        let lp = crate::domain::loops::Loop {
            id: "wf-test".to_string(),
            name: "Loop".to_string(),
            description: None,
            workdir: dir.path().to_string_lossy().to_string(),
            status: LoopStatus::Draft,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            active_run_pool_id: None,
            on_completed: None,
        };
        let spec = crate::domain::loops::LoopSpec {
            id: "spec-test".to_string(),
            loop_id: Some(lp.id.clone()),
            name: "Spec".to_string(),
            description: Some("Objective:\n- test".to_string()),
            position: 1,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        };

        db.insert_loop(&lp)?;
        db.insert_loop_spec(&spec)?;

        let notifications = Arc::new(MockNotificationService::default());
        Ok((
            dir,
            Arc::clone(&db),
            LoopEngine::new(
                db,
                Arc::clone(&notifications) as Arc<dyn NotificationService>,
            ),
            notifications,
            lp.id,
            spec.id,
        ))
    }

    fn init_git_repo(path: &std::path::Path) {
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(path)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .status()
                .expect("git command failed to run");
            assert!(status.success(), "git {:?} failed", args);
        };
        run(&["init", "-q"]);
        // Repo-local identity so a node that shells out to `git commit`
        // works regardless of the machine's global git config.
        run(&["config", "user.name", "Test"]);
        run(&["config", "user.email", "test@example.com"]);
        std::fs::write(path.join("README.md"), "test").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
    }

    fn git_head(path: &std::path::Path) -> String {
        let output = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(path)
            .output()
            .expect("git rev-parse failed to run");
        assert!(output.status.success(), "git rev-parse HEAD failed");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[tokio::test]
    async fn loop_engine_captures_spec_start_head_for_git_workdir() {
        let (dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();
        init_git_repo(dir.path());
        let expected_head = git_head(dir.path());

        db.insert_loop_node(&LoopNode {
            id: "node-check".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        engine.run_loop(loop_id, None, None).await.unwrap();

        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        assert_eq!(
            spec.spec_start_head.as_deref(),
            Some(expected_head.as_str())
        );
    }

    #[tokio::test]
    async fn loop_engine_substitutes_spec_start_head_in_check_command() {
        let (dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();
        init_git_repo(dir.path());

        db.insert_loop_node(&LoopNode {
            id: "node-check".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "test \"$(git rev-parse HEAD)\" = \"{{spec_start_head}}\"",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Completed);
        assert_eq!(spec.status, LoopSpecStatus::Completed);
    }

    #[tokio::test]
    async fn loop_engine_substitutes_empty_spec_start_head_for_non_git_workdir() {
        let (_dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();

        db.insert_loop_node(&LoopNode {
            id: "node-check".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "test -z \"{{spec_start_head}}\"",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Completed);
        assert_eq!(spec.status, LoopSpecStatus::Completed);
        assert_eq!(spec.spec_start_head, None);
    }

    // ── B37: node-level commit rights, enforced by the engine ────────────

    /// A node whose command runs in the workdir. `commit_rights` is attached
    /// verbatim when `Some`, and omitted entirely when `None` — the two cases
    /// that decide whether the graph opts into enforcement at all.
    fn rights_node(
        spec_id: &str,
        id: &str,
        command: &str,
        commit_rights: Option<bool>,
        position: i64,
    ) -> LoopNode {
        let mut config = serde_json::json!({
            "command": command,
            "success_condition": "exit_code_0"
        });
        if let Some(rights) = commit_rights {
            config["commit_rights"] = serde_json::json!(rights);
        }
        LoopNode {
            id: id.to_string(),
            spec_id: Some(spec_id.to_string()),
            loop_id: None,
            name: id.to_string(),
            kind: LoopNodeKind::Check,
            config,
            position,
            created_at: chrono::Utc::now(),
        }
    }

    const COMMIT_CMD: &str = "git commit -q --allow-empty -m 'unauthorized'";

    #[tokio::test]
    async fn node_without_commit_rights_that_commits_fails_and_routes_via_fail_edge() {
        let (dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();
        init_git_repo(dir.path());
        let head_before = git_head(dir.path());

        // `worker` exits 0 and reports success — but commits. `committer`
        // (never reached) is what makes this graph enforce commit rights.
        db.insert_loop_node(&rights_node(&spec_id, "worker", COMMIT_CMD, None, 1))
            .unwrap();
        db.insert_loop_node(&rights_node(&spec_id, "committer", "true", Some(true), 2))
            .unwrap();
        db.insert_loop_node(&rights_node(&spec_id, "triage", "true", None, 3))
            .unwrap();
        db.insert_loop_edge(&LoopEdge {
            id: "e-pass".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            from_node: "worker".to_string(),
            to_node: "committer".to_string(),
            condition: crate::domain::loops::LoopEdgeCondition::Pass,
        })
        .unwrap();
        db.insert_loop_edge(&LoopEdge {
            id: "e-fail".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            from_node: "worker".to_string(),
            to_node: "triage".to_string(),
            condition: crate::domain::loops::LoopEdgeCondition::Fail,
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let runs = db.list_loop_runs_for_spec(&spec_id).unwrap();
        let worker = runs.iter().find(|r| r.node_id == "worker").unwrap();
        assert_eq!(
            worker.status,
            LoopRunStatus::Fail,
            "committing without commit rights must be a deterministic fail"
        );
        let violation = worker
            .output
            .as_ref()
            .and_then(|o| o.get("commit_rights_violation"))
            .expect("the violation must be recorded on the run's output");
        assert_eq!(
            violation.get("head_before").and_then(|v| v.as_str()),
            Some(head_before.as_str())
        );
        assert_eq!(
            violation.get("head_after").and_then(|v| v.as_str()),
            Some(git_head(dir.path()).as_str())
        );
        assert!(
            violation
                .get("message")
                .and_then(|v| v.as_str())
                .is_some_and(|m| m.contains("no commit rights")),
            "the reason must be stated in plain words"
        );

        // Routed through the fail edge like any other failure — never
        // silently accepted, and never onward to the committer.
        assert!(
            runs.iter().any(|r| r.node_id == "triage"),
            "the fail edge must have been taken"
        );
        assert!(
            !runs.iter().any(|r| r.node_id == "committer"),
            "the pass edge must not have been taken"
        );

        // The unauthorized commit is reported, never rewritten away.
        assert_ne!(git_head(dir.path()), head_before);
    }

    #[tokio::test]
    async fn designated_committer_that_commits_passes() {
        let (dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();
        init_git_repo(dir.path());

        db.insert_loop_node(&rights_node(
            &spec_id,
            "committer",
            COMMIT_CMD,
            Some(true),
            1,
        ))
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        assert_eq!(spec.status, LoopSpecStatus::Completed);
        let runs = db.list_loop_runs_for_spec(&spec_id).unwrap();
        assert_eq!(runs[0].status, LoopRunStatus::Pass);
        assert!(runs[0]
            .output
            .as_ref()
            .is_none_or(|o| o.get("commit_rights_violation").is_none()));
    }

    #[tokio::test]
    async fn node_that_changes_files_without_committing_is_unaffected() {
        let (dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();
        init_git_repo(dir.path());
        let head_before = git_head(dir.path());

        db.insert_loop_node(&rights_node(
            &spec_id,
            "worker",
            "printf changed > README.md",
            None,
            1,
        ))
        .unwrap();
        db.insert_loop_node(&rights_node(&spec_id, "committer", "true", Some(true), 2))
            .unwrap();
        db.insert_loop_edge(&LoopEdge {
            id: "e-pass".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            from_node: "worker".to_string(),
            to_node: "committer".to_string(),
            condition: crate::domain::loops::LoopEdgeCondition::Pass,
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        assert_eq!(spec.status, LoopSpecStatus::Completed);
        let runs = db.list_loop_runs_for_spec(&spec_id).unwrap();
        let worker = runs.iter().find(|r| r.node_id == "worker").unwrap();
        assert_eq!(worker.status, LoopRunStatus::Pass);
        assert_eq!(git_head(dir.path()), head_before);
    }

    #[tokio::test]
    async fn graph_designating_no_committer_keeps_todays_behavior() {
        // Enforcement is opt-in per graph: without a single `commit_rights`
        // node there is no way to tell the designated committer from a
        // violator, so an existing graph must behave exactly as before.
        let (dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();
        init_git_repo(dir.path());

        db.insert_loop_node(&rights_node(&spec_id, "worker", COMMIT_CMD, None, 1))
            .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        assert_eq!(spec.status, LoopSpecStatus::Completed);
        let runs = db.list_loop_runs_for_spec(&spec_id).unwrap();
        assert_eq!(runs[0].status, LoopRunStatus::Pass);
    }

    #[tokio::test]
    async fn commit_rights_enforcement_skips_non_git_workdirs() {
        let (_dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();

        db.insert_loop_node(&rights_node(&spec_id, "worker", "true", None, 1))
            .unwrap();
        db.insert_loop_node(&rights_node(&spec_id, "committer", "true", Some(true), 2))
            .unwrap();
        db.insert_loop_edge(&LoopEdge {
            id: "e-pass".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            from_node: "worker".to_string(),
            to_node: "committer".to_string(),
            condition: crate::domain::loops::LoopEdgeCondition::Pass,
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        assert_eq!(spec.status, LoopSpecStatus::Completed);
    }

    #[test]
    fn commit_rights_are_explicit_configuration_only() {
        let named_committer = rights_node("s", "Commit and push", "true", None, 1);
        assert!(
            !node_has_commit_rights(&named_committer),
            "a node's name must never grant it commit rights"
        );
        assert!(!graph_enforces_commit_rights(std::slice::from_ref(
            &named_committer
        )));

        let designated = rights_node("s", "committer", "true", Some(true), 1);
        assert!(node_has_commit_rights(&designated));
        assert!(graph_enforces_commit_rights(&[named_committer, designated]));

        assert!(!node_has_commit_rights(&rights_node(
            "s",
            "n",
            "true",
            Some(false),
            1
        )));
    }

    // ── B10: spec_start_head frozen-per-attempt, amend-proof ─────────────

    #[tokio::test]
    async fn loop_engine_resume_dispatch_reuses_persisted_spec_start_head_even_if_stale() {
        // `resume_background` (`loop_continue`, autorun's plain resume) is
        // the one path allowed to inherit a spec's already-persisted
        // baseline while it's still `running` — this is what makes resuming
        // a daemon-restart-interrupted spec keep comparing against the HEAD
        // it started at, not whatever HEAD happens to be at resume time.
        let (dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();
        init_git_repo(dir.path());

        db.update_loop_spec_status(&spec_id, LoopSpecStatus::Running, None, None)
            .unwrap();
        db.set_loop_spec_start_head(&spec_id, Some("deadbeef"))
            .unwrap();

        db.insert_loop_node(&LoopNode {
            id: "node-check".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "test \"{{spec_start_head}}\" = \"deadbeef\" && printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        engine
            .run_loop_dispatch(loop_id.clone(), None, None, true)
            .await
            .unwrap();

        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        assert_eq!(spec.status, LoopSpecStatus::Completed);
        assert_eq!(
            spec.spec_start_head.as_deref(),
            Some("deadbeef"),
            "a resumed dispatch must reuse the persisted baseline, not recapture"
        );
    }

    #[tokio::test]
    async fn loop_engine_fresh_relaunch_recaptures_even_when_spec_still_shows_running() {
        // The 2026-07-11 incident: a spec left `running` by a prior,
        // never-reset attempt kept having its stale baseline reused across
        // later relaunches ("12:56 relaunch compared against b9e8928, the
        // HEAD of the ORIGINAL 07:58 launch, two relaunches earlier"). A
        // fresh dispatch — `loop_run`/`start_background_run`, including
        // relaunching a `paused` loop directly instead of via
        // `loop_continue` — must never inherit that: it re-captures
        // regardless of the spec's leftover `running` status.
        let (dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();
        init_git_repo(dir.path());
        let real_head = git_head(dir.path());

        // Simulate the abandoned attempt: still `running`, with a baseline
        // that has nothing to do with the current, real HEAD.
        db.update_loop_spec_status(&spec_id, LoopSpecStatus::Running, None, None)
            .unwrap();
        db.set_loop_spec_start_head(&spec_id, Some("deadbeef"))
            .unwrap();

        db.insert_loop_node(&LoopNode {
            id: "node-check".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "test \"{{spec_start_head}}\" != \"deadbeef\" && printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        // `run_loop` is the fresh-dispatch entry point (same one `loop_run`
        // uses) — no `is_resume` flag reaches it.
        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        assert_eq!(spec.status, LoopSpecStatus::Completed);
        assert_eq!(
            spec.spec_start_head.as_deref(),
            Some(real_head.as_str()),
            "a fresh relaunch must recapture the real current HEAD, not inherit the stale value"
        );
    }

    #[tokio::test]
    async fn loop_engine_check_retry_baseline_stays_frozen_across_reviewer_commits() {
        // Placeholder captured at spec entry must be stable across every
        // node execution of that attempt, including check retries after
        // reviewer iterations — even while the reviewer keeps committing new
        // work in between.
        let (dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();
        init_git_repo(dir.path());
        let initial_head = git_head(dir.path());
        let baseline_log = dir.path().join("baseline.log");
        let counter = dir.path().join("counter");

        // "review": always commits a bit more work and passes.
        db.insert_loop_node(&LoopNode {
            id: "node-review".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "review".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "echo more >> work.txt && git add -A && git commit -q -m more && printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        // "check": logs the substituted baseline every time it runs, and
        // only passes on its third invocation — forcing review<->check to
        // iterate a few times within the same spec attempt.
        db.insert_loop_node(&LoopNode {
            id: "node-check".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": format!(
                    "echo '{{{{spec_start_head}}}}' >> \"{log}\"; n=$(cat \"{counter}\" 2>/dev/null || echo 0); n=$((n+1)); echo $n > \"{counter}\"; [ \"$n\" -ge 3 ] && printf APPROVED || exit 1",
                    log = baseline_log.display(),
                    counter = counter.display(),
                ),
                "success_condition": "exit_code_0"
            }),
            position: 2,
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        db.insert_loop_edge(&LoopEdge {
            id: "edge-review-check".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            from_node: "node-review".to_string(),
            to_node: "node-check".to_string(),
            condition: crate::domain::loops::LoopEdgeCondition::Pass,
        })
        .unwrap();
        db.insert_loop_edge(&LoopEdge {
            id: "edge-check-review".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            from_node: "node-check".to_string(),
            to_node: "node-review".to_string(),
            condition: crate::domain::loops::LoopEdgeCondition::Fail,
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        assert_eq!(spec.status, LoopSpecStatus::Completed);
        assert_eq!(spec.spec_start_head.as_deref(), Some(initial_head.as_str()));

        let logged = std::fs::read_to_string(&baseline_log).unwrap();
        let lines: Vec<&str> = logged.lines().collect();
        assert_eq!(
            lines.len(),
            3,
            "check must have retried exactly twice before passing"
        );
        for line in lines {
            assert_eq!(
                line, initial_head,
                "the substituted baseline must never move across retries, even though \
                 the reviewer committed between every one of them"
            );
        }
    }

    #[tokio::test]
    async fn loop_engine_regression_reviewer_commit_between_implement_and_check_uses_precommit_baseline(
    ) {
        // Regression for the 18:19 incident shape: the reviewer commits as
        // part of this spec's own work, then the check node runs — it must
        // evaluate against the baseline captured *before* that commit and
        // pass, never see its own attempt's commit as "no movement".
        let (dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();
        init_git_repo(dir.path());
        let pre_commit_head = git_head(dir.path());

        db.insert_loop_node(&LoopNode {
            id: "node-implement".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "implement".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "echo change >> work.txt && git add -A && git commit -q -m change && printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        db.insert_loop_node(&LoopNode {
            id: "node-check".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "test \"$(git rev-parse HEAD)\" != \"{{spec_start_head}}\" && printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 2,
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        db.insert_loop_edge(&LoopEdge {
            id: "edge-implement-check".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            from_node: "node-implement".to_string(),
            to_node: "node-check".to_string(),
            condition: crate::domain::loops::LoopEdgeCondition::Pass,
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Completed);
        assert_eq!(spec.status, LoopSpecStatus::Completed);
        assert_eq!(
            spec.spec_start_head.as_deref(),
            Some(pre_commit_head.as_str()),
            "the baseline must stay the pre-commit HEAD, never re-resolved after the \
             reviewer's own commit"
        );

        let runs = db.list_loop_runs_for_spec(&spec_id).unwrap();
        let check_run = runs
            .iter()
            .find(|r| r.node_id == "node-check")
            .expect("check node must have run");
        assert_eq!(check_run.status, LoopRunStatus::Pass);
    }

    #[tokio::test]
    async fn loop_engine_completes_check_and_gate_spec() {
        let (_dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();
        let check = LoopNode {
            id: "node-check".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        };
        let gate = LoopNode {
            id: "node-gate".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "gate".to_string(),
            kind: LoopNodeKind::Gate,
            config: serde_json::json!({
                "evaluate": "output_contains",
                "value": "APPROVED",
                "pass_route": "next_spec"
            }),
            position: 2,
            created_at: chrono::Utc::now(),
        };

        db.insert_loop_node(&check).unwrap();
        db.insert_loop_node(&gate).unwrap();
        db.insert_loop_edge(&LoopEdge {
            id: "edge-pass".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            from_node: check.id.clone(),
            to_node: gate.id.clone(),
            condition: crate::domain::loops::LoopEdgeCondition::Pass,
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        let runs = db.list_loop_runs_for_spec(&spec_id).unwrap();

        assert_eq!(lp.status, LoopStatus::Completed);
        assert_eq!(spec.status, LoopSpecStatus::Completed);
        assert_eq!(runs.len(), 2);
    }

    #[tokio::test]
    async fn loop_engine_fails_spec_when_check_fails_without_route() {
        let (_dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();
        db.insert_loop_node(&LoopNode {
            id: "node-check".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "exit 1",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();

        assert_eq!(lp.status, LoopStatus::Failed);
        assert_eq!(spec.status, LoopSpecStatus::Failed);
    }

    #[test]
    fn resolve_spec_start_retries_last_running_node() {
        let spec = LoopSpec {
            id: "spec".to_string(),
            loop_id: Some("wf".to_string()),
            name: "Spec".to_string(),
            description: None,
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
        let details = crate::domain::loops::LoopSpecDetails {
            spec: spec.clone(),
            nodes: vec![LoopNode {
                id: "node-1".to_string(),
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                name: "Node".to_string(),
                kind: LoopNodeKind::Check,
                config: serde_json::json!({"command": "true"}),
                position: 1,
                created_at: chrono::Utc::now(),
            }],
            edges: vec![],
        };
        let runs = vec![LoopNodeRun {
            id: "run".to_string(),
            loop_id: "wf".to_string(),
            spec_id: spec.id.clone(),
            node_id: "node-1".to_string(),
            status: LoopRunStatus::Fail,
            input: Some(serde_json::json!({"previous": "context"})),
            output: None,
            started_at: chrono::Utc::now(),
            completed_at: Some(chrono::Utc::now()),
            iteration: 1,
            pid: None,
            boot_id: None,
            session_id: None,
        }];

        let (cursor, previous_output, iterations) =
            resolve_spec_start(&details.nodes, &details.edges, &spec, &runs, &[]).unwrap();

        assert_eq!(cursor, SpecCursor::Node("node-1".to_string()));
        assert_eq!(iterations.get("node-1"), Some(&1));
        assert_eq!(
            previous_output.and_then(|value| value.get("previous").cloned()),
            Some(serde_json::json!("context"))
        );
    }

    #[test]
    fn resolve_spec_start_resets_iterations_for_fresh_spec() {
        let spec = LoopSpec {
            id: "spec".to_string(),
            loop_id: Some("wf".to_string()),
            name: "Spec".to_string(),
            description: None,
            position: 1,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        };
        let details = crate::domain::loops::LoopSpecDetails {
            spec: spec.clone(),
            nodes: vec![LoopNode {
                id: "node-1".to_string(),
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                name: "Node".to_string(),
                kind: LoopNodeKind::Check,
                config: serde_json::json!({"command": "true"}),
                position: 1,
                created_at: chrono::Utc::now(),
            }],
            edges: vec![],
        };
        // Historical runs from a previous attempt at this spec: 10 failed
        // iterations that exhausted the budget last time around.
        let runs: Vec<LoopNodeRun> = (0..10)
            .map(|i| LoopNodeRun {
                id: format!("run-{i}"),
                loop_id: "wf".to_string(),
                spec_id: spec.id.clone(),
                node_id: "node-1".to_string(),
                status: LoopRunStatus::Fail,
                input: None,
                output: None,
                started_at: chrono::Utc::now(),
                completed_at: Some(chrono::Utc::now()),
                iteration: i + 1,
                pid: None,
                boot_id: None,
                session_id: None,
            })
            .collect();

        let (cursor, previous_output, iterations) =
            resolve_spec_start(&details.nodes, &details.edges, &spec, &runs, &[]).unwrap();

        assert_eq!(cursor, SpecCursor::Node("node-1".to_string()));
        assert!(previous_output.is_none());
        assert!(iterations.is_empty());
    }

    #[test]
    fn find_entry_node_picks_lowest_position_in_retry_cycle() {
        // implement (pos 1) <-> review (pos 2): every node has an incoming
        // edge, so there is no source node. The entry must be the designated
        // start (lowest position), not an error.
        let spec = LoopSpec {
            id: "spec".to_string(),
            loop_id: Some("wf".to_string()),
            name: "Spec".to_string(),
            description: None,
            position: 1,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        };
        let node = |id: &str, position: i64| LoopNode {
            id: id.to_string(),
            spec_id: Some(spec.id.clone()),
            loop_id: None,
            name: id.to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({}),
            position,
            created_at: chrono::Utc::now(),
        };
        let edge = |id: &str, from: &str, to: &str, condition| LoopEdge {
            id: id.to_string(),
            spec_id: Some(spec.id.clone()),
            loop_id: None,
            from_node: from.to_string(),
            to_node: to.to_string(),
            condition,
        };
        let details = crate::domain::loops::LoopSpecDetails {
            spec: spec.clone(),
            // Insert review before implement so the result cannot depend on
            // node ordering — only on position.
            nodes: vec![node("review", 2), node("implement", 1)],
            edges: vec![
                edge(
                    "e1",
                    "implement",
                    "review",
                    crate::domain::loops::LoopEdgeCondition::Always,
                ),
                edge(
                    "e2",
                    "review",
                    "implement",
                    crate::domain::loops::LoopEdgeCondition::Fail,
                ),
            ],
        };

        assert_eq!(
            find_entry_node(&details.nodes, &details.edges, &details.spec.name).unwrap(),
            "implement"
        );
    }

    #[test]
    fn render_agent_prompt_includes_reporting_contract() {
        let lp = crate::domain::loops::Loop {
            id: "wf".to_string(),
            name: "Loop".to_string(),
            description: None,
            workdir: "/tmp/project".to_string(),
            status: LoopStatus::Draft,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            active_run_pool_id: None,
            on_completed: None,
        };
        let spec = LoopSpec {
            id: "spec".to_string(),
            loop_id: Some("wf".to_string()),
            name: "Spec".to_string(),
            description: Some("Do the thing".to_string()),
            position: 1,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        };
        let node = LoopNode {
            id: "node-1".to_string(),
            spec_id: Some("spec".to_string()),
            loop_id: None,
            name: "Agent".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({}),
            position: 1,
            created_at: chrono::Utc::now(),
        };

        let prompt = render_agent_prompt(
            &lp,
            &spec,
            &node,
            "{{spec_content}}",
            Some(&serde_json::json!({"feedback":"ok"})),
            &lp.workdir,
            "run-1",
        );

        assert!(prompt.contains("loop_complete_node"));
        assert!(prompt.contains("loop_report_blocker"));
        assert!(prompt.contains("run_id=\"run-1\""));
        assert!(prompt.contains("Do the thing"));
        assert!(prompt.contains("\"feedback\": \"ok\""));
    }

    #[test]
    fn bound_previous_feedback_leaves_small_text_unchanged() {
        let text = "small feedback".to_string();
        assert_eq!(bound_previous_feedback(text.clone()), text);
    }

    #[test]
    fn bound_previous_feedback_elides_marker_only_above_threshold() {
        let at_threshold = "a".repeat(PREVIOUS_FEEDBACK_ELISION_THRESHOLD);
        assert!(!bound_previous_feedback(at_threshold).contains("bytes elided"));

        let over_threshold = "a".repeat(PREVIOUS_FEEDBACK_ELISION_THRESHOLD + 1);
        let bounded = bound_previous_feedback(over_threshold);
        assert!(bounded.contains("bytes elided"));
        assert!(bounded.len() < PREVIOUS_FEEDBACK_ELISION_THRESHOLD + 200);
    }

    #[test]
    fn render_agent_prompt_elides_huge_previous_feedback() {
        // A prior node (e.g. a `cargo test` check) can emit a full log many
        // times over the elision threshold — the real incident this fixes
        // was a 65KB test log blowing up argv. The full text must never be
        // interpolated whole; the marker must show it was cut.
        let lp = crate::domain::loops::Loop {
            id: "wf".to_string(),
            name: "Loop".to_string(),
            description: None,
            workdir: "/tmp/project".to_string(),
            status: LoopStatus::Draft,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            active_run_pool_id: None,
            on_completed: None,
        };
        let spec = LoopSpec {
            id: "spec".to_string(),
            loop_id: Some("wf".to_string()),
            name: "Spec".to_string(),
            description: Some("Do the thing".to_string()),
            position: 1,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        };
        let node = LoopNode {
            id: "node-1".to_string(),
            spec_id: Some("spec".to_string()),
            loop_id: None,
            name: "Agent".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({}),
            position: 1,
            created_at: chrono::Utc::now(),
        };
        let huge_log = "x".repeat(500 * 1024);

        let prompt = render_agent_prompt(
            &lp,
            &spec,
            &node,
            "{{previous_feedback}}",
            Some(&serde_json::json!({"stdout": huge_log})),
            &lp.workdir,
            "run-1",
        );

        assert!(prompt.contains("bytes elided"));
        assert!(prompt.len() < 600 * 1024);
    }

    fn sample_agent_node() -> LoopNode {
        LoopNode {
            id: "node-agent".to_string(),
            spec_id: Some("spec".to_string()),
            loop_id: None,
            name: "Agent".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({}),
            position: 1,
            created_at: chrono::Utc::now(),
        }
    }

    /// A throwaway `Database` for `run_agent_process` tests that only need
    /// somewhere to (harmlessly) persist a pid — no loop/spec/node rows are
    /// inserted, so `set_loop_run_pid`/`update_loop_run_result` against the
    /// fake `run_id` below just affect zero rows.
    fn test_db() -> (TempDir, Database) {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        (dir, db)
    }

    fn sample_strategy(binary: &str) -> crate::domain::cli_strategy::CliStrategy {
        crate::domain::cli_strategy::CliStrategy {
            binary: binary.to_string(),
            headless_mode: String::new(),
            model_flag: None,
            supports_working_dir: false,
            working_dir_flag: None,
            env_vars: HashMap::new(),
            prompt_via_stdin: false,
            session_id_set_flag: None,
            session_list_cmd: None,
            session_list_format_args: None,
            session_id_pattern: None,
            session_resume_cmd: None,
        }
    }

    /// Seed a spec + agent node + `running` run under `loop_id` so
    /// `run_agent_process` tests can read the run row back (`loop_runs`
    /// enforces foreign keys). Returns the inserted node.
    fn seed_agent_run(db: &Database, loop_id: &str, run_id: &str) -> LoopNode {
        let spec = standalone_spec("sid-spec", 1);
        db.insert_loop_spec(&spec).unwrap();
        let mut node = sample_agent_node();
        node.spec_id = Some(spec.id.clone());
        db.insert_loop_node(&node).unwrap();
        db.insert_loop_run(&LoopNodeRun {
            id: run_id.to_string(),
            loop_id: loop_id.to_string(),
            spec_id: spec.id,
            node_id: node.id.clone(),
            status: LoopRunStatus::Running,
            input: None,
            output: None,
            started_at: chrono::Utc::now(),
            completed_at: None,
            iteration: 1,
            pid: None,
            boot_id: None,
            session_id: None,
        })
        .unwrap();
        node
    }

    #[tokio::test]
    async fn run_agent_process_records_set_at_spawn_session_id() {
        let (_dir, db, _engine, loop_id) = bare_loop_fixture().unwrap();
        let node = seed_agent_run(&db, &loop_id, "run-sid");
        let cli = Cli::new("test-cli");
        let mut strategy = sample_strategy("/bin/echo");
        strategy.session_id_set_flag = Some("--session-id".to_string());

        run_agent_process(
            &db, "run-sid", &cli, &strategy, &node, "prompt", None, "/tmp", 1, None,
        )
        .await
        .unwrap();

        let run = db.get_loop_run("run-sid").unwrap().unwrap();
        let sid = run
            .session_id
            .expect("set-at-spawn platform must record a session id");
        uuid::Uuid::parse_str(&sid).expect("recorded session id must be a uuid");
    }

    #[tokio::test]
    async fn run_agent_process_leaves_session_id_null_without_set_flag() {
        let (_dir, db, _engine, loop_id) = bare_loop_fixture().unwrap();
        let node = seed_agent_run(&db, &loop_id, "run-nosid");
        let cli = Cli::new("test-cli");
        let strategy = sample_strategy("/bin/echo");

        run_agent_process(
            &db,
            "run-nosid",
            &cli,
            &strategy,
            &node,
            "prompt",
            None,
            "/tmp",
            1,
            None,
        )
        .await
        .unwrap();

        let run = db.get_loop_run("run-nosid").unwrap().unwrap();
        assert_eq!(
            run.session_id, None,
            "no set flag and no capture: session_id must stay NULL"
        );
    }

    /// Writes a fake session-aware CLI as an executable shell script with two
    /// modes dispatched on its first arg. `list` prints the ids in
    /// `$STATEFILE` as opencode-family JSON (`[{"id":"..."}]`), or exits 1
    /// when `$FAIL_LIST` is set. `run` is the agent invocation (headless mode
    /// is `run`); it appends `$APPEND_ID` to `$STATEFILE` when set (simulating
    /// the CLI creating a new session), then exits 0. This lets one binary
    /// serve as both the agent process and the session list the capture diffs
    /// — exactly how the real CLIs behave.
    fn write_fake_session_cli(dir: &std::path::Path) -> std::path::PathBuf {
        let script = dir.join("fake-session-cli");
        std::fs::write(
            &script,
            r#"#!/bin/sh
case "$1" in
  list)
    [ -n "$FAIL_LIST" ] && exit 1
    printf '['
    sep=""
    if [ -f "$STATEFILE" ]; then
      while IFS= read -r line; do
        [ -z "$line" ] && continue
        printf '%s{"id":"%s"}' "$sep" "$line"
        sep=","
      done < "$STATEFILE"
    fi
    printf ']\n'
    ;;
  run)
    [ -n "$APPEND_ID" ] && echo "$APPEND_ID" >> "$STATEFILE"
    echo done
    ;;
esac
"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// Strategy for the fake session CLI: `run` headless mode, `list` session
    /// command, opencode-family id pattern. `env` carries the fixture's
    /// `STATEFILE`/`APPEND_ID`/`FAIL_LIST` toggles to both invocations.
    fn fake_session_strategy(
        binary: &std::path::Path,
        env: HashMap<String, String>,
    ) -> crate::domain::cli_strategy::CliStrategy {
        crate::domain::cli_strategy::CliStrategy {
            binary: binary.to_string_lossy().to_string(),
            headless_mode: "run".to_string(),
            model_flag: None,
            supports_working_dir: false,
            working_dir_flag: None,
            env_vars: env,
            prompt_via_stdin: false,
            session_id_set_flag: None,
            session_list_cmd: Some("list".to_string()),
            session_list_format_args: None,
            session_id_pattern: Some(r#""id"\s*:\s*"([^"]+)""#.to_string()),
            session_resume_cmd: None,
        }
    }

    #[tokio::test]
    async fn run_agent_process_captures_new_session_id_after_run() {
        let (_dir, db, _engine, loop_id) = bare_loop_fixture().unwrap();
        let node = seed_agent_run(&db, &loop_id, "run-cap");
        let scratch = tempdir().unwrap();
        let statefile = scratch.path().join("sessions");
        std::fs::write(&statefile, "ses_pre_existing\n").unwrap();
        let script = write_fake_session_cli(scratch.path());

        let mut env = HashMap::new();
        env.insert("STATEFILE".to_string(), statefile.to_string_lossy().into());
        env.insert("APPEND_ID".to_string(), "ses_brand_new".to_string());
        let strategy = fake_session_strategy(&script, env);
        let cli = Cli::new("fake");

        let execution = run_agent_process(
            &db, "run-cap", &cli, &strategy, &node, "prompt", None, "/tmp", 1, None,
        )
        .await
        .unwrap();

        assert_eq!(execution.status, LoopRunStatus::Pass);
        let run = db.get_loop_run("run-cap").unwrap().unwrap();
        assert_eq!(
            run.session_id.as_deref(),
            Some("ses_brand_new"),
            "the single new session in the after-list must be attributed to the run"
        );
    }

    #[tokio::test]
    async fn run_agent_process_no_new_session_leaves_session_id_null() {
        let (_dir, db, _engine, loop_id) = bare_loop_fixture().unwrap();
        let node = seed_agent_run(&db, &loop_id, "run-nonew");
        let scratch = tempdir().unwrap();
        let statefile = scratch.path().join("sessions");
        std::fs::write(&statefile, "ses_pre_existing\n").unwrap();
        let script = write_fake_session_cli(scratch.path());

        // No APPEND_ID: the run creates no session, so the diff is empty.
        let mut env = HashMap::new();
        env.insert("STATEFILE".to_string(), statefile.to_string_lossy().into());
        let strategy = fake_session_strategy(&script, env);
        let cli = Cli::new("fake");

        let execution = run_agent_process(
            &db,
            "run-nonew",
            &cli,
            &strategy,
            &node,
            "prompt",
            None,
            "/tmp",
            1,
            None,
        )
        .await
        .unwrap();

        assert_eq!(execution.status, LoopRunStatus::Pass);
        let run = db.get_loop_run("run-nonew").unwrap().unwrap();
        assert_eq!(
            run.session_id, None,
            "no new session in the diff must leave session_id NULL"
        );
    }

    #[tokio::test]
    async fn run_agent_process_list_failure_leaves_null_and_verdict_unaffected() {
        let (_dir, db, _engine, loop_id) = bare_loop_fixture().unwrap();
        let node = seed_agent_run(&db, &loop_id, "run-listfail");
        let scratch = tempdir().unwrap();
        let statefile = scratch.path().join("sessions");
        std::fs::write(&statefile, "ses_pre_existing\n").unwrap();
        let script = write_fake_session_cli(scratch.path());

        // FAIL_LIST makes every `list` invocation exit non-zero. Capture must
        // silently give up (NULL) while the run's own verdict is untouched.
        let mut env = HashMap::new();
        env.insert("STATEFILE".to_string(), statefile.to_string_lossy().into());
        env.insert("APPEND_ID".to_string(), "ses_brand_new".to_string());
        env.insert("FAIL_LIST".to_string(), "1".to_string());
        let strategy = fake_session_strategy(&script, env);
        let cli = Cli::new("fake");

        let execution = run_agent_process(
            &db,
            "run-listfail",
            &cli,
            &strategy,
            &node,
            "prompt",
            None,
            "/tmp",
            1,
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            execution.status,
            LoopRunStatus::Pass,
            "a broken session-list command must never change the run verdict"
        );
        let run = db.get_loop_run("run-listfail").unwrap().unwrap();
        assert_eq!(run.session_id, None, "capture failure must leave NULL");
    }

    #[tokio::test]
    async fn run_agent_process_set_at_spawn_skips_list_capture() {
        // A platform with BOTH a set-at-spawn flag and a list command must
        // use set-at-spawn (uuid, known before spawn) and never run the list
        // diff — set-at-spawn takes strict precedence.
        let (_dir, db, _engine, loop_id) = bare_loop_fixture().unwrap();
        let node = seed_agent_run(&db, &loop_id, "run-precedence");
        let scratch = tempdir().unwrap();
        let statefile = scratch.path().join("sessions");
        std::fs::write(&statefile, "ses_pre_existing\n").unwrap();
        let script = write_fake_session_cli(scratch.path());

        let mut env = HashMap::new();
        env.insert("STATEFILE".to_string(), statefile.to_string_lossy().into());
        env.insert("APPEND_ID".to_string(), "ses_brand_new".to_string());
        let mut strategy = fake_session_strategy(&script, env);
        strategy.session_id_set_flag = Some("--session-id".to_string());
        let cli = Cli::new("fake");

        run_agent_process(
            &db,
            "run-precedence",
            &cli,
            &strategy,
            &node,
            "prompt",
            None,
            "/tmp",
            1,
            None,
        )
        .await
        .unwrap();

        let run = db.get_loop_run("run-precedence").unwrap().unwrap();
        let sid = run.session_id.expect("set-at-spawn must record an id");
        uuid::Uuid::parse_str(&sid)
            .expect("recorded id must be the set-at-spawn uuid, not a listed session id");
    }

    // ── RS2: resume on fail-edge bounce ─────────────────────────────────

    /// Fake CLI that records its full argv (one arg per line, `===` between
    /// invocations) to `$ARGV_FILE`. When resuming (its argv contains
    /// `$RESUME_FLAG`) and `$FAIL_RESUME` is set, it exits nonzero at once to
    /// simulate a rejected resume flag; otherwise it prints `done` and exits 0.
    fn write_argv_echo_cli(dir: &std::path::Path) -> std::path::PathBuf {
        let script = dir.join("argv-echo-cli");
        std::fs::write(
            &script,
            r#"#!/bin/sh
{
  for a in "$@"; do printf '%s\n' "$a"; done
  printf '===\n'
} >> "$ARGV_FILE"
is_resume=0
for a in "$@"; do [ "$a" = "$RESUME_FLAG" ] && is_resume=1; done
if [ "$is_resume" = "1" ] && [ -n "$FAIL_RESUME" ]; then
  echo "resume rejected" >&2
  exit 1
fi
echo done
"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// Write a `~/.canopy/config.toml` fixture holding a single CLI named
    /// `resume-cli` backed by the argv-echo script, so `Cli::strategy()`
    /// resolves it under a [`HomeGuard`].
    fn write_resume_cli_home(cli: crate::domain::cli_config::CliConfig) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        let canopy_dir = home.path().join(".canopy");
        std::fs::create_dir_all(&canopy_dir).unwrap();
        let config = crate::domain::canopy_config::CanopyConfig {
            configured_at: Some(chrono::Utc::now().to_rfc3339()),
            clis: vec![cli],
            ..Default::default()
        };
        config.save(&canopy_dir).unwrap();
        home
    }

    /// Build the `resume-cli` [`CliConfig`] for the argv-echo binary.
    fn argv_cli_config(
        binary: &std::path::Path,
        env: HashMap<String, String>,
        resume: Option<&str>,
        set_flag: Option<&str>,
        list_cmd: Option<&str>,
    ) -> crate::domain::cli_config::CliConfig {
        crate::domain::cli_config::CliConfig {
            name: "resume-cli".into(),
            binary: binary.to_string_lossy().into_owned(),
            headless_mode: "run".into(),
            env_vars: env,
            session_resume_cmd: resume.map(str::to_string),
            session_id_set_flag: set_flag.map(str::to_string),
            session_list_cmd: list_cmd.map(str::to_string),
            session_list_format_args: list_cmd.map(|_| "--format json".to_string()),
            session_id_pattern: list_cmd.map(|_| r#""id"\s*:\s*"([^"]+)""#.to_string()),
            ..Default::default()
        }
    }

    /// An agent node driven by the `resume-cli` platform, with optional extra
    /// config keys merged in (e.g. `{"resume": false}`).
    fn resume_agent_node(extra: Value) -> LoopNode {
        let mut config = serde_json::json!({ "platform": "resume-cli" });
        if let Value::Object(extra) = extra {
            for (k, v) in extra {
                config[k] = v;
            }
        }
        LoopNode {
            id: "node-impl".to_string(),
            spec_id: Some("sid-spec".to_string()),
            loop_id: None,
            name: "impl".to_string(),
            kind: LoopNodeKind::Agent,
            config,
            position: 1,
            created_at: chrono::Utc::now(),
        }
    }

    /// Drive `execute_agent_node` once against the argv-echo CLI, returning the
    /// node execution, the (re-read) run row, and the recorded argv text.
    async fn run_resume_agent_node(
        node_extra: Value,
        resume_session_id: Option<&str>,
        set_flag: Option<&str>,
        list_cmd: Option<&str>,
        fail_resume: bool,
    ) -> (NodeExecution, LoopNodeRun, String) {
        let (dir, db, _engine, loop_id) = bare_loop_fixture().unwrap();
        let argv_file = dir.path().join("argv.log");
        let script = write_argv_echo_cli(dir.path());
        let mut env = HashMap::new();
        env.insert(
            "ARGV_FILE".to_string(),
            argv_file.to_string_lossy().into_owned(),
        );
        env.insert("RESUME_FLAG".to_string(), "--resume".to_string());
        if fail_resume {
            env.insert("FAIL_RESUME".to_string(), "1".to_string());
        }
        let cli = argv_cli_config(&script, env, Some("--resume"), set_flag, list_cmd);
        let home = write_resume_cli_home(cli);
        let node = resume_agent_node(node_extra);

        // Acquire the HomeGuard lock BEFORE seeding the run row: the
        // resume-failure fallback compares the run's age against
        // `infra_crash_max_seconds`, so `started_at` must be stamped right
        // before the spawn. Seeding first and then blocking on the (shared,
        // serialized) HomeGuard under a loaded full suite could otherwise
        // inflate the measured age past the window and defeat the fallback.
        let guard = HomeGuard::set(home.path());
        seed_agent_run(&db, &loop_id, "run-r");
        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        let spec = db.get_loop_spec("sid-spec").unwrap().unwrap();
        let execution = execute_agent_node(
            &db,
            &lp,
            &spec,
            &node,
            None,
            "run-r",
            dir.path().to_str().unwrap(),
            resume_session_id,
        )
        .await
        .unwrap();
        drop(guard);

        let run = db.get_loop_run("run-r").unwrap().unwrap();
        let argv = std::fs::read_to_string(&argv_file).unwrap_or_default();
        (execution, run, argv)
    }

    #[tokio::test]
    async fn resume_uses_resume_flag_and_incremental_prompt() {
        let (execution, run, argv) =
            run_resume_agent_node(Value::Null, Some("ses_prev"), None, None, false).await;
        assert_eq!(execution.status, LoopRunStatus::Pass);
        assert!(argv.contains("--resume"), "resume flag must be passed");
        assert!(argv.contains("ses_prev"), "the resumed id must be passed");
        // Incremental prompt: the resume continuation marker, but NOT the full
        // cold `[SPEC]` block the session already holds.
        assert!(argv.contains("[CONTINUE]"), "resume prompt must be sent");
        assert!(
            !argv.contains("# [SPEC]"),
            "a resume must not re-render the full spec block"
        );
        // The resumed run records the SAME session id; capture is skipped.
        assert_eq!(run.session_id.as_deref(), Some("ses_prev"));
    }

    #[tokio::test]
    async fn resume_false_config_forces_cold_start() {
        let (execution, _run, argv) = run_resume_agent_node(
            serde_json::json!({ "resume": false }),
            Some("ses_prev"),
            None,
            None,
            false,
        )
        .await;
        assert_eq!(execution.status, LoopRunStatus::Pass);
        assert!(
            !argv.contains("--resume"),
            "resume:false must force a cold start"
        );
        assert!(
            argv.contains("# [SPEC]"),
            "cold start renders the full spec"
        );
    }

    #[tokio::test]
    async fn first_visit_without_session_is_cold() {
        // No resume_session_id offered (first visit to the node) → cold.
        let (execution, _run, argv) =
            run_resume_agent_node(Value::Null, None, None, None, false).await;
        assert_eq!(execution.status, LoopRunStatus::Pass);
        assert!(!argv.contains("--resume"));
        assert!(argv.contains("# [SPEC]"));
    }

    #[tokio::test]
    async fn resume_failure_falls_back_to_cold_and_verdict_from_cold_run() {
        // The resume attempt is rejected at spawn (FAIL_RESUME); the engine
        // must fall back to a cold start whose (passing) verdict the node uses.
        let (execution, _run, argv) =
            run_resume_agent_node(Value::Null, Some("ses_prev"), None, None, true).await;
        assert_eq!(
            execution.status,
            LoopRunStatus::Pass,
            "verdict must come from the cold fallback run"
        );
        assert!(argv.contains("--resume"), "the resume attempt ran first");
        assert!(
            argv.contains("# [SPEC]"),
            "the cold fallback ran and rendered the full spec"
        );
    }

    #[tokio::test]
    async fn resume_skips_set_at_spawn_and_list_capture() {
        // Platform has BOTH a set-at-spawn flag and a session-list command, so
        // a cold start would either mint a uuid or diff a session list. On a
        // resume, neither may run: the run row must keep exactly the resumed id.
        let (execution, run, argv) = run_resume_agent_node(
            Value::Null,
            Some("ses_prev"),
            Some("--set"),
            Some("list"),
            false,
        )
        .await;
        assert_eq!(execution.status, LoopRunStatus::Pass);
        assert_eq!(
            run.session_id.as_deref(),
            Some("ses_prev"),
            "resumed run keeps the resumed id — no set-at-spawn uuid, no listed id"
        );
        assert!(
            !argv.contains("--set"),
            "set-at-spawn flag must not be injected on a resumed spawn"
        );
    }

    #[tokio::test]
    async fn bounce_resumes_second_visit_after_cold_first_visit() {
        // Integration: an agent node that passes into a check that fails once
        // (bouncing back to the agent) then passes. The agent's first visit is
        // cold and captures a session (via set-at-spawn); the bounce resumes it.
        let (dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();
        let argv_file = dir.path().join("argv.log");
        let counter = dir.path().join("counter");
        let script = write_argv_echo_cli(dir.path());
        let mut env = HashMap::new();
        env.insert(
            "ARGV_FILE".to_string(),
            argv_file.to_string_lossy().into_owned(),
        );
        env.insert("RESUME_FLAG".to_string(), "--resume".to_string());
        // set-at-spawn capture on the cold run gives the bounce something to
        // resume; resume-by-id enables the bounce itself.
        let cli = argv_cli_config(&script, env, Some("--resume"), Some("--set"), None);
        let home = write_resume_cli_home(cli);

        db.insert_loop_node(&LoopNode {
            id: "node-impl".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "impl".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({ "platform": "resume-cli" }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        // Gate check: fails its first run (bounce), passes the second.
        db.insert_loop_node(&LoopNode {
            id: "node-gate".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "gate".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": format!(
                    "n=$(cat \"{c}\" 2>/dev/null || echo 0); n=$((n+1)); echo $n > \"{c}\"; [ \"$n\" -ge 2 ] && printf APPROVED || exit 1",
                    c = counter.display(),
                ),
                "success_condition": "exit_code_0"
            }),
            position: 2,
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        db.insert_loop_edge(&LoopEdge {
            id: "edge-impl-gate".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            from_node: "node-impl".to_string(),
            to_node: "node-gate".to_string(),
            condition: crate::domain::loops::LoopEdgeCondition::Pass,
        })
        .unwrap();
        db.insert_loop_edge(&LoopEdge {
            id: "edge-gate-impl".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            from_node: "node-gate".to_string(),
            to_node: "node-impl".to_string(),
            condition: crate::domain::loops::LoopEdgeCondition::Fail,
        })
        .unwrap();

        let guard = HomeGuard::set(home.path());
        engine.run_loop(loop_id.clone(), None, None).await.unwrap();
        drop(guard);

        // Two runs of the agent node: the first cold, the second resumed.
        let mut impl_runs: Vec<LoopNodeRun> = db
            .list_loop_runs_for_spec(&spec_id)
            .unwrap()
            .into_iter()
            .filter(|r| r.node_id == "node-impl")
            .collect();
        impl_runs.sort_by_key(|r| r.started_at);
        assert_eq!(impl_runs.len(), 2, "agent node must have run twice");
        let first_sid = impl_runs[0]
            .session_id
            .clone()
            .expect("cold first run captures a set-at-spawn session id");
        assert_eq!(
            impl_runs[1].session_id.as_deref(),
            Some(first_sid.as_str()),
            "the bounce must resume — and record — the first run's session id"
        );

        let argv = std::fs::read_to_string(&argv_file).unwrap();
        assert!(
            argv.contains("--set"),
            "the first (cold) visit sets a session id at spawn"
        );
        assert!(
            argv.contains("--resume") && argv.contains(&first_sid),
            "the second visit resumes the first run's session by id"
        );
    }

    /// Build a loop with a single loop-level agent node backed by the argv-echo
    /// `resume-cli` (set-at-spawn capture + resume-by-id), queue `member_specs`
    /// into `pool-1`, run the pool, and hand back the argv log path plus the db.
    /// Each grouped member shares the one loop-level node id `node-impl`, which
    /// is exactly what a warm-context queue looks like: several small specs
    /// draining one loop graph.
    async fn run_grouped_pool(
        member_specs: &[(&str, Option<&str>)],
    ) -> (Arc<Database>, std::path::PathBuf) {
        let (dir, db, engine, loop_id) = bare_loop_fixture().unwrap();
        let argv_file = dir.path().join("argv.log");
        let script = write_argv_echo_cli(dir.path());
        let mut env = HashMap::new();
        env.insert(
            "ARGV_FILE".to_string(),
            argv_file.to_string_lossy().into_owned(),
        );
        env.insert("RESUME_FLAG".to_string(), "--resume".to_string());
        // set-at-spawn capture on a cold run; resume-by-id for the handoff.
        let cli = argv_cli_config(&script, env, Some("--resume"), Some("--set"), None);
        let home = write_resume_cli_home(cli);

        for (position, (spec_id, _)) in member_specs.iter().enumerate() {
            db.insert_loop_spec(&standalone_spec(spec_id, (position as i64) + 1))
                .unwrap();
        }
        insert_pool_with_grouped_members(&db, "pool-1", member_specs);

        // Loop-level agent node: every pool member with no graph of its own
        // drains this shared node, so grouped siblings share the node id.
        db.insert_loop_node(&LoopNode {
            id: "node-impl".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "impl".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({ "platform": "resume-cli" }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        let guard = HomeGuard::set(home.path());
        engine
            .run_loop(loop_id.clone(), Some("pool-1".to_string()), None)
            .await
            .unwrap();
        drop(guard);

        // Keep `dir` alive until after the run (workdir + argv log live in it).
        let argv_file = std::fs::canonicalize(&argv_file).unwrap_or(argv_file);
        std::mem::forget(dir);
        (db, argv_file)
    }

    fn impl_session(db: &Database, spec_id: &str) -> Option<String> {
        db.list_loop_runs_for_spec(spec_id)
            .unwrap()
            .into_iter()
            .find(|r| r.node_id == "node-impl")
            .and_then(|r| r.session_id)
    }

    #[tokio::test]
    async fn grouped_spec_resumes_prior_siblings_session() {
        // RS3 positive handoff: spec-a cold-starts and captures a session;
        // spec-b in the same group resumes it on its first node run and records
        // the SAME session id — the ONE exception to RS2's "first visit cold".
        let (db, argv_file) =
            run_grouped_pool(&[("spec-a", Some("ctx")), ("spec-b", Some("ctx"))]).await;

        assert_eq!(
            db.get_loop_spec("spec-a").unwrap().unwrap().status,
            LoopSpecStatus::Completed
        );
        assert_eq!(
            db.get_loop_spec("spec-b").unwrap().unwrap().status,
            LoopSpecStatus::Completed
        );

        let sid_a = impl_session(&db, "spec-a").expect("spec-a cold-start captures a session");
        let sid_b = impl_session(&db, "spec-b").expect("spec-b records a session");
        assert_eq!(
            sid_b, sid_a,
            "the grouped sibling must continue — and record — spec-a's session"
        );

        let argv = std::fs::read_to_string(&argv_file).unwrap();
        assert!(
            argv.contains("--resume") && argv.contains(&sid_a),
            "spec-b's first visit resumes spec-a's session by id"
        );
    }

    #[tokio::test]
    async fn ungrouped_specs_never_cross_resume() {
        // RS7: with no group, each spec cold-starts — spec-b mints its OWN
        // set-at-spawn session and never touches spec-a's.
        let (db, argv_file) = run_grouped_pool(&[("spec-a", None), ("spec-b", None)]).await;

        let sid_a = impl_session(&db, "spec-a").expect("spec-a captures a session");
        let sid_b = impl_session(&db, "spec-b").expect("spec-b captures its own session");
        assert_ne!(
            sid_a, sid_b,
            "ungrouped specs must not share a session across the queue"
        );

        let argv = std::fs::read_to_string(&argv_file).unwrap();
        assert!(
            !argv.contains("--resume"),
            "no resume flag may appear for an ungrouped queue"
        );
    }

    #[tokio::test]
    async fn run_agent_process_reports_spawn_failure_as_node_fail_not_hard_error() {
        // Simulates the E2BIG incident: the process fails to spawn. This
        // must come back as a failed node run (routed like any other node
        // failure) rather than an `Err` that would abort the whole loop.
        let (_dir, db) = test_db();
        let cli = Cli::new("test-cli");
        let mut strategy = sample_strategy("/nonexistent/somewhere/definitely-not-a-binary");
        strategy.prompt_via_stdin = false;
        let node = sample_agent_node();

        let execution = run_agent_process(
            &db, "run-test", &cli, &strategy, &node, "prompt", None, "/tmp", 1, None,
        )
        .await
        .expect("spawn failure must not propagate as a hard error");

        assert_eq!(execution.status, LoopRunStatus::Fail);
        assert!(execution.summary.contains("failed to spawn"));
        assert!(execution.output.get("error").is_some());
    }

    /// B39: a permanent spawn failure (missing binary) must produce a
    /// `spawn_permanent` flag in the output and must NOT be classified as an
    /// infra crash — exactly one run row, no `infra_attempt` marker.
    #[tokio::test]
    async fn b39_permanent_spawn_failure_skips_infra_retry() {
        let (_dir, db) = test_db();
        let cli = Cli::new("test-cli");
        let strategy = sample_strategy("/nonexistent/somewhere/definitely-not-a-binary");
        let node = sample_agent_node();

        let execution = run_agent_process(
            &db, "run-test", &cli, &strategy, &node, "prompt", None, "/tmp", 1, None,
        )
        .await
        .expect("spawn failure must not propagate as a hard error");

        assert_eq!(execution.status, LoopRunStatus::Fail);
        assert!(
            execution
                .output
                .get("spawn_permanent")
                .and_then(Value::as_bool)
                == Some(true),
            "missing binary must set spawn_permanent flag"
        );
        assert!(
            execution.output.get("infra_attempt").is_none(),
            "permanent failure must not carry infra_attempt marker"
        );
        assert!(
            execution.output.get("infra_retry_skipped").is_some(),
            "the run row must record why the retry was skipped"
        );

        let run = LoopNodeRun {
            id: "run-test".to_string(),
            loop_id: "loop1".to_string(),
            spec_id: "spec1".to_string(),
            node_id: node.id.clone(),
            status: LoopRunStatus::Running,
            input: None,
            output: None,
            started_at: chrono::Utc::now(),
            completed_at: None,
            iteration: 1,
            pid: None,
            boot_id: None,
            session_id: None,
        };
        assert!(
            !is_infra_crash(&node, &execution, &run, 0, 3, 60),
            "permanent spawn failure must not be classified as infra crash"
        );
    }

    /// B39, the case actually observed in loop f9d070bc: the CLI's configured
    /// binary resolves to nothing, so the failure happens while *building* the
    /// command rather than at spawn. It is just as permanent, and must be
    /// classified as such without matching on the rendered message.
    #[tokio::test]
    async fn b39_unresolvable_binary_is_permanent_at_build_time() {
        let (_dir, db) = test_db();
        let cli = Cli::new("test-cli");
        // Bare name (not an absolute path) that is in neither PATH nor
        // `~/.<binary>/bin/<binary>` — the mimo failure mode.
        let strategy = sample_strategy("canopy-b39-definitely-not-installed");
        let node = sample_agent_node();

        let execution = run_agent_process(
            &db, "run-test", &cli, &strategy, &node, "prompt", None, "/tmp", 1, None,
        )
        .await
        .expect("spawn failure must not propagate as a hard error");

        assert_eq!(execution.status, LoopRunStatus::Fail);
        assert_eq!(
            execution
                .output
                .get("spawn_permanent")
                .and_then(Value::as_bool),
            Some(true),
            "an unresolvable binary must be classified permanent at build time"
        );
        assert!(
            execution.output.get("infra_retry_skipped").is_some(),
            "the run row must record why the retry was skipped"
        );
    }

    /// B39: a transient spawn failure (command-build error, not NotFound)
    /// still qualifies for infra-crash retry.
    #[tokio::test]
    async fn b39_transient_spawn_failure_still_retried() {
        let output = serde_json::json!({
            "kind": "agent",
            "node_id": "node-agent",
            "cli": "test-cli",
            "error": "some transient build error",
        });
        let execution = NodeExecution {
            status: LoopRunStatus::Fail,
            output,
            summary: "failed to spawn".to_string(),
        };
        let node = sample_agent_node();
        let run = LoopNodeRun {
            id: "run1".to_string(),
            loop_id: "loop1".to_string(),
            spec_id: "spec1".to_string(),
            node_id: node.id.clone(),
            status: LoopRunStatus::Running,
            input: None,
            output: None,
            started_at: chrono::Utc::now(),
            completed_at: None,
            iteration: 1,
            pid: None,
            boot_id: None,
            session_id: None,
        };
        assert!(
            is_infra_crash(&node, &execution, &run, 0, 3, 60),
            "transient spawn failure without spawn_permanent must still be retried"
        );
    }

    /// SpawnError::from_build treats a typed binary-resolution failure as
    /// permanent and every other command-build error as transient.
    #[test]
    fn spawn_error_from_build_classification() {
        let unresolvable = SpawnError::from_build(
            &crate::domain::cli_strategy::BinaryResolutionError::NotFound {
                binary: "mimo".to_string(),
                fallback: std::path::PathBuf::from("/home/u/.mimo/bin/mimo"),
            }
            .into(),
        );
        assert_eq!(
            unresolvable.permanent_reason,
            Some("cli binary could not be resolved")
        );

        let other = SpawnError::from_build(&anyhow::anyhow!(
            "CLI 'x' has no session_resume_cmd; cannot resume by id"
        ));
        assert!(
            other.permanent_reason.is_none(),
            "an untyped build error must stay transient"
        );
    }

    /// SpawnError::from_io correctly classifies NotFound and PermissionDenied
    /// as permanent (with a reason), and other kinds as transient.
    #[test]
    fn spawn_error_from_io_classification() {
        let not_found = SpawnError::from_io(&std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "binary not found",
        ));
        assert!(
            not_found.permanent_reason.is_some(),
            "NotFound must be permanent"
        );

        let perm_denied = SpawnError::from_io(&std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "permission denied",
        ));
        assert!(
            perm_denied.permanent_reason.is_some(),
            "PermissionDenied must be permanent"
        );

        let broken_pipe = SpawnError::from_io(&std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "broken pipe",
        ));
        assert!(
            broken_pipe.permanent_reason.is_none(),
            "BrokenPipe must be transient"
        );

        let other = SpawnError::from_io(&std::io::Error::other("something else"));
        assert!(other.permanent_reason.is_none(), "Other must be transient");
    }

    #[tokio::test]
    async fn run_agent_process_delivers_multi_hundred_kb_prompt_via_stdin() {
        // Feedback arrives already truncated per `bound_previous_feedback`,
        // but the transport itself must have no input-size cliff either —
        // stdin-mode CLIs must handle an oversized prompt without E2BIG.
        let (_dir, db) = test_db();
        let cli = Cli::new("test-cli");
        let mut strategy = sample_strategy("/bin/cat");
        strategy.prompt_via_stdin = true;
        let node = sample_agent_node();
        let huge_prompt = "y".repeat(500 * 1024);

        let execution = run_agent_process(
            &db,
            "run-test",
            &cli,
            &strategy,
            &node,
            &huge_prompt,
            None,
            "/tmp",
            1,
            None,
        )
        .await
        .unwrap();

        assert_eq!(execution.status, LoopRunStatus::Pass);
        assert_eq!(
            execution.output.get("stdout").and_then(Value::as_str),
            Some(huge_prompt.as_str())
        );
    }

    /// When the composed prompt exceeds `ARGV_SAFETY_THRESHOLD` and the CLI
    /// doesn't have `prompt_via_stdin` set, the loop engine must override the
    /// strategy to force stdin delivery — preventing E2BIG.
    #[tokio::test]
    async fn large_prompt_overrides_strategy_to_stdin() {
        let cli = Cli::new("test-cli");
        // Strategy starts with prompt_via_stdin = false (the problematic
        // default that caused the original E2BIG incident).
        let mut strategy = sample_strategy("/bin/cat");
        assert!(!strategy.prompt_via_stdin);

        // Simulate the override that execute_agent_node applies.
        let large_prompt = "z".repeat(ARGV_SAFETY_THRESHOLD + 1);
        if large_prompt.len() > ARGV_SAFETY_THRESHOLD && !strategy.prompt_via_stdin {
            strategy = strategy.with_stdin_forced();
        }
        assert!(
            strategy.prompt_via_stdin,
            "stdin must be forced for large prompts"
        );

        let node = sample_agent_node();
        let (_dir, db) = test_db();
        let execution = run_agent_process(
            &db,
            "run-test",
            &cli,
            &strategy,
            &node,
            &large_prompt,
            None,
            "/tmp",
            1,
            None,
        )
        .await
        .unwrap();
        assert_eq!(execution.status, LoopRunStatus::Pass);
        assert_eq!(
            execution.output.get("stdout").and_then(Value::as_str),
            Some(large_prompt.as_str())
        );
    }

    /// A prompt just under the threshold must NOT trigger the override —
    /// argv delivery stays active for small prompts.
    #[test]
    fn small_prompt_does_not_force_stdin() {
        let mut strategy = sample_strategy("/bin/cat");
        assert!(!strategy.prompt_via_stdin);

        let small_prompt = "a".repeat(ARGV_SAFETY_THRESHOLD);
        if small_prompt.len() > ARGV_SAFETY_THRESHOLD && !strategy.prompt_via_stdin {
            strategy = strategy.with_stdin_forced();
        }
        assert!(
            !strategy.prompt_via_stdin,
            "stdin must NOT be forced for small prompts"
        );
    }

    #[test]
    fn select_next_step_dedupes_identical_edges_to_same_target() {
        let edge = |id: &str, to: &str, condition| LoopEdge {
            id: id.to_string(),
            spec_id: Some("spec".to_string()),
            loop_id: None,
            from_node: "implement".to_string(),
            to_node: to.to_string(),
            condition,
        };
        let edges = vec![
            edge(
                "e1",
                "review",
                crate::domain::loops::LoopEdgeCondition::Always,
            ),
            edge(
                "e2",
                "review",
                crate::domain::loops::LoopEdgeCondition::Always,
            ),
        ];

        let next = select_next_step(&edges, &[], "implement", LoopRunStatus::Pass).unwrap();

        let sel = next.unwrap();
        assert_eq!(sel.cursor, SpecCursor::Node("review".to_string()));
        assert_eq!(
            sel.edge_condition,
            crate::domain::loops::LoopEdgeCondition::Always
        );
    }

    #[test]
    fn select_next_step_errors_on_distinct_targets() {
        let edge = |id: &str, to: &str, condition| LoopEdge {
            id: id.to_string(),
            spec_id: Some("spec".to_string()),
            loop_id: None,
            from_node: "implement".to_string(),
            to_node: to.to_string(),
            condition,
        };
        let edges = vec![
            edge(
                "e1",
                "review",
                crate::domain::loops::LoopEdgeCondition::Always,
            ),
            edge(
                "e2",
                "deploy",
                crate::domain::loops::LoopEdgeCondition::Always,
            ),
        ];

        let err = select_next_step(&edges, &[], "implement", LoopRunStatus::Pass).unwrap_err();

        assert!(err.to_string().contains("ambiguous outgoing edges"));
    }

    #[test]
    fn select_next_step_resolves_ensemble_fan_out_from_ambiguous_edges() {
        // Three edges from the same predecessor, all targeting distinct
        // nodes — normally ambiguous, but here the distinct targets are
        // exactly one ensemble's full member set, so this must resolve to
        // the ensemble instead of erroring.
        let edge = |id: &str, to: &str| LoopEdge {
            id: id.to_string(),
            spec_id: Some("spec".to_string()),
            loop_id: None,
            from_node: "kickoff".to_string(),
            to_node: to.to_string(),
            condition: crate::domain::loops::LoopEdgeCondition::Always,
        };
        let edges = vec![edge("e1", "m1"), edge("e2", "m2"), edge("e3", "m3")];
        let ensembles = vec![ensemble_details_fixture(
            "ens1",
            "join1",
            &["m1", "m2", "m3"],
        )];

        let next = select_next_step(&edges, &ensembles, "kickoff", LoopRunStatus::Pass).unwrap();

        let sel = next.unwrap();
        assert_eq!(sel.cursor, SpecCursor::Ensemble("ens1".to_string()));
        assert_eq!(
            sel.edge_condition,
            crate::domain::loops::LoopEdgeCondition::Always
        );
    }

    fn ensemble_details_fixture(
        ensemble_id: &str,
        join_node_id: &str,
        member_node_ids: &[&str],
    ) -> crate::domain::loops::EnsembleDetails {
        crate::domain::loops::EnsembleDetails {
            ensemble: crate::domain::loops::Ensemble {
                id: ensemble_id.to_string(),
                spec_id: Some("spec".to_string()),
                loop_id: None,
                name: "Proposers".to_string(),
                prompt_template: "{{spec_content}}".to_string(),
                join_node_id: join_node_id.to_string(),
                entry_from_node: "kickoff".to_string(),
                entry_condition: crate::domain::loops::LoopEdgeCondition::Always,
                min_pass: member_node_ids.len() as i64,
                straggler_timeout_minutes: None,
                timeout_minutes: 30,
                on_pass_to: "arbiter".to_string(),
                on_fail_to: None,
                created_at: chrono::Utc::now(),
            },
            members: member_node_ids
                .iter()
                .enumerate()
                .map(|(i, node_id)| crate::domain::loops::EnsembleMember {
                    ensemble_id: ensemble_id.to_string(),
                    node_id: node_id.to_string(),
                    position: i as i64,
                    platform: "claude".to_string(),
                    model: None,
                })
                .collect(),
        }
    }

    fn second_spec(loop_id: &str, id: &str, position: i64) -> LoopSpec {
        LoopSpec {
            id: id.to_string(),
            loop_id: Some(loop_id.to_string()),
            name: format!("Spec {id}"),
            description: Some(
                "Functional Requirements:\n- A\n\nNon-Functional Requirements:\n- B\n\nObjective:\n- C\n\nConstraints:\n- D\n\nGuidelines:\n- E\n\nIn Scope:\n- F\n\nOut of Scope:\n- G".to_string(),
            ),
            position,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        }
    }

    #[tokio::test]
    async fn loop_engine_runs_loop_level_graph_across_two_specs() {
        // Neither spec has nodes of its own; both walk the loop's shared
        // top-level graph. A loop defined once should drive every spec.
        let (_dir, db, engine, loop_id, spec1_id) = loop_fixture().unwrap();
        let spec2 = second_spec(&loop_id, "spec-2", 2);
        db.insert_loop_spec(&spec2).unwrap();

        let check = LoopNode {
            id: "loop-check".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        };
        let gate = LoopNode {
            id: "loop-gate".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "gate".to_string(),
            kind: LoopNodeKind::Gate,
            config: serde_json::json!({
                "evaluate": "output_contains",
                "value": "APPROVED",
                "pass_route": "next_spec"
            }),
            position: 2,
            created_at: chrono::Utc::now(),
        };
        db.insert_loop_node(&check).unwrap();
        db.insert_loop_node(&gate).unwrap();
        db.insert_loop_edge(&LoopEdge {
            id: "loop-edge-pass".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            from_node: check.id.clone(),
            to_node: gate.id.clone(),
            condition: crate::domain::loops::LoopEdgeCondition::Pass,
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Completed);

        for spec_id in [spec1_id.as_str(), spec2.id.as_str()] {
            let spec = db.get_loop_spec(spec_id).unwrap().unwrap();
            assert_eq!(spec.status, LoopSpecStatus::Completed);
            let runs = db.list_loop_runs_for_spec(spec_id).unwrap();
            assert_eq!(runs.len(), 2);
            assert!(runs.iter().all(|run| run.spec_id == spec_id));
            assert!(runs.iter().any(|run| run.node_id == "loop-check"));
            assert!(runs.iter().any(|run| run.node_id == "loop-gate"));
        }
    }

    #[tokio::test]
    async fn loop_engine_spec_with_own_graph_ignores_loop_level_graph() {
        // The loop-level graph always fails; if it were used, the spec would
        // fail. The spec's own graph always passes, and precedence must
        // favor it — full backwards compatibility.
        let (_dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();

        db.insert_loop_node(&LoopNode {
            id: "loop-check".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "loop-check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "exit 1",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        db.insert_loop_node(&LoopNode {
            id: "spec-check".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "spec-check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        let runs = db.list_loop_runs_for_spec(&spec_id).unwrap();

        assert_eq!(lp.status, LoopStatus::Completed);
        assert_eq!(spec.status, LoopSpecStatus::Completed);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].node_id, "spec-check");
    }

    #[tokio::test]
    async fn loop_engine_iteration_budget_resets_between_specs_on_loop_graph() {
        // A single loop-level node that self-loops on failure, gated by a
        // counter file shared across the whole run. It fails budget-1 times
        // then passes on the budget-th call — exactly the per-node iteration
        // cap. If spec 2's budget carried over from spec 1 instead of
        // resetting, its first attempt would already read as one past the
        // cap and the spec would fail before the check command ever runs
        // again.
        let (_dir, db, engine, loop_id, spec1_id) = loop_fixture().unwrap();
        let spec2 = second_spec(&loop_id, "spec-2", 2);
        db.insert_loop_spec(&spec2).unwrap();

        db.insert_loop_node(&LoopNode {
            id: "flaky".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "flaky".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": format!(
                    "n=$(cat counter.txt 2>/dev/null || echo 0); n=$((n+1)); echo $n > counter.txt; test $n -ge {DEFAULT_MAX_ITERATIONS_PER_NODE}"
                ),
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        db.insert_loop_edge(&LoopEdge {
            id: "self-loop".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            from_node: "flaky".to_string(),
            to_node: "flaky".to_string(),
            condition: crate::domain::loops::LoopEdgeCondition::Fail,
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Completed);

        let spec1 = db.get_loop_spec(&spec1_id).unwrap().unwrap();
        let spec1_runs = db.list_loop_runs_for_spec(&spec1_id).unwrap();
        assert_eq!(spec1.status, LoopSpecStatus::Completed);
        assert_eq!(spec1_runs.len(), DEFAULT_MAX_ITERATIONS_PER_NODE);

        let spec2_saved = db.get_loop_spec(&spec2.id).unwrap().unwrap();
        let spec2_runs = db.list_loop_runs_for_spec(&spec2.id).unwrap();
        assert_eq!(spec2_saved.status, LoopSpecStatus::Completed);
        // Fresh budget: the counter file is already at the cap from spec 1,
        // so spec 2's first (and only) fresh-budget attempt passes
        // immediately.
        assert_eq!(spec2_runs.len(), 1);
    }

    #[tokio::test]
    async fn loop_engine_loop_level_entry_fallback_by_lowest_position() {
        // Retry cycle (implement <-> review): every node has an incoming
        // edge, so there is no source node and the engine must fall back to
        // the lowest-position node as the entry point.
        let (_dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();

        let implement = LoopNode {
            id: "implement".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "implement".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf IMPLEMENT",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        };
        let review = LoopNode {
            id: "review".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "review".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf REVIEW",
                "success_condition": "exit_code_0"
            }),
            position: 2,
            created_at: chrono::Utc::now(),
        };
        db.insert_loop_node(&implement).unwrap();
        db.insert_loop_node(&review).unwrap();
        db.insert_loop_edge(&LoopEdge {
            id: "e1".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            from_node: "implement".to_string(),
            to_node: "review".to_string(),
            condition: crate::domain::loops::LoopEdgeCondition::Always,
        })
        .unwrap();
        db.insert_loop_edge(&LoopEdge {
            id: "e2".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            from_node: "review".to_string(),
            to_node: "implement".to_string(),
            condition: crate::domain::loops::LoopEdgeCondition::Fail,
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        let runs = db.list_loop_runs_for_spec(&spec_id).unwrap();

        assert_eq!(lp.status, LoopStatus::Completed);
        assert_eq!(spec.status, LoopSpecStatus::Completed);
        assert_eq!(runs.len(), 2);
        // "implement" must be the entry: it ran with no previous-node input.
        // "review" ran second, fed by implement's output — proving the walk
        // started at the lowest-position node, not an arbitrary one.
        let implement_run = runs.iter().find(|run| run.node_id == "implement").unwrap();
        let review_run = runs.iter().find(|run| run.node_id == "review").unwrap();
        assert!(implement_run.input.is_none());
        assert!(review_run.input.is_some());
    }

    #[tokio::test]
    async fn loop_engine_fails_spec_with_no_graph_anywhere_and_loop_moves_on() {
        // Neither the spec nor the loop has a graph: the spec must fail with
        // an actionable error instead of the engine erroring out before the
        // spec is even marked failed.
        let (_dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();

        assert_eq!(lp.status, LoopStatus::Failed);
        assert_eq!(spec.status, LoopSpecStatus::Failed);
    }

    // ── R5: `loop_run` with a pool ──────────────────────────────────────

    /// A loop with no bound specs — the pool's own standalone specs supply
    /// the work instead. Distinct from [`loop_fixture`], which always seeds
    /// one bound spec.
    fn bare_loop_fixture() -> Result<(TempDir, Arc<Database>, LoopEngine, String)> {
        let dir = tempdir()?;
        let db = Arc::new(Database::new(&dir.path().join("test.db"))?);
        let lp = crate::domain::loops::Loop {
            id: "wf-test".to_string(),
            name: "Loop".to_string(),
            description: None,
            workdir: dir.path().to_string_lossy().to_string(),
            status: LoopStatus::Draft,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            active_run_pool_id: None,
            on_completed: None,
        };
        db.insert_loop(&lp)?;
        Ok((
            dir,
            Arc::clone(&db),
            LoopEngine::new(db, Arc::new(DefaultNotificationService)),
            lp.id,
        ))
    }

    /// A standalone spec (`loop_id: None`), the shape pool members take —
    /// pool membership never binds the spec to a loop.
    fn standalone_spec(id: &str, position: i64) -> LoopSpec {
        LoopSpec {
            id: id.to_string(),
            loop_id: None,
            name: id.to_string(),
            description: Some(
                "Functional Requirements:\n- A\n\nNon-Functional Requirements:\n- B\n\nObjective:\n- C\n\nConstraints:\n- D\n\nGuidelines:\n- E\n\nIn Scope:\n- F\n\nOut of Scope:\n- G".to_string(),
            ),
            position,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        }
    }

    fn insert_pool_with_members(db: &Database, pool_id: &str, member_ids: &[&str]) {
        db.insert_pool(&crate::domain::pools::Pool {
            id: pool_id.to_string(),
            name: pool_id.to_string(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        for spec_id in member_ids {
            db.append_pool_member(pool_id, spec_id, None).unwrap();
        }
    }

    /// RS3 variant of [`insert_pool_with_members`]: each member is `(spec_id,
    /// group_name)`, so a test can queue grouped and ungrouped members side by
    /// side.
    fn insert_pool_with_grouped_members(
        db: &Database,
        pool_id: &str,
        members: &[(&str, Option<&str>)],
    ) {
        db.insert_pool(&crate::domain::pools::Pool {
            id: pool_id.to_string(),
            name: pool_id.to_string(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        for (spec_id, group) in members {
            db.append_pool_member(pool_id, spec_id, *group).unwrap();
        }
    }

    #[tokio::test]
    async fn loop_engine_pool_run_walks_loop_graph_across_pool_specs_in_queue_order() {
        // Two standalone specs, queued into the pool in the *opposite* order
        // of their `position` field — proving the pool's queue order drives
        // execution, not the spec's own position. Each pass through the
        // shared loop-level check node commits to the workdir's git repo, so
        // the spec that captures the pre-commit HEAD ran first.
        let (dir, db, engine, loop_id) = bare_loop_fixture().unwrap();
        init_git_repo(dir.path());
        let initial_head = git_head(dir.path());

        let spec_a = standalone_spec("pool-spec-a", 1);
        let spec_b = standalone_spec("pool-spec-b", 2);
        db.insert_loop_spec(&spec_a).unwrap();
        db.insert_loop_spec(&spec_b).unwrap();
        // Queue order: b, then a — the reverse of position order.
        insert_pool_with_members(&db, "pool-1", &[&spec_b.id, &spec_a.id]);

        db.insert_loop_node(&LoopNode {
            id: "loop-check".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "echo committed >> log.txt && git add -A && git commit -q -m spec && printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        engine
            .run_loop(loop_id.clone(), Some("pool-1".to_string()), None)
            .await
            .unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Completed);

        let spec_a_after = db.get_loop_spec(&spec_a.id).unwrap().unwrap();
        let spec_b_after = db.get_loop_spec(&spec_b.id).unwrap().unwrap();
        assert_eq!(spec_a_after.status, LoopSpecStatus::Completed);
        assert_eq!(spec_b_after.status, LoopSpecStatus::Completed);
        assert_eq!(db.list_loop_runs_for_spec(&spec_a.id).unwrap().len(), 1);
        assert_eq!(db.list_loop_runs_for_spec(&spec_b.id).unwrap().len(), 1);

        // spec_b ran first: nothing had been committed yet.
        assert_eq!(
            spec_b_after.spec_start_head.as_deref(),
            Some(initial_head.as_str())
        );
        // spec_a ran second: spec_b's node had already committed by then.
        assert_ne!(
            spec_a_after.spec_start_head.as_deref(),
            Some(initial_head.as_str())
        );
    }

    /// B18, end to end: the real incident. A pool-driven run's in-flight
    /// member is left `running` by a daemon restart — a dangling node run
    /// with no live process behind it — while another member sits `pending`
    /// right behind it in the queue. G2 boot reconcile must reset the
    /// in-flight member back to `pending` in the same pass it interrupts the
    /// dangling run, and the resumed dispatch (what `loop_continue`'s
    /// `retry_current_node` triggers via `resume_background`, simulated here
    /// by calling `run_loop_dispatch` directly with `is_resume: true`) must
    /// pick the interrupted member up FIRST — never skip straight past it to
    /// the next queued member, which is exactly how it got orphaned in the
    /// 2026-07-14 incident.
    #[tokio::test]
    async fn loop_engine_restart_recovery_runs_interrupted_pool_spec_first() {
        let (dir, db, engine, loop_id) = bare_loop_fixture().unwrap();
        init_git_repo(dir.path());
        let initial_head = git_head(dir.path());

        let mut interrupted = standalone_spec("pool-interrupted", 1);
        interrupted.status = LoopSpecStatus::Running;
        let next = standalone_spec("pool-next", 2);
        db.insert_loop_spec(&interrupted).unwrap();
        db.insert_loop_spec(&next).unwrap();
        insert_pool_with_members(&db, "pool-1", &[&interrupted.id, &next.id]);

        db.update_loop_status(
            &loop_id,
            LoopStatus::Running,
            Some(chrono::Utc::now()),
            None,
        )
        .unwrap();
        db.set_loop_active_run_pool(&loop_id, Some("pool-1"))
            .unwrap();

        db.insert_loop_node(&LoopNode {
            id: "loop-check".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "echo committed >> log.txt && git add -A && git commit -q -m spec && printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        // The daemon-restart artifact: a node run stuck `running` for the
        // in-flight spec, no live process behind it (no pid, no boot_id —
        // exactly what a dead daemon leaves for reconcile to find).
        db.insert_loop_run(&LoopNodeRun {
            id: "run-interrupted".to_string(),
            loop_id: loop_id.clone(),
            spec_id: interrupted.id.clone(),
            node_id: "loop-check".to_string(),
            status: LoopRunStatus::Running,
            input: None,
            output: None,
            started_at: chrono::Utc::now(),
            completed_at: None,
            iteration: 1,
            pid: None,
            boot_id: None,
            session_id: None,
        })
        .unwrap();

        // G2 boot reconcile.
        assert_eq!(db.reconcile_orphaned_loops().unwrap(), 1);
        let lp_after_reconcile = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp_after_reconcile.status, LoopStatus::Paused);
        let interrupted_after_reconcile = db.get_loop_spec(&interrupted.id).unwrap().unwrap();
        assert_eq!(
            interrupted_after_reconcile.status,
            LoopSpecStatus::Pending,
            "reconcile must reset the in-flight member back to pending, not leave it running"
        );

        // `loop_continue { retry_current_node }`: resume with the loop's
        // persisted pool context, same as `resume_background`. The loop is left
        // `Paused` (as reconcile set it) — the dispatch's own atomic claim (B42)
        // owns the flip to `Running`, so no caller pre-flips it anymore.
        engine
            .run_loop_dispatch(loop_id.clone(), Some("pool-1".to_string()), None, true)
            .await
            .unwrap();

        let lp_final = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp_final.status, LoopStatus::Completed);
        let interrupted_final = db.get_loop_spec(&interrupted.id).unwrap().unwrap();
        let next_final = db.get_loop_spec(&next.id).unwrap().unwrap();
        assert_eq!(interrupted_final.status, LoopSpecStatus::Completed);
        assert_eq!(next_final.status, LoopSpecStatus::Completed);

        // The interrupted spec ran FIRST — against the pre-existing HEAD,
        // before anything was committed — not skipped in favor of `next`.
        assert_eq!(
            interrupted_final.spec_start_head.as_deref(),
            Some(initial_head.as_str()),
            "the interrupted spec must be the first thing the resumed run picks up"
        );
        assert_ne!(
            next_final.spec_start_head.as_deref(),
            Some(initial_head.as_str()),
            "the next queued member must still run, but only after the interrupted one"
        );
    }

    #[tokio::test]
    async fn loop_engine_run_workdir_override_is_used_as_check_node_cwd() {
        // The loop's own workdir must be left untouched by an override — only
        // the check node's actual working directory should change.
        let db_dir = tempdir().unwrap();
        let loop_workdir = tempdir().unwrap();
        let override_workdir = tempdir().unwrap();
        let db = Arc::new(Database::new(&db_dir.path().join("test.db")).unwrap());
        let engine = LoopEngine::new(Arc::clone(&db), Arc::new(DefaultNotificationService));

        let lp = crate::domain::loops::Loop {
            id: "wf-workdir".to_string(),
            name: "Loop".to_string(),
            description: None,
            workdir: loop_workdir.path().to_string_lossy().to_string(),
            status: LoopStatus::Draft,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            active_run_pool_id: None,
            on_completed: None,
        };
        db.insert_loop(&lp).unwrap();
        let spec = standalone_spec("bound-spec", 1);
        let mut bound_spec = spec.clone();
        bound_spec.loop_id = Some(lp.id.clone());
        db.insert_loop_spec(&bound_spec).unwrap();

        let override_path = override_workdir.path().to_string_lossy().to_string();
        db.insert_loop_node(&LoopNode {
            id: "loop-check".to_string(),
            spec_id: None,
            loop_id: Some(lp.id.clone()),
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": format!("test \"$(pwd)\" = \"{override_path}\""),
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        engine
            .run_loop(lp.id.clone(), None, Some(override_path.clone()))
            .await
            .unwrap();

        let lp_after = db.get_loop(&lp.id).unwrap().unwrap();
        let spec_after = db.get_loop_spec(&bound_spec.id).unwrap().unwrap();
        assert_eq!(lp_after.status, LoopStatus::Completed);
        assert_eq!(spec_after.status, LoopSpecStatus::Completed);
        // The loop's own workdir is unchanged by the run-level override.
        assert_eq!(
            lp_after.workdir,
            loop_workdir.path().to_string_lossy().to_string()
        );
    }

    #[tokio::test]
    async fn loop_engine_legacy_run_without_pool_id_only_touches_bound_specs() {
        // A standalone spec exists in the DB (e.g. pool backlog) but isn't
        // added to any pool and isn't bound to this loop. Calling run_loop
        // without pool_id must behave exactly as before pools existed: only
        // the loop's own bound specs are touched.
        let (_dir, db, engine, loop_id, bound_spec_id) = loop_fixture().unwrap();
        let untouched = standalone_spec("untouched-standalone", 99);
        db.insert_loop_spec(&untouched).unwrap();

        db.insert_loop_node(&LoopNode {
            id: "loop-check".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        let bound = db.get_loop_spec(&bound_spec_id).unwrap().unwrap();
        let untouched_after = db.get_loop_spec(&untouched.id).unwrap().unwrap();

        assert_eq!(lp.status, LoopStatus::Completed);
        assert_eq!(bound.status, LoopSpecStatus::Completed);
        assert_eq!(untouched_after.status, LoopSpecStatus::Pending);
        assert!(db
            .list_loop_runs_for_spec(&untouched.id)
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn loop_engine_pool_run_skips_already_completed_members() {
        let (_dir, db, engine, loop_id) = bare_loop_fixture().unwrap();

        let mut done = standalone_spec("pool-done", 1);
        done.status = LoopSpecStatus::Completed;
        let pending = standalone_spec("pool-pending", 2);
        db.insert_loop_spec(&done).unwrap();
        db.insert_loop_spec(&pending).unwrap();
        insert_pool_with_members(&db, "pool-1", &[&done.id, &pending.id]);

        db.insert_loop_node(&LoopNode {
            id: "loop-check".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        engine
            .run_loop(loop_id.clone(), Some("pool-1".to_string()), None)
            .await
            .unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        let done_after = db.get_loop_spec(&done.id).unwrap().unwrap();
        let pending_after = db.get_loop_spec(&pending.id).unwrap().unwrap();

        assert_eq!(lp.status, LoopStatus::Completed);
        assert_eq!(done_after.status, LoopSpecStatus::Completed);
        assert_eq!(pending_after.status, LoopSpecStatus::Completed);
        // The already-completed spec was skipped outright: no run recorded.
        assert!(db.list_loop_runs_for_spec(&done.id).unwrap().is_empty());
        assert_eq!(db.list_loop_runs_for_spec(&pending.id).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn loop_engine_pool_run_retains_context_on_genuine_completion_for_progress() {
        let (_dir, db, engine, loop_id) = bare_loop_fixture().unwrap();

        let spec = standalone_spec("pool-spec", 1);
        db.insert_loop_spec(&spec).unwrap();
        insert_pool_with_members(&db, "pool-1", &[&spec.id]);

        db.insert_loop_node(&LoopNode {
            id: "loop-check".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        engine
            .run_loop(loop_id.clone(), Some("pool-1".to_string()), None)
            .await
            .unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Completed);
        // B31: a genuinely finished pool run keeps its `active_run_pool_id`
        // as last-run context so `loop list` / `loop info` can still render
        // its real progress instead of a misleading `0/0`. B8's
        // anti-pollution guarantee is upheld elsewhere: every launch path
        // re-persists this field before the first spec runs, so a later
        // fresh `loop_run` against a different pool overwrites it.
        assert_eq!(
            lp.active_run_pool_id.as_deref(),
            Some("pool-1"),
            "a genuinely finished pool run must keep the run context so its queue progress \
             stays queryable"
        );
        // The progress the CLI/MCP surfaces (mirrored by `loop_progress` in
        // `daemon/loop_cli.rs`) is a real `1/1`, not `0/0`.
        assert_eq!(
            engine
                .spec_progress(&loop_id, lp.active_run_pool_id.as_deref())
                .unwrap(),
            (1, 1),
            "completed pool loop must report n/n progress, not 0/0"
        );
    }

    /// If `pool_next_pending_spec_id` finds no `pending` member to pick, but a
    /// member is nonetheless left non-terminal (e.g. `running`, from a crash
    /// mid-spec that never got reset), the pool isn't genuinely finished —
    /// the loop must not be marked `completed` out from under it. This is
    /// the guard that keeps a resumed pool run from repeating the incident's
    /// false-completion (17 of 20 pool specs still pending, loop marked
    /// completed anyway).
    #[tokio::test]
    async fn loop_engine_pool_run_does_not_complete_loop_while_member_left_running() {
        let (_dir, db, engine, loop_id) = bare_loop_fixture().unwrap();

        let mut stuck = standalone_spec("pool-stuck", 1);
        stuck.status = LoopSpecStatus::Running;
        db.insert_loop_spec(&stuck).unwrap();
        insert_pool_with_members(&db, "pool-1", &[&stuck.id]);

        engine
            .run_loop(loop_id.clone(), Some("pool-1".to_string()), None)
            .await
            .unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(
            lp.status,
            LoopStatus::Running,
            "must not be marked completed while a pool member is still non-terminal"
        );
        assert_eq!(
            lp.active_run_pool_id.as_deref(),
            Some("pool-1"),
            "the run context must survive so a later resume still knows the pool"
        );
    }

    // ── R6: live pools — append and reorder while running ────────────────

    async fn wait_for_file(path: &std::path::Path) {
        for _ in 0..500 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for file: {}", path.display());
    }

    fn record_node(id: &str, spec_id: &str, log: &std::path::Path, label: &str) -> LoopNode {
        LoopNode {
            id: id.to_string(),
            spec_id: Some(spec_id.to_string()),
            loop_id: None,
            name: id.to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": format!(
                    "echo {label} >> \"{log}\" && printf APPROVED",
                    log = log.display(),
                ),
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        }
    }

    fn touch_gate_node(
        id: &str,
        spec_id: &str,
        marker: &std::path::Path,
        gate: &std::path::Path,
    ) -> LoopNode {
        LoopNode {
            id: id.to_string(),
            spec_id: Some(spec_id.to_string()),
            loop_id: None,
            name: id.to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": format!(
                    "touch \"{marker}\"; while [ ! -f \"{gate}\" ]; do sleep 0.02; done; printf APPROVED",
                    marker = marker.display(),
                    gate = gate.display(),
                ),
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn loop_engine_pool_run_picks_up_spec_appended_mid_run() {
        // A spec appended to the pool while the run is in flight must still
        // get executed before the run ends: the engine re-queries the pool
        // for its next pending member at each spec boundary instead of
        // iterating a list frozen at launch.
        let (dir, db, engine, loop_id) = bare_loop_fixture().unwrap();
        let started_marker = dir.path().join("started.marker");
        let go_marker = dir.path().join("go.marker");
        let order_log = dir.path().join("order.log");

        let spec_a = standalone_spec("pool-spec-a", 1);
        let spec_b = standalone_spec("pool-spec-b", 2);
        db.insert_loop_spec(&spec_a).unwrap();
        db.insert_loop_spec(&spec_b).unwrap();
        // spec_b exists in the DB but is NOT yet in the pool — it's appended
        // below, while spec_a is mid-run.
        insert_pool_with_members(&db, "pool-1", &[&spec_a.id]);

        db.insert_loop_node(&touch_gate_node(
            "node-a",
            &spec_a.id,
            &started_marker,
            &go_marker,
        ))
        .unwrap();
        db.insert_loop_node(&record_node("node-b", &spec_b.id, &order_log, "spec-b"))
            .unwrap();

        let run_engine = engine.clone();
        let run_loop_id = loop_id.clone();
        let handle = tokio::spawn(async move {
            run_engine
                .run_loop(run_loop_id, Some("pool-1".to_string()), None)
                .await
        });

        wait_for_file(&started_marker).await;
        // spec_a is mid-run (blocked on the gate). Append spec_b to the pool
        // now, while the run is in flight.
        db.append_pool_member("pool-1", &spec_b.id, None).unwrap();
        std::fs::write(&go_marker, "").unwrap();

        handle.await.unwrap().unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        let spec_a_after = db.get_loop_spec(&spec_a.id).unwrap().unwrap();
        let spec_b_after = db.get_loop_spec(&spec_b.id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Completed);
        assert_eq!(spec_a_after.status, LoopSpecStatus::Completed);
        assert_eq!(
            spec_b_after.status,
            LoopSpecStatus::Completed,
            "spec appended mid-run must still be executed before the run ends"
        );
        assert_eq!(db.list_loop_runs_for_spec(&spec_b.id).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn loop_engine_pool_run_reorder_changes_pick_order_mid_run() {
        // Reordering the pool's PENDING members while a run is in flight
        // must change which one the engine picks next — proving the pick is
        // a live, fresh query, not a list captured at launch.
        let (dir, db, engine, loop_id) = bare_loop_fixture().unwrap();
        let started_marker = dir.path().join("started.marker");
        let go_marker = dir.path().join("go.marker");
        let order_log = dir.path().join("order.log");

        let spec_a = standalone_spec("pool-spec-a", 1);
        let spec_b = standalone_spec("pool-spec-b", 2);
        let spec_c = standalone_spec("pool-spec-c", 3);
        db.insert_loop_spec(&spec_a).unwrap();
        db.insert_loop_spec(&spec_b).unwrap();
        db.insert_loop_spec(&spec_c).unwrap();
        // Queue order at launch: a, b, c.
        insert_pool_with_members(&db, "pool-1", &[&spec_a.id, &spec_b.id, &spec_c.id]);

        db.insert_loop_node(&touch_gate_node(
            "node-a",
            &spec_a.id,
            &started_marker,
            &go_marker,
        ))
        .unwrap();
        db.insert_loop_node(&record_node("node-b", &spec_b.id, &order_log, "spec-b"))
            .unwrap();
        db.insert_loop_node(&record_node("node-c", &spec_c.id, &order_log, "spec-c"))
            .unwrap();

        let run_engine = engine.clone();
        let run_loop_id = loop_id.clone();
        let handle = tokio::spawn(async move {
            run_engine
                .run_loop(run_loop_id, Some("pool-1".to_string()), None)
                .await
        });

        wait_for_file(&started_marker).await;
        // spec_a is mid-run. Swap the two PENDING members' order: c before b.
        db.reorder_pool_members(
            "pool-1",
            &[spec_a.id.clone(), spec_c.id.clone(), spec_b.id.clone()],
        )
        .unwrap();
        std::fs::write(&go_marker, "").unwrap();

        handle.await.unwrap().unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Completed);

        let order = std::fs::read_to_string(&order_log).unwrap();
        let lines: Vec<&str> = order.lines().collect();
        assert_eq!(
            lines,
            vec!["spec-c", "spec-b"],
            "reorder mid-run must change which pending spec runs next"
        );
    }

    #[tokio::test]
    async fn loop_engine_pool_run_ends_when_no_pending_members_remain() {
        // Sanity check underpinning both tests above: with no gating at all,
        // a pool run with N pending members ends after exactly N specs run,
        // and picks up an appended spec before completing.
        let (dir, db, engine, loop_id) = bare_loop_fixture().unwrap();
        let order_log = dir.path().join("order.log");

        let spec_a = standalone_spec("pool-spec-a", 1);
        let spec_b = standalone_spec("pool-spec-b", 2);
        db.insert_loop_spec(&spec_a).unwrap();
        db.insert_loop_spec(&spec_b).unwrap();
        insert_pool_with_members(&db, "pool-1", &[&spec_a.id, &spec_b.id]);

        db.insert_loop_node(&record_node("node-a", &spec_a.id, &order_log, "spec-a"))
            .unwrap();
        db.insert_loop_node(&record_node("node-b", &spec_b.id, &order_log, "spec-b"))
            .unwrap();

        engine
            .run_loop(loop_id.clone(), Some("pool-1".to_string()), None)
            .await
            .unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Completed);
        assert!(db.pool_next_pending_spec_id("pool-1").unwrap().is_none());

        let order = std::fs::read_to_string(&order_log).unwrap();
        assert_eq!(order.lines().collect::<Vec<_>>(), vec!["spec-a", "spec-b"]);
    }

    // ── E2BIG resilience: spawn failure routes through fail edge ──────

    /// Agent spawn failure produces the correct NodeExecution shape that
    /// `select_next_step` can route. This tests the contract between
    /// `run_agent_process` (which catches E2BIG / spawn errors) and the
    /// graph router (which selects the next node based on status).
    #[tokio::test]
    async fn agent_spawn_failure_node_execution_is_routable() {
        let (_dir, db) = test_db();
        let cli = Cli::new("test-cli");
        let mut strategy = sample_strategy("/nonexistent/somewhere/definitely-not-a-binary");
        strategy.prompt_via_stdin = false;
        let node = sample_agent_node();

        let execution = run_agent_process(
            &db, "run-test", &cli, &strategy, &node, "prompt", None, "/tmp", 1, None,
        )
        .await
        .expect("spawn failure must not propagate as a hard error");

        // The execution must be a Fail — exactly what select_next_step matches
        // against the Fail edge condition.
        assert_eq!(execution.status, LoopRunStatus::Fail);
        assert!(execution.summary.contains("failed to spawn"));

        // Verify the output JSON has the fields the loop engine expects.
        let output = &execution.output;
        assert_eq!(output.get("kind").and_then(Value::as_str), Some("agent"));
        assert_eq!(
            output.get("node_id").and_then(Value::as_str),
            Some("node-agent")
        );
        assert!(output.get("error").is_some(), "must include error message");
    }

    /// The full E2BIG resilience path: prompt is built (with elision),
    /// delivered via stdin (no argv cliff), and a spawn failure is caught
    /// as a node-level failure. This exercises the three components that
    /// together prevent the E2BIG incident from recurring:
    /// 1. `bound_previous_feedback` — truncates large prior output
    /// 2. `CliStrategy::build_command` with `prompt_via_stdin` — avoids argv
    /// 3. `run_agent_process` — catches spawn errors as node failures
    #[tokio::test]
    async fn e2big_resilience_path_elision_stdin_and_spawn_failure() {
        // 1. Simulate a huge previous_feedback (like a 65KB cargo test log).
        let huge_log = "x".repeat(500 * 1024);
        let bounded = bound_previous_feedback(huge_log.clone());
        assert!(
            bounded.contains("bytes elided"),
            "large feedback must be elided"
        );
        assert!(
            bounded.len() < 200 * 1024,
            "elided feedback must be well under argv limit"
        );

        // 2. Render the full prompt — elision must survive composition.
        let lp = crate::domain::loops::Loop {
            id: "wf".to_string(),
            name: "Loop".to_string(),
            description: None,
            workdir: "/tmp/project".to_string(),
            status: LoopStatus::Draft,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            active_run_pool_id: None,
            on_completed: None,
        };
        let spec = LoopSpec {
            id: "spec".to_string(),
            loop_id: Some("wf".to_string()),
            name: "Spec".to_string(),
            description: Some("Do the thing".to_string()),
            position: 1,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        };
        let node = LoopNode {
            id: "node-1".to_string(),
            spec_id: Some("spec".to_string()),
            loop_id: None,
            name: "Agent".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({}),
            position: 1,
            created_at: chrono::Utc::now(),
        };
        let prompt = render_agent_prompt(
            &lp,
            &spec,
            &node,
            "{{previous_feedback}}",
            Some(&serde_json::json!({"stdout": huge_log})),
            &lp.workdir,
            "run-1",
        );
        assert!(
            prompt.contains("bytes elided"),
            "composed prompt must contain the elision marker"
        );
        assert!(
            prompt.len() < 300 * 1024,
            "composed prompt must stay well under argv limit"
        );

        // 3. Deliver via stdin — no E2BIG even for oversized prompts.
        //    `cat` echoes stdin to stdout; the full output must arrive intact
        //    (the output contains the prompt twice: once via {{previous_feedback}}
        //    template substitution, once as the explicit # [PREVIOUS FEEDBACK]
        //    section — so stdout != prompt; we check it arrives in full instead).
        let (_dir, db) = test_db();
        let cli = Cli::new("test-cli");
        let mut strategy = sample_strategy("/bin/cat");
        strategy.prompt_via_stdin = true;
        let stdin_node = sample_agent_node();

        let execution = run_agent_process(
            &db,
            "run-test",
            &cli,
            &strategy,
            &stdin_node,
            &prompt,
            None,
            "/tmp",
            1,
            None,
        )
        .await;
        let result = execution.expect("stdin delivery must not fail");
        assert_eq!(result.status, LoopRunStatus::Pass);
        let stdout = result.output.get("stdout").and_then(Value::as_str).unwrap();
        assert!(
            stdout.contains("bytes elided"),
            "stdout from cat must contain the elision marker"
        );

        // 4. Spawn failure caught as node failure (not hard error).
        let mut fail_strategy = sample_strategy("/nonexistent/binary");
        fail_strategy.prompt_via_stdin = false;
        let fail_result = run_agent_process(
            &db,
            "run-test",
            &cli,
            &fail_strategy,
            &stdin_node,
            &prompt,
            None,
            "/tmp",
            1,
            None,
        )
        .await
        .expect("spawn failure must not propagate as hard error");
        assert_eq!(fail_result.status, LoopRunStatus::Fail);
    }

    /// Full engine integration: a node failure must route through the graph's
    /// fail edge and let the loop continue — never abort the entire loop run.
    /// This proves the resilience contract that the E2BIG fix depends on:
    /// when `run_agent_process` returns a failed `NodeExecution` (instead of
    /// propagating `Err`), the engine routes it through the fail edge.
    #[tokio::test]
    async fn loop_engine_node_failure_routes_through_fail_edge_and_loop_continues() {
        let (_dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();

        // "implement" node: always fails (simulates any node failure,
        // including an agent spawn failure that's caught by
        // `run_agent_process`).
        db.insert_loop_node(&LoopNode {
            id: "node-implement".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "implement".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "exit 1",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        // "review" node: runs after the failure, proving the loop survived.
        db.insert_loop_node(&LoopNode {
            id: "node-review".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "review".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 2,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        // implement --fail--> review
        db.insert_loop_edge(&LoopEdge {
            id: "edge-fail".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            from_node: "node-implement".to_string(),
            to_node: "node-review".to_string(),
            condition: crate::domain::loops::LoopEdgeCondition::Fail,
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        let runs = db.list_loop_runs_for_spec(&spec_id).unwrap();

        // The loop must complete (not fail/abort), the spec must complete
        // (review passed), and both nodes must have run.
        assert_eq!(lp.status, LoopStatus::Completed);
        assert_eq!(spec.status, LoopSpecStatus::Completed);
        assert_eq!(runs.len(), 2);

        let implement_run = runs.iter().find(|r| r.node_id == "node-implement").unwrap();
        assert_eq!(implement_run.status, LoopRunStatus::Fail);

        let review_run = runs.iter().find(|r| r.node_id == "node-review").unwrap();
        assert_eq!(review_run.status, LoopRunStatus::Pass);
    }

    // ── B42: a superseded run is terminal and silent ─────────────────────

    /// The core of the runaway: an in-flight node run superseded by a newer
    /// attempt at the same node must traverse NO edge. Its graph has a fail
    /// edge to a "resilience" node — exactly the shape that manufactured a
    /// fresh Resilience run per killed implementer — and that node must never
    /// run, because a supersede is engine bookkeeping, not a node failure.
    #[cfg(unix)]
    #[tokio::test]
    async fn superseded_run_traverses_no_edge_and_creates_no_resilience_run() {
        let (_dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();

        // "implement": a long-running node we can supersede mid-flight. A
        // killed process exits nonzero, so absent the fix its `Fail` would
        // route straight down the fail edge below.
        db.insert_loop_node(&LoopNode {
            id: "implement".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "implement".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "sleep 30",
                "success_condition": "exit_code_0",
                "timeout_seconds": 60,
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        // "resilience": the fail-edge target that must NEVER run for a supersede.
        db.insert_loop_node(&LoopNode {
            id: "resilience".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "resilience".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf DIAGNOSED",
                "success_condition": "exit_code_0",
            }),
            position: 2,
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        db.insert_loop_edge(&LoopEdge {
            id: "edge-fail".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            from_node: "implement".to_string(),
            to_node: "resilience".to_string(),
            condition: crate::domain::loops::LoopEdgeCondition::Fail,
        })
        .unwrap();

        let engine = Arc::new(engine);
        let dispatch = {
            let engine = Arc::clone(&engine);
            let loop_id = loop_id.clone();
            tokio::spawn(async move { engine.run_loop(loop_id, None, None).await })
        };

        // Once the implement run is live (has a pid), supersede it exactly as a
        // newer attempt at the same node does — the same
        // `terminate_run(&stale, SUPERSEDE_REASON)` the reap loop runs. Polling
        // to the pid makes the ordering deterministic: the row is finalized
        // superseded before the killed process's `wait` ever returns.
        let superseded_run_id = loop {
            if let Some(run) = db.get_active_loop_run_for_node("implement").unwrap() {
                if run.pid.is_some() {
                    terminate_run_row(&db, &run, SUPERSEDE_REASON);
                    break run.id;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };

        dispatch.await.unwrap().unwrap();

        // The superseded run is recorded terminated/superseded...
        let superseded = db.get_loop_run(&superseded_run_id).unwrap().unwrap();
        assert_eq!(superseded.status, LoopRunStatus::Fail);
        assert!(
            run_was_superseded(&superseded),
            "the run must carry the supersede marker"
        );

        // ...and it traversed no edge: NO resilience run was ever created, and
        // the only run for the spec is the one superseded implement run.
        let runs = db.list_loop_runs_for_spec(&spec_id).unwrap();
        assert!(
            runs.iter().all(|r| r.node_id != "resilience"),
            "a superseded run must not route down the fail edge to the resilience node"
        );
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].node_id, "implement");

        // The dispatch stopped silently — it failed nothing and completed
        // nothing; the loop and spec are left for whoever now owns them.
        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Running);
        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        assert_eq!(spec.status, LoopSpecStatus::Running);
    }

    /// A second launch against a loop that already has an in-flight run must be
    /// a silent no-op — the atomic loop claim refuses it, so it can't start a
    /// duplicate dispatch that would supersede the live run at the next node.
    #[tokio::test]
    async fn duplicate_launch_of_running_loop_is_a_noop() {
        let (_dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();

        db.insert_loop_node(&LoopNode {
            id: "implement".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "implement".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf OK",
                "success_condition": "exit_code_0",
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        // Simulate a live dispatch: the loop is already `Running` with an
        // in-flight node run behind it.
        db.update_loop_status(
            &loop_id,
            LoopStatus::Running,
            Some(chrono::Utc::now()),
            None,
        )
        .unwrap();
        db.insert_loop_run(&LoopNodeRun {
            id: "inflight".to_string(),
            loop_id: loop_id.clone(),
            spec_id: spec_id.clone(),
            node_id: "implement".to_string(),
            status: LoopRunStatus::Running,
            input: None,
            output: None,
            started_at: chrono::Utc::now(),
            completed_at: None,
            iteration: 1,
            pid: None,
            boot_id: None,
            session_id: None,
        })
        .unwrap();

        // A second dispatch (autorun/resume racing the live one) must no-op.
        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        // The loop is untouched and the in-flight run was neither superseded
        // nor duplicated.
        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Running);
        let runs = db.list_loop_runs_for_spec(&spec_id).unwrap();
        assert_eq!(
            runs.len(),
            1,
            "the duplicate launch must not create a second run"
        );
        assert_eq!(runs[0].status, LoopRunStatus::Running);
    }

    // ── N1: loop lifecycle notifications ──────────────────────────────────

    #[tokio::test]
    async fn loop_engine_notifies_started_spec_completed_and_finished_on_success() {
        // A retrying check (self-loop on fail) must not spam a
        // spec-completed notification per attempt — only once, when the
        // spec actually reaches `completed`.
        let (dir, db, engine, notifications, loop_id, spec_id) = loop_fixture_with_mock().unwrap();
        let counter = dir.path().join("counter");

        db.insert_loop_node(&LoopNode {
            id: "node-check".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": format!(
                    "n=$(cat \"{counter}\" 2>/dev/null || echo 0); n=$((n+1)); echo $n > \"{counter}\"; [ \"$n\" -ge 3 ] && printf APPROVED || exit 1",
                    counter = counter.display(),
                ),
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        db.insert_loop_edge(&LoopEdge {
            id: "edge-self".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            from_node: "node-check".to_string(),
            to_node: "node-check".to_string(),
            condition: crate::domain::loops::LoopEdgeCondition::Fail,
        })
        .unwrap();

        engine.run_loop(loop_id, None, None).await.unwrap();

        assert_eq!(
            notifications.events(),
            vec![
                RecordedNotification::LoopStarted {
                    loop_name: "Loop".to_string(),
                    spec_count: 1,
                    resumed: false,
                    first_pending: Some("Spec".to_string()),
                },
                RecordedNotification::SpecCompleted {
                    loop_name: "Loop".to_string(),
                    spec_name: "Spec".to_string(),
                    done: 1,
                    total: 1,
                    next_pending: None,
                },
                RecordedNotification::LoopFinishedCompleted {
                    loop_name: "Loop".to_string(),
                    done: 1,
                    total: 1,
                    hook_launched: false,
                },
            ],
            "exactly one start, one spec-completed (not one per retry), and one finish notification"
        );
    }

    #[tokio::test]
    async fn loop_engine_notifies_failed_variant_with_failing_spec_name() {
        let (_dir, _db, engine, notifications, loop_id, spec_id) =
            loop_fixture_with_mock().unwrap();

        _db.insert_loop_node(&LoopNode {
            id: "node-check".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "exit 1",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        engine.run_loop(loop_id, None, None).await.unwrap();

        assert_eq!(
            notifications.events(),
            vec![
                RecordedNotification::LoopStarted {
                    loop_name: "Loop".to_string(),
                    spec_count: 1,
                    resumed: false,
                    first_pending: Some("Spec".to_string()),
                },
                RecordedNotification::LoopFinishedFailed {
                    loop_name: "Loop".to_string(),
                    spec_name: "Spec".to_string(),
                },
            ],
            "a spec that never completes must not fire spec-completed, only start + failed finish"
        );
    }

    #[tokio::test]
    async fn loop_engine_notify_blocked_fires_loop_finished_blocked() {
        let (_dir, _db, engine, notifications, loop_id, _spec_id) =
            loop_fixture_with_mock().unwrap();

        engine
            .notify_blocked(&loop_id, "needs human review")
            .unwrap();

        assert_eq!(
            notifications.events(),
            vec![RecordedNotification::LoopFinishedBlocked {
                loop_name: "Loop".to_string(),
                summary: "needs human review".to_string(),
            }],
        );
    }

    // ── B11: check nodes run under a non-login shell ─────────────────────

    #[tokio::test]
    async fn shell_command_runs_check_commands_with_sh_c_semantics() {
        let mut process = shell_command("printf ok && exit 0");
        let output = process.output().await.unwrap();

        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
    }

    #[tokio::test]
    async fn shell_command_does_not_source_a_profile_with_bashisms() {
        // A login shell (`sh -l`) sources `~/.profile` before running the
        // command; a non-login `sh -c` never does. Point HOME at a profile
        // containing a bashism (`[[ ... ]]`, which dash chokes on with
        // `sh: N: [[: not found`) and confirm it never gets read: no stderr
        // noise, and the command's own exit code is unaffected.
        let fake_home = tempdir().unwrap();
        std::fs::write(fake_home.path().join(".profile"), "[[ x ]]\n").unwrap();

        let mut process = shell_command("exit 0");
        process.env("HOME", fake_home.path());
        let output = process.output().await.unwrap();

        assert!(output.status.success());
        assert!(
            output.stderr.is_empty(),
            "expected no stderr noise from a bashism in ~/.profile, got: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // ── B12: process-group kill on all abnormal ends ────────────────────

    /// Iteration budget exhaustion must terminate any in-flight child
    /// processes and finalize all runs. A check node that always fails
    /// loops back to itself via a self-loop edge until the per-node
    /// iteration budget (DEFAULT_MAX_ITERATIONS_PER_NODE) is hit.
    #[cfg(unix)]
    #[tokio::test]
    async fn iteration_budget_exhaustion_kills_inflight_child() {
        let (_dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();

        // This node always fails. A self-loop edge routes its failure
        // back to itself, forcing retries until the budget is exhausted.
        db.insert_loop_node(&LoopNode {
            id: "flaky".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "flaky".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "sleep 0.2; exit 1",
                "success_condition": "exit_code_0",
                "timeout_seconds": 60,
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        // Self-loop: failure routes back to the same node for retry.
        db.insert_loop_edge(&LoopEdge {
            id: "self-loop".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            from_node: "flaky".to_string(),
            to_node: "flaky".to_string(),
            condition: crate::domain::loops::LoopEdgeCondition::Fail,
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        // The spec must have failed on budget exhaustion.
        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        assert_eq!(spec.status, LoopSpecStatus::Failed);

        // All runs for this spec must be finalized (no longer `running`).
        let runs = db.list_loop_runs_for_spec(&spec_id).unwrap();
        assert_eq!(runs.len(), DEFAULT_MAX_ITERATIONS_PER_NODE);
        assert!(
            runs.iter().all(|r| r.status != LoopRunStatus::Running),
            "no run should still be running after budget exhaustion"
        );
    }

    /// An agent node's timeout must kill the spawned OS process, not just
    /// mark the run failed. Uses `sh -c "sleep 5; touch <marker>"` as a
    /// stand-in for a hung agent CLI (a real long-running child process,
    /// exercised through the exact same `run_agent_process` code path a
    /// real agent CLI goes through) with an immediate timeout (agent
    /// timeouts are minute-granular, so `0` is the only way to force one
    /// without actually waiting a minute): if the timeout kill didn't
    /// happen, the marker would appear ~5s later; if it did, the process is
    /// gone long before that and the marker never appears.
    #[cfg(unix)]
    #[tokio::test]
    async fn agent_timeout_kills_child_process() {
        let (dir, db, _engine, loop_id, spec_id) = loop_fixture().unwrap();
        let marker = dir.path().join("agent_survived");

        let node = LoopNode {
            id: "agent-timeout".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "agent-timeout".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({}),
            position: 1,
            created_at: chrono::Utc::now(),
        };
        db.insert_loop_node(&node).unwrap();
        let run_id = "run-agent-timeout".to_string();
        db.insert_loop_run(&LoopNodeRun {
            id: run_id.clone(),
            loop_id: loop_id.clone(),
            spec_id: spec_id.clone(),
            node_id: node.id.clone(),
            status: LoopRunStatus::Running,
            input: None,
            output: None,
            started_at: chrono::Utc::now(),
            completed_at: None,
            iteration: 1,
            pid: None,
            boot_id: None,
            session_id: None,
        })
        .unwrap();

        let cli = Cli::new("test-cli");
        let mut strategy = sample_strategy("sh");
        strategy.headless_mode = "-c".to_string();
        let prompt = format!("sleep 5; touch \"{}\"", marker.display());

        let result = run_agent_process(
            &db, &run_id, &cli, &strategy, &node, &prompt, None, "/tmp", 0, None,
        )
        .await;
        // B28: a timeout resolves as a failed `NodeExecution`, not a hard
        // error — it must be routable through the graph's fail edge rather
        // than aborting the whole spec.
        let execution = result.expect("a timed-out agent process must not be a hard error");
        assert_eq!(execution.status, LoopRunStatus::Fail);
        assert_eq!(execution.output["error"], "timed out");

        // Wait out the grace period (plus a margin) before checking — the
        // kill is `SIGTERM` now, `SIGKILL` after `KILL_GRACE` on a detached
        // task, and either one reaps a plain `sh`/`sleep` well within that
        // window since neither ignores `SIGTERM`.
        tokio::time::sleep(KILL_GRACE + std::time::Duration::from_secs(2)).await;

        assert!(
            !marker.exists(),
            "agent process should have been killed on timeout; marker file should not exist"
        );
    }

    // ── B28: timeout-as-fail routing ────────────────────────────────────

    /// An agent node that times out must resolve as a FAIL that traverses
    /// its fail edge — not abort the whole spec. The fail edge routes to a
    /// recovery node whose marker file only appears if the loop actually
    /// kept running past the timeout.
    #[tokio::test]
    async fn agent_node_timeout_with_fail_edge_traverses_it() {
        let (dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();
        let marker = dir.path().join("recovered.marker");

        let fake_home = setup_multi_cli_home(&[(
            "hang-cli",
            &write_member_script(dir.path(), "hang.sh", "sleep 5"),
        )]);

        db.insert_loop_node(&LoopNode {
            id: "node-agent-timeout".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "slow-agent".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({
                "platform": "hang-cli",
                "prompt_template": "ignored by the test script",
                "timeout_minutes": 0,
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        db.insert_loop_node(&LoopNode {
            id: "node-recovery".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "recovery".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": format!("touch \"{}\"", marker.display()),
                "success_condition": "exit_code_0"
            }),
            position: 2,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        db.insert_loop_edge(&LoopEdge {
            id: "edge-fail".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            from_node: "node-agent-timeout".to_string(),
            to_node: "node-recovery".to_string(),
            condition: LoopEdgeCondition::Fail,
        })
        .unwrap();

        let _home = HomeGuard::set(fake_home.path());
        let result = engine.run_loop(loop_id.clone(), None, None).await;
        drop(_home);
        result.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Completed);
        assert_eq!(spec.status, LoopSpecStatus::Completed);
        assert!(
            marker.exists(),
            "fail edge must have been traversed after the agent node timed out"
        );

        let runs = db.list_loop_runs_for_spec(&spec_id).unwrap();
        let timeout_run = runs
            .iter()
            .find(|r| r.node_id == "node-agent-timeout")
            .unwrap();
        assert_eq!(timeout_run.status, LoopRunStatus::Fail);
        assert_eq!(timeout_run.output.as_ref().unwrap()["error"], "timed out");
    }

    /// An agent node that times out with no fail edge must fail the spec
    /// (and the loop) cleanly — same as any other dead-end fail — rather
    /// than propagating a hard error out of `run_loop`.
    #[tokio::test]
    async fn agent_node_timeout_without_fail_edge_fails_spec_cleanly() {
        let (dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();

        let fake_home = setup_multi_cli_home(&[(
            "hang-cli",
            &write_member_script(dir.path(), "hang.sh", "sleep 5"),
        )]);

        db.insert_loop_node(&LoopNode {
            id: "node-agent-timeout".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "slow-agent".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({
                "platform": "hang-cli",
                "prompt_template": "ignored by the test script",
                "timeout_minutes": 0,
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        let _home = HomeGuard::set(fake_home.path());
        let result = engine.run_loop(loop_id.clone(), None, None).await;
        drop(_home);
        result.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Failed);
        assert_eq!(spec.status, LoopSpecStatus::Failed);

        let runs = db.list_loop_runs_for_spec(&spec_id).unwrap();
        let timeout_run = runs
            .iter()
            .find(|r| r.node_id == "node-agent-timeout")
            .unwrap();
        assert_eq!(timeout_run.status, LoopRunStatus::Fail);
        assert_eq!(timeout_run.output.as_ref().unwrap()["error"], "timed out");
    }

    /// A check node that times out must behave exactly like any other check
    /// fail: it traverses its fail edge instead of aborting the spec.
    #[tokio::test]
    async fn check_node_timeout_behaves_as_check_fail() {
        let (dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();
        let marker = dir.path().join("recovered.marker");

        db.insert_loop_node(&LoopNode {
            id: "node-check-timeout".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "slow-check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "sleep 5",
                "success_condition": "exit_code_0",
                "timeout_seconds": 0,
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        db.insert_loop_node(&LoopNode {
            id: "node-recovery".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "recovery".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": format!("touch \"{}\"", marker.display()),
                "success_condition": "exit_code_0"
            }),
            position: 2,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        db.insert_loop_edge(&LoopEdge {
            id: "edge-fail".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            from_node: "node-check-timeout".to_string(),
            to_node: "node-recovery".to_string(),
            condition: LoopEdgeCondition::Fail,
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Completed);
        assert_eq!(spec.status, LoopSpecStatus::Completed);
        assert!(
            marker.exists(),
            "fail edge must have been traversed after the check node timed out"
        );

        let runs = db.list_loop_runs_for_spec(&spec_id).unwrap();
        let timeout_run = runs
            .iter()
            .find(|r| r.node_id == "node-check-timeout")
            .unwrap();
        assert_eq!(timeout_run.status, LoopRunStatus::Fail);
        assert_eq!(timeout_run.output.as_ref().unwrap()["error"], "timed out");
    }

    /// An ensemble member whose own agent timeout fires (not the ensemble's
    /// straggler watchdog) must count as a member fail with the "timed out"
    /// marker intact — and must never prevent the join from resolving.
    /// `min_pass: 1` alongside one passing member proves the join still
    /// reaches quorum despite the timed-out member.
    #[tokio::test]
    async fn ensemble_member_agent_timeout_counts_as_member_fail_without_killing_join() {
        let (dir, db, engine, _loop_id, spec_id) = loop_fixture().unwrap();

        let fake_home = setup_multi_cli_home(&[
            (
                "hang-member",
                &write_member_script(dir.path(), "hang.sh", "sleep 5"),
            ),
            (
                "member-ok",
                &write_member_script(dir.path(), "ok.sh", "exit 0"),
            ),
        ]);

        let pass_marker = dir.path().join("pass.marker");
        db.insert_loop_node(&touch_marker_node("on-pass", &spec_id, &pass_marker, 100))
            .unwrap();
        insert_test_ensemble(
            &db,
            &spec_id,
            "kickoff",
            "ens1",
            "join1",
            &[("m-hang", "hang-member"), ("m-ok", "member-ok")],
            1,       // min_pass: only one member needs to pass
            Some(5), // generous straggler window — the member's own timeout must fire first
            "on-pass",
            None,
        );

        // Force the hanging member's own agent timeout to fire immediately,
        // well before the ensemble's straggler watchdog would.
        db.update_loop_node_details(
            "m-hang",
            None,
            None,
            Some(&serde_json::json!({
                "platform": "hang-member",
                "prompt_template": "ignored by the test script",
                "timeout_minutes": 0,
            })),
            None,
        )
        .unwrap();

        let _home = HomeGuard::set(fake_home.path());
        engine
            .run_loop("wf-test".to_string(), None, None)
            .await
            .unwrap();
        drop(_home);

        assert!(
            pass_marker.exists(),
            "join must pass and route onward despite one member timing out"
        );

        let join = join_run(&db, &spec_id, "join1");
        assert_eq!(join.status, LoopRunStatus::Pass);
        assert_eq!(join.output.as_ref().unwrap()["passed"], 1);

        let hang_run = db
            .list_loop_runs_for_spec(&spec_id)
            .unwrap()
            .into_iter()
            .find(|r| r.node_id == "m-hang")
            .unwrap();
        assert_eq!(hang_run.status, LoopRunStatus::Fail);
        assert_eq!(
            hang_run.output.as_ref().unwrap()["error"],
            "timed out",
            "member's own agent timeout must be recorded as such, not a straggler kill"
        );
    }

    // ── N2: on_completed hook tests ────────────────────────────────────

    /// Set up a temporary HOME with a canopy config containing a `test-cli`
    /// entry that points at `/bin/sh` — needed because `Cli::strategy()`
    /// reads from `~/.canopy/config.toml`.
    fn setup_test_cli_home() -> tempfile::TempDir {
        let fake_home = tempfile::tempdir().unwrap();
        let canopy_dir = fake_home.path().join(".canopy");
        std::fs::create_dir_all(&canopy_dir).unwrap();
        let config = crate::domain::canopy_config::CanopyConfig {
            configured_at: Some(chrono::Utc::now().to_rfc3339()),
            clis: vec![crate::domain::cli_config::CliConfig {
                name: "test-cli".to_string(),
                binary: "/bin/sh".to_string(),
                headless_mode: "-c".to_string(),
                model_flag: None,
                supports_working_dir: false,
                working_dir_flag: None,
                env_vars: std::collections::HashMap::new(),
                interactive_args: None,
                fallback_interactive_args: None,
                resume_args: None,
                session_list_cmd: None,
                session_resume_cmd: None,
                accent_color: None,
                yolo_flag: None,
                prompt_via_stdin: false,
                ..Default::default()
            }],
            ..Default::default()
        };
        config.save(&canopy_dir).unwrap();
        fake_home
    }

    /// Serializes [`HomeGuard`] users against each other. `HomeGuard` sets
    /// `CANOPY_HOME_OVERRIDE` rather than the real `HOME` specifically so
    /// unrelated tests (which never read that var) are unaffected — but the
    /// var is still process-wide, so the handful of tests that *do* use it
    /// must not run concurrently with each other.
    static HOME_OVERRIDE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard that sets `CANOPY_HOME_OVERRIDE` (consulted by
    /// [`crate::domain::models::Cli::strategy`]) for the duration of its
    /// lifetime and restores the previous value on drop. Deliberately not
    /// `HOME` itself: an earlier version of this guard swapped the real
    /// `HOME` env var, which raced with concurrently-running tests that
    /// shell out to git (git reads `HOME` for `user.name`/`user.email`),
    /// intermittently failing unrelated reviewer-commit tests under
    /// `cargo test`'s default parallel execution.
    struct HomeGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev: Option<String>,
    }

    impl HomeGuard {
        fn set(path: &std::path::Path) -> Self {
            let lock = HOME_OVERRIDE_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let prev = std::env::var("CANOPY_HOME_OVERRIDE").ok();
            unsafe {
                std::env::set_var("CANOPY_HOME_OVERRIDE", path);
            }
            Self { _lock: lock, prev }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(val) => unsafe {
                    std::env::set_var("CANOPY_HOME_OVERRIDE", val);
                },
                None => unsafe {
                    std::env::remove_var("CANOPY_HOME_OVERRIDE");
                },
            }
        }
    }

    /// Hook fires once when the loop completes. The mock process writes a
    /// marker file so we can verify it actually ran.
    #[tokio::test]
    async fn loop_engine_on_completed_hook_fires_on_completion() {
        let fake_home = setup_test_cli_home();
        let (dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();
        let marker = dir.path().join("hook_fired.marker");

        let node = LoopNode {
            id: "node-check".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        };
        db.insert_loop_node(&node).unwrap();

        let marker_path = marker.to_string_lossy().to_string();
        let hook = crate::domain::loops::LoopCompletionHook {
            platform: "test-cli".to_string(),
            model: None,
            prompt: format!("touch \"{}\"", marker_path),
            timeout_minutes: Some(1),
        };
        db.update_loop_completion_hook(&loop_id, Some(&hook))
            .unwrap();

        // Cli::strategy() reads from $CANOPY_HOME_OVERRIDE/.canopy/config.toml.
        let _home = HomeGuard::set(fake_home.path());
        let result = engine.run_loop(loop_id.clone(), None, None).await;
        drop(_home);
        drop(fake_home);
        result.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Completed);
        assert!(marker.exists(), "on_completed hook must have run");

        let hook_runs = db.list_loop_completion_hook_runs(&loop_id).unwrap();
        assert_eq!(hook_runs.len(), 1);
        assert_eq!(hook_runs[0].status, LoopRunStatus::Pass);
    }

    /// Hook must NOT fire when the loop fails (a spec's check node returns
    /// non-zero). Only `Completed` triggers it.
    #[tokio::test]
    async fn loop_engine_on_completed_hook_does_not_fire_on_failure() {
        let (dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();
        let marker = dir.path().join("hook_should_not_exist.marker");

        db.insert_loop_node(&LoopNode {
            id: "node-check".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "exit 1",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        let marker_path = marker.to_string_lossy().to_string();
        let hook = crate::domain::loops::LoopCompletionHook {
            platform: "test-cli".to_string(),
            model: None,
            prompt: format!("touch \"{}\"", marker_path),
            timeout_minutes: Some(1),
        };
        db.update_loop_completion_hook(&loop_id, Some(&hook))
            .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Failed);
        assert!(
            !marker.exists(),
            "on_completed hook must NOT fire on a failed loop"
        );

        let hook_runs = db.list_loop_completion_hook_runs(&loop_id).unwrap();
        assert!(hook_runs.is_empty(), "no hook runs should be recorded");
    }

    /// After a completed→reset→recomplete cycle, the hook fires again (once
    /// per completion).
    #[tokio::test]
    async fn loop_engine_on_completed_hook_fires_again_after_reset_and_recomplete() {
        let fake_home = setup_test_cli_home();
        let (dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();
        let marker = dir.path().join("hook_count.log");

        db.insert_loop_node(&LoopNode {
            id: "node-check".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        let marker_path = marker.to_string_lossy().to_string();
        let hook = crate::domain::loops::LoopCompletionHook {
            platform: "test-cli".to_string(),
            model: None,
            prompt: format!("echo fire >> \"{}\"", marker_path),
            timeout_minutes: Some(1),
        };
        db.update_loop_completion_hook(&loop_id, Some(&hook))
            .unwrap();

        let _home = HomeGuard::set(fake_home.path());
        // First completion.
        engine.run_loop(loop_id.clone(), None, None).await.unwrap();
        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Completed);

        // Reset and recomplete. `reset_loop`'s default (`specs: None`) leaves
        // an already-`Completed` spec untouched (see its doc) — pass the
        // spec id explicitly so it actually re-runs (B17: a dispatch that
        // executes zero specs must not fire the hook a second time for
        // doing nothing).
        db.reset_loop(&loop_id, Some(std::slice::from_ref(&spec_id)))
            .unwrap();
        engine.run_loop(loop_id.clone(), None, None).await.unwrap();
        drop(_home);
        drop(fake_home);

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Completed);

        let hook_runs = db.list_loop_completion_hook_runs(&loop_id).unwrap();
        assert_eq!(hook_runs.len(), 2, "hook must fire once per completion");
    }

    // ── B17: empty effective spec set is a launch error, not a completion ──

    /// The core incident: a loop with zero bound specs and no `pool_id`
    /// given must refuse to launch — not silently transition to
    /// `Completed`. Status must stay untouched and no run recorded.
    #[tokio::test]
    async fn loop_engine_zero_bound_specs_and_no_pool_is_a_launch_error() {
        let (_dir, db, engine, loop_id) = bare_loop_fixture().unwrap();

        let error = engine
            .run_loop(loop_id.clone(), None, None)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("no specs to run"),
            "unexpected error message: {error}"
        );

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(
            lp.status,
            LoopStatus::Draft,
            "an empty launch must leave the loop's status untouched"
        );
        assert!(
            db.list_loop_runs_for_loop(&loop_id).unwrap().is_empty(),
            "an empty launch must record no run"
        );
    }

    /// (Requirement 3) When the loop's last run was pool-driven and a fresh
    /// `loop_run` arrives without `pool_id` and finds zero bound specs, the
    /// error must name the last pool so a recovery agent can retry
    /// correctly instead of silently discarding the pool context.
    #[tokio::test]
    async fn loop_engine_pool_less_relaunch_after_pool_run_names_last_pool() {
        let (_dir, db, engine, loop_id) = bare_loop_fixture().unwrap();

        // Simulate the incident: a pool-driven run left interrupted (daemon
        // crash, quota failure) — `active_run_pool_id` stays persisted
        // (it's only ever cleared on a *genuine* completion) with pending
        // pool members still queued behind it.
        let pending = standalone_spec("pool-pending", 1);
        db.insert_loop_spec(&pending).unwrap();
        insert_pool_with_members(&db, "pool-1", &[&pending.id]);
        db.set_loop_active_run_pool(&loop_id, Some("pool-1"))
            .unwrap();

        // The recovery agent's mistake: relaunch directly (the loop's own
        // bound specs are still empty — every spec lives in the pool)
        // without passing `pool_id` back.
        let error = engine
            .run_loop(loop_id.clone(), None, None)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("pool-1"),
            "error must name the last pool so a recovery agent can retry correctly: {error}"
        );

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(
            lp.status,
            LoopStatus::Draft,
            "the failed pool-less relaunch must not touch the loop's status"
        );
        assert_eq!(
            lp.active_run_pool_id.as_deref(),
            Some("pool-1"),
            "the pool context must not be silently discarded by the failed relaunch"
        );
    }

    /// (Requirement 4b) A pool run where every member is already completed
    /// must be treated as the same empty-set error, not a fresh completed
    /// run — a pool is shared/reusable, so "nothing pending" is far more
    /// likely a stale/incorrect pool_id than a genuine finish.
    #[tokio::test]
    async fn loop_engine_pool_run_with_all_members_completed_is_a_launch_error() {
        let (_dir, db, engine, loop_id) = bare_loop_fixture().unwrap();

        let mut done = standalone_spec("pool-done", 1);
        done.status = LoopSpecStatus::Completed;
        db.insert_loop_spec(&done).unwrap();
        insert_pool_with_members(&db, "pool-1", &[&done.id]);

        let error = engine
            .run_loop(loop_id.clone(), Some("pool-1".to_string()), None)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("no specs to run"),
            "unexpected error message: {error}"
        );

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(
            lp.status,
            LoopStatus::Draft,
            "an empty pool launch must leave the loop's status untouched"
        );
    }

    /// (Requirement 4c) A normal pool run — real pending members — still
    /// completes and fires the `on_completed` hook exactly once; the B17
    /// guard must not interfere with a genuine completion.
    #[tokio::test]
    async fn loop_engine_normal_pool_run_still_completes_and_fires_hook_once() {
        let fake_home = setup_test_cli_home();
        let (dir, db, engine, loop_id) = bare_loop_fixture().unwrap();
        let marker = dir.path().join("hook_fired.marker");

        let spec = standalone_spec("pool-spec", 1);
        db.insert_loop_spec(&spec).unwrap();
        insert_pool_with_members(&db, "pool-1", &[&spec.id]);

        db.insert_loop_node(&LoopNode {
            id: "loop-check".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        let marker_path = marker.to_string_lossy().to_string();
        let hook = crate::domain::loops::LoopCompletionHook {
            platform: "test-cli".to_string(),
            model: None,
            prompt: format!("touch \"{}\"", marker_path),
            timeout_minutes: Some(1),
        };
        db.update_loop_completion_hook(&loop_id, Some(&hook))
            .unwrap();

        let _home = HomeGuard::set(fake_home.path());
        let result = engine
            .run_loop(loop_id.clone(), Some("pool-1".to_string()), None)
            .await;
        drop(_home);
        drop(fake_home);
        result.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Completed);
        assert!(marker.exists(), "on_completed hook must have run");

        let hook_runs = db.list_loop_completion_hook_runs(&loop_id).unwrap();
        assert_eq!(hook_runs.len(), 1, "hook must fire exactly once");
    }

    /// A loop whose bound specs are non-empty but were *all* already
    /// completed/skipped before this dispatch (e.g. the loop's last spec was
    /// explicitly skipped via `loop_continue`) legitimately completes — the
    /// B17 guard only fires on *zero bound specs*, not "zero pending" — but
    /// must not fire the hook, since this dispatch executed nothing.
    #[tokio::test]
    async fn loop_engine_all_bound_specs_already_done_completes_without_firing_hook() {
        let fake_home = setup_test_cli_home();
        let (dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();
        let marker = dir.path().join("hook_should_not_exist.marker");

        db.update_loop_spec_status(
            &spec_id,
            LoopSpecStatus::Skipped,
            None,
            Some(chrono::Utc::now()),
        )
        .unwrap();

        let marker_path = marker.to_string_lossy().to_string();
        let hook = crate::domain::loops::LoopCompletionHook {
            platform: "test-cli".to_string(),
            model: None,
            prompt: format!("touch \"{}\"", marker_path),
            timeout_minutes: Some(1),
        };
        db.update_loop_completion_hook(&loop_id, Some(&hook))
            .unwrap();

        let _home = HomeGuard::set(fake_home.path());
        let result = engine.run_loop(loop_id.clone(), None, None).await;
        drop(_home);
        drop(fake_home);
        result.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(
            lp.status,
            LoopStatus::Completed,
            "a loop whose only bound spec is already skipped is genuinely done"
        );
        assert!(
            !marker.exists(),
            "on_completed must not fire for a dispatch that executed zero specs"
        );
        assert!(db
            .list_loop_completion_hook_runs(&loop_id)
            .unwrap()
            .is_empty());
    }

    /// Placeholder interpolation: `{{loop_name}}`, `{{workdir}}`,
    /// `{{completed_specs}}` must all be substituted.
    #[tokio::test]
    async fn render_completion_hook_prompt_substitutes_all_placeholders() {
        let lp = crate::domain::loops::Loop {
            id: "wf".to_string(),
            name: "MyLoop".to_string(),
            description: None,
            workdir: "/tmp/proj".to_string(),
            status: LoopStatus::Draft,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            active_run_pool_id: None,
            on_completed: None,
        };
        let completed_specs = vec![
            ("Spec-A".to_string(), "summary A".to_string()),
            ("Spec-B".to_string(), "summary B".to_string()),
        ];

        let result = render_completion_hook_prompt(
            &lp,
            &lp.workdir,
            &completed_specs,
            "Loop={{loop_name}} Workdir={{workdir}} Specs={{completed_specs}}",
        );

        assert_eq!(
            result,
            "Loop=MyLoop Workdir=/tmp/proj Specs=- Spec-A: summary A\n- Spec-B: summary B"
        );
    }

    /// Empty completed_specs list renders `(none)`.
    #[tokio::test]
    async fn render_completion_hook_prompt_empty_specs_shows_none() {
        let lp = crate::domain::loops::Loop {
            id: "wf".to_string(),
            name: "Loop".to_string(),
            description: None,
            workdir: "/tmp".to_string(),
            status: LoopStatus::Draft,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            active_run_pool_id: None,
            on_completed: None,
        };

        let result = render_completion_hook_prompt(&lp, &lp.workdir, &[], "{{completed_specs}}");

        assert_eq!(result, "(none)");
    }

    /// A hook failure does not change the loop's already-final status — the
    /// loop is `Completed` even though the hook exited non-zero.
    #[tokio::test]
    async fn loop_engine_hook_failure_does_not_alter_loop_status() {
        let fake_home = setup_test_cli_home();
        let (_dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();

        db.insert_loop_node(&LoopNode {
            id: "node-check".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        // Hook that always fails (exit 1).
        let hook = crate::domain::loops::LoopCompletionHook {
            platform: "test-cli".to_string(),
            model: None,
            prompt: "exit 1".to_string(),
            timeout_minutes: Some(1),
        };
        db.update_loop_completion_hook(&loop_id, Some(&hook))
            .unwrap();

        let _home = HomeGuard::set(fake_home.path());
        let result = engine.run_loop(loop_id.clone(), None, None).await;
        drop(_home);
        drop(fake_home);
        result.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(
            lp.status,
            LoopStatus::Completed,
            "loop must stay Completed even when its on_completed hook fails"
        );

        let hook_runs = db.list_loop_completion_hook_runs(&loop_id).unwrap();
        assert_eq!(hook_runs.len(), 1);
        assert_eq!(hook_runs[0].status, LoopRunStatus::Fail);
    }

    /// Process group children must die with the parent: spawn a check node
    /// that forks a grandchild via `sh -c` subshell, then verify the
    /// grandchild is gone after the check is terminated.
    #[cfg(unix)]
    #[tokio::test]
    async fn process_group_children_die_with_parent() {
        let (dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();
        let marker = dir.path().join("grandchild_alive");

        // The check node forks a grandchild that sleeps and touches a
        // marker file. If the process group kill works, the grandchild
        // dies before the marker appears.
        db.insert_loop_node(&LoopNode {
            id: "check-pg".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "check-pg".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": format!(
                    "( sleep 60; touch \"{}\" ) & exit 1",
                    marker.display()
                ),
                "success_condition": "exit_code_0",
                "timeout_seconds": 3,
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        // Wait for the grace period + a bit extra for the grandchild.
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;

        assert!(
            !marker.exists(),
            "grandchild process should have been killed by process-group termination; \
             marker file should not exist"
        );
    }

    // ── F1: ensemble execution (execute_ensemble) ───────────────────────

    /// Writes an executable POSIX shell script at `dir/name` with `body` as
    /// its content and returns its absolute path. Used to give each
    /// ensemble member deterministic, script-controlled pass/fail/hang
    /// behavior — the member's actual prompt content is irrelevant (the
    /// script ignores stdin/argv entirely), so this sidesteps having to
    /// reverse-engineer `render_agent_prompt`'s wrapped output as a runnable
    /// shell script.
    fn write_member_script(dir: &std::path::Path, name: &str, body: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path.to_string_lossy().to_string()
    }

    /// A fake `.canopy/config.toml` registering one CLI entry per
    /// `(name, binary)` pair — lets each ensemble member run its own script
    /// under its own `platform` name, so a single ensemble can exercise
    /// pass/fail/hang members side by side in the same run.
    fn setup_multi_cli_home(clis: &[(&str, &str)]) -> tempfile::TempDir {
        let fake_home = tempfile::tempdir().unwrap();
        let canopy_dir = fake_home.path().join(".canopy");
        std::fs::create_dir_all(&canopy_dir).unwrap();
        let config = crate::domain::canopy_config::CanopyConfig {
            configured_at: Some(chrono::Utc::now().to_rfc3339()),
            clis: clis
                .iter()
                .map(|(name, binary)| crate::domain::cli_config::CliConfig {
                    name: name.to_string(),
                    binary: binary.to_string(),
                    headless_mode: String::new(),
                    model_flag: None,
                    supports_working_dir: false,
                    working_dir_flag: None,
                    env_vars: std::collections::HashMap::new(),
                    interactive_args: None,
                    fallback_interactive_args: None,
                    resume_args: None,
                    session_list_cmd: None,
                    session_resume_cmd: None,
                    accent_color: None,
                    yolo_flag: None,
                    prompt_via_stdin: true,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        config.save(&canopy_dir).unwrap();
        fake_home
    }

    /// Builds a real ensemble unit (kickoff -> N members -> join -> pass/fail
    /// exits) directly against the DB, the same shape `loop_add_ensemble`
    /// assembles in one MCP call — but constructed here node-by-node so
    /// engine tests can drive it through the real `execute_ensemble` path
    /// via `LoopEngine::run_loop` without spinning up the MCP server.
    #[allow(clippy::too_many_arguments)]
    fn insert_test_ensemble(
        db: &Database,
        spec_id: &str,
        kickoff_id: &str,
        ensemble_id: &str,
        join_id: &str,
        members: &[(&str, &str)], // (node_id, cli_platform_name)
        min_pass: i64,
        straggler_timeout_minutes: Option<i64>,
        on_pass_to: &str,
        on_fail_to: Option<&str>,
    ) {
        let now = chrono::Utc::now();

        db.insert_loop_node(&LoopNode {
            id: kickoff_id.to_string(),
            spec_id: Some(spec_id.to_string()),
            loop_id: None,
            name: "kickoff".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf ok",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: now,
        })
        .unwrap();

        let member_nodes: Vec<LoopNode> = members
            .iter()
            .enumerate()
            .map(|(i, (node_id, platform))| LoopNode {
                id: node_id.to_string(),
                spec_id: Some(spec_id.to_string()),
                loop_id: None,
                name: format!("member-{}", i + 1),
                kind: LoopNodeKind::Agent,
                config: serde_json::json!({
                    "platform": platform,
                    "prompt_template": "ignored by the member's test script",
                    "timeout_minutes": 5,
                }),
                position: 2 + i as i64,
                created_at: now,
            })
            .collect();

        let join_node = LoopNode {
            id: join_id.to_string(),
            spec_id: Some(spec_id.to_string()),
            loop_id: None,
            name: "quorum".to_string(),
            kind: LoopNodeKind::Join,
            config: serde_json::json!({ "ensemble_id": ensemble_id }),
            position: 2 + members.len() as i64,
            created_at: now,
        };

        let mut edges = Vec::new();
        for (node_id, _) in members {
            edges.push(LoopEdge {
                id: format!("{kickoff_id}->{node_id}"),
                spec_id: Some(spec_id.to_string()),
                loop_id: None,
                from_node: kickoff_id.to_string(),
                to_node: node_id.to_string(),
                condition: LoopEdgeCondition::Always,
            });
            edges.push(LoopEdge {
                id: format!("{node_id}->{join_id}"),
                spec_id: Some(spec_id.to_string()),
                loop_id: None,
                from_node: node_id.to_string(),
                to_node: join_id.to_string(),
                condition: LoopEdgeCondition::Always,
            });
        }
        edges.push(LoopEdge {
            id: format!("{join_id}->pass"),
            spec_id: Some(spec_id.to_string()),
            loop_id: None,
            from_node: join_id.to_string(),
            to_node: on_pass_to.to_string(),
            condition: LoopEdgeCondition::Pass,
        });
        if let Some(fail_to) = on_fail_to {
            edges.push(LoopEdge {
                id: format!("{join_id}->fail"),
                spec_id: Some(spec_id.to_string()),
                loop_id: None,
                from_node: join_id.to_string(),
                to_node: fail_to.to_string(),
                condition: LoopEdgeCondition::Fail,
            });
        }

        let ensemble = crate::domain::loops::Ensemble {
            id: ensemble_id.to_string(),
            spec_id: Some(spec_id.to_string()),
            loop_id: None,
            name: "Test Ensemble".to_string(),
            prompt_template: "ignored by the member's test script".to_string(),
            join_node_id: join_id.to_string(),
            entry_from_node: kickoff_id.to_string(),
            entry_condition: LoopEdgeCondition::Always,
            min_pass,
            straggler_timeout_minutes,
            timeout_minutes: 5,
            on_pass_to: on_pass_to.to_string(),
            on_fail_to: on_fail_to.map(str::to_string),
            created_at: now,
        };
        let ensemble_members: Vec<EnsembleMember> = members
            .iter()
            .enumerate()
            .map(|(i, (node_id, platform))| EnsembleMember {
                ensemble_id: ensemble_id.to_string(),
                node_id: node_id.to_string(),
                position: i as i64,
                platform: platform.to_string(),
                model: None,
            })
            .collect();

        db.insert_ensemble_unit(
            &ensemble,
            &ensemble_members,
            &member_nodes,
            &join_node,
            &edges,
        )
        .unwrap();
    }

    fn touch_marker_node(
        id: &str,
        spec_id: &str,
        marker: &std::path::Path,
        position: i64,
    ) -> LoopNode {
        LoopNode {
            id: id.to_string(),
            spec_id: Some(spec_id.to_string()),
            loop_id: None,
            name: id.to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": format!("touch \"{}\"", marker.display()),
                "success_condition": "exit_code_0"
            }),
            position,
            created_at: chrono::Utc::now(),
        }
    }

    fn join_run(db: &Database, spec_id: &str, join_id: &str) -> LoopNodeRun {
        db.list_loop_runs_for_spec(spec_id)
            .unwrap()
            .into_iter()
            .rfind(|run| run.node_id == join_id)
            .expect("join must have produced a run row")
    }

    /// Wait-all (F1): the join must never fire before every member has
    /// finished. A fast member (instant) and a deliberately slower member
    /// (sleeps ~1s) run side by side; if the engine consolidated as soon as
    /// the fast one finished, the whole ensemble would complete in well
    /// under a second. Asserting on wall-clock elapsed time — not just the
    /// final consolidated output — is what actually proves the wait, since
    /// the output alone can't distinguish "waited" from "raced and got
    /// lucky".
    #[tokio::test]
    async fn ensemble_execute_waits_for_slowest_member_before_joining() {
        let (dir, db, engine, _loop_id, spec_id) = loop_fixture().unwrap();
        let fake_home = setup_multi_cli_home(&[
            (
                "member-fast",
                &write_member_script(dir.path(), "fast.sh", "printf FAST; exit 0"),
            ),
            (
                "member-slow",
                &write_member_script(dir.path(), "slow.sh", "sleep 1; printf SLOW; exit 0"),
            ),
        ]);
        let pass_marker = dir.path().join("pass.marker");
        db.insert_loop_node(&touch_marker_node("on-pass", &spec_id, &pass_marker, 100))
            .unwrap();
        insert_test_ensemble(
            &db,
            &spec_id,
            "kickoff",
            "ens1",
            "join1",
            &[("m-fast", "member-fast"), ("m-slow", "member-slow")],
            2,
            Some(1),
            "on-pass",
            None,
        );

        let _home = HomeGuard::set(fake_home.path());
        let started = std::time::Instant::now();
        engine
            .run_loop("wf-test".to_string(), None, None)
            .await
            .unwrap();
        let elapsed = started.elapsed();
        drop(_home);

        assert!(
            elapsed >= std::time::Duration::from_millis(900),
            "join must not fire before the slow member finishes (elapsed: {elapsed:?})"
        );
        assert!(
            pass_marker.exists(),
            "ensemble must have passed and routed onward"
        );

        let join = join_run(&db, &spec_id, "join1");
        assert_eq!(join.status, LoopRunStatus::Pass);
        let doc = join.output.unwrap()["consolidated_doc"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(doc.contains("FAST") && doc.contains("SLOW"));
    }

    /// Concurrency cap (F1): `with_ensemble_concurrency_cap` must actually
    /// bound how many members run at once, not just accept the value. Three
    /// members each sleep ~0.3s; under a cap of 1 they're forced to run one
    /// at a time, so the ensemble can only finish in >= ~0.9s. Asserting on
    /// wall-clock elapsed time is what actually proves serialization — the
    /// consolidated output alone can't distinguish "capped" from "raced and
    /// got lucky", mirroring `ensemble_execute_waits_for_slowest_member_before_joining`.
    #[tokio::test]
    async fn ensemble_execute_respects_configured_concurrency_cap() {
        let (dir, db, engine, _loop_id, spec_id) = loop_fixture().unwrap();
        let engine = engine.with_ensemble_concurrency_cap(1);
        let fake_home = setup_multi_cli_home(&[
            (
                "member-a",
                &write_member_script(dir.path(), "a.sh", "sleep 0.3; printf A; exit 0"),
            ),
            (
                "member-b",
                &write_member_script(dir.path(), "b.sh", "sleep 0.3; printf B; exit 0"),
            ),
            (
                "member-c",
                &write_member_script(dir.path(), "c.sh", "sleep 0.3; printf C; exit 0"),
            ),
        ]);
        let pass_marker = dir.path().join("pass.marker");
        db.insert_loop_node(&touch_marker_node("on-pass", &spec_id, &pass_marker, 100))
            .unwrap();
        insert_test_ensemble(
            &db,
            &spec_id,
            "kickoff",
            "ens1",
            "join1",
            &[
                ("m-a", "member-a"),
                ("m-b", "member-b"),
                ("m-c", "member-c"),
            ],
            3,
            Some(1),
            "on-pass",
            None,
        );

        let _home = HomeGuard::set(fake_home.path());
        let started = std::time::Instant::now();
        engine
            .run_loop("wf-test".to_string(), None, None)
            .await
            .unwrap();
        let elapsed = started.elapsed();
        drop(_home);

        assert!(
            elapsed >= std::time::Duration::from_millis(850),
            "a concurrency cap of 1 must serialize all three members (elapsed: {elapsed:?})"
        );
        assert!(
            pass_marker.exists(),
            "ensemble must still pass and route onward once serialized"
        );

        let join = join_run(&db, &spec_id, "join1");
        assert_eq!(join.status, LoopRunStatus::Pass);
        let doc = join.output.unwrap()["consolidated_doc"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(doc.contains('A') && doc.contains('B') && doc.contains('C'));
    }

    /// min_pass routing: enough members pass -> join Pass -> on_pass_to.
    #[tokio::test]
    async fn ensemble_execute_min_pass_met_routes_to_on_pass_to() {
        let (dir, db, engine, _loop_id, spec_id) = loop_fixture().unwrap();
        let fake_home = setup_multi_cli_home(&[
            (
                "member-ok-a",
                &write_member_script(dir.path(), "a.sh", "exit 0"),
            ),
            (
                "member-ok-b",
                &write_member_script(dir.path(), "b.sh", "exit 0"),
            ),
            (
                "member-bad",
                &write_member_script(dir.path(), "c.sh", "exit 1"),
            ),
        ]);
        let pass_marker = dir.path().join("pass.marker");
        let fail_marker = dir.path().join("fail.marker");
        db.insert_loop_node(&touch_marker_node("on-pass", &spec_id, &pass_marker, 100))
            .unwrap();
        db.insert_loop_node(&touch_marker_node("on-fail", &spec_id, &fail_marker, 101))
            .unwrap();
        insert_test_ensemble(
            &db,
            &spec_id,
            "kickoff",
            "ens1",
            "join1",
            &[
                ("m-a", "member-ok-a"),
                ("m-b", "member-ok-b"),
                ("m-c", "member-bad"),
            ],
            2,
            Some(1),
            "on-pass",
            Some("on-fail"),
        );

        let _home = HomeGuard::set(fake_home.path());
        engine
            .run_loop("wf-test".to_string(), None, None)
            .await
            .unwrap();
        drop(_home);

        let join = join_run(&db, &spec_id, "join1");
        assert_eq!(join.status, LoopRunStatus::Pass);
        assert_eq!(join.output.unwrap()["passed"], 2);
        assert!(pass_marker.exists(), "must route to on_pass_to");
        assert!(!fail_marker.exists(), "must not route to on_fail_to");
    }

    /// min_pass routing: too few members pass -> join Fail -> on_fail_to.
    #[tokio::test]
    async fn ensemble_execute_min_pass_not_met_routes_to_on_fail_to() {
        let (dir, db, engine, _loop_id, spec_id) = loop_fixture().unwrap();
        let fake_home = setup_multi_cli_home(&[
            (
                "member-ok",
                &write_member_script(dir.path(), "a.sh", "exit 0"),
            ),
            (
                "member-bad-a",
                &write_member_script(dir.path(), "b.sh", "exit 1"),
            ),
            (
                "member-bad-b",
                &write_member_script(dir.path(), "c.sh", "exit 1"),
            ),
        ]);
        let pass_marker = dir.path().join("pass.marker");
        let fail_marker = dir.path().join("fail.marker");
        db.insert_loop_node(&touch_marker_node("on-pass", &spec_id, &pass_marker, 100))
            .unwrap();
        db.insert_loop_node(&touch_marker_node("on-fail", &spec_id, &fail_marker, 101))
            .unwrap();
        insert_test_ensemble(
            &db,
            &spec_id,
            "kickoff",
            "ens1",
            "join1",
            &[
                ("m-a", "member-ok"),
                ("m-b", "member-bad-a"),
                ("m-c", "member-bad-b"),
            ],
            2,
            Some(1),
            "on-pass",
            Some("on-fail"),
        );

        let _home = HomeGuard::set(fake_home.path());
        engine
            .run_loop("wf-test".to_string(), None, None)
            .await
            .unwrap();
        drop(_home);

        let join = join_run(&db, &spec_id, "join1");
        assert_eq!(join.status, LoopRunStatus::Fail);
        assert_eq!(join.output.unwrap()["passed"], 1);
        assert!(!pass_marker.exists(), "must not route to on_pass_to");
        assert!(fail_marker.exists(), "must route to on_fail_to");
    }

    /// Consolidation order is deterministic (member position order), not
    /// completion order: member 1 is the slow one here, member 2 finishes
    /// first, but the consolidated doc must still list member 1 before
    /// member 2.
    #[tokio::test]
    async fn ensemble_execute_consolidates_in_member_position_order() {
        let (dir, db, engine, _loop_id, spec_id) = loop_fixture().unwrap();
        let fake_home = setup_multi_cli_home(&[
            (
                "member-a-slow",
                &write_member_script(dir.path(), "a.sh", "sleep 1; printf A; exit 0"),
            ),
            (
                "member-b-fast",
                &write_member_script(dir.path(), "b.sh", "printf B; exit 0"),
            ),
        ]);
        let pass_marker = dir.path().join("pass.marker");
        db.insert_loop_node(&touch_marker_node("on-pass", &spec_id, &pass_marker, 100))
            .unwrap();
        insert_test_ensemble(
            &db,
            &spec_id,
            "kickoff",
            "ens1",
            "join1",
            &[("m-a", "member-a-slow"), ("m-b", "member-b-fast")],
            2,
            Some(1),
            "on-pass",
            None,
        );

        let _home = HomeGuard::set(fake_home.path());
        engine
            .run_loop("wf-test".to_string(), None, None)
            .await
            .unwrap();
        drop(_home);

        let join = join_run(&db, &spec_id, "join1");
        let doc = join.output.unwrap()["consolidated_doc"]
            .as_str()
            .unwrap()
            .to_string();
        let pos_a = doc
            .find("## member-a-slow")
            .expect("member-a-slow section must exist");
        let pos_b = doc
            .find("## member-b-fast")
            .expect("member-b-fast section must exist");
        assert!(
            pos_a < pos_b,
            "consolidated doc must list members in position order, not completion order"
        );
    }

    /// Straggler kill + fail counting (B12): a member that hangs past the
    /// ensemble's straggler timeout is killed at the OS level (not just
    /// marked failed while the process keeps running), and counts as a
    /// failed member in the join's tally. Both members hang here — using a
    /// `straggler_timeout_minutes: 0` (immediate) alongside a member that's
    /// meant to finish quickly would race the timeout against real work;
    /// isolating the straggler behavior to every member avoids that.
    #[tokio::test]
    async fn ensemble_execute_straggler_timeout_kills_process_and_counts_as_fail() {
        let (dir, db, engine, _loop_id, spec_id) = loop_fixture().unwrap();
        let marker_a = dir.path().join("a_survived.marker");
        let marker_b = dir.path().join("b_survived.marker");
        let fake_home = setup_multi_cli_home(&[
            (
                "member-hang-a",
                &write_member_script(
                    dir.path(),
                    "a.sh",
                    &format!("sleep 3; touch \"{}\"", marker_a.display()),
                ),
            ),
            (
                "member-hang-b",
                &write_member_script(
                    dir.path(),
                    "b.sh",
                    &format!("sleep 3; touch \"{}\"", marker_b.display()),
                ),
            ),
        ]);
        let fail_marker = dir.path().join("fail.marker");
        db.insert_loop_node(&touch_marker_node(
            "on-pass",
            &spec_id,
            &dir.path().join("pass.marker"),
            100,
        ))
        .unwrap();
        db.insert_loop_node(&touch_marker_node("on-fail", &spec_id, &fail_marker, 101))
            .unwrap();
        insert_test_ensemble(
            &db,
            &spec_id,
            "kickoff",
            "ens1",
            "join1",
            &[("m-a", "member-hang-a"), ("m-b", "member-hang-b")],
            1,
            Some(0),
            "on-pass",
            Some("on-fail"),
        );

        let _home = HomeGuard::set(fake_home.path());
        engine
            .run_loop("wf-test".to_string(), None, None)
            .await
            .unwrap();
        drop(_home);

        let join = join_run(&db, &spec_id, "join1");
        assert_eq!(
            join.status,
            LoopRunStatus::Fail,
            "both members killed as stragglers -> zero passed -> join fails"
        );
        assert_eq!(join.output.as_ref().unwrap()["passed"], 0);
        assert!(fail_marker.exists(), "must route to on_fail_to");

        // Give the OS a moment past the members' scripted 3s sleep to prove
        // the processes were actually killed, not merely marked failed
        // while still running in the background.
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        assert!(
            !marker_a.exists(),
            "straggler member a must have been killed"
        );
        assert!(
            !marker_b.exists(),
            "straggler member b must have been killed"
        );
    }

    // ── B26: ensemble member infra-crash retry ──────────────────────────

    /// Like [`insert_test_ensemble`], but merges `member_config` into every
    /// member node's config — used by the B26 tests to set
    /// `infra_backoff_seconds: 0` so retries don't actually sleep.
    fn insert_infra_ensemble(
        db: &Database,
        spec_id: &str,
        members: &[(&str, &str)],
        min_pass: i64,
        straggler_timeout_minutes: Option<i64>,
        member_config: &Value,
    ) {
        let now = chrono::Utc::now();
        db.insert_loop_node(&LoopNode {
            id: "kickoff".to_string(),
            spec_id: Some(spec_id.to_string()),
            loop_id: None,
            name: "kickoff".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({ "command": "printf ok", "success_condition": "exit_code_0" }),
            position: 1,
            created_at: now,
        })
        .unwrap();

        let member_nodes: Vec<LoopNode> = members
            .iter()
            .enumerate()
            .map(|(i, (node_id, platform))| {
                let mut config = serde_json::json!({
                    "platform": platform,
                    "prompt_template": "ignored by the member's test script",
                    "timeout_minutes": 5,
                });
                if let Value::Object(extra) = member_config {
                    for (k, v) in extra {
                        config[k] = v.clone();
                    }
                }
                LoopNode {
                    id: node_id.to_string(),
                    spec_id: Some(spec_id.to_string()),
                    loop_id: None,
                    name: format!("member-{}", i + 1),
                    kind: LoopNodeKind::Agent,
                    config,
                    position: 2 + i as i64,
                    created_at: now,
                }
            })
            .collect();

        let join_node = LoopNode {
            id: "join1".to_string(),
            spec_id: Some(spec_id.to_string()),
            loop_id: None,
            name: "quorum".to_string(),
            kind: LoopNodeKind::Join,
            config: serde_json::json!({ "ensemble_id": "ens1" }),
            position: 2 + members.len() as i64,
            created_at: now,
        };

        let mut edges = Vec::new();
        for (node_id, _) in members {
            edges.push(LoopEdge {
                id: format!("kickoff->{node_id}"),
                spec_id: Some(spec_id.to_string()),
                loop_id: None,
                from_node: "kickoff".to_string(),
                to_node: node_id.to_string(),
                condition: LoopEdgeCondition::Always,
            });
            edges.push(LoopEdge {
                id: format!("{node_id}->join1"),
                spec_id: Some(spec_id.to_string()),
                loop_id: None,
                from_node: node_id.to_string(),
                to_node: "join1".to_string(),
                condition: LoopEdgeCondition::Always,
            });
        }
        edges.push(LoopEdge {
            id: "join1->pass".to_string(),
            spec_id: Some(spec_id.to_string()),
            loop_id: None,
            from_node: "join1".to_string(),
            to_node: "done".to_string(),
            condition: LoopEdgeCondition::Pass,
        });

        // Terminal marker node so a passing join has somewhere to route.
        db.insert_loop_node(&LoopNode {
            id: "done".to_string(),
            spec_id: Some(spec_id.to_string()),
            loop_id: None,
            name: "done".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({ "command": "printf ok", "success_condition": "exit_code_0" }),
            position: 200,
            created_at: now,
        })
        .unwrap();

        let ensemble = crate::domain::loops::Ensemble {
            id: "ens1".to_string(),
            spec_id: Some(spec_id.to_string()),
            loop_id: None,
            name: "Test Ensemble".to_string(),
            prompt_template: "ignored by the member's test script".to_string(),
            join_node_id: "join1".to_string(),
            entry_from_node: "kickoff".to_string(),
            entry_condition: LoopEdgeCondition::Always,
            min_pass,
            straggler_timeout_minutes,
            timeout_minutes: 5,
            on_pass_to: "done".to_string(),
            on_fail_to: None,
            created_at: now,
        };
        let ensemble_members: Vec<EnsembleMember> = members
            .iter()
            .enumerate()
            .map(|(i, (node_id, platform))| EnsembleMember {
                ensemble_id: "ens1".to_string(),
                node_id: node_id.to_string(),
                position: i as i64,
                platform: platform.to_string(),
                model: None,
            })
            .collect();

        db.insert_ensemble_unit(
            &ensemble,
            &ensemble_members,
            &member_nodes,
            &join_node,
            &edges,
        )
        .unwrap();
    }

    fn member_runs(db: &Database, spec_id: &str, node_id: &str) -> Vec<LoopNodeRun> {
        let mut runs: Vec<LoopNodeRun> = db
            .list_loop_runs_for_spec(spec_id)
            .unwrap()
            .into_iter()
            .filter(|r| r.node_id == node_id)
            .collect();
        runs.sort_by_key(|r| r.started_at);
        runs
    }

    /// A crashed member (fast nonzero exit, no self-report) is retried in
    /// place like a lone agent node (B19); succeeding on the retry makes it
    /// count as a pass, so with a second healthy member the join passes 2/2.
    /// (Two members because ensemble fan-out needs more than one entry edge.)
    #[tokio::test]
    async fn ensemble_member_infra_crash_then_succeeds_on_retry_join_passes() {
        let (dir, db, engine, _loop_id, spec_id) = loop_fixture().unwrap();
        let counter = dir.path().join("flap.counter");
        // Crashes (exit 1) on the first attempt, passes (exit 0) on the retry.
        let flap = write_member_script(
            dir.path(),
            "flap.sh",
            &format!(
                "n=$(cat \"{c}\" 2>/dev/null || echo 0); n=$((n+1)); echo $n > \"{c}\"; [ \"$n\" -ge 2 ] && exit 0 || exit 1",
                c = counter.display(),
            ),
        );
        let fake_home = setup_multi_cli_home(&[
            ("member-flap", &flap),
            (
                "member-ok",
                &write_member_script(dir.path(), "ok.sh", "exit 0"),
            ),
        ]);
        insert_infra_ensemble(
            &db,
            &spec_id,
            &[("m-flap", "member-flap"), ("m-ok", "member-ok")],
            2,
            Some(1),
            &serde_json::json!({ "infra_backoff_seconds": 0 }),
        );

        let _home = HomeGuard::set(fake_home.path());
        engine
            .run_loop("wf-test".to_string(), None, None)
            .await
            .unwrap();
        drop(_home);

        let join = join_run(&db, &spec_id, "join1");
        assert_eq!(
            join.status,
            LoopRunStatus::Pass,
            "flaky member passed on retry, healthy member passed -> 2/2 -> join passes"
        );
        assert_eq!(join.output.as_ref().unwrap()["passed"], 2);

        let runs = member_runs(&db, &spec_id, "m-flap");
        assert_eq!(runs.len(), 2, "one crash + one successful retry = two rows");
        assert_eq!(runs[0].status, LoopRunStatus::Fail);
        assert_eq!(
            runs[0].output.as_ref().unwrap()["infra_crash"],
            serde_json::Value::Bool(true),
            "the crashed attempt carries the B19 infra_crash marker"
        );
        assert_eq!(runs[0].output.as_ref().unwrap()["infra_attempt"], 0);
        assert_eq!(runs[1].status, LoopRunStatus::Pass);
    }

    /// A member that keeps crashing exhausts its retry budget (default 2 → 3
    /// attempts) and only then counts as a member fail. With min_pass=2 and a
    /// second, healthy member, the join arithmetic is 1/2 → Fail.
    #[tokio::test]
    async fn ensemble_member_infra_retries_exhausted_counts_as_member_fail() {
        let (dir, db, engine, _loop_id, spec_id) = loop_fixture().unwrap();
        let fake_home = setup_multi_cli_home(&[
            (
                "member-dead",
                &write_member_script(dir.path(), "dead.sh", "exit 1"),
            ),
            (
                "member-ok",
                &write_member_script(dir.path(), "ok.sh", "exit 0"),
            ),
        ]);
        insert_infra_ensemble(
            &db,
            &spec_id,
            &[("m-dead", "member-dead"), ("m-ok", "member-ok")],
            2,
            Some(1),
            &serde_json::json!({ "infra_backoff_seconds": 0 }),
        );

        let _home = HomeGuard::set(fake_home.path());
        engine
            .run_loop("wf-test".to_string(), None, None)
            .await
            .unwrap();
        drop(_home);

        let join = join_run(&db, &spec_id, "join1");
        assert_eq!(
            join.status,
            LoopRunStatus::Fail,
            "one member permanently down -> 1/2 -> join fails"
        );
        assert_eq!(join.output.as_ref().unwrap()["passed"], 1);

        // retry_limit default 2 -> attempts 0,1,2 -> three distinct run rows,
        // the first two carrying infra_crash markers.
        let dead = member_runs(&db, &spec_id, "m-dead");
        assert_eq!(
            dead.len(),
            3,
            "two retries after the first crash = three rows"
        );
        assert!(dead.iter().all(|r| r.status == LoopRunStatus::Fail));
        assert_eq!(
            dead[0].output.as_ref().unwrap()["infra_crash"],
            serde_json::Value::Bool(true)
        );
        assert_eq!(
            dead[1].output.as_ref().unwrap()["infra_crash"],
            serde_json::Value::Bool(true)
        );
        assert_eq!(dead[0].output.as_ref().unwrap()["infra_attempt"], 0);
        assert_eq!(dead[1].output.as_ref().unwrap()["infra_attempt"], 1);
    }

    /// Straggler-window interaction (documented behavior): the ensemble's
    /// straggler timeout bounds the ENTIRE retry sequence, not a single
    /// attempt. When the window expires before a member resolves (still
    /// executing, or mid-backoff between retries), the member is counted as
    /// failed deterministically and its live attempt killed — never left to
    /// retry past the window, never silently abandoned.
    ///
    /// Chosen/documented behavior: fail-deterministically-on-window-expiry.
    /// The members here have infra retry enabled but each sleeps well past the
    /// zero-length straggler window, so the window always expires first — the
    /// retry loop is dropped mid-attempt and both members resolve to Fail
    /// (0/2), exactly as a lone straggler would, rather than being retried out
    /// past the window or hanging the join. (Sleeping members make the kill
    /// deterministic; a fast-exiting member could race a zero-length window.)
    #[tokio::test]
    async fn ensemble_member_straggler_window_bounds_the_retry_sequence() {
        let (dir, db, engine, _loop_id, spec_id) = loop_fixture().unwrap();
        let marker = dir.path().join("retried.marker");
        // Would crash (exit 1) after a 3s sleep and then, on a retry, create a
        // marker — but the zero-length straggler window kills it long before
        // either its crash or any retry can happen.
        let flap = write_member_script(
            dir.path(),
            "flap.sh",
            &format!("sleep 3; touch \"{}\"; exit 1", marker.display()),
        );
        let fake_home = setup_multi_cli_home(&[
            ("member-flap", &flap),
            (
                "member-slow-ok",
                &write_member_script(dir.path(), "ok.sh", "sleep 3; exit 0"),
            ),
        ]);
        insert_infra_ensemble(
            &db,
            &spec_id,
            &[("m-flap", "member-flap"), ("m-ok", "member-slow-ok")],
            1,
            Some(0), // zero-length window: expires before either member resolves
            &serde_json::json!({ "infra_backoff_seconds": 0 }),
        );

        let _home = HomeGuard::set(fake_home.path());
        engine
            .run_loop("wf-test".to_string(), None, None)
            .await
            .unwrap();
        drop(_home);

        let join = join_run(&db, &spec_id, "join1");
        assert_eq!(
            join.status,
            LoopRunStatus::Fail,
            "straggler window expired before any member resolved -> 0/2 -> join fails"
        );
        assert_eq!(join.output.as_ref().unwrap()["passed"], 0);

        // Deterministic resolution: no member run row is left Running.
        assert!(
            member_runs(&db, &spec_id, "m-flap")
                .iter()
                .all(|r| r.status != LoopRunStatus::Running),
            "the straggler-timed-out member must be resolved, not abandoned Running"
        );

        // Prove the member was actually cut off (not retried past the window):
        // its script's post-sleep side effect must never have run.
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        assert!(
            !marker.exists(),
            "the straggler-killed member must not have run past the window (no retry)"
        );
    }

    /// Pool-run compatibility: a pool member spec whose own graph contains
    /// an ensemble must run end to end through a pool dispatch exactly like
    /// any other spec — the ensemble's join routing onward is what lets the
    /// spec (and therefore the pool) reach completion.
    #[tokio::test]
    async fn ensemble_runs_end_to_end_through_a_pool_dispatch() {
        let dir = tempdir().unwrap();
        let db = Arc::new(Database::new(&dir.path().join("test.db")).unwrap());
        let lp = crate::domain::loops::Loop {
            id: "wf-pool-ensemble".to_string(),
            name: "Loop".to_string(),
            description: None,
            workdir: dir.path().to_string_lossy().to_string(),
            status: LoopStatus::Draft,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            active_run_pool_id: None,
            on_completed: None,
        };
        db.insert_loop(&lp).unwrap();
        let engine = LoopEngine::new(Arc::clone(&db), Arc::new(DefaultNotificationService));

        let spec = standalone_spec("pool-ensemble-spec", 1);
        db.insert_loop_spec(&spec).unwrap();
        insert_pool_with_members(&db, "pool-1", &[&spec.id]);

        let fake_home = setup_multi_cli_home(&[
            (
                "member-ok-a",
                &write_member_script(dir.path(), "a.sh", "exit 0"),
            ),
            (
                "member-ok-b",
                &write_member_script(dir.path(), "b.sh", "exit 0"),
            ),
        ]);
        db.insert_loop_node(&touch_marker_node(
            "on-pass",
            &spec.id,
            &dir.path().join("pass.marker"),
            100,
        ))
        .unwrap();
        insert_test_ensemble(
            &db,
            &spec.id,
            "kickoff",
            "ens1",
            "join1",
            &[("m-a", "member-ok-a"), ("m-b", "member-ok-b")],
            2,
            Some(1),
            "on-pass",
            None,
        );

        let _home = HomeGuard::set(fake_home.path());
        engine
            .run_loop(lp.id.clone(), Some("pool-1".to_string()), None)
            .await
            .unwrap();
        drop(_home);

        let lp = db.get_loop(&lp.id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Completed);
        assert_eq!(
            db.pool_next_pending_spec_id("pool-1").unwrap(),
            None,
            "the ensemble-bearing spec must have been fully consumed by the pool run"
        );
    }

    // ── B19: infra crash retry logic ──────────────────────────────────────

    /// Infra crash classification correctly identifies a non-self-reported
    /// agent-node failure within the crash threshold as needing retry.
    #[test]
    fn infra_crash_classification_correct() {
        let now = chrono::Utc::now();
        let run = LoopNodeRun {
            id: "run1".to_string(),
            loop_id: "loop1".to_string(),
            spec_id: "spec1".to_string(),
            node_id: "node1".to_string(),
            status: LoopRunStatus::Running,
            input: None,
            output: None,
            started_at: now,
            completed_at: None,
            iteration: 1,
            pid: None,
            boot_id: None,
            session_id: None,
        };

        let agent_node = LoopNode {
            id: "node1".to_string(),
            spec_id: Some("spec1".to_string()),
            loop_id: None,
            name: "test-agent".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({}),
            position: 1,
            created_at: chrono::Utc::now(),
        };

        let execution = NodeExecution {
            status: LoopRunStatus::Fail,
            output: serde_json::json!({}),
            summary: "crashed".to_string(),
        };

        // Scenario 1: agent node, failed, not self-reported (status=Running),
        // within threshold → should be classified as infra crash
        let self_reported = run.status != LoopRunStatus::Running;
        let duration_secs = (chrono::Utc::now() - run.started_at).num_seconds();
        let is_crash = !self_reported
            && agent_node.kind == LoopNodeKind::Agent
            && execution.status == LoopRunStatus::Fail
            && duration_secs < 60;
        assert!(is_crash, "should classify as infra crash");

        // Scenario 2: check node, same conditions → should NOT be classified
        let check_node = LoopNode {
            id: "node2".to_string(),
            spec_id: Some("spec1".to_string()),
            loop_id: None,
            name: "test-check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({}),
            position: 1,
            created_at: chrono::Utc::now(),
        };
        let is_crash_check = !self_reported
            && check_node.kind == LoopNodeKind::Agent
            && execution.status == LoopRunStatus::Fail
            && duration_secs < 60;
        assert!(
            !is_crash_check,
            "check node should not be classified as crash"
        );

        // Scenario 3: agent node, self-reported fail → should NOT be classified
        let run_self_reported = LoopNodeRun {
            status: LoopRunStatus::Fail,
            ..run.clone()
        };
        let self_reported_bool = run_self_reported.status != LoopRunStatus::Running;
        let is_crash_reported = !self_reported_bool
            && agent_node.kind == LoopNodeKind::Agent
            && execution.status == LoopRunStatus::Fail
            && duration_secs < 60;
        assert!(
            !is_crash_reported,
            "self-reported fail should not be classified as crash"
        );

        // Scenario 4: agent node, failed, slow (> 60s) → should NOT be classified
        let old_run = LoopNodeRun {
            started_at: now - chrono::Duration::seconds(90),
            ..run
        };
        let slow_duration = (chrono::Utc::now() - old_run.started_at).num_seconds();
        let is_crash_slow = !self_reported
            && agent_node.kind == LoopNodeKind::Agent
            && execution.status == LoopRunStatus::Fail
            && slow_duration < 60;
        assert!(
            !is_crash_slow,
            "slow fail should not be classified as crash"
        );
    }

    /// Merging attempt marker into output JSON correctly adds tracking fields.
    #[test]
    fn merge_attempt_marker_adds_fields() {
        let output = serde_json::json!({
            "kind": "agent",
            "exit_code": 1,
        });

        let merged = merge_attempt_marker(&output, 0, true);

        assert_eq!(
            merged.get("infra_attempt").and_then(|v| v.as_u64()),
            Some(0)
        );
        assert_eq!(
            merged.get("infra_crash").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(merged.get("kind").and_then(|v| v.as_str()), Some("agent"));
        assert_eq!(merged.get("exit_code").and_then(|v| v.as_i64()), Some(1));
    }

    /// Read infra config returns defaults when not specified.
    #[test]
    fn read_infra_config_applies_defaults() {
        let node = LoopNode {
            id: "n1".to_string(),
            spec_id: None,
            loop_id: None,
            name: "test".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({}),
            position: 1,
            created_at: chrono::Utc::now(),
        };

        let (retry_limit, crash_max, backoff) = read_infra_config(&node);
        assert_eq!(retry_limit, DEFAULT_INFRA_RETRY_LIMIT);
        assert_eq!(crash_max, DEFAULT_INFRA_CRASH_MAX_SECONDS);
        assert_eq!(backoff, DEFAULT_INFRA_BACKOFF_SECONDS);
    }

    /// Read infra config respects overrides in node config.
    #[test]
    fn read_infra_config_respects_overrides() {
        let node = LoopNode {
            id: "n1".to_string(),
            spec_id: None,
            loop_id: None,
            name: "test".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({
                "infra_retry_limit": 5,
                "infra_crash_max_seconds": 120,
                "infra_backoff_seconds": 0,
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        };

        let (retry_limit, crash_max, backoff) = read_infra_config(&node);
        assert_eq!(retry_limit, 5);
        assert_eq!(crash_max, 120);
        assert_eq!(backoff, 0);
    }

    /// B19: check node nonzero exit is NOT retried as infra crash.
    #[tokio::test]
    async fn infra_crash_check_node_nonzero_exit_not_retried() {
        let (_dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();

        db.insert_loop_node(&LoopNode {
            id: "node-check".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "exit 1",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        db.insert_loop_edge(&LoopEdge {
            id: "edge-self".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            from_node: "node-check".to_string(),
            to_node: "node-check".to_string(),
            condition: LoopEdgeCondition::Fail,
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        let runs = db.list_loop_runs_for_spec(&spec_id).unwrap();

        assert_eq!(spec.status, LoopSpecStatus::Failed);
        assert_eq!(
            runs.len(),
            DEFAULT_MAX_ITERATIONS_PER_NODE,
            "check node should retry through edge, not infra retry"
        );
    }

    /// B19: infra crash retry exhausted routes to fail edge.
    #[tokio::test]
    async fn infra_crash_retry_exhausted_routes_to_fail_edge() {
        let fake_home = setup_test_cli_home();
        let (_dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();

        db.insert_loop_node(&LoopNode {
            id: "node-implement".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "implement".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({
                // test-cli = /bin/sh -c <prompt>: always crashes fast with
                // no self-report — the infra-crash signature.
                "platform": "test-cli",
                "prompt_template": "exit 1",
                "infra_retry_limit": 1,
                "infra_crash_max_seconds": 60,
                "infra_backoff_seconds": 0,
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        db.insert_loop_node(&LoopNode {
            id: "node-fix".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "fix".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf FIXED",
                "success_condition": "exit_code_0"
            }),
            position: 2,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        db.insert_loop_edge(&LoopEdge {
            id: "edge-fail".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            from_node: "node-implement".to_string(),
            to_node: "node-fix".to_string(),
            condition: LoopEdgeCondition::Fail,
        })
        .unwrap();

        let _home = HomeGuard::set(fake_home.path());
        let result = engine.run_loop(loop_id.clone(), None, None).await;
        drop(_home);
        result.unwrap();

        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        let runs = db.list_loop_runs_for_spec(&spec_id).unwrap();

        assert_eq!(spec.status, LoopSpecStatus::Completed);

        let implement_runs: Vec<_> = runs
            .iter()
            .filter(|r| r.node_id == "node-implement")
            .collect();
        let fix_runs: Vec<_> = runs.iter().filter(|r| r.node_id == "node-fix").collect();

        assert_eq!(
            implement_runs.len(),
            2,
            "implement should run twice: initial attempt + 1 infra retry"
        );
        assert!(
            implement_runs.iter().any(|r| {
                r.output
                    .as_ref()
                    .and_then(|o| o.get("infra_crash"))
                    .and_then(|v| v.as_bool())
                    == Some(true)
            }),
            "one implement attempt should carry the infra_crash marker"
        );
        assert_eq!(fix_runs.len(), 1, "fix should run once");
        assert_eq!(fix_runs[0].status, LoopRunStatus::Pass);
    }

    /// B19: an agent that crashes once (fast, no self-report) and succeeds on
    /// the in-place retry completes the spec without traversing any edge, and
    /// the run history shows both attempts with infra markers.
    #[tokio::test]
    async fn infra_crash_then_success_retries_same_node_in_place() {
        let (dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();

        // A fake CLI binary that ignores the rendered prompt entirely: it
        // fails fast on the first invocation and succeeds on the second
        // (marker file tracks invocations). Registered as its own platform
        // in a fixture canopy config, since node prompts are wrapped in a
        // [LOOP CONTEXT] preamble that a plain `sh -c` cannot execute.
        let marker = dir.path().join("infra-marker");
        let script = dir.path().join("flaky-cli");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nif [ -f \"{m}\" ]; then exit 0; else touch \"{m}\"; exit 1; fi\n",
                m = marker.to_string_lossy()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let fake_home = tempfile::tempdir().unwrap();
        let canopy_dir = fake_home.path().join(".canopy");
        std::fs::create_dir_all(&canopy_dir).unwrap();
        let config = crate::domain::canopy_config::CanopyConfig {
            configured_at: Some(chrono::Utc::now().to_rfc3339()),
            clis: vec![crate::domain::cli_config::CliConfig {
                name: "flaky-cli".to_string(),
                binary: script.to_string_lossy().to_string(),
                headless_mode: "-c".to_string(),
                model_flag: None,
                supports_working_dir: false,
                working_dir_flag: None,
                env_vars: std::collections::HashMap::new(),
                interactive_args: None,
                fallback_interactive_args: None,
                resume_args: None,
                session_list_cmd: None,
                session_resume_cmd: None,
                accent_color: None,
                yolo_flag: None,
                prompt_via_stdin: false,
                ..Default::default()
            }],
            ..Default::default()
        };
        config.save(&canopy_dir).unwrap();

        db.insert_loop_node(&LoopNode {
            id: "node-flaky".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "flaky".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({
                "platform": "flaky-cli",
                "prompt_template": "ignored",
                "infra_retry_limit": 2,
                "infra_crash_max_seconds": 60,
                "infra_backoff_seconds": 0,
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        let _home = HomeGuard::set(fake_home.path());
        let result = engine.run_loop(loop_id.clone(), None, None).await;
        drop(_home);
        result.unwrap();

        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        assert_eq!(
            spec.status,
            LoopSpecStatus::Completed,
            "spec should complete after the in-place retry succeeds"
        );

        let runs = db.list_loop_runs_for_spec(&spec_id).unwrap();
        let flaky_runs: Vec<_> = runs.iter().filter(|r| r.node_id == "node-flaky").collect();
        assert_eq!(
            flaky_runs.len(),
            2,
            "crash + successful retry should leave two run rows"
        );

        let crash_run = flaky_runs
            .iter()
            .find(|r| r.status == LoopRunStatus::Fail)
            .expect("one run should be the classified crash");
        let crash_output = crash_run.output.as_ref().unwrap();
        assert_eq!(
            crash_output.get("infra_crash").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            crash_output.get("infra_attempt").and_then(|v| v.as_u64()),
            Some(0)
        );
        assert!(
            flaky_runs.iter().any(|r| r.status == LoopRunStatus::Pass),
            "the retry should pass"
        );
    }
}
