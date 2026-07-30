---
title: Recipes
description: Graphs built end to end — the actual calls, the actual prompts, and what breaks.
order: 9
---

# Recipes

[Usage Patterns](usage-patterns.md) is about shapes. This page is about
building them: the calls in order, the prompts that survived contact with
real models, and the failure each piece is there to prevent.

Every recipe assumes a queue of specs and a graph that drains it. That split
matters more than it looks: the graph is the *machine*, the specs are the
*work*, and keeping them separate is what lets you point the same graph at a
different backlog tomorrow.

---

## Before anything: the spec is the real input

A graph is only as good as what you feed it, and the most common cause of a
disappointing run is not the topology. It is a spec that left a decision to
the implementer.

A spec that says *"choose an appropriate mechanism and document why"* has
delegated architecture to a model optimized for throughput. It will choose
something, it will be defensible, and it will be a decision you did not make
about a system you own. Decide first, then write the spec with the decision
already in it and the reasoning attached:

```
DECISIONS ALREADY MADE - DO NOT RE-LITIGATE
1. Back up with VACUUM INTO, not a file copy. Copying a live SQLite with a
   WAL can produce a broken copy even when the original is healthy.
2. Exactly one backup, verified immediately after it is written. N backups
   force you to choose which to restore, which is the decision you are least
   able to make well mid-incident.
```

Everything below assumes specs written that way. A section named `IN SCOPE` /
`OUT OF SCOPE` is worth more than any prompt engineering downstream of it.

---

## Recipe 1 — Drain a bug backlog unattended

**Shape:** gated implement + resilience (patterns 1 and 3).
**What it is for:** you have twenty small specs and a night.

This is the graph that built most of canopy. Four nodes, six edges.

### The graph

```json
graph_create {
  name: "bugs-then-features",
  workdir: "/path/to/repo"
}
```

### The nodes

```json
graph_add_node {
  graph_id, name: "implement", kind: "agent", position: 1,
  config: {
    platform: "claude", model: "sonnet",
    timeout_minutes: 40,
    prompt_template: "..."     // see below
  }
}

graph_add_node {
  graph_id, name: "gates", kind: "check", position: 2,
  config: {
    command: "cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo nextest run --locked"
  }
}

graph_add_node {
  graph_id, name: "review", kind: "agent", position: 3,
  config: {
    platform: "opencode", model: "some-free-model",
    commit_rights: true,
    timeout_minutes: 25,
    prompt_template: "..."
  }
}

graph_add_node {
  graph_id, name: "resilience", kind: "agent", position: 4,
  config: {
    platform: "opencode", model: "some-other-free-model",
    timeout_minutes: 8,
    prompt_template: "..."
  }
}
```

### The edges

| from | condition | to |
|---|---|---|
| implement | `pass` | gates |
| implement | `fail` | resilience |
| gates | `pass` | review |
| gates | `fail` | implement |
| review | `fail` | implement |
| resilience | `pass` | implement |

`review --pass-->` has no edge: that is how a spec completes.
`resilience --fail-->` has no edge either: the spec fails honestly, having
already scheduled the graph's own wake-up.

### The prompts

The implementer's, in the tagged-section style the engine uses:

```
# [ROLE]
You implement one spec in an existing Rust codebase. You are not the
architect: every design decision you need has already been made and is in
the spec.

# [SPEC]
{{spec_content}}

# [PREVIOUS FEEDBACK]
{{previous_feedback}}

# [HOW]
- Read before you write. Match the surrounding code's naming and idiom.
- You have NO COMMIT RIGHTS. Leave your work uncommitted; a later node
  reviews the diff and commits it.
- Run the project's gates yourself before reporting pass.

# [REPORTING]
Call graph_complete_node with status "pass" only if the spec is fully
implemented. Partial work is "fail" with a description of what is missing.
```

The reviewer's, which carries the load this whole shape exists for:

```
# [ROLE]
You review one diff against one spec, and you commit it if it holds up.

# [WHAT]
The gates already passed. That means nothing is broken. It says nothing
about whether the spec was HONORED, and that distinction is your entire
job.

# [HOW]
- Write a verdict for EVERY requirement in the spec, one line each, BEFORE
  you decide. A reviewer that must enumerate is a reviewer that must look.
- A missing required test is CHANGES_NEEDED even if the code is perfect.
- When in doubt, CHANGES_NEEDED. A bounce costs minutes; a falsely-closed
  spec costs days.
- Do not approve out of courtesy, for effort spent, or because it almost
  meets the spec.
- Stage only the files this spec touched. `git add -A` sweeps a previous
  spec's leftovers into this commit.
- Conventional Commits format. No trailers.
- Prove it: run `git log -1 --oneline` and include the output. A reviewer
  once reported a commit that did not exist.

# [SPEC]
{{spec_content}}
```

The resilience node's, kept deliberately short — it is a cheap model doing
one narrow job:

