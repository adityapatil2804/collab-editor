# Design

## 1. Data model

A document is a sequence of **items**, one per character (Unicode code point).
Items are never removed; a delete sets `deleted = true` (a **tombstone**).

```
Item { id: OpId, origin_left: Option<OpId>, origin_right: Option<OpId>, ch, deleted }
OpId { counter: u64 (Lamport clock), replica: u64 }
Op   { id, seq (per-replica 1,2,3…), kind: Insert{origin_left, origin_right, ch} | Delete{target} }
```

- `id` is globally unique and permanent, so ops refer to characters by identity,
  never by position.
- `origin_left` / `origin_right` are the neighbours the character was typed
  between (tombstones included), as seen by its author.
- `counter` is a Lamport clock: on applying any op, `lamport = max(lamport, op.counter)`,
  and every local op uses `lamport + 1`. So an op's counter is always greater
  than the counter of anything it depends on.
- `seq` numbers each replica's ops 1, 2, 3… It drives deduplication and sync.

## 2. Merge rule (Fugue, list form)

To integrate an insert, find the raw positions of its two origins, `left` and
`right`, and scan the items strictly between them:

```
dest = left + 1; scanning = false
for i in left+1 .. :
    if !scanning: dest = i
    if i == len or i == right: break
    o = item[i]; oL = pos(o.origin_left); oR = pos(o.origin_right)
    if oL < left: break                        # o belongs to an enclosing insert; we go first
    if oL == left:
        if oR < right:  scanning = true        # maybe after o, depends on what follows
        elif oR == right:                      # true sibling: same two origins
            if me.replica < o.replica: break   # deterministic tie-break
            scanning = false
        else: scanning = false
    # oL > left: o is inside a sibling's subtree; skip it
insert at dest
```

Why two origins? RGA uses only `origin_left`. When two users concurrently type
*backwards* (each new character inserted before the previous one) at the same
spot, RGA interleaves their characters ("axbycz"). Using the right origin as
well means a run typed by one user stays contiguous in both directions. This is
the Fugue paper's "maximal non-interleaving" property, and the list
formulation is equivalent to Joseph Gentle's YjsMod. Tests
`concurrent_backward_typing_does_not_interleave` and `no_interleaving_random_runs`
check it.

Convergence only needs every replica to (a) integrate ops in a causally
consistent order and (b) make the same decision for the same pair of items. The
scan depends only on the items present and their origins, never on arrival
order, and ties break on replica id, so all replicas agree.

## 3. Causal delivery, deduplication

An op is **ready** when:
- `op.seq == vv[op.replica] + 1` (the next op from its author), and
- every item it references (`origin_left`, `origin_right`, or delete `target`) exists.

Ops that aren't ready go to a `pending` buffer and are retried after each
successful apply. Ops with `seq <= vv[replica]` are duplicates and are ignored.
So the transport can reorder, duplicate or delay messages freely.

## 4. Sync protocol (version vectors)

`vv: replica -> highest seq applied` summarizes everything a replica has seen.
`ops_since(vv)` returns exactly the ops the other side lacks, because ops from
one replica are applied in seq order.

```
client → server  hello   { doc, replica, vv }
server → client  welcome { ops_since(client vv), server vv, peers }
client → server  ops     ops_since(server vv)
```

There are no acks and no retry queue. If the connection drops while ops are in
flight, the next handshake recomputes what is missing from version vectors.
This is the same anti-entropy pattern as Dynamo-style replication.

## 5. Storage

The merge rule needs: id → raw position, raw position → item, visible position
→ raw position, insert at raw position, and visible count before a raw position.

