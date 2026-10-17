//! Sync server: one thread per connection, one lock per document ("room").
//!
//! Correctness rules:
//! - The server holds a full CRDT replica per document. It never authors ops; it
//!   merges what clients send, persists new ops, and relays them.
//! - Join is atomic with broadcasts: a client is registered and handed
//!   `ops_since(its vv)` under the room lock, and every broadcast also happens
//!   under that lock, so a joining client can never miss an op.
//! - Each client has a bounded outbox. A client that can't keep up is
//!   disconnected instead of growing server memory; it resyncs on reconnect.
//! - The op log is append-only (one JSON line per op), written by a background
//!   thread per document with group commit: whatever accumulated while the last
//!   write/fsync ran goes out in the next single write/fsync. The room lock is
//!   never held across disk I/O. A torn last line from a crash is truncated on
//!   startup.
//! - Durability: a relayed op may not be on the server's disk yet. If the
//!   server crashes, the op still exists on the author's (and other clients')
//!   replicas, and the version-vector handshake re-uploads it on reconnect.

use crate::json::Json;
use crate::protocol::*;
use crate::ws::{self, Message};
use crdt_core::{ApplyResult, Doc};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{Shutdown, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, sync_channel, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Frames queued per client before it is considered too slow.
const OUTBOX: usize = 4096;
/// Ops a document may hold in its causal buffer (missing dependencies).
const MAX_PENDING: usize = 10_000;
const MAX_OPS_PER_MESSAGE: usize = 50_000;
pub const MAX_CONNECTIONS: usize = 4000;

type Frame = Arc<Vec<u8>>;

pub struct Config {
    pub data_dir: Option<PathBuf>,
    pub static_dir: Option<PathBuf>,
    pub fsync: bool,
}

struct Client {
    tx: SyncSender<Frame>,
    stream: TcpStream,
}

struct Room {
    doc: Doc,
    clients: HashMap<u64, Client>,
    presence: HashMap<u64, Json>,
    log: Option<Sender<String>>,
}

/// Group-commit writer for one document's op log.
fn spawn_log_writer(mut f: File, fsync: bool, name: String) -> Sender<String> {
    let (tx, rx) = channel::<String>();
    std::thread::spawn(move || {
        while let Ok(mut batch) = rx.recv() {
            while let Ok(more) = rx.try_recv() {
                batch.push_str(&more);
            }
            let res = f
                .write_all(batch.as_bytes())
                .and_then(|_| if fsync { f.sync_data() } else { Ok(()) });
            if let Err(e) = res {
                eprintln!("{name}: op log write failed: {e}");
            }
        }
    });
    tx
}

impl Room {
    /// Send to every client except `except`; kick clients whose outbox is full.
    fn broadcast(&mut self, except: u64, frame: Frame) {
        let mut dead = Vec::new();
        for (&id, c) in &self.clients {
            if id == except {
                continue;
            }
            match c.tx.try_send(frame.clone()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    let _ = c.stream.shutdown(Shutdown::Both);
                    dead.push(id);
                }
                Err(TrySendError::Disconnected(_)) => dead.push(id),
            }
        }
        for id in dead {
            self.clients.remove(&id);
            self.presence.remove(&id);
        }
    }
}

pub struct Server {
    cfg: Config,
    rooms: Mutex<HashMap<String, Arc<Mutex<Room>>>>,
    next_conn: AtomicU64,
    pub connections: AtomicUsize,
}

fn frame_of(j: &Json) -> Frame {
    Arc::new(ws::text_frame(&j.to_string()))
}

fn valid_doc_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

impl Server {
    pub fn new(cfg: Config) -> Arc<Server> {
        if let Some(d) = &cfg.data_dir {
            std::fs::create_dir_all(d).expect("create data dir");
        }
        Arc::new(Server {
            cfg,
            rooms: Mutex::new(HashMap::new()),
            next_conn: AtomicU64::new(1),
            connections: AtomicUsize::new(0),
        })
    }

