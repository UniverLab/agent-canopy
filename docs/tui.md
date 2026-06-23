---
title: The TUI — Canopy Hub
description: The full-screen terminal interface — agents, split views, dashboard, transfers.
order: 4
---

# The TUI — Canopy Hub

Running `canopy` with no arguments opens the Canopy Hub, a full-screen
[ratatui](https://ratatui.rs/) interface for managing everything in real
time.

## Creating agents

`Ctrl+N` / `n` opens the **NewAgentDialog** with three modes:

1. **Interactive** — PTY session with full vt100 emulation, 24-bit color
   and support for interactive applications.
2. **Background** — cron-scheduled or file-watcher-triggered agent.
3. **Terminal** — raw shell session with per-session command history
   (TOML-backed), cross-session autocomplete search, and a Warp-like
   input mode.

The dialog includes a CLI picker, model picker, seed identity selector
(`◀ None / SeedName ▶`), directory browser, and a yolo-mode toggle.

## Monitoring

- **Split groups** — side-by-side horizontal/vertical views to watch
  multiple agents simultaneously.
- **System dashboard** — CPU, memory, disk, GPU (NVIDIA/Linux/macOS) and
  temperatures with amber/red alert thresholds. Under WSL, metrics come
  from the Windows host via PowerShell.
- **Idle visuals** — Brian's Brain cellular automaton and animated
  kaomoji status messages (whimsg).

## Moving context around

- **Context transfer** — a two-step modal (preview → agent picker)
  transfers conversation context, prompts and output between agents while
  preserving session state and scrollback.
- **RAG transfer** — send semantic search results to another agent as
  injected context.
- **Prompt builder** — structured prompt templates with configurable
  sections (instruction, context, resources, examples) and @-mention
  agent references.
- **Launchpad** — start new sessions with previous-mission recovery and
  auto-injected context.

## Loops in the TUI

The loop editor allows inline editing of node config JSON with
validation. See [Loops](loops.md) for the engine itself.

## Notifications

Native desktop notifications for task completions, failures and watcher
triggers. Platform auto-detected: WSL (PowerShell toasts), macOS
(`osascript`), Linux (`notify-send`).
