//! "Everywhere" mode for apps we cannot split: every new connection (HTTPS via the system proxy,
//! torrents and anything else via SOCKS5) goes to the phone with the most spare capacity, and the
//! bytes are counted live so the balancing and the dashboard see them as they happen.
use crate::app::*;
use crate::engine::StreamGuard;
use crate::net::*;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

/// Fewest open connections per unit of speed. Flows have no known size, so counting them is the
/// best available measure of load; a faster phone is allowed proportionally more of them.
pub fn choose_flow(app: &App, exclude: Option<&Arc<Link>>) -> Option<Arc<Link>> {
    let pick = |ok: &dyn Fn(&Arc<Link>) -> bool| {
        app.links_vec()
            .into_iter()
            .filter(|l| ok(l))
            .map(|l| {
                let load = l.streams.load(Relaxed).max(0) as f64 + l.reserved.load(Relaxed).max(0) as f64 + 1.0;
                (load / l.eff_rate(), l)
            })
            .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(_, l)| l)
    };
    let ex = |l: &Arc<Link>| exclude.map_or(false, |e| Arc::ptr_eq(e, l));
    pick(&|l| l.live() && !l.is_down() && !ex(l)).or_else(|| pick(&|l| l.live() && !ex(l))).or_else(|| pick(&|l| l.live()))
}

async fn copy_count<R: AsyncReadExt + Unpin, W: AsyncWriteExt + Unpin>(r: &mut R, w: &mut W, link: &Link) -> io::Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            let _ = w.shutdown().await;
            return Ok(());
        }
        link.bytes.fetch_add(n as u64, Relaxed);
        w.write_all(&buf[..n]).await?;
    }
}

/// Pipes two sockets together until either side is done; a half-close gets a minute of grace.
pub async fn relay(client: TcpStream, up: TcpStream, link: Arc<Link>) {
    let _g = StreamGuard::new(&link);
    let (mut cr, mut cw) = client.into_split();
    let (mut ur, mut uw) = up.into_split();
    let l1 = link.clone();
    let a = async move { copy_count(&mut cr, &mut uw, &l1).await };
    let b = async move { copy_count(&mut ur, &mut cw, &link).await };
    tokio::pin!(a);
    tokio::pin!(b);
    tokio::select! {
        r = &mut a => { if r.is_ok() { let _ = tokio::time::timeout(Duration::from_secs(60), &mut b).await; } }
        r = &mut b => { if r.is_ok() { let _ = tokio::time::timeout(Duration::from_secs(60), &mut a).await; } }
    }
}

/// Connects to host:port over the best phone, trying a second phone if the first fails.
pub async fn connect_flow(app: &Arc<App>, host: &str, port: u16) -> io::Result<(TcpStream, Arc<Link>)> {
    let mut last: Option<Arc<Link>> = None;
    let mut error = err(io::ErrorKind::NotConnected, "no phone available");
    for _ in 0..2 {
        let Some(link) = choose_flow(app, last.as_ref()) else { break };
        link.reserved.fetch_add(1, Relaxed);
        let r = dial(app, &link, host, port).await;
        link.reserved.fetch_sub(1, Relaxed);
        match r {
            Ok(up) => {
                link.mark_ok();
                return Ok((up.s, link));
            }
            Err(e) => {
                link.mark_fail();
                error = e;
                last = Some(link);
            }
        }
    }
    Err(error)
}

// ---------- SOCKS5 ----------

pub async fn socks_listen(app: Arc<App>, port: u16) -> io::Result<()> {
    let l = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = l.accept().await else { continue };
            let _ = s.set_nodelay(true);
            tokio::spawn(socks_client(app.clone(), s));
        }
    });
    Ok(())
}

async fn socks_reply(s: &mut TcpStream, code: u8) {
    let _ = s.write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0]).await;
}

async fn socks_client(app: Arc<App>, mut s: TcpStream) {
    let r: io::Result<()> = async {
        let mut h = [0u8; 2];
        with_timeout(Duration::from_secs(10), s.read_exact(&mut h)).await?;
        if h[0] != 5 {
            return Err(err(io::ErrorKind::InvalidData, "not socks5"));
        }
        let mut methods = vec![0u8; h[1] as usize];
        s.read_exact(&mut methods).await?;
        if !methods.contains(&0) {
            s.write_all(&[5, 0xFF]).await?;
            return Ok(());
        }
        s.write_all(&[5, 0]).await?;
        let mut q = [0u8; 4];
        s.read_exact(&mut q).await?;
        let host = match q[3] {
            1 => {
                let mut a = [0u8; 4];
                s.read_exact(&mut a).await?;
                std::net::Ipv4Addr::from(a).to_string()
            }
            3 => {
                let mut l = [0u8; 1];
                s.read_exact(&mut l).await?;
                let mut n = vec![0u8; l[0] as usize];
                s.read_exact(&mut n).await?;
                String::from_utf8_lossy(&n).to_string()
            }
            _ => {
                socks_reply(&mut s, 8).await; // IPv6 literals: phones here are IPv4-bound
                return Ok(());
            }
        };
        let mut p = [0u8; 2];
        s.read_exact(&mut p).await?;
        let port = u16::from_be_bytes(p);
        if q[1] == 3 {
            // UDP ASSOCIATE: torrent uTP/DHT, QUIC, voice chat...
            return udp_associate(app, s).await;
        }
        if q[1] != 1 {
            socks_reply(&mut s, 7).await; // BIND is not supported
            return Ok(());
        }
        match connect_flow(&app, &host, port).await {
            Ok((up, link)) => {
                socks_reply(&mut s, 0).await;
                relay(s, up, link).await;
            }
            Err(_) => socks_reply(&mut s, 4).await,
        }
        Ok(())
    }
    .await;
    let _ = r;
}

