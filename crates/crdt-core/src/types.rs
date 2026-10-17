use std::collections::HashMap;

/// Identifies one editing session. Clients pick a random 52-bit id so it survives
/// a round trip through JSON numbers (exact up to 2^53).
pub type ReplicaId = u64;

/// Globally unique id of an operation: Lamport counter + replica.
/// Ordered by `(counter, replica)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OpId {
    pub counter: u64,
    pub replica: ReplicaId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpKind {
    /// Insert `ch` between the items `origin_left` and `origin_right`
    /// (the neighbours it had when it was typed; `None` = document edge).
    Insert {
        origin_left: Option<OpId>,
        origin_right: Option<OpId>,
        ch: char,
    },
    /// Tombstone the item created by `target`.
    Delete { target: OpId },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Op {
    pub id: OpId,
    /// Per-replica sequence number (1, 2, 3, ...). Drives dedup and sync.
    pub seq: u64,
    pub kind: OpKind,
}

impl Op {
    pub fn replica(&self) -> ReplicaId {
        self.id.replica
    }
}

/// For each replica, the highest `seq` applied.
pub type VersionVector = HashMap<ReplicaId, u64>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyResult {
    Applied,
    /// Dependencies missing; held until they arrive.
    Buffered,
    /// Already seen; ignored.
    Duplicate,
}

/// A visible-text change caused by applying an op. Editors use these to update
/// their buffer without re-rendering the whole document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Patch {
    Insert { pos: usize, ch: char },
    Delete { pos: usize },
}

/// One character in the sequence (possibly a tombstone).
#[derive(Clone, Debug)]
pub struct Item {
    pub id: OpId,
    pub origin_left: Option<OpId>,
    pub origin_right: Option<OpId>,
    pub ch: char,
    pub deleted: bool,
}
