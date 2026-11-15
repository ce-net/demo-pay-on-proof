//! Just enough HTTP/1.1 for a local board: one request per connection, Content-Length bodies.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    pub body: Vec<u8>,
}

impl Request {
    pub fn param(&self, name: &str) -> Option<&str> {
        self.query.split('&').find_map(|kv| kv.strip_prefix(name)?.strip_prefix('='))
    }
}

pub struct Response {
    pub status: u16,
    pub kind: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(v: &serde_json::Value) -> Self {
        Response { status: 200, kind: "application/json", body: serde_json::to_vec(v).unwrap() }
    }
    pub fn bytes(kind: &'static str, body: Vec<u8>) -> Self {
        Response { status: 200, kind, body }
    }
    pub fn status(status: u16, msg: &str) -> Self {
        Response { status, kind: "text/plain; charset=utf-8", body: msg.as_bytes().to_vec() }
    }
}

fn read_request(stream: &TcpStream) -> std::io::Result<Request> {
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
    }
    if len > 16 << 20 {
        return Err(std::io::Error::other("body too large"));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    Ok(Request { method, path, query, body })
}

/// Serve forever on `listener`, one thread per connection.
pub fn serve<F>(listener: TcpListener, handler: F)
where
    F: Fn(Request) -> Response + Send + Sync + 'static,
{
    let handler = Arc::new(handler);
    for stream in listener.incoming().flatten() {
        let handler = handler.clone();
        std::thread::spawn(move || {
            let mut stream = stream;
            let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
            let Ok(req) = read_request(&stream) else { return };
            let res = handler(req);
            let reason = match res.status {
                200 => "OK",
                202 => "Accepted",
                404 => "Not Found",
                _ => "Error",
            };
            let head = format!(
                "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
                res.status,
                reason,
                res.kind,
                res.body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&res.body);
        });
    }
}

/// POST a JSON body to `http://host:port/path` and return the response body.
pub fn post_json(url: &str, body: &serde_json::Value) -> Result<serde_json::Value, String> {
    let rest = url.strip_prefix("http://").ok_or("only http:// URLs")?;
    let (hostport, path) = match rest.split_once('/') {
        Some((h, p)) => (h, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    let mut s = TcpStream::connect(hostport).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let data = serde_json::to_vec(body).unwrap();
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {hostport}\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        data.len()
    );
    s.write_all(head.as_bytes()).map_err(|e| e.to_string())?;
    s.write_all(&data).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    s.read_to_end(&mut out).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&out);
    let (_, body) = text.split_once("\r\n\r\n").ok_or("no HTTP body")?;
    serde_json::from_str(body).map_err(|e| e.to_string())
}
