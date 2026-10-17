use collab_server::server::{Config, Server, MAX_CONNECTIONS};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

const USAGE: &str = "collab-server [--host 0.0.0.0] [--port 8080] [--data ./data | --no-persist] \
[--static ./web] [--fsync]

  --port      also read from $PORT
  --data      directory for append-only op logs (default ./data)
  --no-persist  keep documents in memory only
  --static    directory served over HTTP (the web client)
  --fsync     fsync the op log after every write (durable, slower)";

fn main() {
    let mut host = "0.0.0.0".to_string();
    let mut port = std::env::var("PORT").unwrap_or_else(|_| "8080".into());
    let mut data = Some(PathBuf::from("data"));
    let mut static_dir = Some(PathBuf::from("web")).filter(|p| p.is_dir());
    let mut fsync = false;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| panic!("{a} needs a value\n\n{USAGE}"));
        match a.as_str() {
            "--host" => host = val(),
            "--port" => port = val(),
            "--data" => data = Some(PathBuf::from(val())),
            "--no-persist" => data = None,
            "--static" => static_dir = Some(PathBuf::from(val())),
            "--fsync" => fsync = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            _ => panic!("unknown argument {a}\n\n{USAGE}"),
        }
    }

    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr).unwrap_or_else(|e| panic!("bind {addr}: {e}"));
    eprintln!(
        "collab-server listening on http://{addr}  (data: {}, static: {}, fsync: {fsync})",
        data.as_ref().map_or("memory only".into(), |d| d.display().to_string()),
        static_dir.as_ref().map_or("none".into(), |d| d.display().to_string()),
    );

    let server = Server::new(Config {
        data_dir: data,
        static_dir,
        fsync,
    });

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if server.connections.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
            drop(stream);
            continue;
        }
        let server = server.clone();
        server.connections.fetch_add(1, Ordering::Relaxed);
        std::thread::spawn(move || {
            server.handle(stream);
            server.connections.fetch_sub(1, Ordering::Relaxed);
        });
    }
}
