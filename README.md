# collab-editor

A real-time collaborative text editor built on a custom sequence CRDT (Rust + TypeScript). Several people can edit one document at once, work offline, and merge cleanly when they reconnect, without a central server deciding the order of edits.

```
browser (TypeScript)                      server (Rust, std only)
┌──────────────────────────┐   WebSocket  ┌──────────────────────────────┐
│ textarea ⇄ Editor        │◄────────────►│ room per document            │
│ Doc (Fugue CRDT)         │  JSON ops    │  Doc replica (same CRDT)     │
│ UndoManager (own edits)  │              │  relay + presence            │
│ IndexedDB (offline)      │              │  append-only op log          │
│ BroadcastChannel (tabs)  │              │  (group commit, crash-safe)  │
└──────────────────────────┘              └──────────────────────────────┘
```

## Features

- **Custom sequence CRDT.** The merge rule is Fugue (list form, equivalent to YjsMod), so concurrent typing never interleaves, whether people type forwards or backwards. It is implemented twice, in Rust and TypeScript, and the two are checked against each other.
- **Fast storage.** Rust uses blocks + Fenwick trees with an id index, and is about 160× faster than a naive `Vec` at 50K ops. TypeScript uses square-root decomposition.
- **Offline-first, installable.** A service worker lets the app open with no network, and it installs as an app (PWA). Edits persist to IndexedDB and are shared between tabs over BroadcastChannel. On reconnect, a version-vector handshake syncs exactly the missing ops in both directions, with no acks needed.
- **Live cursors and presence.** Cursors are anchored to character ids, not offsets, so they stay on the right character while others edit.
- **Undo/redo of only your own edits.** Undo issues new inverse ops, so it never rewinds a collaborator's work. Undone deletes reappear exactly where they were.
- **Std-only Rust server** (no crates): RFC 6455 WebSockets, a JSON parser, rooms, bounded per-client outboxes that drop slow consumers, and a group-commit op log that survives torn writes.

## Run it

```bash
cd web && npm install && npm run build && cd ..   # compile the TypeScript client
cargo run --release --bin collab-server            # http://localhost:8080
```

Open `http://localhost:8080/?doc=demo` in two windows. Use **Go offline** to simulate losing the network, edit in both windows, then reconnect.

## Deploy (free)

`render.yaml` deploys the Docker image to Render's free tier: push to GitHub, then on render.com choose **New → Blueprint** and pick the repo. The free instance sleeps after 15 idle minutes and has no persistent disk; documents survive restarts because every client keeps an offline copy and re-uploads it on reconnect. `fly.toml` is the paid alternative with a persistent volume.

## Results

All numbers come from a 2-core Xeon VM where the server and every load-generator client share the machine. Full tables are in [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

| | |
|---|---|
| Convergence fuzzing | 10,000 seeds, 2–5 replicas, partitions + reordering + duplicates: 100% converge |
| Cross-language | Rust and TypeScript produce identical text on 400 cases (both directions, incl. undo/redo, emoji) |
| Local editing, 260K-op trace | 208 ms (1.25M ops/s) |
| Block storage vs naive Vec, 50K ops | 35 ms vs 5,657 ms (~160×) |
| Open a 260K-op document (remote replay) | 276 ms |
| 20 clients, 10 edits/s each | p50 0.45 ms, p99 1.1 ms |
| 100 clients, 10 edits/s each | 90K deliveries/s, p50 ~5–7 ms, p99 ~100 ms, server at 77% of one core |
| fsync on every write: per-write → group commit | p50 3.2 s → 69 ms |

## Tests

```bash
scripts/test-all.sh        # everything below, ~10 s
scripts/load.sh 100 100 100   # 100 clients x 100 edits, with server CPU accounting
cargo run --release --example bench -p crdt-core
```

| Test | What it proves |
|---|---|
| `crdt-core/tests/fuzz_convergence.rs` | Convergence under partitions; block storage == naive oracle at every step; patches == text; no interleaving |
| `collab-server/tests/server_integration.rs` | Live sync, offline reconnect, restart + torn-write recovery, presence, malformed input |
| `web/test/crosslang.mjs` + `trace` bin | TypeScript and Rust CRDTs agree byte-for-byte |
| `web/test/editor.mjs` | Textarea diffing (selections, emoji, backspace) and caret preservation, via a DOM shim |
| `web/test/e2e.mjs` | Real TS clients + server: one client offline mid-session, late joiner, undo/redo |
| `web/test/browser.mjs` | Two Chromium "devices": live sync, cursors, both offline, reload with no network, merge, undo, layout |
| `loadtest` bin | N clients, latency percentiles, final byte-identical check against the server |

Mutation checks were run on purpose: flipping the tie-break, disabling the scan state and breaking the Fenwick update each make the suite fail.

## Layout

```
crates/crdt-core/      CRDT: replica.rs (merge rule), storage.rs (Vec + block/Fenwick), bench
crates/collab-server/  server.rs, ws.rs, json.rs, protocol.rs, sha1.rs; bins: loadtest, trace
web/src/               crdt.ts, sync.ts, editor.ts, undo.ts, storage.ts, protocol.ts, main.ts
web/test/              crosslang, editor, e2e
docs/                  DESIGN.md, BENCHMARKS.md
```

## Known limitations

- **Tombstones are never garbage-collected.** Safe GC needs causal stability (every replica has seen the delete), and an offline client can block that forever. See DESIGN.md.
- **The version vector grows by one entry per editing session,** because each page load is a new replica. Yjs has the same trade-off.
- **No authentication.** Anyone with a document name can edit it, and the server trusts clients' ops.
- **One element per character,** about 214 bytes per item including the op log. Run-length encoding would cut this sharply.
- **Real-browser tests run in headless Chromium only.** Safari and Firefox are untested. Docker/Fly files have not yet been run end to end.

## License

MIT