```
# [ROLE]
The previous node's CLI died. Decide whether it can be retried.

# [HOW]
1. Run `date -Iseconds` first. You do not know what time it is.
2. Read the failure output below.
3. If the CLI ran out of quota and names a reset time: compute the next
   instant strictly after now (and less than 24h away), call
   graph_schedule_autorun with it, then report FAIL.
4. If it is a transient failure you can repair: repair it, report PASS.
5. If the working tree is dirty from the death, clean it before reporting.

# [OUTPUT]
{{previous_feedback}}
```

### Running it

```json
graph_run { graph_id, queue_id }
```

The graph drains the queue's pending specs in order. Completed specs are
skipped on relaunch, so recovery after a crash is just `graph_run` again.

### What will bite you

- **`prompt`, not `prompt_template`.** The engine reads `prompt_template`,
  then `prompt_preset`, then falls back to a default. There is no
  `get("prompt")` anywhere in it. A graph configured with `prompt` runs
  silently on the fallback — eight specs here ran with no instructions at all
  before anyone noticed, because the fallback is *plausible* and the results
  look like a model ignoring you.
- **`always` where you meant `pass`.** Covered in pattern 1; it is the single
  most expensive edge mistake.
- **A resilience node on the implementer's quota.** It will be dead exactly
  when it is needed.

---

## Recipe 2 — Spend every free tier before touching a paid one

**Shape:** free-tier cascade (pattern 6). **Requires the router node.**
**What it is for:** four CLIs, four small ceilings, one backlog.

### The idea

Every implementer is the same node with a different harness. A single router
receives all their failures and decides where the work goes next, based on
*why* it failed rather than on the mere fact that it did.

### The nodes

```json
graph_add_node { graph_id, name: "impl-a", kind: "agent", position: 1,
  config: { platform: "cursor",      prompt_template: "..." } }
graph_add_node { graph_id, name: "impl-b", kind: "agent", position: 2,
  config: { platform: "antigravity", prompt_template: "..." } }
graph_add_node { graph_id, name: "impl-c", kind: "agent", position: 3,
  config: { platform: "codex",       prompt_template: "..." } }
graph_add_node { graph_id, name: "impl-d", kind: "agent", position: 4,
  config: { platform: "kiro",        prompt_template: "..." } }

graph_add_node {
  graph_id, name: "triage", kind: "router", position: 5,
  config: {
    platform: "opencode", model: "a-free-model",
    routes: [
      { label: "exhausted", description: "the CLI ran out of quota, credits or session limit" },
      { label: "transient", description: "a crash, a network error, a missing dependency" },
      { label: "broken",    description: "the CLI ran fine and the work itself is wrong" }
    ],
    fallback: "exhausted",
    prompt_template: "..."
  }
}
```

The routes are three because the decisions are three. Resist adding a fourth
until a real run produces a failure that fits none of them.

### The router's prompt

Short, because this node has exactly one decision to make:

```
# [ROLE]
An agent CLI just failed. Classify why, in one word.

# [HOW]
Read the output below. Do not fix anything, do not implement anything, do
not comment on the code.

- "exhausted" — it ran out of quota, credits, or session limit. This also
  covers a CLI that simply stopped responding and hit its timeout: some
  harnesses report a limit, others just go quiet.
- "transient" — it crashed, lost the network, or hit a missing dependency.
- "broken" — the CLI worked normally and the work itself is wrong.

# [OUTPUT]
{{previous_feedback}}
```

Notice what the prompt does *not* do: it names no vendor, matches no message,
parses no format. Every harness ends its quota differently and each one
changes wording between releases; the model generalizes where a pattern
cannot.

### The edges

| from | condition | to |
|---|---|---|
| impl-a | `fail` | triage |
| impl-b | `fail` | triage |
| impl-c | `fail` | triage |
| impl-d | `fail` | triage |
| triage | `route:transient` | impl-a |
| triage | `route:exhausted` | impl-b |
| triage | `route:broken` | *(no edge — the spec fails)* |

### The honest limitation

The exhausted route can only point at *one* next harness, so the cascade is
really "a, then b" per spec rather than a true a→b→c→d walk. Getting the full
walk needs one router per stage, or a memory of which harnesses are already
spent — which the engine does not have. Build the two-stage version, get the
value, and wait for per-harness availability before wiring all four.

---

## Recipe 3 — A review panel that covers four axes

**Shape:** specialist panel (pattern 7).
**Status:** the version below is what works *today*; the ensemble version
waits on per-member prompts.

### Today: four plain nodes in series

Until ensemble members can carry different prompts, a specialist panel is
four ordinary agent nodes chained one after another. Slower than parallel,
identical in coverage:

| from | condition | to |
|---|---|---|
| gates | `pass` | review-security |
| review-security | `pass` | review-performance |
| review-performance | `pass` | review-maintainability |
| review-maintainability | `pass` | review-patterns |
| review-patterns | `pass` | commit |
| *(any review)* | `fail` | implement |

Any specialist failing bounces the work — which is the semantics you want and
which `min_pass` cannot express.

### The specialist prompt shape

One template, one axis substituted. The last two sections are what keep a
cheap model from turning into a style bot:

