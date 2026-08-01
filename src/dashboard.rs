//! The built-in dashboard: serves the same 4-screen UI as the standalone kiosk
//! (embedded at compile time) plus a /stats JSON. Point any browser — or a
//! full-screen kiosk on a small display — at http://<node>:8137/.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use crate::config::DASHBOARD_PORT;
use crate::state::Shared;

const HTML: &str = include_str!("../assets/dashboard.html");

/// Serve an external ~/.sermonindex/dashboard.html if present (so the display can
/// be tweaked without recompiling), otherwise the built-in one.
fn page_html() -> String {
    let ext = crate::config::data_dir().join("dashboard.html");
    std::fs::read_to_string(&ext).unwrap_or_else(|_| HTML.to_string())
}

/// Blocking server — run on its own OS thread.
pub fn serve(shared: Arc<Shared>) {
    let addr = format!("0.0.0.0:{DASHBOARD_PORT}");
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[dashboard] cannot bind {addr}: {e}");
            return;
        }
    };
    eprintln!("[dashboard] serving on http://localhost:{DASHBOARD_PORT}/");
    for stream in listener.incoming().flatten() {
        let shared = shared.clone();
        std::thread::spawn(move || {
            let _ = handle(stream, &shared);
        });
    }
}

fn handle(mut stream: TcpStream, shared: &Arc<Shared>) -> std::io::Result<()> {
    let mut buf = [0u8; 2048];
    let n = stream.read(&mut buf)?;
    let req = String::from_utf8_lossy(&buf[..n]);
    let path = req.split_whitespace().nth(1).unwrap_or("/");

    let (ctype, body) = if path.starts_with("/stats") {
        (
            "application/json",
            shared.stats_json.lock().unwrap().clone(),
        )
    } else {
        ("text/html; charset=utf-8", page_html())
    };

    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}
