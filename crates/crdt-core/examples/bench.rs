//! Benchmarks: `cargo run --release --example bench`
//!
//! Traces are synthetic but shaped like real editing: a cursor that mostly types
//! forward, sometimes backspaces, and occasionally jumps elsewhere. (The standard
//! automerge-perf trace is ~260K ops; the "large" trace matches that size.)
//!
//! Memory is measured with a counting global allocator (live heap bytes).

use crdt_core::{Doc, NaiveDoc, Op, Replica, Storage};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        LIVE.fetch_add(l.size(), Relaxed);
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Relaxed);
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        LIVE.fetch_add(n, Relaxed);
        LIVE.fetch_sub(l.size(), Relaxed);
        System.realloc(p, l, n)
    }
}

#[global_allocator]
static A: Counting = Counting;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

#[derive(Clone, Copy)]
enum Edit {
    Ins(usize, char),
    Del(usize),
}

/// Single-author editing trace with cursor locality.
fn trace(n: usize, seed: u64) -> Vec<Edit> {
    let mut rng = Rng(seed | 1);
    let mut len = 0usize;
    let mut cursor = 0usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let r = rng.below(100);
        if r < 3 {
            cursor = rng.below(len + 1); // jump
        }
        if r >= 3 && r < 15 && cursor > 0 {
            cursor -= 1; // backspace
            out.push(Edit::Del(cursor));
            len -= 1;
        } else {
            let ch = (b'a' + rng.below(26) as u8) as char;
            out.push(Edit::Ins(cursor, ch));
            cursor += 1;
            len += 1;
        }
    }
    out
}

fn run_local<S: Storage>(doc: &mut Replica<S>, t: &[Edit]) {
    for e in t {
        match *e {
            Edit::Ins(p, c) => {
                doc.insert(p, c);
            }
            Edit::Del(p) => {
                doc.delete(p);
            }
        }
    }
}

fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1e3
}

fn row(name: &str, ops: usize, millis: f64) {
    println!(
        "| {name} | {ops} | {millis:.1} | {:.0} |",
        ops as f64 / (millis / 1e3)
    );
}

fn main() {
    println!("## Throughput\n");
    println!("| Benchmark | Ops | Time (ms) | Ops/sec |");
    println!("|---|---|---|---|");

    // 1. Local editing: optimized vs naive storage, same algorithm.
    for &n in &[20_000usize, 50_000] {
        let t = trace(n, 42);
        let s = Instant::now();
        let mut naive = NaiveDoc::new(1);
        run_local(&mut naive, &t);
        row(&format!("Local edits, naive Vec storage, {}K", n / 1000), n, ms(s));
        let s = Instant::now();
        let mut fast = Doc::new(1);
        run_local(&mut fast, &t);
        row(&format!("Local edits, block storage, {}K", n / 1000), n, ms(s));
        assert_eq!(naive.text(), fast.text());
    }

    let big = trace(260_000, 7);
    let before = LIVE.load(Relaxed);
    let s = Instant::now();
    let mut doc = Doc::new(1);
    run_local(&mut doc, &big);
    row("Local edits, block storage, 260K", big.len(), ms(s));
    let doc_bytes = LIVE.load(Relaxed) - before;

    // 2. Remote replay: a client opening the document applies the whole log.
    let log: Vec<Op> = doc.log().to_vec();
    let s = Instant::now();
    let mut fresh = Doc::new(2);
    for op in log.iter().cloned() {
        fresh.apply(op);
    }
    row("Remote replay (open doc), 260K", log.len(), ms(s));
    assert_eq!(fresh.text(), doc.text());

    // 3. Out-of-order replay: worst case for the causal buffer is avoided by
    //    delivering in reverse chunks of 64 (bounded reordering).
    let s = Instant::now();
    let mut shuffled = Doc::new(3);
    for chunk in log[..50_000].chunks(64) {
        for op in chunk.iter().rev().cloned() {
            shuffled.apply(op);
        }
    }
    row("Remote replay, reordered in 64-op windows, 50K", 50_000, ms(s));

    // 4. Offline merge: two replicas diverge for 25K edits each, then sync.
    let mut a = Doc::new(10);
    a.insert_str(0, &"x".repeat(10_000));
    let mut b = Doc::new(11);
    for op in a.log().to_vec() {
        b.apply(op);
    }
    run_local(&mut a, &trace(25_000, 3).iter().map(|e| shift(*e, 10_000)).collect::<Vec<_>>());
    run_local(&mut b, &trace(25_000, 4).iter().map(|e| shift(*e, 0)).collect::<Vec<_>>());
    let s = Instant::now();
    a.sync(&mut b);
    row("Offline merge, 2 x 25K divergent edits", 50_000, ms(s));
    assert_eq!(a.text(), b.text());

    println!("\n## Memory (260K-op trace)\n");
    println!("| Metric | Value |");
    println!("|---|---|");
    println!("| Visible chars | {} |", doc.len());
    println!("| Items incl. tombstones | {} |", doc.raw_len());
    println!("| Live heap (doc incl. op log) | {:.1} MB |", doc_bytes as f64 / 1e6);
    println!(
        "| Bytes per item | {:.0} |",
        doc_bytes as f64 / doc.raw_len() as f64
    );
}

/// Offset a trace so it edits after a prefix of `base` characters.
fn shift(e: Edit, base: usize) -> Edit {
    match e {
        Edit::Ins(p, c) => Edit::Ins(p + base, c),
        Edit::Del(p) => Edit::Del(p + base),
    }
}
