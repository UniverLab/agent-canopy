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
- **Nodes** — three kinds:
  - `agent` — invokes a CLI tool with a prompt template.
  - `check` — executes a shell command.
  - `gate` — validates previous output (e.g. `output_contains`).
- **Edges** — connect nodes with routing conditions: `pass`, `fail`,
  `always`.

## Template variables

Loop prompts support `{{loop_name}}`, `{{spec_content}}`,
`{{node_id}}`, `{{previous_feedback}}` and more — so a failing check can
feed its output back into the retrying agent node.

## Lifecycle

Create → run → (pause / continue) → complete. `loop_continue`
supports **retry** and **skip** strategies for stuck nodes, and
`loop_report_blocker` escalates to a human when intervention is
needed. Iteration limits prevent infinite retry loops.

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

## The 15 MCP tools

| Stage | Tools |
|---|---|
| Authoring | `loop_create`, `loop_update`, `loop_add_spec`, `loop_update_spec`, `loop_add_node`, `loop_update_node`, `loop_add_edge`, `loop_update_edge` |
| Inspection | `loop_get`, `loop_list` |
| Runtime | `loop_run`, `loop_pause`, `loop_continue`, `loop_complete_node`, `loop_report_blocker` |

Loops can be authored programmatically by agents through these tools,
or edited in the [TUI loop editor](tui.md) with inline JSON config
validation.

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
only).
