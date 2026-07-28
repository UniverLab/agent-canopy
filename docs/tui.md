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

- **Agent status colors** — green for working (recent output), blue for
  idle, red for failed, gray for exited. The sidebar shows a colored
  gutter bar per agent.
- **Split groups** — side-by-side horizontal/vertical views to watch
  multiple agents simultaneously.
- **System dashboard** — CPU, memory, disk, GPU (NVIDIA/Linux/macOS) and
  temperatures with amber/red alert thresholds. Under WSL, metrics come
  from the Windows host via PowerShell.
- **Live loop view** — real-time graph rendering of the running loop's
  current spec, auto-following the active node. Manual node inspection
  shows per-node run info (status, elapsed time, output tail, iteration).
  `Esc` toggles between auto-follow and manual mode.
- **Idle visuals** — Brian's Brain cellular automaton and animated
  kaomoji status messages (whimsg).

## Gamification

Canopy tracks 28 achievement-style missions across six categories:

| Category | Missions | Examples |
|---|---|---|
| **Environment** | 4 | Firefly Catcher, Harness Master, Canopy Explorer, Multitasker |
| **Intelligence** | 6 | World Connector, Data Architect, Deep Searcher, Digital Archeologist |
| **Projects** | 3 | Project Polyglot, The Gardener, Data Hoarder |
| **Loop** | 4 | Automation Engineer, Pipeline Pilot, Parallel Vision, Loop Survivor |
| **Seeds** | 4 | First Bloom, Identity Evolved, The Orchard, Deep Roots |
| **SysInfo** | 7 | Full Throttle, Nuclear Winter, VRAM Squeezer, YOLO Pilot |

Missions are checked automatically during normal TUI operation — no
manual action required. Progress is persisted in the database and
visible in the TUI's gamification panel.

## Projects sidebar

The sidebar shows sections for the selected project:

- **Loops** — active loops with spec progress (done/total) and current
  spec indicator. Selecting a loop opens the live loop view.
- **Backlog** — standalone specs tagged to the project's workdir, not
  yet assigned to any loop.
- **History** — completed and failed loops for the project.

Sections are automatically filtered by the selected project's workdir.

## Moving context around

- **Scheduled delivery** — `Shift+Enter` (requires Kitty keyboard
  enhancement protocol support) opens a time picker to send the prompt
  at a chosen hour instead of immediately. Falls back to `Ctrl+S` on
  terminals without Kitty protocol.
- **Context transfer** — a two-step modal (preview → agent picker)
  transfers conversation context, prompts and output between agents while
  preserving session state and scrollback.
- **RAG transfer** — send semantic search results to another agent as
  injected context.
- **Prompt builder** — structured prompt templates with configurable
  sections (instruction, context, resources, examples) and @-mention
  agent references. Press `Ctrl+L` to recall the last prompt you sent
  in the current project, restoring the full prompt text, tools, and
  preset selection.
- **Launchpad** — start new sessions with previous-mission recovery and
  auto-injected context.

## Loops in the TUI

The loop editor allows inline editing of node config JSON with
validation. See [Loops](loops.md) for the engine itself.

## Notifications

Native desktop notifications for task completions, failures, watcher
triggers, and loop lifecycle events (loop started, spec completed with
progress, loop finished with outcome, blocker reported, completion hook
failure). Platform auto-detected: WSL (PowerShell toasts), macOS
(`osascript`), Linux (`notify-send`).