```
# [ROLE]
You review one diff on ONE axis: SECURITY. Other reviewers own the other
axes. Do not review theirs.

# [WHAT]
Injection, authz, secrets in code or logs, unsafe deserialization, TOCTOU,
anything that widens what an attacker can reach.

# [HOW]
- Cite file:line for every finding.
- FAIL only for a defect on your axis that would matter in production.
- Everything else — style, taste, a nicer way to write it — goes in your
  report, never in your verdict.
- Finding nothing is a legitimate and expected outcome. Do not manufacture
  a finding to look useful.

# [SPEC]
{{spec_content}}
```

That last rule is not politeness. A model that believes an empty report
reflects badly on it will produce findings, and a panel of four such models
will bounce every change forever.

### When per-member prompts land

The same panel collapses into one ensemble that runs the four in parallel
with `min_pass` equal to the member count, cutting wall-clock to the slowest
reviewer instead of the sum.

---

## Recipe 4 — Resolve a GitHub issue while you sleep

**Shape:** event intake + adversarial pair (patterns 8 and 9).
**What it is for:** an issue labeled `auto` gets a branch and a proposed fix
by morning.

### The front half: outside events become a file

No public endpoint, no tunnel. One cron line:

```bash
#!/usr/bin/env bash
# ~/bin/canopy-issue-inbox.sh — run from cron every 10 minutes
set -euo pipefail
INBOX="$HOME/.canopy/inbox/github-issue.json"
mkdir -p "$(dirname "$INBOX")"

gh issue list --repo you/yourrepo --label auto --state open \
  --limit 1 --json number,title,body > "$INBOX.tmp"

# Only publish when there is actually an issue, and write atomically so the
# watcher never reads a half-written file.
if [ "$(jq 'length' < "$INBOX.tmp")" -gt 0 ]; then
  mv "$INBOX.tmp" "$INBOX"
else
  rm -f "$INBOX.tmp"
fi
```

The atomic `mv` matters. A watcher fires on the first write event, and a
graph that starts reading a file another process is still writing will read
half an issue.

### The back half: the graph watches that file

```json
graph_create {
  name: "issue-autofix",
  workdir: "/path/to/repo",
  trigger: { type: "watch", path: "~/.canopy/inbox/github-issue.json", events: ["modify"] }
}
```

The intake node turns the file into work and stops:

```
# [ROLE]
Turn one GitHub issue into one spec. You do not fix anything.

# [HOW]
1. Read ~/.canopy/inbox/github-issue.json.
2. Reproduce the problem well enough to state it precisely. An issue is a
   report, not a specification.
3. Write the spec with spec_create: objective, functional requirements,
   in scope, out of scope, acceptance. Where the issue is ambiguous, choose
   the narrowest reading and say so in OUT OF SCOPE.
4. Append it with queue_add_spec.
5. Report pass with the spec id.
```

From there it is Recipe 1, plus an adversary before the commit:

```
# [ROLE]
Your job is to BREAK the change in the diff below. You succeed by finding a
concrete way it fails.

# [HOW]
- Produce a reproduction: an input, a sequence, a race, a boundary. Not an
  opinion, not a "this could be risky".
- Report FAIL if you broke it — include the reproduction.
- Report PASS if you could not. That is a real result and an expected one.
- Do not fix anything. Breaking and repairing are different jobs.
```

Note that the adversary's `fail` means it *succeeded*. Wire
`adversary --fail--> implement` and `adversary --pass--> commit`.

### What to keep an eye on

- **The graph never learns which file woke it.** A watch-fired graph is told
  it fired, not by what, so the intake node reads a fixed path. Watching a
  directory with several inbound files does not work today.
- **The script must be idempotent.** The same issue rewritten produces the
  same spec twice unless the intake node checks for an existing spec, or the
  script only writes on change.
- **Nothing here pushes.** The graph commits on a branch; opening the PR
  stays a human decision, and that is the correct default for work done
  unattended.

---

## Cross-cutting: things that bite in every recipe

**A cycle with no entry node.** The natural retry shape — `implement -->
review` plus `review --fail--> implement` — is a cycle where every node has
an incoming edge. The engine resolves a spec's start node as the one with no
incoming edge, and falls back to the lowest `position`. Set your intended
start to `position: 1` and treat position as load-bearing.

**Duplicate outgoing edges.** Two edges leaving one node, both matching, and
pointing at different targets aborts the run — *after* the node's work is
done. Dedupe by `(from_node, condition)` before you add them.

**The CLI resolves from the daemon's environment, not your shell.** A binary
on your `$PATH` but not the daemon's fails to spawn mid-run. Use an absolute
`binary` in `~/.canopy/config.toml`.

**Small specs are a quota strategy.** A big spec means a long run means a
burnt ceiling means a failed spec. Splitting work into specs that finish in
under half an hour is not tidiness; it is the difference between losing one
spec and losing a night.

**Commit rights are configuration, not instruction.** Three different models
have committed against a capitalised "you have no commit rights" rule, each
time handing the reviewers an empty diff. Mark the committer with
`commit_rights: true` in its node config and let the engine enforce it.
