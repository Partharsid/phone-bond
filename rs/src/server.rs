//! The listener Steam (cache mode, port 80) and proxy clients (port 8118) talk to.
use crate::app::*;
use crate::engine::{self, Req};
use crate::net::*;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

pub async fn bind(addr: SocketAddr) -> io::Result<TcpListener> {
    TcpListener::bind(addr).await
}

pub async fn accept_loop(app: Arc<App>, l: TcpListener) {
    loop {
        let Ok((s, _)) = l.accept().await else {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        let _ = s.set_nodelay(true);
        tokio::spawn(client(app.clone(), s));
    }
}

async fn reply(s: &mut TcpStream, code: u16, text: &str) {
    let b = format!("HTTP/1.1 {code} {text}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let _ = s.write_all(b.as_bytes()).await;
}

fn build_req(app: &App, method: &str, target: &str, headers: Vec<(String, String)>) -> Option<Req> {
    let (host_hdr, path) = if target.starts_with('/') {
        // Steam cache request: path only, the real CDN server is in the Host header.
        let h = hget(&headers, "host")?.to_string();
        let low = h.to_lowercase();
        let loops = low.starts_with("lancache.steamcontent.com") || low.starts_with("localhost") || low.starts_with("127.");
        if loops && !app.cfg.allow_loopback {
            return None; // would loop back to us
        }
        (h, target.to_string())
    } else if let Some(rest) = target.strip_prefix("http://") {
        match rest.find('/') {
            Some(i) => (rest[..i].to_string(), rest[i..].to_string()),
            None => (rest.to_string(), "/".to_string()),
        }
    } else {
        return None;
    };
    let (host, port) = match host_hdr.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() => (h.to_string(), p.parse().unwrap()),
        _ => (host_hdr.clone(), 80),
    };
    if app.seen.lock().unwrap().insert(host.clone()) {
        app.log(format!("Steam is downloading from {host}"));
    }
    let has_range = hget(&headers, "range").is_some();
    Some(Req { method: method.to_string(), host, port, host_hdr, path, headers, has_range })
}

async fn client(app: Arc<App>, mut s: TcpStream) {
    let mut buf = Vec::new();
    loop {
        let hl = match read_head(&mut s, &mut buf, Duration::from_secs(300)).await {
            Ok(n) => n,
            Err(_) => return,
        };
        let head: Vec<u8> = buf.drain(..hl).collect();
        let (method, target, headers) = {
            let mut hs = [httparse::EMPTY_HEADER; 64];
            let mut r = httparse::Request::new(&mut hs);
            if r.parse(&head).is_err() {
                return reply(&mut s, 400, "Bad Request").await;
            }
            (
                r.method.unwrap_or("").to_string(),
                r.path.unwrap_or("").to_string(),
                r.headers.iter().map(|h| (h.name.to_string(), String::from_utf8_lossy(h.value).trim().to_string())).collect::<Vec<_>>(),
            )
        };
        if method == "CONNECT" {
            return tunnel(&app, s, &target).await;
        }
        if method != "GET" && method != "HEAD" {
            return reply(&mut s, 501, "Not Implemented").await;
        }
        let Some(req) = build_req(&app, &method, &target, headers) else {
            return reply(&mut s, 404, "Not Found").await;
        };
        if !engine::serve(&app, &mut s, Arc::new(req)).await {
            return;
        }
    }
}

/// HTTPS and anything else tunnelled via CONNECT (system proxy mode): one phone per connection.
async fn tunnel(app: &Arc<App>, mut s: TcpStream, target: &str) {
    let (host, port) = match target.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(443)),
        None => (target.to_string(), 443),
    };
    match crate::flow::connect_flow(app, &host, port).await {
        Ok((up, link)) => {
            if s.write_all(b"HTTP/1.1 200 Connection Established

").await.is_err() {
                return;
            }
            crate::flow::relay(s, up, link).await;
        }
        Err(_) => reply(&mut s, 502, "Bad Gateway").await,
    }
}
