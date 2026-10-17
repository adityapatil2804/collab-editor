//! A sequence CRDT for collaborative text editing.
//!
//! v0 implements RGA (Replicated Growable Array):
//! - Every character gets a permanent, globally unique [`OpId`] (Lamport counter + replica).
//! - An insert says "put `ch` right after character `parent`" (or at the start).
//! - Concurrent inserts after the same parent are ordered by `OpId`, highest first.
//! - Deletes leave tombstones, so later ops can still refer to deleted characters.
//!
//! Delivery model: ops may arrive late, out of order, or duplicated. Ops whose
//! dependencies are missing are buffered until they become applicable, and
//! duplicates are ignored. That is what makes offline sync safe.
//!
//! v0 favors clarity over speed: lookups are O(n). See README for the
//! optimization roadmap (id index, run-length encoding, Fugue).

use std::collections::HashMap;

pub type ReplicaId = u32;

/// Globally unique id of an operation. Ordered by `(counter, replica)`, which
/// gives a total order consistent with causality (Lamport clock + tiebreak).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OpId {
    pub counter: u64,
    pub replica: ReplicaId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpKind {
    /// Insert `ch` immediately after `parent` (`None` = start of document).
    Insert { parent: Option<OpId>, ch: char },
    /// Tombstone the character created by `target`.
    Delete { target: OpId },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Op {
    pub id: OpId,
    /// Per-replica sequence number (1, 2, 3, ...). Used for dedup and sync.
    pub seq: u64,
    pub kind: OpKind,
}

impl Op {
    pub fn replica(&self) -> ReplicaId {
        self.id.replica
    }
}

/// For each replica, the highest `seq` applied. Summarizes everything a doc has seen.
pub type VersionVector = HashMap<ReplicaId, u64>;

#[derive(Debug, PartialEq, Eq)]
pub enum ApplyResult {
    Applied,
    /// Dependencies missing; held until they arrive.
    Buffered,
    /// Already seen; ignored.
    Duplicate,
}

#[derive(Clone, Debug)]
struct Elem {
    id: OpId,
    ch: char,
    deleted: bool,
}

#[derive(Clone, Debug)]
pub struct Doc {
    replica: ReplicaId,
    lamport: u64,
    /// All characters, including tombstones, in document order.
    elems: Vec<Elem>,
    vv: VersionVector,
    /// Every applied op, in causal order. Source for `ops_since`.
    log: Vec<Op>,
    pending: Vec<Op>,
}

impl Doc {
    pub fn new(replica: ReplicaId) -> Self {
        Self {
            replica,
            lamport: 0,
            elems: Vec::new(),
            vv: HashMap::new(),
            log: Vec::new(),
            pending: Vec::new(),
        }
    }

    pub fn replica(&self) -> ReplicaId {
        self.replica
    }

    pub fn text(&self) -> String {
        self.elems.iter().filter(|e| !e.deleted).map(|e| e.ch).collect()
    }

    /// Number of visible characters.
    pub fn len(&self) -> usize {
        self.elems.iter().filter(|e| !e.deleted).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn version(&self) -> &VersionVector {
        &self.vv
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn tombstone_count(&self) -> usize {
        self.elems.iter().filter(|e| e.deleted).count()
    }

    // ---------- local edits ----------

    /// Insert `ch` at visible position `pos`. Returns the op to broadcast.
    pub fn insert(&mut self, pos: usize, ch: char) -> Op {
        assert!(pos <= self.len(), "insert position {pos} out of bounds");
        let parent = if pos == 0 {
            None
        } else {
            Some(self.elems[self.raw_index(pos - 1)].id)
        };
        self.local(OpKind::Insert { parent, ch })
    }

    pub fn insert_str(&mut self, pos: usize, s: &str) -> Vec<Op> {
        s.chars()
            .enumerate()
            .map(|(i, ch)| self.insert(pos + i, ch))
            .collect()
    }

    /// Delete the character at visible position `pos`. Returns the op to broadcast.
    pub fn delete(&mut self, pos: usize) -> Op {
        assert!(pos < self.len(), "delete position {pos} out of bounds");
        let target = self.elems[self.raw_index(pos)].id;
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

    // ---------- remote ops ----------

    /// Apply an op from any source. Safe to call with late, reordered, or duplicate ops.
    pub fn apply(&mut self, op: Op) -> ApplyResult {
        if self.is_seen(&op) {
            return ApplyResult::Duplicate;
        }
        if !self.is_ready(&op) {
            if !self.pending.contains(&op) {
                self.pending.push(op);
            }
            return ApplyResult::Buffered;
        }
        self.integrate(op);
        self.drain_pending();
        ApplyResult::Applied
    }

    /// All ops this doc has that a peer with version `vv` is missing.
    pub fn ops_since(&self, vv: &VersionVector) -> Vec<Op> {
        self.log
            .iter()
            .filter(|op| op.seq > vv.get(&op.replica()).copied().unwrap_or(0))
            .cloned()
            .collect()
    }

    /// Anti-entropy: bring two docs to the same state by exchanging missing ops.
    pub fn sync(a: &mut Doc, b: &mut Doc) {
        let to_b = a.ops_since(b.version());
        let to_a = b.ops_since(a.version());
        for op in to_b {
            b.apply(op);
        }
        for op in to_a {
            a.apply(op);
        }
    }

    // ---------- internals ----------

    fn seen(&self, r: ReplicaId) -> u64 {
        self.vv.get(&r).copied().unwrap_or(0)
    }

    fn is_seen(&self, op: &Op) -> bool {
        op.seq <= self.seen(op.replica())
    }

    /// Causal readiness: next op from its replica, and the char it references exists.
    fn is_ready(&self, op: &Op) -> bool {
        if op.seq != self.seen(op.replica()) + 1 {
            return false;
        }
        match &op.kind {
            OpKind::Insert { parent: None, .. } => true,
            OpKind::Insert { parent: Some(p), .. } => self.find(*p).is_some(),
            OpKind::Delete { target } => self.find(*target).is_some(),
        }
    }

    fn integrate(&mut self, op: Op) {
        self.lamport = self.lamport.max(op.id.counter);
        match &op.kind {
            OpKind::Insert { parent, ch } => {
                let mut i = match parent {
                    None => 0,
                    Some(p) => self.find(*p).expect("parent checked in is_ready") + 1,
                };
                // RGA rule: skip concurrent siblings with higher ids (and their subtrees,
                // which always carry even higher ids under Lamport clocks).
                while i < self.elems.len() && self.elems[i].id > op.id {
                    i += 1;
                }
                self.elems.insert(
                    i,
                    Elem {
                        id: op.id,
                        ch: *ch,
                        deleted: false,
                    },
                );
            }
            OpKind::Delete { target } => {
                let i = self.find(*target).expect("target checked in is_ready");
                self.elems[i].deleted = true;
            }
        }
        self.vv.insert(op.replica(), op.seq);
        self.log.push(op);
    }

    fn drain_pending(&mut self) {
        loop {
            self.pending.retain(|op| op.seq > self.vv.get(&op.id.replica).copied().unwrap_or(0));
            let Some(i) = self.pending.iter().position(|op| self.is_ready(op)) else {
                break;
            };
            let op = self.pending.swap_remove(i);
            self.integrate(op);
        }
    }

    fn find(&self, id: OpId) -> Option<usize> {
        self.elems.iter().position(|e| e.id == id)
    }

    /// Map a visible position to an index in `elems` (which includes tombstones).
    fn raw_index(&self, visible: usize) -> usize {
        self.elems
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.deleted)
            .nth(visible)
            .map(|(i, _)| i)
            .expect("visible index in bounds")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_edits() {
        let mut d = Doc::new(1);
        d.insert_str(0, "helo");
        d.insert(3, 'l');
        assert_eq!(d.text(), "hello");
        d.delete(0);
        assert_eq!(d.text(), "ello");
        assert_eq!(d.tombstone_count(), 1);
    }

    #[test]
    fn cat_example_converges() {
        let mut a = Doc::new(1);
        a.insert_str(0, "cat");
        let mut b = a.clone();
        b.replica = 2;
        let op_a = a.insert(3, 's'); // Alice: "cats"
        let op_b = b.delete(0); // Bob: "at"
        a.apply(op_b);
        b.apply(op_a);
        assert_eq!(a.text(), "ats");
        assert_eq!(a.text(), b.text());
    }

    #[test]
    fn duplicates_and_out_of_order() {
        let mut a = Doc::new(1);
        let ops = a.insert_str(0, "abc");
        let mut b = Doc::new(2);
        assert_eq!(b.apply(ops[2].clone()), ApplyResult::Buffered);
        assert_eq!(b.apply(ops[1].clone()), ApplyResult::Buffered);
        assert_eq!(b.apply(ops[0].clone()), ApplyResult::Applied);
        assert_eq!(b.apply(ops[1].clone()), ApplyResult::Duplicate);
        assert_eq!(b.text(), "abc");
        assert_eq!(b.pending_len(), 0);
    }

    #[test]
    fn concurrent_forward_typing_does_not_interleave() {
        let mut a = Doc::new(1);
        let mut b = Doc::new(2);
        a.insert_str(0, "abc");
        b.insert_str(0, "xyz");
        Doc::sync(&mut a, &mut b);
        assert_eq!(a.text(), b.text());
        assert!(a.text() == "abcxyz" || a.text() == "xyzabc", "got {}", a.text());
    }
}
