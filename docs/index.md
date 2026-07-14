---
title: Canopy
description: Self-contained MCP server and TUI for orchestrating AI agent sessions, background tasks, and file event triggers.
order: 1
---

# Canopy

Canopy (`harness-canopy`) is a modern, self-contained **MCP (Model Context
Protocol) server and terminal UI** for orchestrating AI agent sessions,
background tasks and file event triggers. One Rust binary, zero runtime
dependencies.

It turns a single machine into a small agent operations center:

- **Schedule** agents on cron expressions or file-system events.
- **Run** interactive agents in real PTYs with full terminal emulation.
- **Coordinate** multiple agents working in the same workspace.
- **Remember** — a project-scoped knowledge graph that persists facts,
  patterns and session summaries across sessions.
- **Search** your own documents with a local-first RAG pipeline.
- **Automate** multi-step processes with a DAG loop engine.

Everything persists in an embedded SQLite database under `~/.canopy/` —
no external services, no cloud account.

## The pieces

| Piece | What it does |
|---|---|
| **Daemon** | MCP server (Streamable HTTP + stdio), scheduler, watcher engine, database |
| **Canopy Hub (TUI)** | Full-screen terminal UI for agents, loops and system metrics |
| **61 MCP tools** | Agent management, sync, intelligence, seeds, loops, specs, pools, blueprints, RAG, projects |
| **Seed identities** | Persistent, evolvable agent personalities stored as TOML |

## How the documentation is organized

- [Installation](installation.md) — install and set up the daemon.
- [Quick Start](quickstart.md) — daemon, setup wizard, first agent.
- [The TUI — Canopy Hub](tui.md) — the interactive terminal interface.
- [Agents](agents.md) — interactive, background and terminal agents; seed identities.
- [Intelligence & Sync](intelligence-and-sync.md) — knowledge graph and multi-agent coordination.
- [Loops](loops.md) — the DAG loop engine.
- [RAG Pipeline](rag.md) — personal document search.
- [MCP Tools](mcp-tools.md) — all 61 tools by category.
- [CLI Reference](cli-reference.md) — every `canopy` command.

## Part of UniverLab

Canopy is an experiment of [UniverLab](https://github.com/UniverLab),
an open computational laboratory. It follows the lab's engineering
principles: one tool one job, reproducibility first, offline-friendly
design.
