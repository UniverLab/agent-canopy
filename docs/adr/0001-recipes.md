# ADR: Recipes — Installable Loop and Node Designs

## Status
Accepted

## Context

Canopy currently offers **blueprints** — predesigned, reusable node templates stored in the database and referenced by name in `loop_add_node`. Blueprints come in two flavors:

1. **Node blueprints** (`src/domain/blueprints.rs`): A `Blueprint { name, kind, config }` struct. Five builtin specs are seeded at daemon startup (`implementer`, `cargo-gates`, `reviewer-committer`, `commit-check`, `resilience`). Custom blueprints can be created via `blueprint_create` MCP tool.
2. **Ensemble blueprints** (`src/domain/blueprints.rs`): An `EnsembleBlueprint { name, prompt_template, members, min_pass }` struct. One builtin spec (`ensemble-proposers`) is seeded.

Blueprints are stored in two SQLite tables (`blueprints`, `ensemble_blueprints`) and exposed via three MCP tools (`blueprint_list`, `blueprint_create`, `blueprint_delete`). The `loop_add_node` tool accepts a `blueprint` param as an alternative to a full `config`, with optional `config_overrides` for shallow merging.

**Problems with blueprints:**
- They only template **single nodes** (or ensembles), not complete loops.
- There is no mechanism to share/install a complete loop design.
- The database-backed surface (three MCP tools, two tables, seeding logic) is disproportionate to the value delivered.
- Prompt presets (`src/domain/prompts.rs`) are file-backed under `~/.canopy/prompts/` and serve a similar role for agent node prompts — but are disconnected from the blueprint system.

**What exists today that recipes can build on:**
- `loop_export`/`loop_import` (`src/domain/loop_transfer.rs`) already produces a portable JSON document of a loop's design (nodes, edges, ensembles), stripping `platform`/`model` by default for sharing.
- The **dynamic skills system** (`src/dynamic_skills/mod.rs`) provides a proven model: git-sourced, TTL-refreshed, file-backed store at `~/.canopy/skills/`, with `skill_list`/`skill_get` MCP tools.
- Registry sync (`4dbed30`) now reconciles existing entries on refresh (add/update/delete), not just add-and-delete.
- Structural graph validation (`7a1ce71`, `src/domain/validation.rs`) validates loop graphs at import time.

**Source decisions.** This ADR closes CC2. The decisions it records were taken in Intelligence `7b9316f3` (2026-09-01 — "recipes replaces blueprints; the surface is retired, not renamed") and `b24775c4` (2026-08-27 — "installable catalogue of loop and node designs"), inside the scope fixed by `7a20e783`. Where those nodes left a choice open (e.g. "maybe one repo each"), this document makes it and says why.

## Decision

**Replace blueprints with recipes.** Recipes are installable loop and node designs, fetched from git sources into a file-backed store.

**The install model is "reach a git repo of templates by one command", the same way texforge reaches its templates** — a separate template repository, fetched on demand, refreshed in the background, never vendored as the source of truth into the binary. Canopy already has an in-repo realization of exactly that pattern: the **dynamic skills system** (`src/dynamic_skills/mod.rs`) — configurable git sources, a canopy-owned store under `~/.canopy/skills/`, a TTL'd commit-hash check for lazy refresh, and `skill_list`/`skill_get` over MCP. Recipes reuse that machinery rather than growing a second, parallel one. `b24775c4` names the same two reference points ("como skills.sh", "igual que texforge llega a sus plantillas").

### Two resource types, one system

| Resource | What it is | Format |
|----------|-----------|--------|
| **Node recipe** | A predesigned node (what blueprints attempted) | A single node's config + kind + optional prompt |
| **Loop recipe** | A complete loop design (what has no home today) | A `LoopExportDocument` (the format `loop_export` already produces) |

Ensembles belong on the **node side** — an ensemble recipe is a node recipe of kind `ensemble` (or a special variant). Both resource types live in one shared recipe store.

### Repository layout (one git source, two subdirectories)

