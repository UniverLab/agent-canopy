---
title: MCP Tools
description: All 61 MCP tools exposed by the canopy daemon, by category.
order: 9
---

# MCP Tools

The daemon exposes **61 MCP tools** over Streamable HTTP (port 7755) and
stdio. Connect any MCP-capable AI CLI with `canopy mcp`.

## Agent management (13)

| Tool | Description |
|---|---|
| `agent_add` | Create a cron-scheduled background agent |
| `agent_watch` | Create a file-watcher-triggered agent |
| `agent_list` | List registered agents |
| `agent_remove` | Remove an agent |
| `agent_enable` / `agent_disable` | Toggle an agent |
| `agent_schedule_enable` | Schedule a one-shot enable at a future time |
| `agent_run` | Run an agent immediately |
| `agent_status` | Daemon health and agent status |
| `agent_models` | List available models per CLI |
| `agent_logs` | Read execution logs |
| `agent_update` | Update an agent's definition |
| `agent_report` | Report execution status for scheduled tasks |

## Multi-agent sync (4)

| Tool | Description |
|---|---|
| `sync_declare_intent` | Declare a mission with impact level |
| `sync_report_status` | Report workspace stability |
| `sync_broadcast` | Send info/query/answer messages |
| `sync_get_context` | Active missions, chatter, workspace vibe |

## Intelligence (6)

| Tool | Description |
|---|---|
| `intelligence_get_context` | Curated project context for session start |
| `intelligence_upsert` | Persist a fact, pattern or session summary |
| `intelligence_search` | Full-text search across nodes |
| `intelligence_graph_walk` | Traverse the knowledge graph |
| `intelligence_list_projects` | List known projects |
| `intelligence_link_projects` | Link projects with typed relations |

## Seed identity (5)

| Tool | Description |
|---|---|
| `get_identity` | Read the bound seed identity |
| `evolve_identity` | Refine the identity over time |
| `create_seed` / `list_seeds` / `remove_seed` | Manage the seed nursery |

## Loop engine (17)

`loop_create`, `loop_update`, `loop_add_spec`,
`loop_update_spec`, `loop_add_node`, `loop_update_node`,
`loop_add_edge`, `loop_update_edge`, `loop_get`,
`loop_list`, `loop_run`, `loop_reset`, `loop_schedule_autorun`,
`loop_pause`, `loop_continue`,
`loop_complete_node`, `loop_report_blocker` — see
[Loops](loops.md).

## Spec backlog (4)

| Tool | Description |
|---|---|
| `spec_create` | Create a standalone spec (not bound to any loop) |
| `spec_list` | List specs, filterable by workdir and status |
| `spec_update` | Update a spec's name, description, or workdir tag |
| `spec_delete` | Delete an unbound spec |

## Spec pools (5)

| Tool | Description |
|---|---|
| `pool_create` | Create an ordered queue of specs |
| `pool_add_spec` | Append a spec to the end of a pool's queue |
| `pool_list` | List a pool's members, or all pools |
| `pool_remove_spec` | Remove a spec from a pool |
| `pool_reorder` | Full replacement of a pool's queue order |

## Node blueprints (3)

| Tool | Description |
|---|---|
| `blueprint_list` | List all builtins and custom blueprints |
| `blueprint_create` | Create a reusable node config template |
| `blueprint_delete` | Delete a custom blueprint (builtins protected) |

## Project (2)

| Tool | Description |
|---|---|
| `project_search` | Search the automatic project registry |
| `project_update` | Update a project's registry entry |

## RAG (1)

| Tool | Description |
|---|---|
| `rag_search` | Semantic search over indexed documents (rate-limited) |

## Protocol (1)

| Tool | Description |
|---|---|
| `get_tools` | Scope-sensitive action protocols with recommended tool sets |
