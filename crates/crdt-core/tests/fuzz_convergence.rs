//! Randomized convergence tests.
//!
//! Simulates N replicas editing concurrently over an unreliable network:
//! reordered and duplicated delivery, replicas going offline and editing,
//! then reconnecting via version-vector anti-entropy. After everyone
//! reconnects, every replica must hold byte-identical text.
//!
//! Override the number of seeds with `FUZZ_SEEDS=10000 cargo test --release`.

use crdt_core::{Doc, Op};

/// Small deterministic PRNG (xorshift64*) so failures reproduce from a seed.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }
}

fn seeds() -> u64 {
    std::env::var("FUZZ_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(500)
}

fn random_edit(doc: &mut Doc, rng: &mut Rng) -> Op {
    let len = doc.len();
    if len > 0 && rng.chance(30) {
        doc.delete(rng.below(len))
    } else {
        let ch = (b'a' + rng.below(26) as u8) as char;
        doc.insert(rng.below(len + 1), ch)
    }
}

fn run(seed: u64) {
    let mut rng = Rng::new(seed);
    let n = 2 + rng.below(4); // 2..=5 replicas
    let mut docs: Vec<Doc> = (0..n as u32).map(Doc::new).collect();
    let mut inbox: Vec<Vec<Op>> = vec![Vec::new(); n];
    let mut online = vec![true; n];

    for _ in 0..300 {
        let r = rng.below(n);
        match rng.below(100) {
            // Local edit (allowed while offline).
            0..=54 => {
                let op = random_edit(&mut docs[r], &mut rng);
                if online[r] {
                    for d in 0..n {
                        if d != r && online[d] {
                            inbox[d].push(op.clone());
                            if rng.chance(10) {
                                inbox[d].push(op.clone()); // duplicate delivery
                            }
                        }
                    }
                }
            }
            // Deliver a random (not oldest) message: out-of-order delivery.
            55..=84 => {
                if online[r] && !inbox[r].is_empty() {
                    let i = rng.below(inbox[r].len());
                    let op = inbox[r].swap_remove(i);
                    docs[r].apply(op);
                }
            }
            // Go offline / come back online (reconnect = anti-entropy with a peer).
            85..=94 => {
                online[r] = !online[r];
                if online[r] {
                    let peers: Vec<usize> = (0..n).filter(|&p| p != r && online[p]).collect();
                    if !peers.is_empty() {
                        let p = peers[rng.below(peers.len())];
                        let (a, b) = pair_mut(&mut docs, r, p);
                        Doc::sync(a, b);
                    }
                } else {
                    inbox[r].clear(); // messages to an offline replica are lost
                }
            }
            // Background anti-entropy between two online replicas.
            _ => {
                let p = rng.below(n);
                if p != r && online[r] && online[p] {
                    let (a, b) = pair_mut(&mut docs, r, p);
                    Doc::sync(a, b);
                }
            }
        }
    }

    // Everyone reconnects, drains inboxes, then gossips until quiescent.
    for r in 0..n {
        for op in std::mem::take(&mut inbox[r]) {
            docs[r].apply(op);
        }
    }
    for _ in 0..2 {
        for i in 0..n {
            for j in (i + 1)..n {
                let (a, b) = pair_mut(&mut docs, i, j);
                Doc::sync(a, b);
            }
        }
    }

    let expected = docs[0].text();
    for (i, d) in docs.iter().enumerate() {
        assert_eq!(d.text(), expected, "seed {seed}: replica {i} diverged");
        assert_eq!(d.pending_len(), 0, "seed {seed}: replica {i} has stuck ops");
        assert_eq!(d.version(), docs[0].version(), "seed {seed}: version vectors differ");
    }
}

fn pair_mut(v: &mut [Doc], i: usize, j: usize) -> (&mut Doc, &mut Doc) {
    assert_ne!(i, j);
    if i < j {
        let (l, r) = v.split_at_mut(j);
        (&mut l[i], &mut r[0])
    } else {
        let (l, r) = v.split_at_mut(i);
        (&mut r[0], &mut l[j])
    }
}

#[test]
fn converges_under_partitions_reordering_and_duplicates() {
    for seed in 0..seeds() {
        run(seed);
    }
}

/// A single replica must behave exactly like a plain String.
#[test]
fn single_replica_matches_string_model() {
    for seed in 0..seeds() {
        let mut rng = Rng::new(seed);
        let mut doc = Doc::new(0);
        let mut model: Vec<char> = Vec::new();
        for _ in 0..300 {
            if !model.is_empty() && rng.chance(30) {
                let i = rng.below(model.len());
                doc.delete(i);
                model.remove(i);
            } else {
                let i = rng.below(model.len() + 1);
                let ch = (b'a' + rng.below(26) as u8) as char;
                doc.insert(i, ch);
                model.insert(i, ch);
            }
            assert_eq!(doc.text(), model.iter().collect::<String>(), "seed {seed}");
        }
    }
}
