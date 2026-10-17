# Benchmarks

Machine: 2-core Intel Xeon @ 2.10 GHz VM, Linux, Rust 1.97 (release, debuginfo on).
Load tests run the server and all simulated clients on the same 2 cores, so
latency includes contention from the load generator itself. Expect run-to-run
variance of roughly 2× in tail latency.

## CRDT core (`cargo run --release --example bench -p crdt-core`)

Synthetic single-author traces with cursor locality (mostly typing forward, 12%
backspace, 3% cursor jumps). The large trace matches the size of the standard
automerge-perf editing trace (~260K ops); that trace itself was not available
offline.

| Benchmark | Ops | Time (ms) | Ops/sec |
|---|---|---|---|
| Local edits, naive Vec storage, 20K | 20,000 | 486.8 | 41,085 |
| Local edits, block storage, 20K | 20,000 | 19.3 | 1,037,957 |
| Local edits, naive Vec storage, 50K | 50,000 | 5,656.7 | 8,839 |
| Local edits, block storage, 50K | 50,000 | 35.1 | 1,426,227 |
| Local edits, block storage, 260K | 260,000 | 208.1 | 1,249,232 |
| Remote replay (open doc), 260K | 260,000 | 275.6 | 943,353 |
| Remote replay, reordered in 64-op windows, 50K | 50,000 | 121.8 | 410,512 |
| Offline merge, 2 × 25K divergent edits | 50,000 | 95.7 | 522,315 |

The naive storage is O(n) per op, so its cost grows quadratically: 2.5× more
ops took 11.6× longer. Block storage stays roughly flat per op.

| Memory (260K-op trace) | Value |
|---|---|
| Visible chars | 197,806 |
| Items incl. tombstones | 228,903 |
| Live heap (doc incl. op log) | 49.0 MB |
| Bytes per item | 214 |

Most of that is the op log (kept for sync) plus one 40-byte item and one
hash-map entry per character. Run-length encoding consecutive characters from
one replica is the standard fix.

## Server load (`scripts/load.sh CLIENTS EDITS INTERVAL [flags]`)

Each client sends one op per message (keystroke granularity) every
`interval/2 .. interval*1.5` ms. Latency is measured from just before send to
when another client has applied the op.

| Scenario | Deliveries/s | p50 | p99 | Server CPU | All replicas + server identical |
|---|---|---|---|---|---|
| 20 clients, 100 edits, ~100 ms, memory | 3.6K | 0.45 ms | 1.1 ms | 8% of 1 core | yes |
| 100 clients, 100 edits, ~100 ms, memory | 90K | 5.3 ms | 99 ms | 77% of 1 core | yes |
| 100 clients, op log, no fsync | 91K | 6.9 ms | 99 ms | 78% of 1 core | yes |
| 100 clients, op log, fsync each write (inside room lock), v1 | 91K | 3,255 ms | 9,633 ms | — | yes |
| 100 clients, op log, fsync with group commit, v2 | 91K | 69 ms | 603 ms | 74% of 1 core | yes |
| 300 clients, 50 edits, ~200 ms, memory | 379K | 15 s | 34 s | 60% of 1 core | yes |

How to read the last two rows:

- **Group commit.** v1 called `fsync` while holding the document lock, so every
  op waited for the disk and all other ops queued behind it. v2 hands encoded
  ops to a per-document writer thread that writes and fsyncs whatever
  accumulated since the previous flush, in one go. The relay no longer waits on
  disk at all, which cut p50 latency by about 47×.
- **300 clients.** The server process used only 60% of one core, so the
  bottleneck is the load generator: 300 full client replicas on the same
  2 cores, each parsing and applying all 15,000 ops. These numbers say nothing
  about the server's real limit; that would need clients on separate machines.
