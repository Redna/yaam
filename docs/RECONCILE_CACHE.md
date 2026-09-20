# Reconcile cache — making the code graph cheap to rebuild and correct across sessions

Status: **design** (2026-09-20). Legs 1–3 below.

## Problem

Layer 0 (code topology) is regenerable and — since the #221 arc — **not persisted**,
because the *event-delta* mechanism was 93–99% whole-graph `LINK_NODES` re-emission.
That decision threw out two different things together: the event log (which was the
bloat) and the derived graph (cheap to keep as a compact snapshot).

The result today: a fresh session replays an empty log, re-walks every source file with
tree-sitter, and re-resolves every reference through a **cold tsserver** — paying the
cold-start race on every startup, for files that did not change. A ~142-file workspace
therefore starts with an empty-ish graph *and* the most expensive possible path to fill it.

## Design: one content-addressed cache

```
<workspace>/.yaam/reconcile-cache.json        # gitignored, compact, current-state only
{
  "version": 1,
  "parser": "typescript-language-server@5.3.0",   # bump => whole cache invalid
  "files": {
    "<rel-path>": {
      "hash": "sha256:<content>",
      "deps": { "<rel-path>": "sha256:<content>" },   # resolved IMPORT targets
      "decls": [ /* tree-sitter entities */ ],
      "refs":  [ /* LSP-resolved edges: {name,type,line,col,target,targetLine,targetCol} */ ],
      "signatures": { "<entity-id>": "<hover signature>" },
      "diagnostics": { "errors": N, "warnings": N },
      "implements": [ /* {entity, interface} */ ]
    }
  }
}
```

**Validity** — an entry is usable iff *all* hold:

1. `hash(file content)` matches the stored hash;
2. every path in `deps` still hashes to its stored value (a rename in `a.ts` changes the
   answer for `b.ts`, so the dependency closure must be part of the key);
3. `version` and `parser` match the running engine's.

**On reconcile:** hash → **hit**: replay `decls` + `refs` as events, zero tree-sitter,
zero LSP. **Miss:** parse/resolve that file only, then write the entry back. Deleted files
prune their entries; `deps` pointing at pruned files invalidate.

**Switches:** `YAAM_RECONCILE_CACHE=off` (bypass + rebuild), `YAAM_RECONCILE_CACHE_DIR`
(override location for tests). Writes are best-effort and must never fail a reconcile.

**Why this is not the old bloat:** one compact file of *current state* keyed by content
hash — not an append-only stream. An unchanged file costs one hash, not one resolution.

## Legs

1. **The cache** (this document): `decls` + `refs` only. Correctness first: hit/miss,
   dependency invalidation, version invalidation, prune, and an end-to-end check that a
   restart with no changes performs **zero LSP work**.
2. **Richer facts on the same key:** `signatures` (`textDocument/hover` per entity),
   `diagnostics` (`textDocument/diagnostic` per file → counts on the `File` node),
   `implements` (`textDocument/implementation` → `IMPLEMENTS` edges). Cached by the same
   hash, so they add no per-session cost.
3. **Coverage sweep:** a full reconcile must visit **every** workspace source file, not
   just touched ones — "who calls X" is only truthful if every caller's references were
   resolved. Cheap with the cache: unchanged files are hits.

## Constraints

- The LSP stays a **cache, never a dependency**: with `YAAM_LSP_RESOLVER=false` the graph
  degrades to tree-sitter structure rather than breaking.
- Layer 1 (notes/workspaces) remains the only durable knowledge; this cache is
  regenerable and may be deleted at any time without losing a note or a decision.
- Each capability is batched **per file per reconcile**, never per reference, so adding
  features multiplies round-trips only for changed files.
