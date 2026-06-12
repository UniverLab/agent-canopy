---
title: MCP Tools
description: All 46 MCP tools exposed by the canopy daemon, by category.
order: 9
---

# MCP Tools

The daemon exposes **46 MCP tools** over Streamable HTTP (port 7755) and
stdio. Connect any MCP-capable AI CLI with `canopy mcp`.

## Agent management (12)

| Tool | Description |
|---|---|
| `agent_add` | Create a cron-scheduled background agent |
| `agent_watch` | Create a file-watcher-triggered agent |
| `agent_list` | List registered agents |
| `agent_remove` | Remove an agent |
| `agent_enable` / `agent_disable` | Toggle an agent |
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

## Workflow engine (15)

`workflow_create`, `workflow_update`, `workflow_add_spec`,
`workflow_update_spec`, `workflow_add_node`, `workflow_update_node`,
`workflow_add_edge`, `workflow_update_edge`, `workflow_get`,
`workflow_list`, `workflow_run`, `workflow_pause`, `workflow_continue`,
`workflow_complete_node`, `workflow_report_blocker` — see
[Workflows](workflows.md).

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
