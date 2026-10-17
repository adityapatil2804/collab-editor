//! Randomized tests.
//!
//! 1. Convergence: N replicas edit concurrently over an unreliable network
//!    (reordered and duplicated delivery, replicas going offline and editing,
//!    reconnecting via version-vector anti-entropy). Afterwards every replica
//!    must hold identical text and version vectors, with nothing stuck.
//! 2. Differential: every replica is shadowed by a `NaiveDoc` (same algorithm,
//!    plain Vec storage) fed the same ops in the same order. The optimized
//!    storage must produce the same text at every step.
//! 3. Patches: each replica also keeps a plain `Vec<char>` updated only through
//!    the patches `apply_collect` returns. It must always equal `text()`.
//!
//! More seeds: `FUZZ_SEEDS=10000 cargo test --release`.

use crdt_core::{Doc, NaiveDoc, Op, Patch};

/// Deterministic PRNG (xorshift64*) so any failure reproduces from its seed.
pub struct Rng(u64);
impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    pub fn chance(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }
}

fn seeds() -> u64 {
    std::env::var("FUZZ_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(400)
}

/// A replica plus its oracle twin and patch-driven mirror.
struct Node {
    doc: Doc,
    naive: NaiveDoc,
    mirror: Vec<char>,
}

impl Node {
    fn new(id: u64) -> Self {
        Node {
            doc: Doc::new(id),
            naive: NaiveDoc::new(id),
            mirror: Vec::new(),
        }
    }

    fn local_edit(&mut self, rng: &mut Rng) -> Op {
        let len = self.doc.len();
        let op = if len > 0 && rng.chance(30) {
            let p = rng.below(len);
            self.mirror.remove(p);
            self.doc.delete(p)
        } else {
            let p = rng.below(len + 1);
            let ch = (b'a' + rng.below(26) as u8) as char;
            self.mirror.insert(p, ch);
            self.doc.insert(p, ch)
        };
        self.naive.apply(op.clone());
        op
    }

    fn receive(&mut self, op: Op) {
        let mut patches = Vec::new();
        let r1 = self.doc.apply_collect(op.clone(), &mut patches);
        let r2 = self.naive.apply(op);
        assert_eq!(r1, r2, "apply results differ");
        for p in patches {
            match p {
                Patch::Insert { pos, ch } => self.mirror.insert(pos, ch),
                Patch::Delete { pos } => {
                    self.mirror.remove(pos);
                }
            }
        }
    }

    fn check(&self, ctx: &str) {
        let t = self.doc.text();
        assert_eq!(t, self.naive.text(), "{ctx}: block storage != naive oracle");
        assert_eq!(
            t,
            self.mirror.iter().collect::<String>(),
            "{ctx}: patches out of sync"
        );
    }
}

fn sync(nodes: &mut [Node], i: usize, j: usize) {
    let to_j = nodes[i].doc.ops_since(nodes[j].doc.version());
    let to_i = nodes[j].doc.ops_since(nodes[i].doc.version());
    for op in to_j {
        nodes[j].receive(op);
    }
    for op in to_i {
        nodes[i].receive(op);
    }
}

fn run(seed: u64) {
    let mut rng = Rng::new(seed);
    let n = 2 + rng.below(4); // 2..=5 replicas
    let mut nodes: Vec<Node> = (0..n as u64).map(|i| Node::new(1000 + i)).collect();
    let mut inbox: Vec<Vec<Op>> = vec![Vec::new(); n];
    let mut online = vec![true; n];

    for step in 0..300 {
        let r = rng.below(n);
        match rng.below(100) {
            0..=54 => {
                let op = nodes[r].local_edit(&mut rng);
                if online[r] {
                    for d in 0..n {
                        if d != r && online[d] {
                            inbox[d].push(op.clone());
                            if rng.chance(10) {
                                inbox[d].push(op.clone());
                            }
                        }
                    }
                }
            }
            55..=84 => {
                if online[r] && !inbox[r].is_empty() {
                    let i = rng.below(inbox[r].len());
                    let op = inbox[r].swap_remove(i);
                    nodes[r].receive(op);
                }
            }
            85..=94 => {
                online[r] = !online[r];
                if online[r] {
                    let peers: Vec<usize> = (0..n).filter(|&p| p != r && online[p]).collect();
                    if !peers.is_empty() {
                        let p = peers[rng.below(peers.len())];
                        sync(&mut nodes, r, p);
                    }
                } else {
                    inbox[r].clear();
                }
            }
            _ => {
                let p = rng.below(n);
                if p != r && online[r] && online[p] {
                    sync(&mut nodes, r, p);
                }
            }
        }
        if step % 10 == 0 {
            nodes[r].check(&format!("seed {seed} step {step} replica {r}"));
        }
    }

    for r in 0..n {
        for op in std::mem::take(&mut inbox[r]) {
            nodes[r].receive(op);
        }
    }
    for _ in 0..2 {
        for i in 0..n {
            for j in (i + 1)..n {
                sync(&mut nodes, i, j);
            }
        }
    }

    let expected = nodes[0].doc.text();
    for (i, node) in nodes.iter().enumerate() {
        node.check(&format!("seed {seed} final replica {i}"));
        assert_eq!(node.doc.text(), expected, "seed {seed}: replica {i} diverged");
        assert_eq!(node.doc.pending_len(), 0, "seed {seed}: replica {i} stuck ops");
        assert_eq!(node.doc.version(), nodes[0].doc.version(), "seed {seed}: vv differ");
    }
}

#[test]
fn converges_under_partitions_reordering_and_duplicates() {
    for seed in 0..seeds() {
        run(seed);
    }
}

/// A single replica must behave exactly like a plain String (long enough to
/// force many block splits in BlockStorage).
#[test]
fn single_replica_matches_string_model() {
    for seed in 0..seeds() / 4 {
        let mut rng = Rng::new(seed);
        let mut doc = Doc::new(7);
        let mut model: Vec<char> = Vec::new();
        for i in 0..2000 {
            if !model.is_empty() && rng.chance(25) {
                let p = rng.below(model.len());
                doc.delete(p);
                model.remove(p);
            } else {
                let p = rng.below(model.len() + 1);
                let ch = (b'a' + rng.below(26) as u8) as char;
                doc.insert(p, ch);
                model.insert(p, ch);
            }
            if i % 97 == 0 {
                assert_eq!(doc.text(), model.iter().collect::<String>(), "seed {seed}");
            }
        }
        assert_eq!(doc.text(), model.iter().collect::<String>(), "seed {seed}");
    }
}

/// Concurrent typing at the same spot must never interleave characters,
/// whatever direction each user types in.
#[test]
fn no_interleaving_random_runs() {
    for seed in 0..seeds() {
        let mut rng = Rng::new(seed);
        let mut base = Doc::new(1);
        base.insert_str(0, "0123456789");
        let at = rng.below(11);
        let mut a = Doc::new(2);
        let mut b = Doc::new(3);
        for op in base.log().to_vec() {
            a.apply(op.clone());
            b.apply(op);
        }
        let backwards_a = rng.chance(50);
        let backwards_b = rng.chance(50);
        let wa: String = (0..1 + rng.below(6)).map(|_| 'A').collect();
        let wb: String = (0..1 + rng.below(6)).map(|_| 'b').collect();
        for (doc, w, back) in [(&mut a, &wa, backwards_a), (&mut b, &wb, backwards_b)] {
            for (k, ch) in w.chars().enumerate() {
                if back {
                    doc.insert(at, ch);
                } else {
                    doc.insert(at + k, ch);
                }
            }
        }
        a.sync(&mut b);
        let t = a.text();
        assert_eq!(t, b.text());
        let middle: String = t.chars().filter(|c| !c.is_ascii_digit()).collect();
        assert!(
            middle == format!("{wa}{wb}") || middle == format!("{wb}{wa}"),
            "seed {seed}: interleaved {middle:?}"
        );
    }
}
