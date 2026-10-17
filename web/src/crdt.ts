// Sequence CRDT, TypeScript port of crates/crdt-core.
//
// Same merge rule (Fugue list form / YjsMod) and same op format, so a document
// edited in the browser converges with the Rust server byte-for-byte. This is
// checked by test/crosslang.mjs in both directions.
//
// Storage uses square-root decomposition: items live in blocks of <= 256, each
// block knows its visible count, and each item points at its block. Position
// lookups cost O(#blocks + block size) instead of O(n).

export type ReplicaId = number;

export interface OpId {
  counter: number;
  replica: ReplicaId;
}

export type OpKind =
  | { type: "ins"; left: OpId | null; right: OpId | null; ch: string }
  | { type: "del"; target: OpId };

export interface Op {
  id: OpId;
  seq: number;
  kind: OpKind;
}

export type VersionVector = Map<ReplicaId, number>;

export type Patch = { type: "ins"; pos: number; ch: string } | { type: "del"; pos: number };

export type ApplyResult = "applied" | "buffered" | "duplicate";

interface Block {
  items: Item[];
  vis: number;
}

interface Item {
  id: OpId;
  left: OpId | null;
  right: OpId | null;
  ch: string;
  deleted: boolean;
  block: Block;
}

const MAX_BLOCK = 256;

export const idKey = (id: OpId): string => `${id.counter}:${id.replica}`;

export const sameId = (a: OpId | null, b: OpId | null): boolean =>
  a === b || (a !== null && b !== null && a.counter === b.counter && a.replica === b.replica);

/** Random 52-bit replica id (exact as a JSON number). */
export function randomReplica(): ReplicaId {
  const r = new Uint32Array(2);
  globalThis.crypto.getRandomValues(r);
  return (r[0] & 0xfffff) * 2 ** 32 + r[1] + 1;
}

export class Doc {
  readonly replica: ReplicaId;
  readonly vv: VersionVector = new Map();
  /** Every applied op in causal order; source for opsSince. */
  readonly log: Op[] = [];
  private lamport = 0;
  private blocks: Block[] = [];
  private index = new Map<string, Item>();
  private pending: Op[] = [];
  private size = 0;
  private visible = 0;

  constructor(replica: ReplicaId) {
    this.replica = replica;
  }

  // ------------------------------------------------------------ queries

  text(): string {
    const parts: string[] = [];
    for (const b of this.blocks) for (const it of b.items) if (!it.deleted) parts.push(it.ch);
    return parts.join("");
  }

  /** Visible characters (code points). */
  get length(): number {
    return this.visible;
  }

  get rawLength(): number {
    return this.size;
  }

  get pendingCount(): number {
    return this.pending.length;
  }

  charOf(id: OpId): string | undefined {
    return this.index.get(idKey(id))?.ch;
  }

  isVisible(id: OpId): boolean {
    const it = this.index.get(idKey(id));
    return it !== undefined && !it.deleted;
  }

  // ------------------------------------------------------------ local edits

  /** Insert one code point at visible position `pos`. */
  insert(pos: number, ch: string): Op {
    if (pos < 0 || pos > this.visible) throw new RangeError(`insert at ${pos}`);
    const raw = pos === 0 ? 0 : this.rawOfVisible(pos - 1) + 1;
    const left = pos === 0 ? null : this.itemAt(raw - 1).id;
    const right = raw < this.size ? this.itemAt(raw).id : null;
    return this.local({ type: "ins", left, right, ch });
  }

  /** Delete the code point at visible position `pos`. */
  delete(pos: number): Op {
    if (pos < 0 || pos >= this.visible) throw new RangeError(`delete at ${pos}`);
    const target = this.itemAt(this.rawOfVisible(pos)).id;
    return this.local({ type: "del", target });
  }

  /** Delete a specific character by id (used by undo). Null if already gone. */
  deleteById(id: OpId): { op: Op; patch: Patch } | null {
    if (!this.isVisible(id)) return null;
    const patches: Patch[] = [];
    const op = this.local({ type: "del", target: id }, patches);
    return { op, patch: patches[0] };
  }

  /** Insert `ch` directly after item `id`, even if that item is deleted (used by undo). */
  insertAfterId(id: OpId, ch: string): { op: Op; patch: Patch } {
    const raw = this.rawPos(this.mustGet(id)) + 1;
    const right = raw < this.size ? this.itemAt(raw).id : null;
    const patches: Patch[] = [];
    const op = this.local({ type: "ins", left: id, right, ch }, patches);
    return { op, patch: patches[0] };
  }

  private local(kind: OpKind, patches: Patch[] = []): Op {
    this.lamport += 1;
    const op: Op = {
      id: { counter: this.lamport, replica: this.replica },
      seq: this.seen(this.replica) + 1,
      kind,
    };
    this.applyCollect(op, patches);
    return op;
  }

  // ------------------------------------------------------------ remote ops

  apply(op: Op): ApplyResult {
    return this.applyCollect(op, []);
  }

  /** Apply any op (late, reordered or duplicate is fine); append visible changes to `patches`. */
  applyCollect(op: Op, patches: Patch[]): ApplyResult {
    if (op.seq <= this.seen(op.id.replica)) return "duplicate";
    if (!this.isReady(op)) {
      if (!this.pending.some((p) => sameId(p.id, op.id))) this.pending.push(op);
      return "buffered";
    }
    this.integrate(op, patches);
    this.drainPending(patches);
    return "applied";
  }

  opsSince(vv: VersionVector): Op[] {
    return this.log.filter((op) => op.seq > (vv.get(op.id.replica) ?? 0));
  }

  // ------------------------------------------------------------ cursors