    fn room(&self, name: &str) -> io::Result<Arc<Mutex<Room>>> {
        let mut rooms = self.rooms.lock().unwrap();
        if let Some(r) = rooms.get(name) {
            return Ok(r.clone());
        }
        let mut doc = Doc::new(0);
        let mut log = None;
        if let Some(dir) = &self.cfg.data_dir {
            let path = dir.join(format!("{name}.oplog"));
            let mut f = OpenOptions::new()
                .read(true)
                .append(true)
                .create(true)
                .open(&path)?;
            let mut content = String::new();
            f.read_to_string(&mut content)?;
            // Drop a torn trailing line (crash mid-write) so appends stay valid.
            let good_len = content.rfind('\n').map_or(0, |i| i + 1);
            if good_len < content.len() {
                eprintln!("{name}: truncating torn final line in op log");
                f.set_len(good_len as u64)?;
                f.seek(SeekFrom::End(0))?;
            }
            let (mut loaded, mut bad) = (0, 0);
            for line in content[..good_len].lines() {
                match Json::parse(line).and_then(|j| op_from_json(&j)) {
                    Ok(op) => {
                        doc.apply(op);
                        loaded += 1;
                    }
                    Err(_) => bad += 1,
                }
            }
            if loaded + bad > 0 {
                eprintln!("{name}: loaded {loaded} ops ({bad} unreadable)");
            }
            log = Some(spawn_log_writer(f, self.cfg.fsync, name.to_string()));
        }
        let room = Arc::new(Mutex::new(Room {
            doc,
            clients: HashMap::new(),
            presence: HashMap::new(),
            log,
        }));
        rooms.insert(name.to_string(), room.clone());
        Ok(room)
    }

    /// Current text of a document (for export and tests).
    pub fn doc_text(&self, name: &str) -> Option<String> {
        if !valid_doc_name(name) {
            return None;
        }
        self.room(name).ok().map(|r| r.lock().unwrap().doc.text())
    }

    // ------------------------------------------------------------ HTTP

    pub fn handle(self: &Arc<Self>, stream: TcpStream) {
        let _ = stream.set_nodelay(true);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
        let mut reader = match stream.try_clone() {
            Ok(s) => BufReader::new(s),
            Err(_) => return,
        };
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            return;
        }
        let mut headers = HashMap::new();
        for _ in 0..64 {
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }
        let mut parts = request_line.split_whitespace();
        let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
        let path = target.split('?').next().unwrap_or("/");

