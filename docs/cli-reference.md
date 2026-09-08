---
title: CLI Reference
description: Every canopy command and flag.
order: 12
---

# CLI Reference

```
canopy [command] [options]
```

Running `canopy` with no command opens the [TUI](tui.md) (running setup
first if needed, and checking for updates).

## Daemon

| Command | Description |
|---|---|
| `canopy daemon start` | Start the daemon (MCP server, scheduler, watchers) |
| `canopy daemon stop` | Stop the daemon |
| `canopy daemon status` | Show daemon status |
| `canopy daemon restart` | Restart the daemon |
| `canopy daemon logs` | Show the daemon log |
| `canopy daemon install-service` | Register as a system service |
| `canopy daemon uninstall-service` | Remove the system service |

## Setup & diagnostics

| Command | Description |
|---|---|
| `canopy setup` | Interactive setup wizard — detects AI CLIs, generates `~/.canopy/config.toml` |
| `canopy setup --local-registry <path>` | Use a local registry directory (for registry development) |
| `canopy mcp` | MCP wizard — sync/add/remove canopy's MCP entry across platforms |
| `canopy doctor` | Full health diagnostics |

## Loop inspection

| Command | Description |
|---|---|
| `canopy loop list` | List all loops with status and spec progress |
| `canopy loop list --workdir <path>` | Filter loops by workdir |
| `canopy loop info <id-or-name>` | Detailed status for a single loop (specs, runs, hooks) |

Loop ids can be specified by exact id, exact name, or unambiguous id
prefix. Ambiguous references list the candidates.

## Loop control

Mirrors the `loop_run`/`loop_pause`/`loop_continue`/`loop_reset`/
`loop_schedule_autorun` MCP tools — a second way to reach the same daemon
operations when an MCP client can't. Every command resolves `<id-or-name>`
locally (same rule as loop inspection above) and then delegates the actual
state change to the daemon over its existing MCP endpoint; it never writes
to the database directly. If the daemon isn't reachable on the resolved
port, the error says so explicitly instead of looking like the loop is
missing.

| Command | Description |
|---|---|
| `canopy loop run <id-or-name>` | Run a loop in the background, spec by spec |
| `canopy loop run <id-or-name> --queue <queue-id>` | Run the queue's pending specs through the loop's graph instead |
| `canopy loop run <id-or-name> --workdir <path>` | Override the loop's workdir for this run only |
| `canopy loop pause <id-or-name>` | Pause a running loop after the current node finishes |
| `canopy loop continue <id-or-name> --retry-current-node` | Resume a paused loop by retrying the current node |
| `canopy loop continue <id-or-name> --skip-next-spec` | Resume a paused loop by skipping to the next spec |
| `canopy loop reset <id-or-name>` | Reset a completed/failed loop back to pending (prompts for confirmation) |
| `canopy loop reset <id-or-name> --specs <id>...` | Reset specific spec ids, even if already completed |
| `canopy loop reset <id-or-name> --yes` | Skip the confirmation prompt |
| `canopy loop autorun <id-or-name> --at <iso8601>` | Schedule a one-shot resume at a future time |
| `canopy loop autorun <id-or-name> --quota-reset-message <text>` | Schedule a resume from a raw CLI quota-limit message |
| `canopy loop autorun <id-or-name> --cancel` | Cancel a pending autorun schedule |

## Spec backlog

| Command | Description |
|---|---|
| `canopy spec create --name <name> --description <text>` | Create a standalone spec (a backlog item) |
| `canopy spec create ... --workdir <path>` | Tag the new spec with a workdir, for backlog filtering |
| `canopy spec create ... --queue <queue-id>` | Also append the new spec to an existing queue |
| `canopy spec create ... --queue <queue-id> --group <group>` | Add it to the queue within a context group |
| `canopy spec complete <spec-id> --reason <text>` | Mark a standalone spec as completed |
| `canopy spec skip <spec-id> --reason <text>` | Mark a standalone spec as skipped |
| `canopy spec reopen <spec-id> --reason <text>` | Reopen a completed/skipped spec back to pending |
| `canopy spec convert` | Migrate legacy heading-format spec bodies to the tagged `<spec>` format (skips tagged and running specs; prints a report) |

## RAG

| Command | Description |
|---|---|
| `canopy rag auto-index start` | Resume automatic indexing (default state) |
| `canopy rag auto-index stop` | Pause indexing without losing the queue |
| `canopy rag report` | Detailed per-file indexing report |

## Plumbing

| Command | Description |
|---|---|
| `canopy stdio` | Run the MCP server over stdio |
| `canopy bridge --id <session>` | Stdio sidecar proxy that injects canopy identity headers |

## Global flags

| Flag | Description |
|---|---|
| `-p, --port <port>` | Override the daemon port (default 7755) |
| `--help` | Show help for any command |
| `--version` | Show canopy version |
