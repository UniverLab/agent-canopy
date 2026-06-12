---
title: Workflows
description: The DAG workflow engine — specs, nodes, edges, gates and lifecycle.
order: 7
---

# Workflows

The workflow engine runs multi-step processes as a DAG, combining agent
invocations, shell checks and validation gates.

## Structure

```
Workflow
└── Spec (ordered)
    └── Node ──Edge(pass/fail/always)──▶ Node
```

- **Specs** — ordered units of work inside a workflow.
- **Nodes** — three kinds:
  - `agent` — invokes a CLI tool with a prompt template.
  - `check` — executes a shell command.
  - `gate` — validates previous output (e.g. `output_contains`).
- **Edges** — connect nodes with routing conditions: `pass`, `fail`,
  `always`.

## Template variables

Workflow prompts support `{{workflow_name}}`, `{{spec_content}}`,
`{{node_id}}`, `{{previous_feedback}}` and more — so a failing check can
feed its output back into the retrying agent node.

## Lifecycle

Create → run → (pause / continue) → complete. `workflow_continue`
supports **retry** and **skip** strategies for stuck nodes, and
`workflow_report_blocker` escalates to a human when intervention is
needed. Iteration limits prevent infinite retry loops.

## The 15 MCP tools

| Stage | Tools |
|---|---|
| Authoring | `workflow_create`, `workflow_update`, `workflow_add_spec`, `workflow_update_spec`, `workflow_add_node`, `workflow_update_node`, `workflow_add_edge`, `workflow_update_edge` |
| Inspection | `workflow_get`, `workflow_list` |
| Runtime | `workflow_run`, `workflow_pause`, `workflow_continue`, `workflow_complete_node`, `workflow_report_blocker` |

Workflows can be authored programmatically by agents through these tools,
or edited in the [TUI workflow editor](tui.md) with inline JSON config
validation.
