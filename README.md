```                                                         
  ██████   ██████   ████████    ██████  ████████  █████ ████
 ███░░███ ░░░░░███ ░░███░░███  ███░░███░░███░░███░░███ ░███ 
░███ ░░░   ███████  ░███ ░███ ░███ ░███ ░███ ░███ ░███ ░███ 
░███  ███ ███░░███  ░███ ░███ ░███ ░███ ░███ ░███ ░███ ░███ 
░░██████ ░░████████ ████ █████░░██████  ░███████  ░░███████ 
 ░░░░░░   ░░░░░░░░ ░░░░ ░░░░░  ░░░░░░   ░███░░░    ░░░░░███ 
                                        ░███       ███ ░███ 
                                        █████     ░░██████  
                                       ░░░░░       ░░░░░░   
```

<p align="center">
  <a href="https://github.com/UniverLab/harness-canopy/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/UniverLab/harness-canopy/ci.yml?branch=main&style=for-the-badge&label=CI" alt="CI"/></a>
  <a href="https://crates.io/crates/harness-canopy"><img src="https://img.shields.io/crates/v/harness-canopy?style=for-the-badge&logo=rust&logoColor=white" alt="Crates.io"/></a>
  <img src="https://img.shields.io/badge/Status-Active-27AE60?style=for-the-badge" alt="Status"/>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-2E8B57?style=for-the-badge" alt="License"/></a>
</p>

harness-canopy is a modern, self-contained MCP (Model Context Protocol) server and TUI for orchestrating AI agent sessions, background tasks, and file event triggers. Designed for reliability, modularity, and performance — it enables advanced scheduling, persistent knowledge graphs, multi-agent coordination, loop automation, and interactive terminal management with zero runtime dependencies.

---

### Demo

![Demo](demo/dist/demo.gif)

---

## Installation

**Linux / macOS:**

```bash
curl -fsSL https://raw.githubusercontent.com/UniverLab/harness-canopy/main/scripts/install.sh | sh
```

Or via cargo: `cargo install harness-canopy` (the binary is `canopy`).
See [`docs/installation.md`](docs/installation.md) for all methods and first-time setup.

## Documentation

Full documentation lives in [`docs/`](docs/): installation, quick start, the
TUI, agents and seed identities, intelligence & sync, loops, the RAG
pipeline, all 64 MCP tools, and the complete CLI reference.

---


## Features

### 🎯 Core Platform

- **🚀 High-Performance Scheduler** — Event-driven cron scheduler using Tokio with zero polling overhead. Computes precise wake-up times and sleeps until needed; CPU usage drops to near-zero when idle.
- **📊 Real-time File Watcher** — Instantly reacts to file system events (create, modify, delete, move) using the `notify` crate with configurable debouncing, recursive directory monitoring, and macOS FSEvents compatibility.
- **💾 Persistent State** — All tasks, watchers, execution logs, agent state, sync messages, intelligence nodes, loops, and projects are stored in an embedded SQLite database with WAL mode and automatic schema migration.
- **🔄 Auto-Update** — Checks GitHub releases daily for new stable versions, downloads the platform-specific binary (linux-musl, macos-darwin; x86_64/aarch64), and atomically replaces the running executable.
- **🔔 Cross-Platform Notifications** — Native desktop notifications for task completions, failures, and watcher triggers. Auto-detects platform: WSL (PowerShell toasts with AUMID), macOS (`osascript`), Linux (`notify-send`).

### 🤖 Agent Management

- **Interactive PTY Agents** — Each agent runs in a dedicated pseudo-terminal with full vt100 emulation, 24-bit color support, cursor positioning, and interactive applications.
- **Terminal Sessions** — Raw shell sessions with per-session command history (TOML-backed), cross-session autocomplete search, and Warp-like input mode for efficient command entry.
- **Background Agents** — Cron-scheduled and file-watcher-triggered agents with configurable timeouts, automatic retries, execution logging (5 MB rotation), and per-run status tracking.
- **Seed Identity System** — Persistent, evolvable agent identities stored as structured TOML at `~/.canopy/seeds/<id>/identity.toml`. Each seed has a unique name, family, behavioral directives, and personality traits. Binded sessions receive the seed's prompt injection automatically. The `evolve_identity` MCP tool lets agents refine themselves over time. 4 KB size cap, case-insensitive name uniqueness, and mandatory field validation.
- **Seed Nursery** — Collaborative workspace for creating new seed identities. Creates a temporary directory with a draft `identity.toml` and CLI-specific instruction files (e.g. `CLAUDE.md`, `AGENTS.md`) that guide the agent to interview the user and define the seed's personality. Validates and registers the seed on completion.
- **Context Transfer** — Seamlessly transfer conversation context, prompts, and output between agents while preserving session state and scrollback history.
- **Prompt Builder** — Structured prompt templates with configurable sections (instruction, context, resources, examples), section picker, and @-mention agent references.
- **Launchpad** — Start new interactive sessions with previous mission recovery, mission input, and auto-injected context.