        let is_upgrade = headers
            .get("upgrade")
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
        if method == "GET" && path == "/ws" && is_upgrade {
            if let Some(key) = headers.get("sec-websocket-key") {
                let resp = format!(
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
                     Connection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
                    ws::accept_key(key)
                );
                let mut w = &stream;
                if w.write_all(resp.as_bytes()).is_ok() {
                    let _ = stream.set_read_timeout(None);
                    self.session(stream, reader);
                }
                return;
            }
        }
        if method != "GET" {
            return respond(&stream, "405 Method Not Allowed", "text/plain", b"method not allowed");
        }
        if path == "/health" {
            return respond(&stream, "200 OK", "text/plain", b"ok");
        }
        if let Some(name) = path.strip_prefix("/export/") {
            return match self.doc_text(name) {
                Some(t) => respond(&stream, "200 OK", "text/plain; charset=utf-8", t.as_bytes()),
                None => respond(&stream, "400 Bad Request", "text/plain", b"bad doc name"),
            };
        }
        self.serve_static(&stream, path);
    }

    fn serve_static(&self, stream: &TcpStream, path: &str) {
        let Some(root) = &self.cfg.static_dir else {
            return respond(stream, "404 Not Found", "text/plain", b"not found");
        };
        let rel = if path == "/" { "index.html" } else { path.trim_start_matches('/') };
        if rel.split('/').any(|seg| seg == ".." || seg.starts_with('.')) {
            return respond(stream, "400 Bad Request", "text/plain", b"bad path");
        }
        let file = root.join(rel);
        let ctype = match file.extension().and_then(|e| e.to_str()) {
            Some("html") => "text/html; charset=utf-8",
            Some("js") => "text/javascript; charset=utf-8",
            Some("css") => "text/css; charset=utf-8",
            Some("svg") => "image/svg+xml",
            Some("json") | Some("map") => "application/json",
            _ => "application/octet-stream",
        };
        match std::fs::read(&file) {
            Ok(body) => respond(stream, "200 OK", ctype, &body),
            Err(_) => respond(stream, "404 Not Found", "text/plain", b"not found"),
        }
    }

    // ------------------------------------------------------------ WebSocket

    fn session(self: &Arc<Self>, stream: TcpStream, mut reader: BufReader<TcpStream>) {
        let conn = self.next_conn.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = sync_channel::<Frame>(OUTBOX);
        let Ok(mut out) = stream.try_clone() else { return };
        let writer = std::thread::spawn(move || {
            // Coalesce queued frames into one write: far fewer syscalls under load.
            let mut buf = Vec::new();
            while let Ok(frame) = rx.recv() {
                buf.clear();
                buf.extend_from_slice(&frame);
                while buf.len() < 64 * 1024 {
                    match rx.try_recv() {
                        Ok(f) => buf.extend_from_slice(&f),
                        Err(_) => break,
                    }
                }
                if out.write_all(&buf).is_err() {
                    break;
                }
            }
            let _ = out.shutdown(Shutdown::Both);
        });

        let mut joined: Option<(Arc<Mutex<Room>>, u64)> = None;
        loop {
            match ws::read_message(&mut reader) {
                Ok(Message::Text(s)) => {
                    if let Err(e) = self.on_text(conn, &tx, &stream, &mut joined, &s) {
                        let err = msg("error", vec![("msg", Json::Str(e))]);
                        let _ = tx.try_send(frame_of(&err));
                        break;
                    }
                }
                Ok(Message::Ping(p)) => {
                    let _ = tx.try_send(Arc::new(ws::encode_frame(ws::OP_PONG, &p, None)));
                }
                Ok(Message::Pong(_)) | Ok(Message::Binary(_)) => {}
                Ok(Message::Close) | Err(_) => break,
            }
        }

        if let Some((room, replica)) = joined {
            let mut r = room.lock().unwrap();
            r.clients.remove(&conn);
            r.presence.remove(&conn);
            let leave = msg("leave", vec![("replica", Json::Num(replica as f64))]);
            r.broadcast(conn, frame_of(&leave));
        }
        let _ = tx.try_send(Arc::new(ws::encode_frame(ws::OP_CLOSE, &[], None)));
        drop(tx);
        let _ = writer.join();
    }

    fn on_text(
        &self,
        conn: u64,
        tx: &SyncSender<Frame>,
        stream: &TcpStream,
        joined: &mut Option<(Arc<Mutex<Room>>, u64)>,
        text: &str,
    ) -> Result<(), String> {
        let m = Json::parse(text)?;
        match m.get("t").and_then(Json::as_str) {
            Some("hello") => {
                if joined.is_some() {
                    return Err("already joined".into());
                }
                let name = m.get("doc").and_then(Json::as_str).unwrap_or("");
                if !valid_doc_name(name) {
                    return Err("doc name must be 1-64 chars of [A-Za-z0-9_-]".into());
                }
                let replica = m.get("replica").and_then(Json::as_u64).ok_or("bad replica")?;
                let vv = vv_from_json(m.get("vv"))?;
                let room = self.room(name).map_err(|e| e.to_string())?;
                {
                    let mut r = room.lock().unwrap();
                    let welcome = msg(
                        "welcome",
                        vec![
                            ("ops", ops_to_json(&r.doc.ops_since(&vv))),
                            ("vv", vv_to_json(r.doc.version())),
                            ("peers", Json::Arr(r.presence.values().cloned().collect())),
                        ],
                    );
                    tx.try_send(frame_of(&welcome)).map_err(|_| "outbox full")?;
                    let stream = stream.try_clone().map_err(|e| e.to_string())?;
                    r.clients.insert(conn, Client { tx: tx.clone(), stream });
                }
                *joined = Some((room, replica));
                Ok(())
            }
            Some("ops") => {
                let (room, _) = joined.as_ref().ok_or("send hello first")?;
                let ops = ops_from_json(m.get("ops").ok_or("missing ops")?)?;
                if ops.len() > MAX_OPS_PER_MESSAGE {
                    return Err("too many ops in one message".into());
                }
                let mut r = room.lock().unwrap();
                let mut fresh = Vec::new();
                for op in ops {
                    if r.doc.apply(op.clone()) != ApplyResult::Duplicate {
                        fresh.push(op);
                    }
                }
                if r.doc.pending_len() > MAX_PENDING {
                    return Err("too many ops with missing dependencies".into());
                }
                if fresh.is_empty() {
                    return Ok(());
                }
                if let Some(log) = &r.log {
                    let mut buf = String::new();
                    for op in &fresh {
                        op_to_json(op).write(&mut buf);
                        buf.push('\n');
                    }
                    let _ = log.send(buf);
                }
                let out = msg("ops", vec![("ops", ops_to_json(&fresh))]);
                r.broadcast(conn, frame_of(&out));
                Ok(())
            }
            Some("presence") => {
                let (room, replica) = joined.as_ref().ok_or("send hello first")?;
                let s = |k: &str, n: usize| {
                    Json::Str(truncate_chars(m.get(k).and_then(Json::as_str).unwrap_or(""), n))
                };
                let cursor = match m.get("cursor") {
                    None | Some(Json::Null) => Json::Null,
                    Some(c) => id_to_json(id_from_json(c)?),
                };
                let p = msg(
                    "presence",
                    vec![
                        ("replica", Json::Num(*replica as f64)),
                        ("name", s("name", 40)),
                        ("color", s("color", 20)),
                        ("cursor", cursor),
                    ],
                );
                let mut r = room.lock().unwrap();
                r.presence.insert(conn, p.clone());
                r.broadcast(conn, frame_of(&p));
                Ok(())
            }
            _ => Err("unknown message type".into()),
        }
    }
}

fn respond(stream: &TcpStream, status: &str, ctype: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
         Cache-Control: no-cache\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut w = stream;
    let _ = w.write_all(head.as_bytes()).and_then(|_| w.write_all(body));
}