- `VecStorage`: a plain `Vec<Item>`; everything O(n). Used as the test oracle.
- `BlockStorage` (Rust): items in an arena, ordered by blocks of ≤ 128 arena
  indices. Two Fenwick trees over blocks (item counts, visible counts) give
  O(log #blocks) "which block holds position k" and prefix sums. A
  `HashMap<OpId, arena index>` plus `block_of[]` gives id → block. In-block work
  is O(128). Splitting a block rebuilds the Fenwick trees in O(#blocks), which is
  amortized over 64 inserts.
- TypeScript uses the simpler square-root decomposition (blocks of ≤ 256, linear
  scan over blocks, each item points at its block object).

Differential fuzzing feeds identical op streams to both storages and requires
identical text at every step.

## 6. Server

- One OS thread per connection plus one writer thread per connection.
  Thread-per-connection is simple and fine to a few thousand sockets; an
  async runtime (tokio) is the next step past that.
- One `Mutex<Room>` per document. Apply, persist-enqueue and broadcast all
  happen under it. The hello handler registers the client **and** computes
  `ops_since` under the same lock, so no op can slip between "snapshot sent" and
  "subscribed to broadcasts".
- Each client has a bounded outbox (4,096 frames). If it fills, the client is
  disconnected rather than letting server memory grow; it resyncs on reconnect.
  This is backpressure by eviction.
- The writer thread coalesces queued frames into one `write` (up to 64 KB).
- Encoded broadcast frames are `Arc`-shared: JSON-encoded once per op batch,
  not once per recipient.
- Limits: 8 MB message, 50K ops per message, 10K pending ops per document,
  4,000 connections, document names restricted to `[A-Za-z0-9_-]{1,64}`, and
  static files never serve `..` or dotfiles.

### Persistence

An append-only op log per document, one JSON line per op. On startup it is
replayed through `apply`, so order on disk doesn't matter. A crash mid-write
can leave a torn last line, which is detected and truncated before appending.
Writes go through a per-document **group commit** thread: it takes everything
queued, does one `write` + optional `fsync`, and repeats. The room lock never
waits on the disk. A relayed op may therefore not be durable on the server yet;
if the server crashes, the op still lives on client replicas and is re-uploaded
by the handshake.

## 7. Client

- **Editor binding.** On each `input` event, diff the previous and new value in
  code points, with the common suffix limited so it never extends past the
  caret. The difference becomes delete ops and then insert ops. Remote patches
  shift the local selection: an insert or delete strictly before the caret
  moves it.
- **Unicode.** CRDT positions are code points; textarea offsets are UTF-16. The
  editor converts at the boundary, so emoji are one item, never two surrogate
  halves. The server rejects inserts that are not exactly one code point.
- **Cursors.** A cursor is stored as the id of the character to its left. Ids
  never move, so remote cursors stay attached while text shifts around them.
- **Undo.** Each user has their own stack. Undoing an insert deletes those ids;
  undoing a delete inserts a copy right after each tombstone (`insertAfterId`).
  Edits within 700 ms of the same kind form one group. Never rewinds others'
  edits.
- **Offline.** Ops are saved to IndexedDB with merge-on-write (union by op id in
  one transaction), so two tabs never clobber each other. Each page load is a new
  replica id, because two tabs sharing one id would reuse seq numbers.
  BroadcastChannel relays ops between tabs even with no network.

## 8. Testing strategy

1. Unit tests for each piece (SHA-1/Base64 vectors, the RFC 6455 handshake
   example, frame encodings, JSON edge cases, Fenwick search).
2. Property/fuzz tests with deterministic seeds: convergence, the
   oracle-vs-optimized storage comparison, patch consistency, non-interleaving,
   and a single replica against a `String` model.
3. Cross-language traces in both directions, including emoji and undo/redo ops.
4. Integration tests against a live server, including crash recovery.
5. Load tests with latency percentiles and a final byte-equality check.
6. Mutation checks: deliberately broken variants must fail the suite.

## 9. Not done / next steps

- **Tombstone GC.** A delete is *causally stable* once every replica has seen
  it (`min over known replicas' vv >= delete's seq`). Stable tombstones whose
  neighbours are also stable can be dropped, but only if no future op can name
  them as an origin. Offline clients block stability indefinitely. The practical
  fix is a compaction epoch: the server snapshots, and clients that are too old
  must reload from the snapshot.
- **Run-length encoding.** Store a run of consecutive chars from one replica as
  one item with a length (Yjs, Diamond Types). This is the big memory and speed
  win.
- **Snapshots.** Replaying the full log on load is O(ops); periodic snapshots
  would bound startup time.
- **Horizontal scaling.** Rooms are independent, so shard by document name with
  sticky routing. One document's throughput is bounded by its single lock.
- **Auth.** Sign ops or authenticate sockets, and check that op authors match
  the authenticated replica.
