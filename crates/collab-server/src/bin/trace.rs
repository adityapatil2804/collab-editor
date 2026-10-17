//! Cross-language consistency check between the Rust and TypeScript CRDTs.
//!
//!   trace gen <out.json>   Rust replicas edit concurrently; write all ops + final text.
//!   trace check <in.json>  Apply ops written by the TypeScript client; compare text.
//!
//! File format: {"cases":[{"ops":[Op...],"text":"..."}]}

use collab_server::json::Json;
use collab_server::protocol::{ops_from_json, ops_to_json};
use crdt_core::Doc;

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n.max(1) as u64) as usize
    }
}

/// Includes multi-byte and astral-plane characters to exercise code-point handling.
const ALPHABET: &[char] = &['a', 'b', 'c', ' ', '\n', 'é', 'ß', '中', '😀', '🚀', '"', '\\'];

fn gen(path: &str) {
    let mut cases = Vec::new();
    for seed in 1..=200u64 {
        let mut rng = Rng(seed * 0x9E3779B97F4A7C15 | 1);
        let n = 2 + rng.below(3);
        let mut docs: Vec<Doc> = (0..n).map(|i| Doc::new(100 + i as u64 * 977)).collect();
        for _ in 0..150 {
            let r = rng.below(n);
            let len = docs[r].len();
            if len > 0 && rng.below(4) == 0 {
                docs[r].delete(rng.below(len));
            } else {
                let ch = ALPHABET[rng.below(ALPHABET.len())];
                docs[r].insert(rng.below(len + 1), ch);
            }
            if rng.below(10) == 0 {
                let (i, j) = (rng.below(n), rng.below(n));
                if i != j {
                    let (a, b) = if i < j {
                        let (l, h) = docs.split_at_mut(j);
                        (&mut l[i], &mut h[0])
                    } else {
                        let (l, h) = docs.split_at_mut(i);
                        (&mut h[0], &mut l[j])
                    };
                    a.sync(b);
                }
            }
        }
        for i in 1..n {
            let (l, h) = docs.split_at_mut(i);
            l[0].sync(&mut h[0]);
        }
        for i in 1..n {
            let (l, h) = docs.split_at_mut(i);
            l[0].sync(&mut h[0]);
        }
        cases.push(Json::Obj(vec![
            ("ops".into(), ops_to_json(docs[0].log())),
            ("text".into(), Json::Str(docs[0].text())),
        ]));
    }
    let out = Json::Obj(vec![("cases".into(), Json::Arr(cases))]);
    std::fs::write(path, out.to_string()).unwrap();
    eprintln!("wrote 200 Rust-generated cases to {path}");
}

fn check(path: &str) {
    let j = Json::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
    let cases = j.get("cases").unwrap().as_arr().unwrap();
    for (i, c) in cases.iter().enumerate() {
        let ops = ops_from_json(c.get("ops").unwrap()).unwrap();
        let mut doc = Doc::new(1);
        // Reverse order: maximal reordering, forces the causal buffer to work.
        for op in ops.into_iter().rev() {
            doc.apply(op);
        }
        let want = c.get("text").unwrap().as_str().unwrap();
        assert_eq!(doc.pending_len(), 0, "case {i}: ops stuck in buffer");
        assert_eq!(doc.text(), want, "case {i}: Rust result differs from TypeScript");
    }
    eprintln!("Rust agrees with TypeScript on {} cases", cases.len());
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match (args.get(1).map(String::as_str), args.get(2)) {
        (Some("gen"), Some(p)) => gen(p),
        (Some("check"), Some(p)) => check(p),
        _ => {
            eprintln!("usage: trace gen <out.json> | trace check <in.json>");
            std::process::exit(2);
        }
    }
}
