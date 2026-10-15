# 30. Memories stored locally with embeddings in SQLite

- Status: Accepted
- Date: 2026-10-10

## Context

`Memory` was a client for mcp-memory-service's REST API. The service has
to run separately, and the tool fails when it is down.

## Decision

- `Memory` keeps the same tool input (search, store, list, get, update,
  delete, health) and stores memories itself, in `memory.db` in the config
  dir. That file is separate from `sessions.db`, so the two schemas don't
  share versioning. Old memories in mcp-memory-service are not imported.
- Content is embedded locally with fastembed (all-MiniLM-L6-v2, 384
  dimensions, the same model mcp-memory-service uses). The model is
  downloaded on first use to `$XDG_CACHE_HOME/rho/fastembed`
  (`~/.cache/rho/fastembed`) and stays loaded. `index::embed` holds one
  instance per process, shared with `History` (ADR 0032).
- sqlite-vec (the `sqlite-vec` crate, compiled in and registered with
  `sqlite3_auto_extension`) ranks memories with `vec_distance_cosine` over
  an ordinary table with a BLOB column. It does a full scan, which is
  enough for thousands of memories.
- The hash is the SHA-256 of the content. Store refuses the same content,
  and content with a cosine similarity of 0.95 or more, unless a
  `conversationId` is given (it goes into the metadata). `agentId` adds an
  `agent:<id>` tag. Update replaces content, tags and type and merges
  metadata (JSON merge patch). New content is embedded again and changes
  the hash; update refuses content another memory already has.
- Output is cut at `tools::MAX_OUTPUT`. There is no temp file with the full
  output any more.

## Consequences

- No separate service. The first memory call needs the network and waits
  for the model download. Later calls work offline.
- onnxruntime comes from nixpkgs: the flake's devShell sets
  `ORT_LIB_LOCATION` and `ORT_PREFER_DYNAMIC_LINK`, and `ort` links
  `libonnxruntime.so` (RUNPATH into the Nix store). fastembed's download
  feature is off, so builds outside the devShell fail to link. A release
  needs a Nix package for rho.
- The binary gets bigger: tokenizers and sqlite-vec's C code.
- The embedding runs in rho's process, outside the sandbox, like the rest
  of the extension.
