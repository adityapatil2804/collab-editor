//! A replica of a collaboratively edited text document.
//!
//! The merge rule is the list formulation of Fugue (equivalent to "YjsMod",
//! from Joseph Gentle's reference-crdts): each character remembers the two
//! neighbours it was typed between (`origin_left`, `origin_right`), and
//! concurrent inserts at the same spot are placed by a deterministic scan.
//! Unlike plain RGA, this never interleaves two users' concurrently typed runs,
//! whether they type forwards or backwards.

use crate::storage::Storage;
use crate::types::*;

#[derive(Clone)]
pub struct Replica<S: Storage> {
    replica: ReplicaId,
    lamport: u64,
    store: S,
    vv: VersionVector,
    /// Every applied op, in causal (apply) order. Source for `ops_since`.
    log: Vec<Op>,
    pending: Vec<Op>,
}

impl<S: Storage> Replica<S> {
    pub fn new(replica: ReplicaId) -> Self {
        Self {
            replica,
            lamport: 0,
            store: S::default(),
            vv: VersionVector::new(),
            log: Vec::new(),
            pending: Vec::new(),
        }
    }

    pub fn replica(&self) -> ReplicaId {
        self.replica
    }

    pub fn text(&self) -> String {
        self.store.text()
    }

    /// Visible characters.
    pub fn len(&self) -> usize {
        self.store.visible_len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Items including tombstones.
    pub fn raw_len(&self) -> usize {
        self.store.len()
    }

    pub fn tombstone_count(&self) -> usize {
        self.store.len() - self.store.visible_len()
    }

    pub fn version(&self) -> &VersionVector {
        &self.vv
    }

    pub fn log(&self) -> &[Op] {
        &self.log
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    // ------------------------------------------------------------ local edits

    /// Insert `ch` at visible position `pos`; returns the op to broadcast.
    pub fn insert(&mut self, pos: usize, ch: char) -> Op {
        assert!(pos <= self.len(), "insert position {pos} out of bounds");
        let raw = if pos == 0 {
            0
        } else {
            self.store.raw_of_visible(pos - 1) + 1
        };
        let origin_left = if pos == 0 {
            None
        } else {
            Some(self.store.item_at(raw - 1).id)
        };
        let origin_right = if raw < self.store.len() {
            Some(self.store.item_at(raw).id)
        } else {
            None
        };
        self.local(OpKind::Insert {
            origin_left,
            origin_right,
            ch,
        })
    }

    pub fn insert_str(&mut self, pos: usize, s: &str) -> Vec<Op> {
        s.chars()
            .enumerate()
            .map(|(i, ch)| self.insert(pos + i, ch))
            .collect()
    }

    /// Delete the character at visible position `pos`; returns the op to broadcast.
    pub fn delete(&mut self, pos: usize) -> Op {
        assert!(pos < self.len(), "delete position {pos} out of bounds");
        let target = self.store.item_at(self.store.raw_of_visible(pos)).id;
        self.local(OpKind::Delete { target })
    }

    fn local(&mut self, kind: OpKind) -> Op {
        self.lamport += 1;
        let op = Op {
            id: OpId {
                counter: self.lamport,
                replica: self.replica,
            },
            seq: self.seen(self.replica) + 1,
            kind,
        };
        let r = self.apply(op.clone());
        debug_assert_eq!(r, ApplyResult::Applied);
        op
    }

    // ------------------------------------------------------------ remote ops

    /// Apply an op from any source. Safe with late, reordered or duplicate ops.
    pub fn apply(&mut self, op: Op) -> ApplyResult {
        let mut sink = Vec::new();
        self.apply_collect(op, &mut sink)
    }

    /// Like [`apply`](Self::apply), and appends the resulting visible-text
    /// changes (including from any buffered ops it unblocked) to `patches`.
    pub fn apply_collect(&mut self, op: Op, patches: &mut Vec<Patch>) -> ApplyResult {
        if self.is_seen(&op) {
            return ApplyResult::Duplicate;
        }
        if !self.is_ready(&op) {
            if !self.pending.contains(&op) {
                self.pending.push(op);
            }
            return ApplyResult::Buffered;
        }
        self.integrate(op, patches);
        self.drain_pending(patches);
        ApplyResult::Applied
    }

    /// All ops this replica has that a peer at version `vv` is missing.
    pub fn ops_since(&self, vv: &VersionVector) -> Vec<Op> {
        self.log
            .iter()
            .filter(|op| op.seq > vv.get(&op.replica()).copied().unwrap_or(0))
            .cloned()
            .collect()
    }

    /// Anti-entropy: exchange missing ops in both directions.
    pub fn sync<T: Storage>(&mut self, other: &mut Replica<T>) {
        let to_other = self.ops_since(other.version());
        let to_self = other.ops_since(self.version());
        for op in to_other {
            other.apply(op);
        }
        for op in to_self {
            self.apply(op);
        }
    }

    // ------------------------------------------------------------ internals

    fn seen(&self, r: ReplicaId) -> u64 {
        self.vv.get(&r).copied().unwrap_or(0)
    }

    fn is_seen(&self, op: &Op) -> bool {
        op.seq <= self.seen(op.replica())
    }

    /// Causally ready: the next op from its replica, and every item it refers to exists.
    fn is_ready(&self, op: &Op) -> bool {
        if op.seq != self.seen(op.replica()) + 1 {
            return false;
        }
        let has = |id: &Option<OpId>| id.map_or(true, |id| self.store.contains(id));
        match &op.kind {
            OpKind::Insert {
                origin_left,
                origin_right,
                ..
            } => has(origin_left) && has(origin_right),
            OpKind::Delete { target } => self.store.contains(*target),
        }
    }

    fn pos_of(&self, id: Option<OpId>, none: isize) -> isize {
        id.map_or(none, |id| self.store.raw_pos(id) as isize)
    }

    fn integrate(&mut self, op: Op, patches: &mut Vec<Patch>) {
        self.lamport = self.lamport.max(op.id.counter);
        match op.kind {
            OpKind::Insert {
                origin_left,
                origin_right,
                ch,
            } => {
                let len = self.store.len() as isize;
                let left = self.pos_of(origin_left, -1);
                let right = self.pos_of(origin_right, len);

                // Scan the items between our two origins. Each one is either
                // concurrent with us (and must be ordered deterministically) or
                // belongs to a subtree we must skip over.
                let mut dest = left + 1;
                let mut scanning = false;
                let mut i = left + 1;
                loop {
                    if !scanning {
                        dest = i;
                    }
                    if i == len || i == right {
                        break;
                    }
                    let other = self.store.item_at(i as usize);
                    let (o_left_id, o_right_id, o_replica) =
                        (other.origin_left, other.origin_right, other.id.replica);
                    let o_left = self.pos_of(o_left_id, -1);
                    let o_right = self.pos_of(o_right_id, len);

                    if o_left < left {
                        // `other` belongs to an enclosing insert; we go before it.
                        break;
                    } else if o_left == left {
                        if o_right < right {
                            // Might go after `other`; decided by what follows.
                            scanning = true;
                        } else if o_right == right {
                            // True sibling conflict: lower replica id goes first.
                            if op.id.replica < o_replica {
                                break;
                            }
                            scanning = false;
                        } else {
                            scanning = false;
                        }
                    }
                    // o_left > left: `other` is inside a sibling's subtree; skip it.
                    i += 1;
                }

                let dest = dest as usize;
                self.store.insert(
                    dest,
                    Item {
                        id: op.id,
                        origin_left,
                        origin_right,
                        ch,
                        deleted: false,
                    },
                );
                patches.push(Patch::Insert {
                    pos: self.store.visible_before(dest),
                    ch,
                });
            }
            OpKind::Delete { target } => {
                let raw = self.store.raw_pos(target);
                let pos = self.store.visible_before(raw);
                if self.store.mark_deleted(target) {
                    patches.push(Patch::Delete { pos });
                }
            }
        }
        self.vv.insert(op.replica(), op.seq);
        self.log.push(op);
    }

    fn drain_pending(&mut self, patches: &mut Vec<Patch>) {
        if self.pending.is_empty() {
            return;
        }
        loop {
            let vv = &self.vv;
            self.pending
                .retain(|op| op.seq > vv.get(&op.replica()).copied().unwrap_or(0));
            let Some(i) = self.pending.iter().position(|op| self.is_ready(op)) else {
                break;
            };
            let op = self.pending.swap_remove(i);
            self.integrate(op, patches);
        }
    }
}
