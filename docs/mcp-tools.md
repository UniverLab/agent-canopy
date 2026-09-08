---
title: MCP Tools
description: All 83 MCP tools exposed by the canopy daemon, by category.
order: 11
---

# MCP Tools

The daemon exposes **83 MCP tools** over Streamable HTTP (port 7755) and
stdio. Connect any MCP-capable AI CLI with `canopy mcp`.

## Agent management (14)

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
| `agent_probe` | Test platform+model liveness before loop_run |

## Multi-agent sync (4)

| Tool | Description |
|---|---|
| `sync_declare_intent` | Declare a mission with impact level |
| `sync_report_status` | Report workspace stability |
| `sync_broadcast` | Send info/query/answer messages |
| `sync_get_context` | Active missions, chatter, workspace vibe |

## Intelligence (8)

| Tool | Description |
|---|---|
| `intelligence_get_context` | Curated project context for session start |
| `intelligence_upsert` | Persist a fact, pattern or session summary |
| `intelligence_search` | Full-text search across nodes |
| `intelligence_graph_walk` | Traverse the knowledge graph |
| `intelligence_list_projects` | List known projects |
| `intelligence_link_projects` | Link projects with typed relations |
| `intelligence_delete_node` | Delete an intelligence node and its relations |
| `intelligence_delete_relation` | Delete a single relation by ID |

## Seed identity (5)

| Tool | Description |
|---|---|
| `get_identity` | Read the bound seed identity |
| `evolve_identity` | Refine the identity over time |
| `create_seed` / `list_seeds` / `remove_seed` | Manage the seed nursery |

## Loop engine (32)

`loop_create`, `loop_update`, `loop_add_spec`,
`loop_update_spec`, `loop_add_node`, `loop_update_node`,
`loop_add_edge`, `loop_update_edge`, `loop_delete_edge`,
`loop_delete_node`, `loop_add_ensemble`,
`loop_update_ensemble`, `loop_get`,
`loop_list`, `loop_run`, `loop_reset`, `loop_schedule_autorun`,
`loop_pause`, `loop_continue`,
`loop_complete_node`, `loop_report_blocker`,
`loop_export`, `loop_import`, `loop_archive`, `loop_restore`,
`loop_node_runs_list`, `loop_node_run_get`,
`loop_copy_node`, `loop_copy_ensemble`,
`loop_audit_node_configs`, `loop_schedule_continue`,
`loop_preflight` — see [Loops](loops.md).

## Spec backlog (7)

| Tool | Description |
|---|---|
| `spec_create` | Create a standalone spec (not bound to any loop) |
| `spec_list` | List specs, filterable by workdir and status |
| `spec_update` | Update a spec's name, description, or workdir tag |
| `spec_delete` | Delete an unbound spec |
| `spec_set_status` | Admin transition: complete, skip, or reopen a standalone spec |
| `spec_section_get` | Extract one canonical section (`<objective>`, `<constraints>`, …) from a spec body |
| `spec_convert` | Convert legacy heading-format spec bodies to the tagged `<spec>` format (skips tagged and running specs) |

Spec bodies use the tagged `<spec>` format with all seven canonical section
tags — `<objective>`, `<functional_requirements>`,
`<non_functional_requirements>`, `<constraints>`, `<guidelines>`, `<in_scope>`,
`<out_of_scope>` — with markdown inside each section. Writes that do not parse
are rejected naming the offending tag; legacy heading-format bodies remain
readable until `spec_convert` migrates them.

## Spec queues (5)

| Tool | Description |
|---|---|
| `queue_create` | Create an ordered queue of specs |
| `queue_add_spec` | Append a spec to the end of a queue |
| `queue_list` | List a queue's members, or all queues |
| `queue_remove_spec` | Remove a spec from a queue |
| `queue_reorder` | Full replacement of a queue's order |

## Node blueprints (3)

| Tool | Description |
|---|---|
| `blueprint_list` | List all builtins and custom blueprints |
| `blueprint_create` | Create a reusable node config template |
| `blueprint_delete` | Delete a custom blueprint (builtins protected) |

## Project (4)

| Tool | Description |
|---|---|
| `project_search` | Search the project registry |
| `project_update` | Update a project's registry entry |
| `project_remap` | Remap a moved/renamed workdir instead of orphaning history |
| `project_register` | Explicitly register a project directory (no marker file needed) |

## RAG (1)

| Tool | Description |
|---|---|
| `rag_search` | Semantic search over indexed documents (rate-limited) |

## Protocol (1)

| Tool | Description |
|---|---|
| `get_tools` | Scope-sensitive action protocols with recommended tool sets |
