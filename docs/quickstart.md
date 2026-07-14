---
title: Quick Start
description: Start the daemon, run the setup wizard, and launch your first agent.
order: 3
---

# Quick Start

## 1. Start the daemon

```bash
canopy daemon start
```

The daemon owns the MCP server (Streamable HTTP on port 7755 + stdio),
the cron scheduler, the file watcher engine and the database.

## 2. Configure (first time)

```bash
canopy setup
```

Choose your AI CLI platforms (Claude Code, and others from the registry),
model flags, temperature units and RAG settings.

## 3. Launch the TUI

```bash
canopy
```

Opens the full-screen **Canopy Hub**. Press `Ctrl+N` (or `n`) to create a
new agent:

- **Interactive** — a live PTY session with your chosen AI CLI.
- **Background** — cron-scheduled or file-watcher-triggered.
- **Terminal** — a raw shell session with per-session history.

Optionally select a **seed identity** to give the agent a persistent
personality.

## 4. Wire up your AI CLI via MCP

```bash
canopy mcp
```

The MCP wizard syncs, adds and removes canopy's MCP server entry across
all detected platforms, converting formats automatically (JSON ↔ TOML).
After that, your AI agents can call all
[61 MCP tools](mcp-tools.md) — schedule background agents, store
knowledge, coordinate with each other.

## 5. Verify

```bash
canopy doctor          # full health diagnostics
canopy daemon status   # is the daemon running?
canopy daemon logs     # tail the daemon log
```

## Where to go next

- [Agents](agents.md) — the three agent kinds and seed identities.
- [Intelligence & Sync](intelligence-and-sync.md) — persistent knowledge.
- [Loops](loops.md) — multi-step automation.
- [RAG Pipeline](rag.md) — index and search your documents.