// ---------- SOCKS5 UDP ----------
// One association = one UDP socket on this PC for the app, and one bound to a phone for the internet.
// It lives as long as the app keeps its TCP control connection open.

/// Wraps a datagram from `from` in the SOCKS5 UDP header (IPv4 only).
fn udp_wrap(from: SocketAddr, data: &[u8]) -> Option<Vec<u8>> {
    let SocketAddr::V4(a) = from else { return None };
    let mut out = vec![0, 0, 0, 1];
    out.extend_from_slice(&a.ip().octets());
    out.extend_from_slice(&a.port().to_be_bytes());
    out.extend_from_slice(data);
    Some(out)
}

/// Splits a SOCKS5 UDP datagram into (host, port, payload offset).
fn udp_unwrap(p: &[u8]) -> Option<(String, u16, usize)> {
    if p.len() < 10 || p[2] != 0 {
        return None; // too short, or fragmented (not supported)
    }
    let (host, end) = match p[3] {
        1 => (std::net::Ipv4Addr::new(p[4], p[5], p[6], p[7]).to_string(), 8),
        3 => {
            let l = p[4] as usize;
            if p.len() < 5 + l + 2 {
                return None;
            }
            (String::from_utf8_lossy(&p[5..5 + l]).to_string(), 5 + l)
        }
        _ => return None,
    };
    let port = u16::from_be_bytes([*p.get(end)?, *p.get(end + 1)?]);
    Some((host, port, end + 2))
}

async fn udp_associate(app: Arc<App>, mut s: TcpStream) -> io::Result<()> {
    let Some(link) = choose_flow(&app, None) else {
        socks_reply(&mut s, 1).await;
        return Ok(());
    };
    let local = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
    let out = UdpSocket::bind(SocketAddr::new(link.ip.into(), 0)).await?;
    let port = local.local_addr()?.port();
    let mut r = vec![5, 0, 0, 1, 127, 0, 0, 1];
    r.extend_from_slice(&port.to_be_bytes());
    s.write_all(&r).await?;
    let _g = StreamGuard::new(&link);
    let mut client: Option<SocketAddr> = None;
    let (mut a, mut b, mut ctl) = (vec![0u8; 65536], vec![0u8; 65536], [0u8; 64]);
    loop {
        tokio::select! {
            // The app closing the control connection ends the association.
            n = s.read(&mut ctl) => { if matches!(n, Ok(0) | Err(_)) { return Ok(()); } }
            r = local.recv_from(&mut a) => {
                let Ok((n, from)) = r else { return Ok(()) };
                client = Some(from);
                let Some((host, port, off)) = udp_unwrap(&a[..n]) else { continue };
                let Ok(ip) = resolve(&app, &link, &host).await else { continue };
                link.bytes.fetch_add((n - off) as u64, Relaxed);
                let _ = out.send_to(&a[off..n], SocketAddr::new(ip.into(), port)).await;
            }
            r = out.recv_from(&mut b) => {
                let Ok((n, from)) = r else { continue };
                let (Some(c), Some(w)) = (client, udp_wrap(from, &b[..n])) else { continue };
                link.bytes.fetch_add(n as u64, Relaxed);
                let _ = local.send_to(&w, c).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn udp_header_round_trip() {
        let from: SocketAddr = "1.2.3.4:5678".parse().unwrap();
        let w = udp_wrap(from, b"hi").unwrap();
        let (h, p, off) = udp_unwrap(&w).unwrap();
        assert_eq!((h.as_str(), p, &w[off..]), ("1.2.3.4", 5678, &b"hi"[..]));
    }
    #[test]
    fn udp_domain_and_fragments() {
        let mut d = vec![0, 0, 0, 3, 7];
        d.extend_from_slice(b"example");
        d.extend_from_slice(&[0, 53, 9]);
        let (h, p, off) = udp_unwrap(&d).unwrap();
        assert_eq!((h.as_str(), p, d[off]), ("example", 53, 9));
        d[2] = 1; // fragmented: refused
        assert!(udp_unwrap(&d).is_none());
    }
}
