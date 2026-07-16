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
  - `join` — engine-managed; closes an [ensemble](#ensembles), never created
    directly.
- **Edges** — connect nodes with routing conditions: `pass`, `fail`,
  `always`.

## Template variables

Loop prompts support `{{loop_name}}`, `{{spec_content}}`,
`{{node_id}}`, `{{previous_feedback}}` and more — so a failing check can
feed its output back into the retrying agent node.

## Ensembles

An **ensemble** is a group of 2-8 `agent` nodes that receive the same
prompt in parallel, plus a `join` gate that waits for every member
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
the join consolidates their proposals, and an arbiter (or the
implementer itself) receives all of them and implements the consensus
— expensive quota is spent once, on a pre-digested task; (b) 2-3 free
reviewer models review the same diff in parallel and the join
consolidates their findings into one verdict.

Members are homogeneous by design in v1 — they differ only by
`platform`/`model`, share the one prompt, and can't be edited
individually. `loop_update_ensemble` changes the shared prompt
(propagated to every member), the member list, join config
(`min_pass`, `straggler_timeout_minutes`), and exit wiring, all
without touching member nodes directly. `loop_get` returns the
ensemble as one unit (`ensemble_id`, members, join config) alongside
its expanded nodes.

The join fires only once every member branch has terminated
(pass, fail, or straggler timeout past
`straggler_timeout_minutes`, which kills the still-running process
and counts it as a fail) — it never fires early. Its consolidated
output is one document with a `## <platform/model> [pass|fail]`
section per member, in member order. The join reports pass when at
least `min_pass` members passed. Member execution is capped by a
global concurrency limit (default 4) so an 8-member ensemble queues
rather than forking every member at once.

Members are agent nodes only, and nested ensembles (an ensemble wired
into another ensemble's members or join) are rejected. A bounce back
into an ensemble re-runs every member and costs one iteration against
the ensemble's shared budget. The TUI loop view renders an ensemble
collapsed as one box (`name [N models]` + join) with live per-member
state while running, expandable on inspect.

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

## Spec pools

A **pool** is an ordered queue of existing specs decoupled from any one
loop. When a loop runs against a pool, it drains the pool's pending
specs (in queue order) through the loop's graph instead of its own
bound specs:

| Tool | Description |
|---|---|
| `pool_create` | Create an empty pool |
| `pool_add_spec` | Append a spec to the end of a pool's queue |
| `pool_list` | List a pool's members (or all pools) |
| `pool_remove_spec` | Remove a spec from a pool |
| `pool_reorder` | Full replacement of a pool's queue order |

Pool membership is unaffected by `loop_run` — specs stay standalone.
The `loop info` CLI and `loop_get` MCP tool show pool-driven progress
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
