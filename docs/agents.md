---
title: Agents
description: Interactive PTY agents, background cron/watcher agents, terminal sessions and seed identities.
order: 5
---

# Agents

Canopy manages three kinds of agents.

## Interactive agents

Each interactive agent runs your chosen AI CLI in a dedicated
pseudo-terminal with full vt100 emulation, 24-bit color and cursor
positioning — anything that works in a terminal works inside canopy.

## Background agents

Background agents run unattended, triggered by:

- **Cron schedules** — an event-driven Tokio scheduler computes precise
  wake-up times and sleeps until needed; near-zero CPU when idle.
- **File events** — create/modify/delete/move events via the `notify`
  crate, with configurable debouncing and recursive directory monitoring.

They support configurable timeouts, automatic retries, execution logging
(5 MB rotation) and per-run status tracking. Prompts support template
variables: `{{TIMESTAMP}}`, `{{TASK_ID}}`, `{{LOG_PATH}}`,
`{{FILE_PATH}}`, `{{EVENT_TYPE}}`.

Background agents are managed from the TUI or by other agents through the
[agent MCP tools](mcp-tools.md) (`agent_add`, `agent_watch`,
`agent_run`, `agent_status`, …).

## Terminal sessions

Raw shell sessions with per-session command history (TOML-backed),
cross-session autocomplete search and a Warp-like input mode.

## Seed identities

A **seed** is a persistent, evolvable agent identity stored as structured
TOML at `~/.canopy/seeds/<id>/identity.toml`:

```toml
name = "Quercus"
created_at = "2026-01-01T00:00:00Z"

[directives]
general = [
    "Prioritize type-safety",
    "Explain structural changes before executing"
]

[traits]
tone = "Concise, Technical"
focus = "Refactoring, Architecture"
```

- Each seed has a unique name (case-insensitive), behavioral directives
  and personality traits; identities are capped at 4 KB and validated.
- Sessions bound to a seed receive its prompt injection automatically.
- The `evolve_identity` MCP tool lets agents refine their own identity
  over time; `create_seed`, `list_seeds` and `remove_seed` manage the
  nursery.

## Context transfer

Conversation context, prompts and output can be transferred between
agents while preserving session state and scrollback history — from the
TUI or as part of automated flows.