### 🧠 Intelligence V2 — Knowledge Graph

- **Project-Scoped Knowledge** — Store facts, patterns, and session summaries scoped to individual projects via `project_hash` (SHA-256 of canonical workdir path, truncated to 8 hex). Auto-detected from the session workdir — agents never need to set it manually.
- **Full-Text Search** — Search intelligence nodes by query and optional kind filter across all projects.
- **Graph Walk** — Traverse the knowledge graph from any node up to a configurable depth, returning connected facts, patterns, and cross-references.
- **Project Relationships** — Link projects with typed relations (`depends_on`, `complements`, `relates_to`, `independent`) and query related projects for context enrichment.
- **Context Retrieval** — `intelligence_get_context` auto-detects the project, returns a curated mix of session knowledge, project facts, and related-project summaries.

### 🔄 Multi-Agent Sync

- **Mission Declaration** — Agents declare high-level missions with impact levels (`low`/`high`/`breaking`) so peers can see what's happening.
- **Workspace Status** — Report workspace stability (`stable`/`unstable`/`testing`) to coordinate safe concurrent work.
- **Broadcast Messaging** — Info, query, and answer messages between agents in the same workdir.
- **Active Context** — `sync_get_context` returns active missions, recent chatter, and a computed workspace "vibe" (worst status among active intents).

### 🔀 Loop DAG Engine

