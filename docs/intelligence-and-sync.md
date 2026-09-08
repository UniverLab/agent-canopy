---
title: Intelligence & Sync
description: The project-scoped knowledge graph and multi-agent workspace coordination.
order: 6
---

# Intelligence & Sync

## Intelligence — the knowledge graph

Canopy persists what agents learn, scoped to projects.

- **Project scoping** — knowledge is keyed by `project_hash` (SHA-256 of
  the canonical workdir path, truncated to 8 hex chars), auto-detected
  from the session workdir. Agents never set it manually.
- **Node kinds** — facts, patterns and session summaries.
- **Full-text search** — `intelligence_search` queries nodes by text and
  optional kind filter across all projects.
- **Graph walk** — `intelligence_graph_walk` traverses from any node up
  to a configurable depth, returning connected facts, patterns and
  cross-references.
- **Project relationships** — `intelligence_link_projects` links projects
  with typed relations (`depends_on`, `complements`, `extends`, `publishes`;
  `relates_to` accepted as legacy; `contains` is derived from registry paths
  and cannot be hand-linked). Scoped reads traverse outbound edges (each hit
  marked with `via_project` + `via_relation`, `contains` hops read as
  `inherited:contains`), and full context warns about inbound `depends_on`
  dependents via `dependency_impact`.
- **Context retrieval** — `intelligence_get_context` auto-detects the
  project and returns a curated mix of session knowledge, project facts
  and related-project summaries.

A typical agent session starts with `intelligence_get_context` and ends
with an `intelligence_upsert` of a session summary — so the next session
starts smarter.

## Project registry

Canopy registers a project only explicitly: a `.canopy-project` marker file
in the directory root enables automatic registration, or call
`project_register` with the directory path. Registration derives `contains`
edges from registry paths automatically. `project_search` and
`project_update` expose the registry to agents; registry noise is removable
via project deletion / `canopy clean`, and `project_remap` moves a project
(including its graph node) after a directory rename or move.

## Sync — multi-agent coordination

When several agents share a workspace, the sync layer keeps them from
stepping on each other:

- **Mission declaration** — `sync_declare_intent` declares a high-level
  mission with an impact level (`low` / `high` / `breaking`).
- **Workspace status** — `sync_report_status` reports stability
  (`stable` / `unstable` / `testing`).
- **Broadcast messaging** — `sync_broadcast` sends info, query and answer
  messages between agents in the same workdir.
- **Active context** — `sync_get_context` returns active missions, recent
  chatter and a computed workspace "vibe" (the worst status among active
  intents).

Messages flow through per-workdir in-memory broadcast channels and are
persisted to the database; relevant ones are auto-upserted as
intelligence nodes.

## Action protocol advisor

`get_tools` returns scope-sensitive action protocols (`session_start`,
`file_write`, `test_run`, `close_session`, `multi_agent`) with risk
levels and recommended tool sets — a built-in playbook agents consult
before acting.
