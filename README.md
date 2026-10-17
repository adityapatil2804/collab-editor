# collab-editor

A real-time collaborative text editor built on a custom sequence CRDT in Rust.

**Status:** weeks 1–2: CRDT engine and fuzz harness done. Sync server, editor UI and benchmarks come next.

## What works now (`crates/crdt-core`)

- **RGA sequence CRDT.** Every character has a unique `OpId` (Lamport counter + replica id). Concurrent inserts at the same spot are ordered by id, and deletes leave tombstones.
- **Unreliable delivery is safe.** Ops can arrive late, out of order or duplicated. Ops with missing dependencies are buffered, and duplicates are ignored (`ApplyResult::{Applied, Buffered, Duplicate}`).
- **Offline sync.** Version vectors (`replica → highest seq seen`) plus `ops_since(vv)` let two replicas exchange exactly the ops each is missing (`Doc::sync`).
- **Fuzz-tested convergence.** 2–5 replicas make random edits under network partitions, offline editing, reordered delivery and duplicate messages. After reconnecting, every replica must hold identical text, identical version vectors and no stuck ops.

## Run

```bash
cargo test --release                                             # 500 seeds
FUZZ_SEEDS=10000 cargo test --release --test fuzz_convergence    # 10K seeds
```

Current result: 10,000/10,000 seeds converge.

## Layout

```
crates/crdt-core/
  src/lib.rs                  # Doc, Op, OpId, apply/sync, RGA integration
  tests/fuzz_convergence.rs   # multi-replica partition fuzzer + String model check
```

## Known limitations of v0 (the roadmap)

| Limitation | Fix | Week |
|---|---|---|
| O(n) id lookup and position mapping | `HashMap<OpId, index>` or a B-tree/rope with subtree counts | 2 |
| One element per character | Run-length encode runs of consecutive chars from one replica | 2 |
| RGA interleaves concurrent *backward* typing | Implement Fugue and add an interleaving test | 2–3 |
| Tombstones never collected | Collect once a delete is causally stable (every known replica has seen it) | 5 |
| No persistence | Op log + snapshots (server: Postgres; client: IndexedDB) | 3, 5 |
| No benchmarks | Compare against `yrs` (Yjs in Rust) and `automerge` on the automerge-perf editing trace | 6 |

## Next: week 3

WebSocket sync server (Rust + `axum`/`tokio`): relay ops, persist the op log, and use the version-vector handshake on reconnect.

## License

MIT
