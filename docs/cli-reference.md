---
title: CLI Reference
description: Every canopy command and flag.
order: 10
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
