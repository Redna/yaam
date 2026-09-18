# Memory footprint — why YAAM is heavy and how to make it light

Status: **memory layer disabled on this host** (`YAAM_DISABLED=true` in the PI WEB
sessiond drop-in; the daemon binary is also non-executable as a belt-and-braces
brake). Re-enable only after the acceptance criteria at the bottom are met.

## What happened (2026-09-18)

The host (7.7 GB RAM, **no swap**) became unresponsive with two to three
`yaam-engine` daemons resident at 1.2–2.2 GB each. The box rebooted.

## Measured footprint per daemon

| situation | RSS |
| --- | --- |
| idle, small graph | **289 MB** |
| 4 s after start with an **empty** `events.jsonl` | **612 MB** |
| after replaying this workspace's 229 MB / 194k-event local log | **830 MB in seconds**, earlier 2.2 GB |

## Root causes

**The floor is the model and the caches, not the data.** A daemon holds the ONNX
runtime plus the `gte-small` model (134 MB on disk), the embedding cache, the ANN
index, the BM25 index, the tokenizer and the graph. That is ~600 MB *per daemon*
before a single user note exists. Duplicates multiply the floor, not just the data.

**The graph is RAM-resident by design.** `CONCURRENCY_SPEC.md` states it plainly:
"Graph relationships and nodes reside exclusively in RAM." Every node keeps its
embedding (384 floats ≈ 1.5 KB plus parse/JSON overhead), and nodes are never
evicted. The workspace graph had grown to **1,809 Section nodes** — largely
because `.claude/plugins/...` documentation was indexed as Sections.

**The local log only grows.** `events.jsonl` is append-only and never compacted
locally; it reached **229 MB / 194,126 lines**, so every daemon start replays all
of it and rebuilds the graph + BM25 + ANN from scratch. Replay is the spike.

**Duplicates came from bugs, not need.** One daemon per *workspace* is deliberate
(port file in the workspace, no graph mixing, one `rootUri` for
`typescript-language-server`); the spec also lists a per-*host* daemon, which was
never implemented. Duplicates for the *same* workspace had three causes, all fixed
in `7371f93` / `148fc80`:

1. the client keyed the port file on `process.cwd()`, so under a shared host
   (PI WEB's session daemon runs in `$HOME`) every workspace collided on
   `$HOME/.yaam/daemon.port` and the workspace's own file was never used;
2. a single failed 500 ms probe could delete a live daemon's port file;
3. a daemon never noticed its port file going missing, so it stayed alive but
   unreachable and clients spawned replacements.

`MEMORY_GROWTH_HANDOFF.md` documents the same failure family (41 MB input →
8.7 GB RSS) and the shipped fix capped chunk counts; vectors and the absence of
eviction still dominate.

## Options, in order of return on effort

1. **One daemon per host, namespaced by workspace.** Removes N × (model + caches +
   indexes). Workspace isolation becomes a namespace rather than a process; the
   LSP still needs one `rootUri`, so keep one language-server child per workspace
   inside the single daemon.
2. **Index code on demand, and never index junk.** Layer 0 (code topology) is
   regenerable from the checkout and does not need to be resident or durable;
   Layer 1 (workspaces + scratchpad notes) is tiny and is the part that must
   survive across sessions. Keep `SKIP_DIRS` honest (`.claude`, `.pi-web`,
   `session-logs*`, `screenshots`, caches — done in `cbb771e`) and consider
   excluding docs from eager indexing entirely.
3. **Lazy embeddings.** Embed notes and query results, not every reconciled node.
   The ANN index can be built on demand for the subset that is actually searchable.
4. **Idle eviction and a hard ceiling.** Drop ANN vectors when idle (they rebuild),
   and refuse to grow past a configurable RSS limit instead of being OOM-killed.
5. **Bound the local log.** ~~Cap or compact `events.jsonl`~~ — **done
   (2026-09-18): reconcile-derived events are no longer persisted by default**
   (`YAAM_PERSIST_RECONCILE=false`). Layer 0 (code topology) still fills the
   in-memory graph and stays searchable; only Layer 1 (notes, workspaces) and
   explicit mutations are written. Measured: a two-file reconcile produces the
   same graph and search hits with **0** log lines, versus **5** with
   `YAAM_PERSIST_RECONCILE=true`. Set the variable to `true` to restore the old
   behaviour. Still to do here: a cap/compaction policy for long-lived Layer 1
   logs. See also the reconcile-bloat issue (`~/yaam-issue-delta-bloat.md`).
6. **Swap on the host.** Not a YAAM fix, but it converts a lock-up into a slow
   patch. 4 GB is enough for this stack.

## Acceptance criteria before re-enabling

- **AC-1** A daemon's idle RSS is ≤ 300 MB and its post-reconcile RSS is ≤ 600 MB
  on this workspace, measured and recorded.
- **AC-2** Exactly **one** daemon serves a workspace regardless of how many
  sessions, subsessions or reloads are active; `pgrep -x yaam-engine | wc -l` is 1.
- **AC-3** The local `events.jsonl` stays bounded (a documented cap or compaction)
  and a cold start replays it in under ~2 s.
- **AC-4** A note written through `yaam_workspace_append_note` is durable and
  retrievable in the same session, and the tool reports the real outcome (it used
  to say "Note added" while the write silently failed).
- **AC-5** Interactive calls (`yaam_search`) answer within a stated budget while a
  full reconcile is running (separate connection per reconciler — done in
  `148fc80` — plus prioritisation if needed).
- **AC-6** The host has swap, or the daemon honours an RSS ceiling; a memory spike
  degrades instead of taking the machine down.

## Re-enable checklist

```sh
# 1. swap (needs sudo)
sudo fallocate -l 4G /swapfile && sudo chmod 600 /swapfile && sudo mkswap /swapfile && sudo swapon /swapfile

# 2. un-brake the daemon binary
chmod +x ~/.pi/agent/git/github.com/Redna/yaam/src-rust/target/release/yaam-engine ~/.yaam-cache/yaam-engine

# 3. remove the YAAM_DISABLED line from the sessiond drop-in, keep auto-compact off
$EDITOR ~/.config/systemd/user/pi-web-sessiond.service.d/yaam.conf
systemctl --user daemon-reload

# 4. restart the session daemon when nothing important is running, then watch:
watch -n 2 'pgrep -x yaam-engine | wc -l; ps -o rss= -p $(pgrep -x yaam-engine | head -1)'
```

## References

- `MEMORY_GROWTH_HANDOFF.md` — the daemon-side growth investigation
- `CONCURRENCY_SPEC.md` — per-workspace vs per-host daemon, shared state
- `docs/DISTRIBUTED_AGENTS.md` — the Git-based memory pipeline (`YAAM_DISABLE_AUTO_COMPACT`)
- Fixes landed: `7371f93` (workspace-scoped state, port probes, heartbeat),
  `148fc80` (separate reconciler connection, awaited writes), `cbb771e`
  (kill-switch, `SKIP_DIRS`, port-file ENOENT)
