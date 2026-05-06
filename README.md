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

harness-canopy is a modern, self-contained MCP (Multi-Agent Control Point) server for orchestrating AI agent tasks and file event triggers. Designed for reliability, modularity, and performance, it enables advanced scheduling, file watching, and interactive agent management with zero runtime dependencies.

---

## Features

### 🎯 Core Capabilities

- **🚀 High-Performance Scheduler:** Event-driven cron scheduler using Tokio with zero polling overhead. Computes precise wake-up times and sleeps until needed, reducing CPU usage to near-zero when idle.
- **📊 Real-time File Watcher:** Instantly reacts to file system events (create, modify, delete, move) using the notify crate with configurable debouncing and recursive directory monitoring.
- **💾 Persistent State Management:** All tasks, watchers, execution logs, and agent state are stored in an embedded SQLite database with automatic migrations and transaction safety.
- **🧠 Personal RAG Pipeline:** Markdown/PDF notes are semantically chunked, embedded with your configured model, and stored with vector metadata for higher quality retrieval.

### 🤖 Agent Orchestration

- **Interactive PTY Agents:** Each agent runs in a dedicated pseudo-terminal (PTY) with full vt100 emulation, supporting 24-bit colors, cursor positioning, and interactive applications.
- **Terminal Sessions:** Raw shell sessions with command history, tab autocomplete, directory navigation, and Warp-like input mode for efficient command entry.
- **Split View Mode:** Side-by-side or stacked terminal/agent sessions with independent focus, allowing simultaneous monitoring of multiple agents.
- **Context Transfer:** Seamlessly transfer context between agents while preserving session state and scrollback history. Capture conversation history, prompts, and outputs from one agent and inject them into another.
- **Prompt Builder:** Structured prompt templates with configurable sections (instruction, context, resources, examples) to create well-formatted prompts for agents.

### 🔧 Advanced Task Management

- **Flexible Scheduling:** Support for cron expressions, one-time tasks, and event-triggered watchers with configurable timeouts and expiration.
- **Execution Control:** Task locking, concurrency limits, and per-run logging with detailed execution history and status tracking.
- **Auto-Update System:** Automatically checks for and installs stable releases from GitHub at 24-hour intervals, ensuring you always have the latest features and fixes. Canopy detects new stable releases on startup and uses the built-in `scripts/install.sh` to perform atomic binary replacement.

### 🌐 Cross-Platform Support

- **Single Static Binary:** Zero runtime dependencies — just download and run on Linux, macOS, or Windows.
- **Platform-Specific Optimizations:** Native filesystem monitoring, process management, and terminal handling for each operating system.
- **Unified Configuration:** Consistent CLI and API interface across all supported platforms.

### 🧩 Modular Architecture

- **Clear Separation of Concerns:** Independent modules for application logic, daemon lifecycle, database persistence, domain models, execution engine, scheduling, TUI, and file watching.
- **Extensible Design:** Easy to add new CLI integrations, custom triggers, or agent types without modifying core components.
- **Test Coverage:** Comprehensive unit and integration tests with 100% code coverage for critical paths.

---

## Architecture Overview

- **Daemon:** Owns the MCP server, scheduler, watcher engine, and database. Exposes a Streamable HTTP API and stdio mode.
- **Scheduler:** Computes next fire times for all active tasks, sleeping until needed. Wakes instantly on changes.
- **Watcher Engine:** Reacts to file system events, triggering tasks as defined.
- **Executor:** Runs tasks and agents, manages locking, logs, and status.
- **TUI:** Interactive terminal UI for managing agents and viewing output in real time.

---

## Main Modules

- `application/` — Application ports and abstractions
- `daemon/` — Daemon process and lifecycle
- `db/` — SQLite persistence and migrations
- `domain/` — Core models: Task, Watcher, ExecutionLog, etc.
- `executor/` — Task and agent execution logic
- `scheduler/` — Internal cron scheduler
- `tui/` — Terminal UI and agent management
- `watchers/` — File system watcher engine

---

## Usage

1. **Start the daemon:**
   ```bash
   canopy daemon start
   ```
2. **Configure Personal RAG (optional but recommended):**
   Run `canopy setup` and choose an `embeddings_model` plus `similarity_threshold` for semantic chunking. Canopy will embed indexed notes during daemon ingestion and persist the vectors for similarity search.
3. **Add tasks and watchers:**
   Use the CLI or API to register scheduled tasks and file event watchers. Each task can specify:
   - `id`, `prompt`, `schedule_expr`, `cli`, `model`, `working_dir`, `timeout_minutes`, etc.
   - Watchers specify `path`, `events`, and trigger logic.
4. **Monitor and manage:**
   - View logs, status, and manage agents interactively via the TUI.
   - All state is persisted in `~/.canopy/tasks.db`.

**Note:** Canopy automatically checks for updates every 24 hours and installs stable releases. No manual intervention required! The system verifies GitHub releases, downloads the appropriate binary for your platform, and performs an atomic replacement of the running executable.

---

## Extending

- Add new CLI integrations by extending the `domain` and `executor` modules.
- Implement custom triggers or agent types by building on the modular architecture.

---

## Tech Stack

- Rust 2021, Tokio, Axum, rusqlite, notify, vt100, ratatui, clap, serde, tracing

---

## License

MIT — see [LICENSE](LICENSE) for details.

---

Made with ❤️ by [JheisonMB](https://github.com/JheisonMB) and [UniverLab](https://github.com/UniverLab)