```
recipes/
  nodes/
    implementer/
      recipe.json      # { kind: "agent", config: { prompt_preset: "implementer" }, ... }
    cargo-gates/
      recipe.json
    reviewer-committer/
      recipe.json
    commit-check/
      recipe.json
    resilience/
      recipe.json
    ensemble-proposers/
      recipe.json      # { kind: "ensemble", prompt_template: "...", members: [...], ... }
  loops/
    cascade/
      recipe.json      # A LoopExportDocument
    tdd-cycle/
      recipe.json
```

A recipe repo is a git repository with this structure. The default source is `https://github.com/UniverLab/recipes` (configurable via `config.toml`).

### Installation and storage

Recipes are installed into `~/.canopy/recipes/` (mirroring the skills store at `~/.canopy/skills/`).

```
~/.canopy/
  recipes/
    nodes/
      implementer/
        recipe.json
        .metadata.json   # { source_url, commit_hash, installed_at }
      ...
    loops/
      cascade/
        recipe.json
        .metadata.json
```

### MCP tools

Remove:
- `blueprint_list`
- `blueprint_create`
- `blueprint_delete`

Add:
- `recipe_list` — List installed recipes (nodes and loops), with source URL and install status. Mirrors `skill_list`.
- `recipe_get` — Fetch a recipe's full content. For node recipes, returns the config. For loop recipes, returns the `LoopExportDocument`. Mirrors `skill_get`.
- `recipe_install` — Install a recipe from a configured source. Takes a recipe name and type (`node` or `loop`). Fetches from git, validates (for loops: runs `validate_loop_graph`), and writes to the store.
- `recipe_update` — Update an installed recipe to the latest version from its source. Uses the reconciliation logic from `4dbed30` (update existing entries, not just add/delete).
- `recipe_uninstall` — Remove an installed recipe.

### CLI commands

```
canopy recipe list [--type node|loop]
canopy recipe install <name> [--type node|loop]
canopy recipe update <name> [--type node|loop]
canopy recipe uninstall <name> [--type node|loop]
```

The verb is `install`, not the `import` that `b24775c4` sketched before the name was fixed: `loop_import`/`loop_export` already own "import"/"export" for raw loop JSON, and `install`/`uninstall`/`update` line up with the cross-tool CLI surface that `7a20e783` (decision 11) and `e6b760b7` want across all five tools. Every CLI command is reachable identically from the MCP tools above — same code path, two front ends.

### What a recipe carries besides the graph

A **node recipe** carries:
- `kind`: `agent`, `check`, `gate`, or `ensemble`
- `config`: The node's config (for agents: `prompt_template` or `prompt_preset`; for checks: `command`; for ensembles: `prompt_template`, `members`, `min_pass`, etc.)
- `description`: One-line description
- `tags`: Optional list of tags for filtering

A **loop recipe** carries:
- The full `LoopExportDocument` (name, description, nodes, edges, ensembles)
- `requirements`: What the recipe needs from the installer
  - `platforms`: List of platforms used (e.g. `["claude", "opencode"]`) — the installer must have these configured
  - `skills`: List of skills required (e.g. `["rust-idiomatic-patterns"]`)
  - `workdir_pattern`: Optional regex for valid working directories (e.g. `"**/rust-project"`)

The installer (`recipe_install`) validates requirements:
- For loop recipes: runs `validate_loop_graph` (from `7a1ce71`) before accepting. This is structural only (edges, reachability, one entry, pass/fail coverage) — it does **not** check that platforms or models are set.
- Checks that required platforms are configured (warns, does not block).
- Checks that required skills are available (warns, does not block).

#### Platform and model: what the recipe declares vs. what the installer picks

`loop_export` strips `platform` and `model` from every agent node unless `--with-models`, and that elision is correct for sharing — a recipe pinned to `claude/opus` is dead weight to someone who runs `opencode`. The same is true of node recipes: a shared predesigned node ships without a harness. So:

- `requirements.platforms` on a loop recipe is a **hint** for the installer's preflight check, never a value written into a node.
- A recipe **never pins a model.** If an author exports `--with-models` to ship a reference config, the installer treats those values as overridable defaults, not requirements.
- `recipe_install` for a loop recipe creates the loop exactly as `loop_import` does today: agent nodes land with `platform`/`model` unset, and the response returns the nodes still needing a harness — the `agent_nodes_missing_platform` signal `loop_import` already produces (`src/domain/loop_transfer.rs`).
- The operator, or an agent via `loop_update_node`, then assigns a concrete `platform` + `model` per node. `loop_preflight`/`loop_run` enforce a valid pair before the loop can run. **A freshly installed loop recipe is structurally valid but not yet runnable** until this step is done — this is the intended state, not a defect.
- For a node recipe, the harness is supplied the same way it is for any hand-written node: at the point the node is instantiated into a loop (`loop_add_node` with a full `config`, or `loop_update_node` afterwards). Install-time validation of a node recipe is shape-only (`validate_node_config`'s structural checks), skipping the agent-requires-platform rule that a stripped recipe cannot satisfy.

### Where the seeded builtins come from

Today the five node patterns (`implementer`, `cargo-gates`, `reviewer-committer`, `commit-check`, `resilience`) and the one ensemble pattern (`ensemble-proposers`) are hardcoded specs in the binary, reseeded into SQLite at every daemon startup — **offline-safe by construction.** That property must not regress: a daemon on a laptop with no network still boots with the proven pattern available.

Once blueprints are gone, those six become **the first entries of the default recipe source** (`https://github.com/UniverLab/recipes`, pre-configured in `config.toml`), surfaced by `recipe_list` as available-but-not-installed and fetched lazily on first use — the dynamic-skills model, with **no blocking network fetch at daemon startup.**

To keep the offline guarantee, the same six recipe documents are **embedded in the binary** (`include_str!`) as a fallback catalog: if the default source is unreachable, `recipe_list` still lists them and `recipe_install` still works from the embedded copy. A reachable source always wins over the embedded copy — the precedence rule from `a2875ce9` (binary-bundled skills must not shadow the fresher downloaded ones), applied here from the start rather than as a later fix.

**Prompt presets** (`src/domain/prompts.rs`) remain unchanged. They are file-backed under `~/.canopy/prompts/` and are referenced by `prompt_preset` in agent node configs. The `implementer`, `reviewer`, and `resilience` presets are still seeded at daemon startup. Node recipes for these agents reference the presets via `config.prompt_preset`.

### How an installed recipe stays current

Based on the corrected registry sync from `4dbed30`:

1. Each installed recipe has a `.metadata.json` with `source_url`, `commit_hash`, `installed_at`.
2. `recipe_list` checks the TTL (configurable, default 15 minutes). If the TTL has expired, it fetches the latest commit hash from the source (without downloading the full recipe).
3. If the commit hash has changed, the recipe is marked as `outdated` in the listing.
4. `recipe_update` fetches the latest version from the source, validates it, and overwrites the installed version.
5. The reconciliation model from `4dbed30` is used: a per-entry **add / update / delete / unchanged** pass (not the previous add-and-delete-only behaviour), with a three-way merge so a local modification to an installed recipe survives an update unless the upstream change touches the same field.

### Configuration

Add to `src/domain/canopy_config.rs`:

```rust
pub struct RecipesConfig {
    pub sources: Vec<RecipeSource>,
    pub ttl_minutes: u64,
}

pub struct RecipeSource {
    pub url: String,
    pub git_ref: Option<String>,
}
```

Default `config.toml`:

```toml
[recipes]
ttl_minutes = 15

[[recipes.sources]]
url = "https://github.com/UniverLab/recipes"
```

## Consequences

### Positive
- **Complete loop designs can be shared**, not just individual nodes.
- **Git-sourced**: recipes are versioned, reviewable, and forkable.
- **File-backed**: recipes are human-readable JSON files, not database rows.
- **Unified surface**: one system (recipes) replaces two (node blueprints + ensemble blueprints) and adds loop sharing.
- **Proven model**: mirrors the dynamic skills system, which is already working.

### Negative
- **Migration effort**: existing loops that reference blueprints keep working (configs are inline), but any tooling or docs that mention `blueprint_*` must be updated.
- **Two stores to manage**: recipes (`~/.canopy/recipes/`) and skills (`~/.canopy/skills/`) are separate, though they could be unified later.
- **Git dependency for updates**: staying current requires git access to the source; first use offline falls back to the embedded builtin catalog only.
- **Embedded fallback must be kept in sync**: the `include_str!` copy of the six builtins has to be refreshed when the default source's builtins change, or an offline install serves a stale pattern.
- **Two-step install for loop recipes**: an installed loop recipe is structurally valid but not runnable until a harness is assigned per agent node — one more step than `blueprint`-based `loop_add_node`, which took `config_overrides` inline.

### Neutral
- **Prompt presets remain separate**: they are file-backed and referenced by `prompt_preset`, not part of the recipe system.
- **`loop_export`/`loop_import` unchanged**: loop recipes use the same `LoopExportDocument` format.

## Alternatives Rejected

### Alternative 1: Keep blueprints, add loop export/import as a separate surface
**Rejected because:** This leaves two disconnected systems (blueprints for nodes, export/import for loops). Recipes unify them under one model.

### Alternative 2: Database-backed recipes (like blueprints)
**Rejected because:** File-backed is more transparent, versionable (via git), and aligns with the skills system. Database-backed recipes would require the same seeding/sync logic we're trying to move away from.

### Alternative 3: One recipe type (only loops, or only nodes)
**Rejected because:** Both are useful. Node recipes replace what blueprints attempted. Loop recipes fill a gap (no way to share complete designs today).

### Alternative 4: Separate stores for node recipes and loop recipes
**Rejected because:** One store (`~/.canopy/recipes/`) with subdirectories (`nodes/`, `loops/`) is simpler and mirrors the skills system's flat structure.

## Blueprint Removal Inventory

### Files to remove

| File | What it contains | Fate |
|------|-----------------|------|
| `src/domain/blueprints.rs` | `Blueprint`, `EnsembleBlueprint`, `builtin_blueprint_specs()`, `builtin_ensemble_blueprint_specs()`, `merge_blueprint_config()`, `validate_blueprint_deletable()` | **Remove entirely** |
| `src/db/blueprints.rs` | `insert_blueprint`, `get_blueprint_by_name`, `list_blueprints`, `delete_blueprint_by_name`, `seed_builtin_blueprints`, `update_builtin_blueprint` | **Remove entirely** |
| `src/db/ensembles.rs` (ensemble blueprint functions) | `insert_ensemble_blueprint`, `get_ensemble_blueprint_by_name`, `list_ensemble_blueprints`, `delete_ensemble_blueprint_by_name`, `seed_builtin_ensemble_blueprints` | **Remove** (keep other ensemble functions) |

### Files to modify

| File | What to change |
|------|---------------|
| `src/db/mod.rs` | Remove `CREATE TABLE blueprints` and `CREATE TABLE ensemble_blueprints` from the schema, the `seed_builtin_blueprints()` / `seed_builtin_ensemble_blueprints()` calls in `Database::new()`, and the `pub mod blueprints;` declaration. |
| `src/domain/mod.rs` | Remove `pub mod blueprints;` |
| `src/daemon/handler.rs` | Remove `blueprint_list`, `blueprint_create`, `blueprint_delete` MCP tools. Remove `validate_blueprint_exists()`, `blueprint_json()`, `resolve_node_kind_and_config()` (replace with logic that only accepts `config`). Remove the `blueprint` param from `loop_add_node` and `loop_add_ensemble`, and the `db.seed_builtin_blueprints()` call in the test harness. Remove all blueprint-related tests. |
| `src/daemon/params.rs` | Remove `BlueprintCreateParams`, `BlueprintDeleteParams`. Remove `blueprint` and `config_overrides` from `LoopAddNodeParams`, and `blueprint` from `LoopAddEnsembleParams`. **Leave `LoopCopyNodeParams.config_overrides` alone** — `loop_copy_node` uses it independently of blueprints. |
| `src/domain/prompts.rs` | **No changes**. Prompt presets remain unchanged (see below). |
| `src/domain/loops.rs` | Remove the `EnsembleBlueprint` mention in the `EnsembleMemberSpec` doc comment (~line 911) and its intra-doc link target. |
| `src/db/ensembles.rs` | Remove `insert/get/list/delete_ensemble_blueprint*` and `seed_builtin_ensemble_blueprints`, and the doc-comment cross-references to `update_builtin_blueprint` / `seed_builtin_blueprints`. Keep every non-blueprint ensemble function. |
| `README.md` | Remove the "Node Blueprints" feature bullet, the `blueprint_*` row and count in the MCP-tools table, and the "node blueprints" item in the loop-engine paragraph. Add the recipes surface. |
| `docs/loops.md` | Remove the "Node blueprints" section and the `#node-blueprints` cross-reference at the ensemble-default paragraph. Replace with a pointer to the recipes page. |
| `docs/mcp-tools.md` | Replace the "Node blueprints (3)" section with the `recipe_*` tools. |
| `docs/index.md` | Drop "blueprints" from the tool-summary line. |

### Database migration

Add a migration to drop the `blueprints` and `ensemble_blueprints` tables. Existing loops are not affected — they store node configs inline, not as blueprint references.

### Test impact

- Remove all tests in `src/db/blueprints.rs` (395 lines).
- Remove all tests in `src/domain/blueprints.rs` (485 lines).
- Remove blueprint-related tests in `src/daemon/handler.rs` (~127 non-test lines, many test references).
- Remove ensemble blueprint tests in `src/db/ensembles.rs`.
- Update `loop_add_node` and `loop_add_ensemble` tests to use inline configs instead of blueprint references.

### What existing loops break?

**None.** Existing loops store node configs inline in the `loop_nodes` table. The `blueprint` param in `loop_add_node` is only used at node creation time — once a node is created, its config is persisted and the blueprint reference is discarded. Removing blueprints does not affect existing loops.

### What about the builtin node patterns?

The five builtin node patterns (`implementer`, `cargo-gates`, `reviewer-committer`, `commit-check`, `resilience`) and one builtin ensemble pattern (`ensemble-proposers`) become the first entries in the default recipe source (`https://github.com/UniverLab/recipes`), fetched from git like any other recipe. The `Blueprint`/`EnsembleBlueprint` structs and the SQLite seeding go away; the six documents are kept `include_str!`-embedded as an offline fallback catalog (see "Where the seeded builtins come from"), with a reachable source always taking precedence.

### What about prompt presets?

Prompt presets (`src/domain/prompts.rs`) remain unchanged. They are file-backed under `~/.canopy/prompts/` and are seeded at daemon startup. Node recipes for agent nodes reference these presets via `config.prompt_preset`.

## Implementation Notes (non-binding)

Implementation is out of scope for this ADR and is specified later in its own spec, derived from the decisions above. The sketch below is orientation for that spec, not a commitment to a file layout or a test list.

### Scope check

This spec's work is entirely within this repository. No cross-repo commits are needed. The default recipe source (`https://github.com/UniverLab/recipes`) is a separate repo, but its creation is out of scope for this spec (see "Out of Scope" in the spec).

### Files to create (for the recipes system)

| File | What it contains |
|------|-----------------|
| `src/domain/recipes.rs` | `Recipe`, `NodeRecipe`, `LoopRecipe` structs. `builtin_node_recipe_specs()`, `builtin_loop_recipe_specs()`. |
| `src/recipe_store/mod.rs` | `RecipeStore` (mirrors `SkillStore`). `list()`, `get()`, `install()`, `update()`, `uninstall()`. |
| `src/recipe_store/source.rs` | `GitSource` for recipes (mirrors `src/dynamic_skills/source.rs`). |
| `src/daemon/recipes_cli.rs` | CLI commands: `canopy recipe list/install/update/uninstall`. |
| `src/daemon/handler.rs` (new tools) | `recipe_list`, `recipe_get`, `recipe_install`, `recipe_update`, `recipe_uninstall` MCP tools. |

### Validation

Loop recipes must pass `validate_loop_graph` (from `7a1ce71`) before installation — structural checks only, which a platform/model-stripped document satisfies. Node recipes pass `validate_node_config`'s **shape** checks (kind-appropriate keys, no unknown keys), but not its agent-requires-`platform`/`cli` rule, which a stripped recipe by design cannot meet; the harness is supplied when the node is instantiated into a loop (see "Platform and model" above).

### Tests

- `recipe_store_list_returns_installed_and_available_recipes`
- `recipe_install_fetches_from_source_and_writes_to_store`
- `recipe_install_validates_loop_graph_before_accepting`
- `recipe_update_reconciles_existing_entry`
- `recipe_uninstall_removes_from_store`
- `recipe_list_marks_outdated_recipes_when_ttl_expires`
- `builtin_node_recipes_are_seeded_from_default_source`