  /** Anchor for a caret at visible `pos`: the id of the character before it. */
  anchorAt(pos: number): OpId | null {
    if (pos <= 0 || this.visible === 0) return null;
    return this.itemAt(this.rawOfVisible(Math.min(pos, this.visible) - 1)).id;
  }

  /** Visible caret position for an anchor; undefined if the anchor isn't known yet. */
  resolveAnchor(anchor: OpId | null): number | undefined {
    if (anchor === null) return 0;
    const it = this.index.get(idKey(anchor));
    if (!it) return undefined;
    return this.visibleBefore(this.rawPos(it)) + (it.deleted ? 0 : 1);
  }

  // ------------------------------------------------------------ internals

  private seen(r: ReplicaId): number {
    return this.vv.get(r) ?? 0;
  }

  private has(id: OpId | null): boolean {
    return id === null || this.index.has(idKey(id));
  }

  private isReady(op: Op): boolean {
    if (op.seq !== this.seen(op.id.replica) + 1) return false;
    return op.kind.type === "ins"
      ? this.has(op.kind.left) && this.has(op.kind.right)
      : this.has(op.kind.target);
  }

  private mustGet(id: OpId): Item {
    const it = this.index.get(idKey(id));
    if (!it) throw new Error(`unknown id ${idKey(id)}`);
    return it;
  }

  private posOf(id: OpId | null, none: number): number {
    return id === null ? none : this.rawPos(this.mustGet(id));
  }

  private integrate(op: Op, patches: Patch[]): void {
    this.lamport = Math.max(this.lamport, op.id.counter);
    const k = op.kind;
    if (k.type === "ins") {
      const len = this.size;
      const left = this.posOf(k.left, -1);
      const right = this.posOf(k.right, len);
      let dest = left + 1;
      let scanning = false;
      for (let i = left + 1; ; i++) {
        if (!scanning) dest = i;
        if (i === len || i === right) break;
        const other = this.itemAt(i);
        const oLeft = this.posOf(other.left, -1);
        const oRight = this.posOf(other.right, len);
        if (oLeft < left) break;
        if (oLeft === left) {
          if (oRight < right) {
            scanning = true;
          } else if (oRight === right) {
            if (op.id.replica < other.id.replica) break;
            scanning = false;
          } else {
            scanning = false;
          }
        }
      }
      this.insertAt(dest, { id: op.id, left: k.left, right: k.right, ch: k.ch, deleted: false });
      patches.push({ type: "ins", pos: this.visibleBefore(dest), ch: k.ch });
    } else {
      const it = this.mustGet(k.target);
      if (!it.deleted) {
        const pos = this.visibleBefore(this.rawPos(it));
        it.deleted = true;
        it.block.vis -= 1;
        this.visible -= 1;
        patches.push({ type: "del", pos });
      }
    }
    this.vv.set(op.id.replica, op.seq);
    this.log.push(op);
  }

  private drainPending(patches: Patch[]): void {
    while (this.pending.length > 0) {
      this.pending = this.pending.filter((op) => op.seq > this.seen(op.id.replica));
      const i = this.pending.findIndex((op) => this.isReady(op));
      if (i < 0) break;
      const [op] = this.pending.splice(i, 1);
      this.integrate(op, patches);
    }
  }

  // ------------------------------------------------------------ block storage

  private rawPos(it: Item): number {
    let raw = 0;
    for (const b of this.blocks) {
      if (b === it.block) return raw + b.items.indexOf(it);
      raw += b.items.length;
    }
    throw new Error("item not in any block");
  }

  private locate(raw: number): [Block, number] {
    for (const b of this.blocks) {
      if (raw < b.items.length) return [b, raw];
      raw -= b.items.length;
    }
    throw new RangeError("raw index out of range");
  }

  private itemAt(raw: number): Item {
    const [b, off] = this.locate(raw);
    return b.items[off];
  }

  private rawOfVisible(vis: number): number {
    let raw = 0;
    for (const b of this.blocks) {
      if (vis < b.vis) {
        for (let off = 0; off < b.items.length; off++) {
          if (!b.items[off].deleted) {
            if (vis === 0) return raw + off;
            vis--;
          }
        }
      }
      vis -= b.vis;
      raw += b.items.length;
    }
    throw new RangeError("visible index out of range");
  }

  private visibleBefore(raw: number): number {
    let vis = 0;
    for (const b of this.blocks) {
      if (raw >= b.items.length) {
        vis += b.vis;
        raw -= b.items.length;
        continue;
      }
      for (let off = 0; off < raw; off++) if (!b.items[off].deleted) vis++;
      return vis;
    }
    return vis;
  }

  private insertAt(raw: number, fields: Omit<Item, "block">): void {
    let block: Block;
    let off: number;
    if (this.blocks.length === 0) {
      block = { items: [], vis: 0 };
      this.blocks.push(block);
      off = 0;
    } else if (raw >= this.size) {
      block = this.blocks[this.blocks.length - 1];
      off = block.items.length;
    } else {
      [block, off] = this.locate(raw);
    }
    const item: Item = { ...fields, block };
    block.items.splice(off, 0, item);
    if (!item.deleted) {
      block.vis += 1;
      this.visible += 1;
    }
    this.size += 1;
    this.index.set(idKey(item.id), item);

    if (block.items.length > MAX_BLOCK) {
      const tail = block.items.splice(block.items.length >> 1);
      const nb: Block = { items: tail, vis: 0 };
      for (const t of tail) {
        t.block = nb;
        if (!t.deleted) nb.vis++;
      }
      block.vis -= nb.vis;
      this.blocks.splice(this.blocks.indexOf(block) + 1, 0, nb);
    }
  }
}
