---
title: Loops
description: The DAG loop engine — specs, nodes, edges, gates and lifecycle.
order: 7
---

# Loops

The loop engine runs multi-step processes as a DAG, combining agent
invocations, shell checks and validation gates.

## Structure

```
Loop
└── Spec (ordered)
    └── Node ──Edge(pass/fail/always)──▶ Node
```

- **Specs** — ordered units of work inside a loop.
- **Nodes** — four kinds:
  - `agent` — invokes a CLI tool with a prompt template.
  - `check` — executes a shell command.
  - `gate` — validates previous output (e.g. `output_contains`).
  - `quorum` — engine-managed node that closes an
    [ensemble](#ensembles), never created directly.
- **Edges** — connect nodes with routing conditions: `pass`, `fail`,
  `always`.

## Template variables

Agent node prompts support these placeholders:

| Variable | Expands to |
|---|---|
| `{{loop_name}}` | The loop's name |
| `{{workdir}}` | The loop's working directory |
| `{{spec_id}}` | The spec's unique ID |
| `{{spec_name}}` | The spec's name |
| `{{spec_content}}` | The spec's description (falls back to name) |
| `{{node_id}}` | The node's unique ID |
| `{{previous_feedback}}` | JSON output of the previous node (truncated if oversized) |

Check node commands additionally support `{{spec_start_head}}` — the
git HEAD commit at the start of the spec run, useful for verifying
that code actually changed.

## Ensembles

An **ensemble** is a group of 2-8 `agent` nodes that receive the same
prompt in parallel, plus a quorum that waits for every member
before routing onward. It replaces what would otherwise be N member
nodes, N prompts, and 2N+2 edges wired by hand — `loop_add_ensemble`
creates the whole unit in one call:

```
loop_add_ensemble {
  loop_id, name: "proposers",
  prompt_template: "...",              # one prompt, shared by every member
  members: [
    { platform: "opencode", model: "mimo-v2.5-free" },
    { platform: "opencode", model: "glm-4.6-free" },
    { platform: "opencode", model: "qwen3-coder-free" }
  ],
  from_node, condition: "always",       # entry wiring
  min_pass: 2,                          # default: all members
  on_pass_to, on_fail_to                # exit wiring
}
```

Canonical uses: (a) N cheap/free models draft a solution in parallel,
the quorum consolidates their proposals, and an arbiter (or the
implementer itself) receives all of them and implements the consensus
— expensive quota is spent once, on a pre-digested task; (b) 2-3 free
reviewer models review the same diff in parallel and the quorum
consolidates their findings into one verdict.

Members are homogeneous by design in v1 — they differ only by
`platform`/`model`, share the one prompt, and can't be edited
individually. `loop_update_ensemble` changes the shared prompt
(propagated to every member), the member list, quorum config
(`min_pass`, `straggler_timeout_minutes`), and exit wiring, all
without touching member nodes directly. `loop_get` returns the
ensemble as one unit (`ensemble_id`, members, quorum config) alongside
its expanded nodes.

The quorum fires only once every member branch has terminated
(pass, fail, or straggler timeout past
`straggler_timeout_minutes`, which kills the still-running process
and counts it as a fail) — it never fires early. Its consolidated
output is one document with a `## <platform/model> [pass|fail]`
section per member, in member order. The quorum reports pass when at
least `min_pass` members passed. Member execution is capped by a
global concurrency limit (default 4) so an 8-member ensemble queues
rather than forking every member at once.

Members are agent nodes only, and nested ensembles (an ensemble wired
into another ensemble's members or quorum) are rejected. A bounce back
into an ensemble re-runs every member and costs one iteration against
the ensemble's shared budget. The TUI loop view renders an ensemble
collapsed as one box (`name [N models]` + quorum) with live per-member
state while running, expandable on inspect.

## Commit rights

Most graphs want exactly one node to land work in git — a committer at
the end of the chain — while every earlier node leaves its changes
uncommitted so the reviewers downstream have a real diff to review.
Asking for that in the prompt does not hold: three different models
have committed anyway against an explicit, capitalised "you have no
commit rights" rule, and each time the reviewers that followed were
handed an empty diff and the quality gate quietly became a no-op.

So the engine enforces it. Mark the committer in its **node config**:

```
commit_rights: true
```

The engine records `git rev-parse HEAD` before and after every node it
runs and compares the two. A node without `commit_rights` that moved
HEAD is a deterministic **fail**, whatever the node itself reported —
it routes through the `fail` edge like any other failure, and the
reason (`Node 'X' committed but has no commit rights: HEAD moved
a1b2c3 -> d4e5f6`) lands in the run output, in `canopy loop info`, and
in the next node's `{{previous_feedback}}`.

Three things are deliberate:

- **Enforcement is opt-in per graph.** It activates only once some node
  in the graph declares `commit_rights: true`. A graph that designates
  nobody can't be told apart from one written before this key existed,
  so enforcing there would fail the very node it relies on to land
  work. Existing graphs are unchanged until you name a committer.
- **Rights are explicit configuration**, never inferred from a node's
  name, kind, or prompt. Nodes without the key have no commit rights.
- **The engine never undoes the commit.** It reports and routes; it
  will not `reset`, `revert`, or rewrite your history, because the
  unauthorized commit usually contains the *correct* work made by the
  wrong node, and an unattended daemon rewriting history is a far worse
  failure than the one it is fixing. Both hashes are in the output —
  undo it yourself if it doesn't belong.

Ensemble members are checked as a group rather than individually: they
run concurrently against one workdir, so a moved HEAD can't be
attributed to a single member, and the quorum fails as a whole.
Non-git workdirs are unaffected, as is any node that edits files
without committing — the normal case.

## Lifecycle

Create → run → (pause / continue) → complete. `loop_continue`
supports **retry** and **skip** strategies for stuck nodes, and
`loop_report_blocker` escalates to a human when intervention is
needed. Iteration limits prevent infinite retry loops. `loop_reset`
returns a completed/failed loop to pending so `loop_run` can restart
it, and `loop_schedule_autorun` sets a future time at which the loop
auto-resumes (useful for quota-limited loops that fail and need to
wait before retrying).

## `on_completed` hook

A loop can carry one optional `on_completed` hook: an agent-node-style
config (`platform`, `model`, `prompt`, `timeout_minutes`) set via
`loop_update`. The engine fires it exactly once, right when a run
transitions to `completed` — never on `failed` or `paused`, never
retroactively for a loop that completed before the hook was configured,
and never twice for the same completion. A completed → `loop_reset` →
completed cycle fires it again, once per completing run.

The hook's `prompt` supports its own small placeholder set (not the
full node template-variable list above):

- `{{loop_name}}` — the loop's name.
- `{{workdir}}` — the loop's working directory.
- `{{completed_specs}}` — name + one-line summary of each spec this run
  completed, one per line (`(none)` if the run completed zero specs).

It runs through the same spawn path as a loop agent node (detached,
process-group tracked), and its run is recorded and visible in
`loop_get` / `canopy loop info` alongside the graph's node runs — but
its pass/fail never changes the loop's final status, since the run is
already `completed` by the time it fires. A hook failure logs a
warning and sends a desktop notification if available; the
loop-completed notification itself notes when a hook was launched.

The first intended use is a documentation-maintenance agent: on
completion, review the specs this run closed, the resulting code, and
`docs/`/`README`, then update the docs to match what actually shipped.

## Standalone spec backlog

Specs don't have to belong to a loop. The spec backlog lets you create,
list, update and delete specs independently, optionally tagging each to
a workdir for filtering:

| Tool | Description |
|---|---|
| `spec_create` | Create a standalone spec |
| `spec_list` | List specs (filterable by workdir, status) |
| `spec_update` | Update a spec's name, description, or workdir tag |
| `spec_delete` | Delete an unbound spec |
| `spec_set_status` | Admin transition: complete, skip, or reopen a standalone spec |

The TUI sidebar shows backlog specs under the **Backlog** section,
filtered to the selected project's workdir.

## Spec queues

A **queue** is an ordered list of existing specs decoupled from any one
loop. When a loop runs against a queue, it drains the queue's pending
specs (in queue order) through the loop's graph instead of its own
bound specs:

| Tool | Description |
|---|---|
| `queue_create` | Create an empty queue |
| `queue_add_spec` | Append a spec to the end of a queue |
| `queue_list` | List a queue's members (or all queues) |
| `queue_remove_spec` | Remove a spec from a queue |
| `queue_reorder` | Full replacement of a queue's order |

Pass a queue to `loop_run` via `queue_id` (the deprecated `pool_id`
still works). The former `pool_*` tool names remain as deprecated
back-compat aliases of the `queue_*` tools above.

Queue membership is unaffected by `loop_run` — specs stay standalone.
The `loop info` CLI and `loop_get` MCP tool show queue-driven progress
by reconstructing what ran from the run history.

## The 24 MCP tools

| Stage | Tools |
|---|---|
| Authoring | `loop_create`, `loop_update`, `loop_add_spec`, `loop_update_spec`, `loop_add_node`, `loop_update_node`, `loop_add_edge`, `loop_update_edge`, `loop_add_ensemble`, `loop_update_ensemble` |
| Inspection | `loop_get`, `loop_list` |
| Runtime | `loop_run`, `loop_reset`, `loop_schedule_autorun`, `loop_pause`, `loop_continue`, `loop_complete_node`, `loop_report_blocker` |

Loops can be authored programmatically by agents through these tools,
or edited in the [TUI loop editor](tui.md) with inline JSON config
validation. The `canopy loop list` and `canopy loop info` CLI
subcommands provide read-only inspection from the terminal.

## Node blueprints

Rather than pasting a full `config` into every `loop_add_node` call, a
node can reference a **blueprint** — a reusable `{name, kind, config}`
template — by name:

```
loop_add_node { spec_id, name, blueprint: "cargo-gates" }
loop_add_node { spec_id, name, blueprint: "implementer-claude", config_overrides: { "model": "opus" } }
```

`config_overrides` is a shallow merge on top of the blueprint's config
template — override keys win, every other templated key is preserved.
An unknown blueprint name returns an actionable error listing every
available blueprint.

Five builtins are seeded automatically at daemon startup if missing
(re-seeded if deleted from the DB directly): `implementer-claude`,
`cargo-gates`, `reviewer-committer-mimo`, `commit-check`,
`resilience-mimo`. Builtins can't be deleted. Manage blueprints with
`blueprint_list`, `blueprint_create`, and `blueprint_delete` (custom
only). The TUI sidebar lists available blueprints, and the loop editor
validates blueprint references inline.
