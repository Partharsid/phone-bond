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
use tokio::net::{TcpListener, TcpStream};

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
        if q[1] != 1 {
            socks_reply(&mut s, 7).await; // only CONNECT (torrent peers, HTTPS, chat apps...)
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
