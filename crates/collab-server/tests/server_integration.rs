//! End-to-end tests against a real server on a local port.

use collab_server::json::Json;
use collab_server::protocol::*;
use collab_server::server::{Config, Server};
use collab_server::ws::{Client, Message};
use crdt_core::{Doc, Op};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn start(cfg: Config) -> (String, Arc<Server>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let server = Server::new(cfg);
    let s = server.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let s = s.clone();
            std::thread::spawn(move || s.handle(stream));
        }
    });
    (addr, server)
}

fn mem() -> Config {
    Config { data_dir: None, static_dir: None, fsync: false }
}

/// A test peer: a WebSocket connection plus a local replica.
struct Peer {
    ws: Client,
    doc: Doc,
}

impl Peer {
    fn join(addr: &str, name: &str, doc: Doc) -> Peer {
        let mut ws = Client::connect(addr, "/ws").unwrap();
        ws.reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let hello = msg(
            "hello",
            vec![
                ("doc", Json::Str(name.into())),
                ("replica", Json::Num(doc.replica() as f64)),
                ("vv", vv_to_json(doc.version())),
            ],
        );
        ws.send_text(&hello.to_string()).unwrap();
        let mut p = Peer { ws, doc };
        let welcome = p.next_json(Duration::from_secs(5)).expect("welcome");
        assert_eq!(welcome.get("t").unwrap().as_str(), Some("welcome"));
        for op in ops_from_json(welcome.get("ops").unwrap()).unwrap() {
            p.doc.apply(op);
        }
        // Upload whatever the server is missing (offline edits).
        let server_vv = vv_from_json(welcome.get("vv")).unwrap();
        let missing = p.doc.ops_since(&server_vv);
        p.send(&missing);
        p
    }

    fn send(&mut self, ops: &[Op]) {
        if ops.is_empty() {
            return;
        }
        let m = msg("ops", vec![("ops", ops_to_json(ops))]);
        self.ws.send_text(&m.to_string()).unwrap();
    }

    fn type_text(&mut self, pos: usize, s: &str) {
        let ops = self.doc.insert_str(pos, s);
        self.send(&ops);
    }

    fn next_json(&mut self, timeout: Duration) -> Option<Json> {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            match self.ws.recv() {
                Ok(Message::Text(s)) => return Some(Json::parse(&s).unwrap()),
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => return None,
            }
        }
        None
    }

    /// Apply incoming ops until the doc text satisfies `done` or time runs out.
    fn pump_until(&mut self, done: impl Fn(&str) -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        while !done(&self.doc.text()) {
            assert!(Instant::now() < end, "timed out; text = {:?}", self.doc.text());
            if let Some(m) = self.next_json(Duration::from_millis(100)) {
                if m.get("t").unwrap().as_str() == Some("ops") {
                    for op in ops_from_json(m.get("ops").unwrap()).unwrap() {
                        self.doc.apply(op);
                    }
                }
            }
        }
    }
}

