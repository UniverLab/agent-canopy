---
title: Installation
description: Install canopy, run the setup wizard, and keep it updated automatically.
order: 2
---

# Installation

## Quick install

**Linux / macOS:**

```bash
curl -fsSL https://raw.githubusercontent.com/UniverLab/harness-canopy/main/scripts/install.sh | sh
```

## Via cargo

```bash
cargo install harness-canopy
```

The crate is `harness-canopy`; the installed binary is **`canopy`**.
Available on [crates.io](https://crates.io/crates/harness-canopy).

## From source

```bash
git clone https://github.com/UniverLab/harness-canopy.git
cd harness-canopy
cargo build --release
# Binary at target/release/canopy
```

## First-time setup

```bash
canopy setup
```

The interactive wizard detects installed AI CLIs from a GitHub-hosted
registry, configures binary paths, model flags, headless modes,
environment variables and temperature units, and generates
`~/.canopy/config.toml`. Running `canopy` with no arguments triggers
setup automatically if it has never run.

## Auto-update

The daemon checks GitHub releases for new stable versions every 24 hours,
downloads the platform-specific binary (linux-musl, macos-darwin;
x86_64/aarch64) and atomically replaces the running executable.

## Running as a service

```bash
canopy daemon install-service     # register with systemd/launchd
canopy daemon uninstall-service
```

## Data directory

All state lives under `~/.canopy/`:

| Data | Location | Format |
|------|----------|--------|
| Structured data | `~/.canopy/background_agents.db` | SQLite (WAL mode) |
| Vector embeddings | `~/.canopy/rag/vectors.lancedb` | LanceDB |
| Seed identities | `~/.canopy/seeds/<id>/identity.toml` | TOML |
| Terminal history | `~/.canopy/terminals/<name>/history.toml` | TOML |
| Configuration | `~/.canopy/config.toml` | TOML |
| Agent logs | `~/.canopy/logs/<id>.log` | Text (5 MB rotation) |
| Daemon log | `~/.canopy/daemon.log` | Text |

## Health check

```bash
canopy doctor
```

Checks the data directory, database, config, harnesses, RAG status, file
watchers, daemon process, registry connectivity and auto-update health.
