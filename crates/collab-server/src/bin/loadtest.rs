//! Load test: N simulated clients edit one document concurrently.
//!
//!   cargo run --release --bin loadtest -- --addr 127.0.0.1:8080 --clients 100
//!
//! Each client types at a human-like rate. Latency is measured from just before
//! a client sends an op to the moment another client has applied it (same
//! machine, same clock). At the end every client and the server must hold
//! identical text.

use collab_server::json::Json;
use collab_server::protocol::*;
use collab_server::ws::{Client, Message};
use crdt_core::{Doc, OpId};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

struct Args {
    addr: String,
    clients: usize,
    edits: usize,
    interval_ms: u64,
    doc: String,
}

fn parse_args() -> Args {
    let mut a = Args {
        addr: "127.0.0.1:8080".into(),
        clients: 100,
        edits: 100,
        interval_ms: 100,
        doc: format!("load-{}", std::process::id()),
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let v = it.next().expect("missing value");
        match k.as_str() {
            "--addr" => a.addr = v,
            "--clients" => a.clients = v.parse().unwrap(),
            "--edits" => a.edits = v.parse().unwrap(),
            "--interval-ms" => a.interval_ms = v.parse().unwrap(),
            "--doc" => a.doc = v,
            _ => panic!("unknown arg {k}"),
        }
    }
    a
}

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n.max(1) as u64) as usize
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn http_get(addr: &str, path: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(s, "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    resp.split_once("\r\n\r\n").map(|x| x.1.to_string()).unwrap_or_default()
}

fn main() {
    let args = Arc::new(parse_args());
    let sent_at: Arc<Mutex<HashMap<OpId, Instant>>> = Arc::default();
    let latencies: Arc<Mutex<Vec<f64>>> = Arc::default();
    let docs: Vec<Arc<Mutex<Doc>>> = (0..args.clients)
        .map(|i| Arc::new(Mutex::new(Doc::new(10_000 + i as u64))))
        .collect();
    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(Barrier::new(args.clients + 1));

    eprintln!(
        "{} clients x {} edits every ~{} ms on doc '{}'",
        args.clients, args.edits, args.interval_ms, args.doc
    );

    let mut handles = Vec::new();
    for (i, doc) in docs.iter().enumerate() {
        let (args, doc, sent_at, latencies, stop, ready) = (
            args.clone(),
            doc.clone(),
            sent_at.clone(),
            latencies.clone(),
            stop.clone(),
            ready.clone(),
        );
        handles.push(std::thread::spawn(move || {
            let mut ws = Client::connect(&args.addr, "/ws").expect("connect");
            let replica = doc.lock().unwrap().replica();
            let hello = msg(
                "hello",
                vec![
                    ("doc", Json::Str(args.doc.clone())),
                    ("replica", Json::Num(replica as f64)),
                    ("vv", Json::Obj(vec![])),
                ],
            );
            ws.send_text(&hello.to_string()).unwrap();
            let Ok(Message::Text(w)) = ws.recv() else { panic!("no welcome") };
            for op in ops_from_json(Json::parse(&w).unwrap().get("ops").unwrap()).unwrap() {
                doc.lock().unwrap().apply(op);
            }

            // Reader: apply remote ops, record latency.
            let mut reader = ws.reader;
            reader
                .get_ref()
                .set_read_timeout(Some(Duration::from_millis(200)))
                .unwrap();
            let rdoc = doc.clone();
            let (rsent, rlat, rstop) = (sent_at.clone(), latencies.clone(), stop.clone());
            let reader_thread = std::thread::spawn(move || {
                let mut local = Vec::new();
                while !rstop.load(Ordering::Relaxed) {
                    match collab_server::ws::read_message(&mut reader) {
                        Ok(Message::Text(s)) => {
                            let m = Json::parse(&s).unwrap();
                            if m.get("t").and_then(Json::as_str) != Some("ops") {
                                continue;
                            }
                            let ops = ops_from_json(m.get("ops").unwrap()).unwrap();
                            let mut d = rdoc.lock().unwrap();
                            for op in ops {
                                let id = op.id;
                                d.apply(op);
                                if let Some(t) = rsent.lock().unwrap().get(&id) {
                                    local.push(t.elapsed().as_secs_f64() * 1e3);
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(_) => {}
                    }
                }
                rlat.lock().unwrap().extend(local);
            });

            ready.wait();
            let mut rng = Rng(0x9E37 + i as u64 * 7919);
            let mut ws_out = ws.writer;
            let mut client = |s: &str| {
                let frame = collab_server::ws::encode_frame(
                    collab_server::ws::OP_TEXT,
                    s.as_bytes(),
                    Some([1, 2, 3, 4]),
                );
                ws_out.write_all(&frame).unwrap();
            };
            for _ in 0..args.edits {
                let jitter = rng.below(args.interval_ms as usize + 1) as u64;
                std::thread::sleep(Duration::from_millis(args.interval_ms / 2 + jitter));
                let op = {
                    let mut d = doc.lock().unwrap();
                    let len = d.len();
                    if len > 0 && rng.below(5) == 0 {
                        d.delete(rng.below(len))
                    } else {
                        let ch = (b'a' + rng.below(26) as u8) as char;
                        d.insert(rng.below(len + 1), ch)
                    }
                };
                sent_at.lock().unwrap().insert(op.id, Instant::now());
                let m = msg("ops", vec![("ops", ops_to_json(&[op]))]);
                client(&m.to_string());
            }
            reader_thread
        }));
    }

    ready.wait();
    let start = Instant::now();
    let readers: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let send_secs = start.elapsed().as_secs_f64();

    // Wait for every replica to have every op.
    let total = (args.clients * args.edits) as u64;
    let deadline = Instant::now() + Duration::from_secs(30);
    let converged = loop {
        let done = docs
            .iter()
            .all(|d| d.lock().unwrap().version().values().sum::<u64>() == total);
        if done || Instant::now() > deadline {
            break done;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    stop.store(true, Ordering::Relaxed);
    for r in readers {
        r.join().unwrap();
    }

    let texts: Vec<String> = docs.iter().map(|d| d.lock().unwrap().text()).collect();
    let server_text = http_get(&args.addr, &format!("/export/{}", args.doc));
    let identical = texts.iter().all(|t| *t == texts[0]) && server_text == texts[0];

    let mut lat = std::mem::take(&mut *latencies.lock().unwrap());
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let deliveries = lat.len();

    println!("| Metric | Value |");
    println!("|---|---|");
    println!("| Clients | {} |", args.clients);
    println!("| Ops sent | {total} |");
    println!("| Ops delivered to other clients | {deliveries} |");
    println!("| Send phase | {send_secs:.1} s |");
    println!("| Ops/sec into server | {:.0} |", total as f64 / send_secs);
    println!("| Deliveries/sec out of server | {:.0} |", deliveries as f64 / send_secs);
    println!("| Latency p50 | {:.2} ms |", percentile(&lat, 0.50));
    println!("| Latency p95 | {:.2} ms |", percentile(&lat, 0.95));
    println!("| Latency p99 | {:.2} ms |", percentile(&lat, 0.99));
    println!("| Latency max | {:.2} ms |", lat.last().copied().unwrap_or(f64::NAN));
    println!("| Final doc length | {} chars |", texts[0].chars().count());
    println!(
        "| All {} clients + server identical | {} |",
        args.clients,
        if converged && identical { "yes" } else { "NO" }
    );
    if !(converged && identical) {
        std::process::exit(1);
    }
}
