//! Upstream connections: bound to one phone's IP, with DNS through that phone's own gateway,
//! keep-alive pooling, and a tiny HTTP/1.1 head reader.
use crate::app::{App, Link};
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream, UdpSocket};
use tokio::time::timeout;

pub struct Up {
    pub s: TcpStream,
    pub host: String,
    pub port: u16,
    pub last: Instant,
}

pub fn err(kind: io::ErrorKind, msg: &str) -> io::Error {
    io::Error::new(kind, msg.to_string())
}

pub async fn with_timeout<T>(d: Duration, f: impl std::future::Future<Output = io::Result<T>>) -> io::Result<T> {
    match timeout(d, f).await {
        Ok(r) => r,
        Err(_) => Err(err(io::ErrorKind::TimedOut, "timed out")),
    }
}

// ---------- DNS through the phone's own gateway ----------
// Each carrier then gets the CDN edge closest to it, not the one nearest to whichever phone
// Windows happens to prefer.

fn dns_query(host: &str) -> Vec<u8> {
    let mut q = vec![0x5b, 0x0d, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in host.split('.') {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.extend_from_slice(&[0, 0, 1, 0, 1]);
    q
}

fn skip_name(p: &[u8], mut i: usize) -> Option<usize> {
    loop {
        let l = *p.get(i)? as usize;
        if l == 0 {
            return Some(i + 1);
        }
        if l & 0xC0 == 0xC0 {
            return Some(i + 2);
        }
        i += 1 + l;
    }
}

fn dns_parse(p: &[u8]) -> Option<(Ipv4Addr, u32)> {
    if p.len() < 12 || p[3] & 0x0F != 0 {
        return None;
    }
    let an = u16::from_be_bytes([p[6], p[7]]) as usize;
    let mut i = skip_name(p, 12)? + 4;
    for _ in 0..an {
        i = skip_name(p, i)?;
        let t = u16::from_be_bytes([*p.get(i)?, *p.get(i + 1)?]);
        let ttl = u32::from_be_bytes([*p.get(i + 4)?, *p.get(i + 5)?, *p.get(i + 6)?, *p.get(i + 7)?]);
        let rl = u16::from_be_bytes([*p.get(i + 8)?, *p.get(i + 9)?]) as usize;
        i += 10;
        if t == 1 && rl == 4 {
            return Some((Ipv4Addr::new(*p.get(i)?, *p.get(i + 1)?, *p.get(i + 2)?, *p.get(i + 3)?), ttl));
        }
        i += rl;
    }
    None
}

async fn dns_via_gateway(link: &Link, gw: Ipv4Addr, host: &str) -> io::Result<(Ipv4Addr, u32)> {
    let sock = UdpSocket::bind(SocketAddr::new(link.ip.into(), 0)).await?;
    sock.connect(SocketAddr::new(gw.into(), 53)).await?;
    sock.send(&dns_query(host)).await?;
    let mut buf = [0u8; 512];
    let n = with_timeout(Duration::from_millis(1500), sock.recv(&mut buf)).await?;
    dns_parse(&buf[..n]).ok_or_else(|| err(io::ErrorKind::NotFound, "no A record"))
}

pub async fn resolve(app: &App, link: &Link, host: &str) -> io::Result<Ipv4Addr> {
    let done = |a: Ipv4Addr| {
        if a.is_loopback() && !app.cfg.allow_loopback {
            Err(err(io::ErrorKind::Other, "loops back to us"))
        } else {
            Ok(a)
        }
    };
    if let Ok(a) = host.parse::<Ipv4Addr>() {
        return done(a);
    }
    if let Some((a, exp)) = link.dns.lock().unwrap().get(host).copied() {
        if exp > Instant::now() {
            return done(a);
        }
    }
    let (gw, dns_ok) = {
        let s = link.st.lock().unwrap();
        (s.gw, s.dns_ok)
    };
    if let (Some(gw), true) = (gw, dns_ok) {
        match dns_via_gateway(link, gw, host).await {
            Ok((a, ttl)) => {
                link.dns.lock().unwrap().insert(host.into(), (a, Instant::now() + Duration::from_secs(ttl.clamp(30, 300) as u64)));
                return done(a);
            }
            Err(e) if e.kind() == io::ErrorKind::TimedOut || e.kind() == io::ErrorKind::ConnectionRefused => {
                link.st.lock().unwrap().dns_ok = false; // this phone has no DNS relay
            }
            Err(_) => {}
        }
    }
    let addrs = with_timeout(Duration::from_secs(4), tokio::net::lookup_host((host, 0))).await?;
    let a = addrs
        .filter_map(|s| match s.ip() {
            std::net::IpAddr::V4(v) => Some(v),
            _ => None,
        })
        .next()
        .ok_or_else(|| err(io::ErrorKind::NotFound, "no IPv4 address"))?;
    link.dns.lock().unwrap().insert(host.into(), (a, Instant::now() + Duration::from_secs(60)));
    done(a)
}

// ---------- connections ----------

pub async fn dial(app: &App, link: &Arc<Link>, host: &str, port: u16) -> io::Result<Up> {
    let ip = resolve(app, link, host).await?;
    let sock = TcpSocket::new_v4()?;
    sock.bind(SocketAddr::new(link.ip.into(), 0))?;
    let _ = sock.set_recv_buffer_size(std::env::var("PB_RCVBUF").ok().and_then(|v| v.parse().ok()).unwrap_or(4 << 20));
    let s = with_timeout(Duration::from_secs(6), sock.connect(SocketAddr::new(ip.into(), port))).await?;
    s.set_nodelay(true)?;
    Ok(Up { s, host: host.to_string(), port, last: Instant::now() })
}

/// A warm keep-alive connection when there is one (`true`), else a fresh one.
pub async fn connect(app: &App, link: &Arc<Link>, host: &str, port: u16, fresh: bool) -> io::Result<(Up, bool)> {
    if !fresh {
        let mut pool = link.pool.lock().unwrap();
        while let Some(i) = pool.iter().rposition(|u| u.host == host && u.port == port) {
            let u = pool.swap_remove(i);
            if u.last.elapsed() < Duration::from_secs(15) {
                return Ok((u, true));
            }
        }
    }
    Ok((dial(app, link, host, port).await?, false))
}

pub fn pool_put(link: &Link, mut up: Up) {
    up.last = Instant::now();
    let mut pool = link.pool.lock().unwrap();
    pool.retain(|u| u.last.elapsed() < Duration::from_secs(15));
    if pool.len() >= 48 {
        pool.remove(0);
    }
    pool.push(up);
}

// ---------- HTTP/1.1 ----------

pub fn find(buf: &[u8], pat: &[u8]) -> Option<usize> {
    buf.windows(pat.len()).position(|w| w == pat)
}

/// Reads until the end of the header block; returns its length. Extra bytes stay in `buf`.
pub async fn read_head(s: &mut TcpStream, buf: &mut Vec<u8>, d: Duration) -> io::Result<usize> {
    let mut scanned = 0;
    loop {
        if let Some(p) = find(&buf[scanned.min(buf.len())..], b"\r\n\r\n") {
            return Ok(scanned.min(buf.len()) + p + 4);
        }
        scanned = buf.len().saturating_sub(3);
        if buf.len() > 64 * 1024 {
            return Err(err(io::ErrorKind::InvalidData, "header too large"));
        }
        let mut tmp = [0u8; 8192];
        let n = with_timeout(d, s.read(&mut tmp)).await?;
        if n == 0 {
            return Err(err(io::ErrorKind::UnexpectedEof, "closed"));
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

pub struct RHead {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
}

pub fn parse_resp(b: &[u8]) -> io::Result<RHead> {
    let mut h = [httparse::EMPTY_HEADER; 64];
    let mut r = httparse::Response::new(&mut h);
    match r.parse(b) {
        Ok(httparse::Status::Complete(_)) => Ok(RHead {
            status: r.code.unwrap_or(502),
            reason: r.reason.unwrap_or("").to_string(),
            headers: r
                .headers
                .iter()
                .map(|h| (h.name.to_string(), String::from_utf8_lossy(h.value).trim().to_string()))
                .collect(),
        }),
        _ => Err(err(io::ErrorKind::InvalidData, "bad response")),
    }
}

pub fn hget<'a>(h: &'a [(String, String)], name: &str) -> Option<&'a str> {
    h.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

pub const HOP: [&str; 8] =
    ["proxy-connection", "proxy-authorization", "connection", "keep-alive", "te", "trailer", "upgrade", "transfer-encoding"];

pub fn is_hop(name: &str) -> bool {
    HOP.iter().any(|h| name.eq_ignore_ascii_case(h))
}

/// `range` is inclusive on both ends.
pub async fn send_request(
    up: &mut Up,
    method: &str,
    path: &str,
    host_hdr: &str,
    fwd: &[(String, String)],
    range: Option<(usize, usize)>,
) -> io::Result<()> {
    let mut r = format!("{method} {path} HTTP/1.1\r\nHost: {host_hdr}\r\nConnection: keep-alive\r\n");
    for (k, v) in fwd {
        if is_hop(k) || k.eq_ignore_ascii_case("host") || (range.is_some() && k.eq_ignore_ascii_case("range")) {
            continue;
        }
        r.push_str(k);
        r.push_str(": ");
        r.push_str(v);
        r.push_str("\r\n");
    }
    if let Some((a, b)) = range {
        r.push_str(&format!("Range: bytes={a}-{b}\r\n"));
    }
    r.push_str("\r\n");
    with_timeout(Duration::from_secs(8), up.s.write_all(r.as_bytes())).await
}
