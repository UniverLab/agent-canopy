---
title: RAG Pipeline
description: Local-first semantic search over your own documents.
order: 8
---

# RAG Pipeline

Canopy includes a personal RAG (retrieval-augmented generation) pipeline:
index your own documents, search them semantically, and inject results
into agent sessions.

## Capabilities

- **Semantic search** — embed and query Markdown, MDX and PDF documents.
- **Local-first embeddings** — local ONNX models (fastembed/BGE,
  multilingual-e5) or remote APIs (OpenAI, Gemini) if you prefer.
- **Language-aware chunking** — Markdown split by headings with paragraph
  fallback, similarity-aware merging, and overlap for context
  preservation.
- **Robust PDF extraction** — runs in an isolated subprocess so a parser
  crash can't take down the daemon; HTML detection and raw-text salvage
  fallback.
- **Vector store** — embeddings persisted in LanceDB under
  `~/.canopy/rag/vectors.lancedb`.

## Auto-ingestion

A background watcher monitors your configured RAG roots with a 3-second
debounce, enqueues changes, and reconciles orphan chunks on startup.

```bash
canopy rag auto-index start   # resume automatic indexing (default)
canopy rag auto-index stop    # pause without losing the queue
canopy rag report             # detailed per-file indexing report
```

## Exclusions

Create a `.canopy/ragignore` file with regex patterns to exclude files
and directories from indexing.

## Searching

Agents call the `rag_search` MCP tool (rate-limited to 10 calls/minute
per agent with a sliding window). From the TUI, the **RAG transfer
modal** sends search results to any agent as injected context.

RAG roots and embedding settings are configured during `canopy setup`.