- **Ordered Specs** — Loops contain sequenced `Spec`s, each with `Node`s connected by `Edge`s with routing conditions (`pass`/`fail`/`always`).
- **Standalone Spec Backlog** — Specs exist independently from loops; tag them to a workdir for filtering. Managed via `spec_create`, `spec_list`, `spec_update`, `spec_delete`.
- **Spec Pools** — Ordered queues of existing specs that a loop drains one by one. Append and reorder while a loop is running via `pool_create`, `pool_add_spec`, `pool_list`, `pool_remove_spec`, `pool_reorder`.
- **Node Kinds** — `agent` nodes invoke CLI tools with prompt templates; `check` nodes execute shell commands; `gate` nodes validate previous output (e.g., `output_contains`); `join` is the engine-managed gate that closes an ensemble.
- **Ensembles** — `loop_add_ensemble` creates a parallel group of 2-8 agent-node members sharing one prompt, plus a wait-all join gate that consolidates their outputs and routes onward, in a single MCP call. `loop_update_ensemble` edits the shared prompt, member list, and join/exit config as one unit. See [docs/loops.md](docs/loops.md#ensembles).
- **Node Blueprints** — Reusable `{name, kind, config}` templates referenced by name in `loop_add_node`. Five builtins seeded at startup; custom blueprints via `blueprint_create`/`blueprint_delete`/`blueprint_list`.
- **Full Lifecycle** — Create, update, run, pause, continue (retry or skip), reset, complete nodes, and report blockers for human intervention. `loop_schedule_autorun` resumes failed/completed loops at a future time.
- **`on_completed` Hook** — Post-completion agent execution (e.g. documentation maintenance) that fires once per completion.
- **Template Variables** — Loop prompts support `{{loop_name}}`, `{{workdir}}`, `{{spec_id}}`, `{{spec_name}}`, `{{spec_content}}`, `{{node_id}}`, `{{previous_feedback}}`, and `{{spec_start_head}}` (git HEAD at spec start, for check nodes).
- **24 MCP Tools** — Complete authoring, inspection, runtime, spec, pool, and blueprint management.

### 📚 Personal RAG Pipeline

- **Semantic Search** — Embed and query personal documents (Markdown, MDX, PDF) with local ONNX models (fastembed/BGE, multilingual-e5) or remote APIs (OpenAI, Gemini).
- **Language-Aware Chunking** — Markdown split by headings with paragraph fallback, similarity-aware merging, and overlap for context preservation.
- **PDF Extraction** — Isolated subprocess prevents parser crashes from taking down the daemon, with HTML detection and raw-text salvage fallback.
- **Auto-Ingestion** — Background watcher monitors configured RAG roots with 3-second debounce, enqueues changes, and reconciles orphan chunks on startup.
- **Per-Agent Rate Limiting** — 10 calls/minute sliding window per agent.
- **`.canopy/ragignore`** — Regex-based exclusion patterns for files and directories.

### 🖥️ Interactive TUI (Canopy Hub)

- **NewAgentDialog** — Three modes (Interactive, Background Cron/Watch, Terminal) with CLI picker, model picker, seed identity selector (`◀ None / SeedName ▶`), directory browser, and yolo mode toggle.
- **Scheduled Delivery** — `Shift+Enter` (Kitty keyboard protocol) sends prompts at a chosen time instead of immediately.
- **Split Groups** — Side-by-side horizontal/vertical views for monitoring multiple agents simultaneously.
- **System Dashboard** — CPU, memory, disk, GPU (NVIDIA/Linux/macOS), temperatures with amber/red alert thresholds. WSL queries Windows host metrics via PowerShell.
- **Live Loop View** — Real-time graph rendering with auto-follow on the running node, manual node inspection, and per-node run info (status, elapsed, output tail).
- **Agent Status Colors** — Green for working (recent output), blue for idle, red for failed, gray for exited.
- **Projects Sidebar** — Sections for active loops, backlog specs, and loop history, filterable by project workdir.
- **Loop Editor** — Inline node config JSON editing with validation.
- **RAG Transfer Modal** — Send semantic search results to other agents as injected context.
- **Context Transfer** — Two-step modal (preview → agent picker) to inject conversation context between sessions.
- **Brian's Brain** — 3-state cellular automaton with auto-noise for idle state visualization.
- **Whimsg** — Animated kaomoji status messages with typing effects.
- **Gamification** — 28 achievement-style missions across 6 categories (Environment, Intelligence, Projects, Loop, Seeds, SysInfo) tracked automatically during normal TUI operation.

### 🔧 Additional Features

- **Prompt Template Engine** — Background agent prompts support `{{TIMESTAMP}}`, `{{TASK_ID}}`, `{{LOG_PATH}}`, `{{FILE_PATH}}`, `{{EVENT_TYPE}}` placeholders.
- **Project Registry** — Automatic project index keyed by workdir hash. README descriptions extracted from project roots (min 20 words, before first `##` heading). Search and update via `project_search` and `project_update` MCP tools.
- **Action Protocol Advisor** — `get_tools` MCP tool returns scope-sensitive action protocols (`session_start`, `file_write`, `test_run`, `close_session`, `multi_agent`) with risk levels and recommended tool sets.
- **Setup Wizard** — Interactive `canopy setup` detects installed AI CLIs from a GitHub-hosted registry, configures binary paths, model flags, headless modes, environment variables, and temperature units. Generates `~/.canopy/config.toml`.
- **MCP Wizard** — `canopy mcp` subcommand for syncing, adding, and removing MCP server entries across all detected platforms with automatic format conversion (JSON ↔ TOML).
- **Skills Manager** — Global skill directory at `~/.agents/skills/` with cross-platform symlinks. List, validate symlink integrity, and remove installed skills across platforms.
- **Doctor Diagnostics** — `canopy doctor` checks data directory, database, config, harnesses, RAG status, file watchers, daemon process, registry connectivity, and auto-update health.
- **Desktop Notifications** — Cross-platform alerts for task completions, failures, watcher triggers, RAG indexing events, and loop lifecycle (started, spec completed, finished with outcome, blocker, hook failure).

---

## MCP Tools (64)

| Category | Tools |
|----------|-------|
| **Agent Management** (13) | `agent_add`, `agent_watch`, `agent_list`, `agent_remove`, `agent_enable`, `agent_disable`, `agent_schedule_enable`, `agent_run`, `agent_status`, `agent_models`, `agent_logs`, `agent_update`, `agent_report` |
| **Multi-Agent Sync** (4) | `sync_declare_intent`, `sync_report_status`, `sync_broadcast`, `sync_get_context` |
| **Intelligence V2** (6) | `intelligence_get_context`, `intelligence_upsert`, `intelligence_search`, `intelligence_graph_walk`, `intelligence_list_projects`, `intelligence_link_projects` |
| **Seed Identity** (5) | `get_identity`, `evolve_identity`, `create_seed`, `list_seeds`, `remove_seed` |
| **Loop Engine** (19) | `loop_create`, `loop_update`, `loop_add_spec`, `loop_update_spec`, `loop_add_node`, `loop_update_node`, `loop_add_edge`, `loop_update_edge`, `loop_add_ensemble`, `loop_update_ensemble`, `loop_get`, `loop_list`, `loop_run`, `loop_reset`, `loop_schedule_autorun`, `loop_pause`, `loop_continue`, `loop_complete_node`, `loop_report_blocker` |
| **Spec Backlog** (5) | `spec_create`, `spec_list`, `spec_update`, `spec_delete`, `spec_set_status` |
| **Spec Pools** (5) | `pool_create`, `pool_add_spec`, `pool_list`, `pool_remove_spec`, `pool_reorder` |
| **Node Blueprints** (3) | `blueprint_list`, `blueprint_create`, `blueprint_delete` |
| **Project** (2) | `project_search`, `project_update` |
| **RAG** (1) | `rag_search` |
| **Protocol** (1) | `get_tools` |

---

## Architecture Overview

- **Daemon** — Owns the MCP server (Streamable HTTP on port 7755 + stdio), scheduler, watcher engine, and database. Exposes all 64 MCP tools.
- **Scheduler** — Computes next fire times for all active tasks, sleeping until needed. Wakes instantly on changes.
- **Watcher Engine** — Reacts to file system events, triggering tasks as defined.
- **Executor** — Runs tasks and agents, manages locking, logs, and status.
- **Intelligence V2** — Project-scoped knowledge graph with node/edge CRUD, graph walk, project relationships.
- **Sync Manager** — Per-workdir in-memory broadcast channels (64 capacity), DB persistence, and intelligence node auto-upsert.
- **Loop Engine** — DAG execution engine: check/gate/agent node runners, iteration limits, pause/continue, blocker reporting, spec pools, node blueprints, scheduled autorun.
- **RAG Pipeline** — Background ingestion, language-aware chunking, embedding client, vector store, and rate-limited search.
- **TUI** — Full-screen ratatui terminal UI for managing agents, viewing output, loops, and system metrics in real time.
- **Gamification** — Mission tracker with 28 achievements across 6 categories, persisted in the database.
- **Skills Manager** — Global skills directory with cross-platform symlinks and integrity validation.

---

## Main Modules

- `application/` — Application ports and abstractions
- `autoupdate/` — Self-update system (GitHub releases)
- `daemon/` — MCP server, handler, params, RAG CLI, doctor
- `db/` — SQLite persistence and migrations (agents, runs, sessions, sync, intelligence, loops, projects, seeds, groups, state)
- `domain/` — Core models: Agent, Trigger, WatchEvent, Project, SeedIdentity, Loop, LoopSpec, LoopPool, LoopNodeBlueprint, SyncMessage, IntelligenceNode
- `executor/` — Task and agent execution logic
- `rag/` — RAG pipeline (ingestion, chunking, embedding, vector store, rate limiting)
- `scheduler/` — Internal cron scheduler with template variables
- `sync_manager/` — Multi-agent coordination broadcast
- `tui/` — Terminal UI: agent management, dialogs, sidebar, system dashboard, context transfer
- `watchers/` — File system watcher engine
- `loop_engine/` — DAG loop execution engine

---

## Data Storage

| Data | Location | Format |
|------|----------|--------|
| Structured data | `~/.canopy/background_agents.db` | SQLite (WAL mode) |
| Vector embeddings | `~/.canopy/rag/vectors.lancedb` | LanceDB |
| Seed identities | `~/.canopy/seeds/<id>/identity.toml` | TOML |
| Terminal history | `~/.canopy/terminals/<name>/history.toml` | TOML |
| Configuration | `~/.canopy/config.toml` | TOML |
| Agent logs | `~/.canopy/logs/<id>.log` | Text (5 MB rotation) |
| Daemon log | `~/.canopy/daemon.log` | Text |
| RAG indexed files | `~/.canopy/rag/` | Markdown, PDF |

---

## Usage

1. **Start the daemon:**
   ```bash
   canopy daemon start
   ```

2. **Configure (first time):**
   ```bash
   canopy setup
   ```
   Choose AI CLI platforms, model flags, temperature units, and RAG settings. Generates `~/.canopy/config.toml`.

3. **Launch the TUI:**
   ```bash
   canopy
   ```
   Opens the full-screen Canopy Hub. Press `Ctrl+N` or `n` to create a new agent (Interactive, Background, or Terminal). Select a seed identity to give the agent a persistent personality.

4. **Check health:**
   ```bash
   canopy doctor
   ```

5. **Manage MCP servers across platforms:**
   ```bash
   canopy mcp
   ```

6. **RAG indexing:**
   ```bash
   canopy rag auto-index start   # Start background watcher
   canopy rag auto-index stop    # Stop
   canopy rag report             # Per-file indexing report
   ```

All state persists in `~/.canopy/`. The daemon auto-updates to stable releases every 24 hours.

---

## Tech Stack

|Rust 2021| Tokio | Axum | rusqlite | LanceDB | notify | vt100 | ratatui | clap | serde | tracing | fastembed |

---

## License

MIT — see [LICENSE](LICENSE) for details.

---

An experiment of [UniverLab](https://github.com/UniverLab) — an open computational laboratory.
Made with ❤️ by [JheisonMB](https://github.com/JheisonMB)