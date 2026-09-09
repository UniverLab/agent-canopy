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
- **Nodes** — five kinds:
  - `agent` — invokes a CLI tool with a prompt template.
  - `check` — executes a shell command.
  - `gate` — validates previous output (e.g. `output_contains`).
  - `router` — classifies and routes by token matching with fallback.
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
git HEAD commit at the start of the spec run — and
`{{spec_committed_head}}` — the git HEAD left behind by the last
`commit_rights: true` node's own execution during *this* spec run,
empty until one actually commits (see [Commit rights](#commit-rights)).

`{{spec_start_head}}` only proves *some* commit landed since the spec
began; it's satisfied just as well by a commit from outside this run
sharing the same worktree (another agent, a human, a cherry-pick) as
by this spec's own work. A check node that exists to gate "did this
spec's work actually get committed" should use
`{{spec_committed_head}}` instead:

```bash
test -z "$(git status --porcelain -- src/)" \
  && test -n "{{spec_committed_head}}" \
  && test "$(git rev-parse HEAD)" = "{{spec_committed_head}}"
```

This requires a node in the graph to declare `commit_rights: true` —
without one, nothing ever populates `{{spec_committed_head}}` and the
check above always fails. `{{spec_start_head}}` remains useful on its
own for anything that only needs "did the tree change at all", where
a concurrent commit from outside this run isn't a concern.

## Ensembles

An **ensemble** is a group of 2-8 `agent` nodes that receive the same
prompt in parallel, plus a quorum that waits for every member
before routing onward. It replaces what would otherwise be N member
nodes, N prompts, and 2N+2 edges wired by hand — `loop_add_ensemble`
creates the whole unit in one call:

```json
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

Members differ by `platform`/`model`, and each may set its own
`prompt_override` to review the same input from a different angle instead
of sharing the template. `loop_update_ensemble` changes the shared prompt
(propagated to every member without its own override), the member list,
quorum config (`min_pass`, `straggler_timeout_minutes`), exit wiring
(`on_pass_to`/`on_fail_to` take a node id or another ensemble's id, chaining
quorums with no intermediate node), and entry wiring (`from_node` replaces
every entry; `add_entry_from`/`remove_entry_from` add or detach one source so
several nodes can enter with no relay), all without touching member nodes
directly. `loop_delete_ensemble` removes the whole unit. `loop_get` returns the
ensemble as one unit (`ensemble_id`, members, quorum config, every entry
source) alongside its expanded nodes.

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

The same `commit_rights: true` node is also what populates
`{{spec_committed_head}}`: whenever its own execution moves HEAD, that
new HEAD is recorded against this spec run, overwritten each time it
commits again (e.g. a review/retry cycle). A downstream check node
uses it to verify *this run's own committer* landed a commit, not
merely that HEAD differs from wherever the spec started — see
[Template variables](#template-variables).

**Editing the recommended check command text here does not retroactively
touch any graph.** A node's `command` is whatever text was written into
its config when the graph was authored (`loop_add_node`, or copied from
a blueprint) — the engine reads it fresh at execution time but never
rewrites it. A spec already using the old `{{spec_start_head}}`-only
comparison keeps using it until someone explicitly runs
`loop_update_node` on it; only graphs authored or edited after adopting
`{{spec_committed_head}}` get the stronger check.

## Self-report requirement

A harness can exit 0 having done nothing: every tool call refused,
a quota exhausted, a provider outage — the process still ends cleanly
and the loop engine sees a clean exit code. By default that is
recorded as `Pass` if the process also produced output, exactly as it
always has. Set `require_report: true` in an agent node's config to
close that gap for a node whose graph depends on being able to tell:

```
require_report: true
```

With the flag set, an agent run that exits 0 but never calls
`loop_complete_node` itself is recorded as a deterministic **fail**
(`failure_kind: "no_report"`) and routes down the node's `fail` edge —
it is never retried as an infra crash, since nothing actually crashed.
An explicit self-report always wins regardless of this flag, pass or
fail; `require_report` only judges the case where none was ever made.

Whether or not the flag is set, every agent run that finishes without
calling `loop_complete_node` carries `"unreported": true` in its output
— this is unconditional, so a resilience node downstream can always
tell "the harness ran and chose not to report" apart from "the harness
never ran," without needing `require_report` itself. Ensemble members
are judged individually, exactly like a lone node.

## Lifecycle

Create → run → (pause / continue) → complete. `loop_continue`
supports **retry** and **skip** strategies for stuck nodes, and
`loop_report_blocker` escalates to a human when intervention is
needed. Iteration limits prevent infinite retry loops. `loop_reset`
returns a completed/failed loop to pending so `loop_run` can restart
it, and `loop_schedule_autorun` sets a future time at which the loop
auto-resumes (useful for quota-limited loops that fail and need to
wait before retrying). Call it again with `at` omitted to cancel a
pending schedule.

## Concurrency

Running many loops at once, against different working directories, is
a supported capability — not an accident of the implementation. Start,
run, pause, resume, or finish one loop, and no other loop's status,
specs, node runs, or worktree are affected. There is no cap on how
many loops can run concurrently, and nothing needs to be configured to
enable it: it is the default behavior of the daemon.

The one boundary: **two loops must not share a workdir.** Two loops
racing to commit, check out, or edit files in the same working tree
will fight over it — the engine does nothing to make that safe, and
doing so is deliberately out of scope. Point concurrent loops at
different working directories (or at worktrees of the same repo) and
they run independently with no coordination required from you.

## Event-keyed hooks

A loop carries hooks keyed by event, over four events: `on_completed`,
`on_failed`, `on_blocked`, `on_spec_completed`. More than one hook may be
registered for a single event, via `loop_update`'s `hooks` map (each key is
an event name, each value is an ordered array of hook configs). Hooks
registered for one event run in declaration order. There are three hook
modes — exactly one per hook:

- **Agent** (`platform` + `prompt`, optional `model`/`effort`/
  `timeout_minutes`): launches a CLI process with the rendered prompt.
- **Command** (`command`): runs a shell command directly with the same
  `{{...}}` placeholders substituted before execution.
- **Interactive** (`prompt` + `target_session_id`): enqueues a message into
  a live interactive session, exactly as if sent from the promptbuilder,
  marked as hook-originated. Fire-and-forget: the hook never reads or waits
  for a reply.

- `on_completed` fires exactly once when a run transitions to `completed`
  (only when the dispatch completed at least one spec). A completed →
  `loop_reset` → completed cycle fires it again, once per completing run.
- `on_failed` fires when the loop reaches `failed`.
- `on_blocked` fires when it stops with a blocker set (transition to
  `paused` with blocker data).
- `on_spec_completed` fires once per spec reaching `completed`, whether the
  specs come from the loop's own bound specs or from a queue.

**Hooks are not retroactive** — a hook registered after its event has
already happened does not fire. A hook that fails is recorded and never
changes the loop's own status, and never stops the remaining hooks of that
event from running.

Every hook run records the event it served and which hook of that event it
was (`event`, `hook_index`), and is readable from `loop_get`'s
`completion_hook_runs` and `canopy loop info`'s hook-runs listing alongside
the graph's node runs.

Each event exposes what its consumer needs, in addition to `{{loop_name}}`
and `{{workdir}}`: `on_completed` keeps `{{completed_specs}}` (name +
one-line summary of each spec this run completed, one per line, `(none)` if
the run completed zero specs); `on_spec_completed` gets `{{spec_name}}` and
`{{spec_id}}`; `on_failed` and `on_blocked` get `{{blocker}}` and `{{node}}`
(the name of the node that ended the run). A prompt carrying a marker its
event cannot bind is refused and recorded as a failed hook run.

The existing configuration keeps working, unchanged and unattended:
whatever a loop has in its `on_completed` column becomes a hook on the
`on_completed` event with no user action and no loss, and a `loop_update`
call passing today's `on_completed` shape still registers that hook.

The first intended use is a documentation-maintenance agent: on
completion, review the specs this run closed, the resulting code, and
`docs/`/`README`, then update the docs to match what actually shipped.

An interactive hook targets the exact session id in its own config, and
supports the same event placeholders as the other modes, rendered before
enqueueing. The message carries its provenance — loop id and event — as
structure alongside the prompt, so the recipient can tell it came from a
hook without that origin being buried in the prompt text. A session id is
not stable over time: a hook configured with one will eventually point at
a session that no longer exists, and firing then fails loudly naming the
id rather than redirecting the message anywhere else.

A message enqueued while no TUI is running is queued, not lost: it stays
pending and is delivered when a TUI later starts.

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
| `spec_section_get` | Extract one canonical section from a spec body |
| `spec_convert` | Migrate legacy heading-format spec bodies to the tagged `<spec>` format |

Spec bodies are written in the tagged `<spec>` format — all seven canonical
section tags (`<objective>`, `<functional_requirements>`,
`<non_functional_requirements>`, `<constraints>`, `<guidelines>`, `<in_scope>`,
`<out_of_scope>`) with markdown inside each. A body that does not parse is
rejected on write with the offending tag named. Existing heading-format specs
stay readable during the transition and are reported as needing conversion;
`spec_convert` migrates them one row at a time and never rewrites a running
spec.

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

Pass a queue to `loop_run` via `queue_id`.

Queue membership is unaffected by `loop_run` — specs stay standalone.
The `loop info` CLI and `loop_get` MCP tool show queue-driven progress
by reconstructing what ran from the run history.

## The 32 MCP tools

| Stage | Tools |
|---|---|
| Authoring | `loop_create`, `loop_update`, `loop_add_spec`, `loop_update_spec`, `loop_add_node`, `loop_update_node`, `loop_add_edge`, `loop_update_edge`, `loop_delete_edge`, `loop_delete_node`, `loop_add_ensemble`, `loop_update_ensemble`, `loop_delete_ensemble`, `loop_copy_node`, `loop_copy_ensemble`, `loop_audit_node_configs` |
| Sharing | `loop_export`, `loop_import`, `loop_archive`, `loop_restore` |
| Inspection | `loop_get`, `loop_list`, `loop_node_runs_list`, `loop_node_run_get` |
| Runtime | `loop_run`, `loop_reset`, `loop_schedule_autorun`, `loop_schedule_continue`, `loop_pause`, `loop_continue`, `loop_complete_node`, `loop_report_blocker`, `loop_preflight` |

Loops can be authored programmatically by agents through these tools,
or edited in the [TUI loop editor](tui.md) with inline JSON config
validation. The `canopy loop` CLI subcommands mirror the runtime tools
from the terminal: `list`/`info`/`export` are read-only inspection, and
`import`/`run`/`pause`/`continue`/`reset`/`autorun` delegate the
matching MCP tool to the daemon — a second way to drive a loop when an
MCP client can't reach it. See the
[CLI reference](cli-reference.md#loop-control).

## Export and import

A loop's design — its name, description, nodes, edges, and ensembles —
can leave one machine as a single JSON file and be recreated on
another. Sharing a loop becomes sending a file, not narrating the
`loop_add_node`/`loop_add_edge`/`loop_add_ensemble` calls that built
it, and the file is also a diff: review a change to a loop, or keep
one in a repo next to the code it operates on.

```
canopy loop export <loop_id> [--output <path>] [--with-models]
canopy loop import <path> [--workdir <dir>] [--name <name>]
```

`export` writes to `--output`, or to stdout (so it can be piped) when
omitted. `import` always creates a **new** loop — it never updates,
merges, or overwrites an existing one; `--workdir` defaults to the
current directory, and `--name` overrides the file's own name. If the
resolved name is already taken in the target workdir, import still
succeeds under a numeric suffix (`"My Loop (2)"`) and reports which
name it used. The same two operations exist as MCP tools,
`loop_export { loop_id, with_models? }` and
`loop_import { document, workdir?, name? }`, so a loop is drivable end
to end through MCP as well as the CLI.

What the file **excludes** is deliberate: no ids (edges reference
nodes by `name`, which is what makes the file reviewable and
hand-editable — node names must therefore be unique within an exported
loop, or export refuses with the names it found), no `workdir`, no
specs, and no run/status state. A loop file is a shape and a set of
instructions, not somebody else's backlog or history.

`platform`/`model` are stripped from every agent node and ensemble
member by default, for the same reason a [node blueprint](#node-blueprints)
never carries one: a shared design pinned to a harness or model the
recipient doesn't have is broken on arrival, and one pinned to a model
they do have is worse, since it silently spends their quota on someone
else's choice. Pass `--with-models` (`with_models: true` over MCP) only
when exporting your own loop to restore later on your own machine.
`import`'s response always lists every agent node left without a
platform, so there's exactly one thing to check before running an
imported loop: `nodes_missing_platform` in the MCP response, or the
same list printed by the CLI.

An [ensemble](#ensembles) round-trips as one ensemble — not as its
expanded member/quorum nodes — via its own `ensembles` array entry.

Here is a complete, hand-writable example: an implementer, a 2-model
ensemble of reviewers, and a committer the quorum routes to on pass.

```json
{
  "format_version": 1,
  "name": "implement-and-review",
  "description": "Implement a spec, get two model opinions, then commit.",
  "nodes": [
    {
      "name": "implementer",
      "kind": "agent",
      "position": 1,
      "config": {
        "prompt_template": "Implement: {{spec_content}}",
        "timeout_minutes": 30
      }
    },
    {
      "name": "committer",
      "kind": "agent",
      "position": 4,
      "config": {
        "prompt_template": "Review the feedback and commit if satisfied.",
        "commit_rights": true,
        "timeout_minutes": 15
      }
    }
  ],
  "edges": [],
  "ensembles": [
    {
      "name": "reviewers",
      "prompt_template": "Review this diff for correctness: {{previous_feedback}}",
      "entry_from_node": "implementer",
      "entry_condition": "always",
      "on_pass_to": "committer",
      "min_pass": 2,
      "timeout_minutes": 20,
      "members": [
        {},
        {}
      ]
    }
  ]
}
```

Note what is absent: no `id` anywhere, no `platform`/`model` on
`implementer`/`committer`/the ensemble's members (this file was
exported without `--with-models` — `import` will report all three as
`nodes_missing_platform`), and the ensemble's own member/quorum nodes
never appear in `nodes` — only its `entry_from_node`/`on_pass_to`
(both plain node names) and its `members` array do.

## Archive and restore

Loops can be archived instead of deleted — they leave the main
`loop_list`/sidebar view but their row, specs, and full run history
are untouched and can be restored at any time:

```
canopy loop archive <loop_id>
canopy loop restore <loop_id>
```

Over MCP: `loop_archive { loop_id }` and `loop_restore { loop_id }`.
Archiving refuses a `running` loop — pause it first. The archived loop
is still reachable directly by id via `loop_get` regardless of
archived state. Restoring is a plain flag flip — no data is lost or
moved.

## Node run history

When a loop fails, the node run history lets you diagnose exactly what
happened:

```
canopy loop runs <loop_id> [--node <node_id>] [--limit <n>]
```

Over MCP: `loop_node_runs_list { loop_id, node_id?, spec_id?, limit? }`
returns the most recent runs first (default 20, capped at 200).
`loop_node_run_get { run_id }` fetches one run's full stored input and
output, including `infra_attempt`/`infra_crash` markers when present.
Secret-shaped substrings are redacted before the output crosses the
boundary.

This is the second step of failure diagnosis: list to find the
offending run, then fetch its output here — the exact stderr/stdout/
reported_output the engine recorded.

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