fn wait_for(f: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !f() {
        assert!(Instant::now() < end, "condition not reached");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn live_edits_reach_other_clients() {
    let (addr, server) = start(mem());
    let mut a = Peer::join(&addr, "live", Doc::new(11));
    let mut b = Peer::join(&addr, "live", Doc::new(22));
    a.type_text(0, "hello");
    b.pump_until(|t| t == "hello");
    b.type_text(5, " world");
    a.pump_until(|t| t == "hello world");
    wait_for(|| server.doc_text("live").as_deref() == Some("hello world"));
}

#[test]
fn offline_edits_merge_on_reconnect() {
    let (addr, server) = start(mem());
    let mut a = Peer::join(&addr, "offline", Doc::new(1));
    let mut b = Peer::join(&addr, "offline", Doc::new(2));
    a.type_text(0, "shared ");
    b.pump_until(|t| t == "shared ");

    // A goes offline (drops its connection) and keeps editing.
    let mut a_doc = a.doc.clone();
    drop(a);
    a_doc.insert_str(7, "offline-A");
    b.type_text(7, "online-B ");

    // A reconnects with its version vector; both sides catch up.
    let mut a = Peer::join(&addr, "offline", a_doc);
    a.pump_until(|t| t.contains("online-B") && t.contains("offline-A"));
    b.pump_until(|t| t.contains("offline-A"));
    assert_eq!(a.doc.text(), b.doc.text());
    let a_text = a.doc.text();
    wait_for(|| server.doc_text("offline") == Some(a_text.clone()));
}

#[test]
fn op_log_survives_restart_and_torn_writes() {
    let dir: PathBuf = std::env::temp_dir().join(format!("collab-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let cfg = || Config { data_dir: Some(dir.clone()), static_dir: None, fsync: true };

    {
        let (addr, server) = start(cfg());
        let mut a = Peer::join(&addr, "durable", Doc::new(5));
        a.type_text(0, "persist me");
        wait_for(|| server.doc_text("durable").as_deref() == Some("persist me"));
    }
    let log = dir.join("durable.oplog");
    // The log is written asynchronously (group commit); wait for all 10 ops.
    wait_for(|| std::fs::read_to_string(&log).map_or(false, |c| c.lines().count() == 10));

    // Simulate a crash mid-write: a partial JSON line at the end of the log.
    let mut content = std::fs::read_to_string(&log).unwrap();
    content.push_str("{\"id\":[99,5],\"s\":");
    std::fs::write(&log, content).unwrap();

    let (addr, server) = start(cfg());
    assert_eq!(server.doc_text("durable").as_deref(), Some("persist me"));
    let mut b = Peer::join(&addr, "durable", Doc::new(6));
    assert_eq!(b.doc.text(), "persist me");
    b.type_text(10, "!");
    wait_for(|| server.doc_text("durable").as_deref() == Some("persist me!"));
    wait_for(|| std::fs::read_to_string(&log).map_or(false, |c| c.ends_with("}\n") && c.lines().count() == 11));

    // The torn line was truncated, so a third start replays cleanly.
    let (_, server3) = start(cfg());
    assert_eq!(server3.doc_text("durable").as_deref(), Some("persist me!"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn presence_is_relayed_and_replayed_to_newcomers() {
    let (addr, _server) = start(mem());
    let mut a = Peer::join(&addr, "presence", Doc::new(1));
    let p = msg(
        "presence",
        vec![
            ("name", Json::Str("Alice".into())),
            ("color", Json::Str("#e11".into())),
            ("cursor", Json::Null),
        ],
    );
    a.ws.send_text(&p.to_string()).unwrap();
    std::thread::sleep(Duration::from_millis(100));

    // A newcomer gets Alice's presence inside its welcome message.
    let mut ws = Client::connect(&addr, "/ws").unwrap();
    ws.send_text(r#"{"t":"hello","doc":"presence","replica":2,"vv":{}}"#).unwrap();
    let Message::Text(w) = ws.recv().unwrap() else { panic!() };
    let w = Json::parse(&w).unwrap();
    let peers = w.get("peers").unwrap().as_arr().unwrap();
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].get("name").unwrap().as_str(), Some("Alice"));
}

#[test]
fn malformed_input_gets_error_and_disconnect() {
    let (addr, _server) = start(mem());
    for bad in [
        "not json",
        r#"{"t":"ops","ops":[]}"#,                     // ops before hello
        r#"{"t":"hello","doc":"../etc","replica":1}"#, // bad doc name
    ] {
        let mut ws = Client::connect(&addr, "/ws").unwrap();
        ws.send_text(bad).unwrap();
        let Message::Text(m) = ws.recv().unwrap() else { panic!("expected error for {bad}") };
        assert_eq!(Json::parse(&m).unwrap().get("t").unwrap().as_str(), Some("error"));
        assert!(matches!(ws.recv(), Ok(Message::Close) | Err(_)));
    }
}
