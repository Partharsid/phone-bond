//! Fast download: paste any http(s) link and phone-bond downloads it itself over all phones.
//! The file is cut into pieces; every phone runs several connections that each pull one piece
//! at a time (a faster phone simply finishes more pieces). The last pieces are raced on two phones.
//! Works for HTTPS too, because here phone-bond is the client, so nothing has to be intercepted.
use crate::app::*;
use crate::engine::{choose, StreamGuard};
use crate::net::*;
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
type Conn = Box<dyn Io>;

pub struct Dl {
    pub id: u64,
    pub url: String,
    pub name: Mutex<String>,
    pub path: Mutex<String>,
    pub total: AtomicU64,
    pub done: AtomicU64,
    pub live: AtomicU64,
    pub speed: Mutex<f64>,
    pub status: Mutex<(&'static str, String)>,
    pub cancel: AtomicBool,
    pub per_link: Mutex<HashMap<String, u64>>,
    pub started: Instant,
    pub ended: Mutex<Option<Instant>>,
}

impl Dl {
    pub fn json(&self) -> serde_json::Value {
        let st = self.status.lock().unwrap();
        let secs = self.ended.lock().unwrap().unwrap_or_else(Instant::now).duration_since(self.started).as_secs_f64();
        let mut links: Vec<(String, u64)> = self.per_link.lock().unwrap().iter().map(|(k, v)| (k.clone(), *v)).collect();
        links.sort();
        serde_json::json!({
            "id": self.id, "name": *self.name.lock().unwrap(), "url": self.url, "status": st.0, "msg": st.1,
            "total": self.total.load(Relaxed), "done": self.done.load(Relaxed), "speed": *self.speed.lock().unwrap(),
            "secs": secs, "links": links.iter().map(|(n, b)| serde_json::json!({"name": n, "bytes": b})).collect::<Vec<_>>(),
        })
    }
    fn set(&self, status: &'static str, msg: impl Into<String>) {
        *self.status.lock().unwrap() = (status, msg.into());
        if matches!(status, "done" | "error" | "cancelled") {
            *self.ended.lock().unwrap() = Some(Instant::now());
        }
    }
    fn live_add(&self, link: &Link, n: usize) {
        self.live.fetch_add(n as u64, Relaxed);
        *self.per_link.lock().unwrap().entry(link.name.clone()).or_insert(0) += n as u64;
    }
}

// ---------- URLs ----------

#[derive(Clone)]
struct Target {
    tls: bool,
    host: String,
    port: u16,
    path: String,
}

impl Target {
    fn host_hdr(&self) -> String {
        if (self.tls && self.port == 443) || (!self.tls && self.port == 80) {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

fn parse_url(u: &str) -> Option<Target> {
    let u = u.trim();
    let (tls, rest) = if let Some(r) = u.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = u.strip_prefix("http://") {
        (false, r)
    } else {
        return None;
    };
    let rest = rest.split('#').next()?;
    let (hp, path) = match rest.find(['/', '?']) {
        Some(i) if rest.as_bytes()[i] == b'/' => (&rest[..i], rest[i..].to_string()),
        Some(i) => (&rest[..i], format!("/{}", &rest[i..])),
        None => (rest, "/".to_string()),
    };
    let hp = hp.rsplit('@').next()?;
    let (host, port) = match hp.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() => (h.to_string(), p.parse().ok()?),
        _ => (hp.to_string(), if tls { 443 } else { 80 }),
    };
    if host.is_empty() {
        return None;
    }
    Some(Target { tls, host, port, path })
}

fn redirect_target(cur: &Target, loc: &str) -> Option<Target> {
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return parse_url(loc);
    }
    let scheme = if cur.tls { "https" } else { "http" };
    if let Some(r) = loc.strip_prefix("//") {
        return parse_url(&format!("{scheme}://{r}"));
    }
    let path = if loc.starts_with('/') {
        loc.to_string()
    } else {
        let dir = cur.path.rsplit_once('/').map_or("", |x| x.0);
        format!("{dir}/{loc}")
    };
    Some(Target { path, ..cur.clone() })
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() + 0 && i + 2 <= b.len() - 1 + 0 {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn file_name(t: &Target, h: &RHead) -> String {
    let mut name = String::new();
    if let Some(cd) = hget(&h.headers, "content-disposition") {
        if let Some(i) = cd.to_lowercase().find("filename*=") {
            let v = cd[i + 10..].split(';').next().unwrap_or("").trim().trim_matches('"');
            name = pct_decode(v.rsplit("''").next().unwrap_or(v));
        } else if let Some(i) = cd.to_lowercase().find("filename=") {
            name = cd[i + 9..].split(';').next().unwrap_or("").trim().trim_matches('"').to_string();
        }
    }
    if name.is_empty() {
        name = pct_decode(t.path.split('?').next().unwrap_or("").rsplit('/').next().unwrap_or(""));
    }
    let clean: String = name.chars().filter(|c| !r#"<>:"/\|?*"#.contains(*c) && !c.is_control()).collect();
    let clean = clean.trim().trim_matches('.').to_string();
    if clean.is_empty() { "download".into() } else { clean }
}

fn unique_path(name: &str) -> PathBuf {
    let dir = PathBuf::from(std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into())).join("Downloads");
    let _ = std::fs::create_dir_all(&dir);
    let p = PathBuf::from(name);
    let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "download".into());
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let mut c = dir.join(name);
    let mut n = 1;
    while c.exists() || PathBuf::from(format!("{}.phonebond", c.display())).exists() {
        c = dir.join(format!("{stem} ({n}){ext}"));
        n += 1;
    }
    c
}

// ---------- connections ----------

fn tls_config() -> Arc<rustls::ClientConfig> {
    static C: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    C.get_or_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Arc::new(
            rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .expect("tls versions")
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    })
    .clone()
}

async fn open(app: &App, link: &Arc<Link>, t: &Target) -> io::Result<Conn> {
    let up = dial(app, link, &t.host, t.port).await?;
    if !t.tls {
        return Ok(Box::new(up.s));
    }
    let name = rustls::pki_types::ServerName::try_from(t.host.clone()).map_err(|_| err(io::ErrorKind::InvalidInput, "bad host name"))?;
    let s = with_timeout(Duration::from_secs(10), tokio_rustls::TlsConnector::from(tls_config()).connect(name, up.s)).await?;
    Ok(Box::new(s))
}

async fn read_head_io(c: &mut Conn, buf: &mut Vec<u8>) -> io::Result<usize> {
    let mut scanned = 0usize;
    loop {
        if let Some(p) = find(&buf[scanned.min(buf.len())..], b"\r\n\r\n") {
            return Ok(scanned.min(buf.len()) + p + 4);
        }
        scanned = buf.len().saturating_sub(3);
        if buf.len() > 64 * 1024 {
            return Err(err(io::ErrorKind::InvalidData, "header too large"));
        }
        let mut tmp = [0u8; 8192];
        let n = with_timeout(Duration::from_secs(12), c.read(&mut tmp)).await?;
        if n == 0 {
            return Err(err(io::ErrorKind::UnexpectedEof, "closed"));
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

async fn send_get(c: &mut Conn, t: &Target, range: Option<(u64, u64)>) -> io::Result<()> {
    let mut r = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: Mozilla/5.0 (compatible; phone-bond/0.4)\r\nAccept: */*\r\nAccept-Encoding: identity\r\nConnection: keep-alive\r\n",
        t.path,
        t.host_hdr()
    );
    if let Some((a, b)) = range {
        r.push_str(&format!("Range: bytes={a}-{b}\r\n"));
    }
    r.push_str("\r\n");
    with_timeout(Duration::from_secs(10), c.write_all(r.as_bytes())).await
}

// ---------- the download ----------

pub fn start(app: &Arc<App>, url: String) -> Result<u64, String> {
    if parse_url(&url).is_none() {
        return Err("That doesn't look like an http:// or https:// link.".into());
    }
    let id = app.next_dl.fetch_add(1, Relaxed);
    let dl = Arc::new(Dl {
        id,
        url: url.trim().to_string(),
        name: Mutex::new(String::new()),
        path: Mutex::new(String::new()),
        total: AtomicU64::new(0),
        done: AtomicU64::new(0),
        live: AtomicU64::new(0),
        speed: Mutex::new(0.0),
        status: Mutex::new(("starting", "Contacting the server".into())),
        cancel: AtomicBool::new(false),
        per_link: Mutex::new(HashMap::new()),
        started: Instant::now(),
        ended: Mutex::new(None),
    });
    {
        let mut d = app.downloads.lock().unwrap();
        d.push(dl.clone());
        let excess = d.len().saturating_sub(8);
        d.drain(..excess);
    }
    let app = app.clone();
    tokio::spawn(async move {
        if let Err(e) = run(&app, &dl).await {
            if dl.cancel.load(Relaxed) {
                dl.set("cancelled", "Cancelled");
            } else {
                dl.set("error", e.to_string());
            }
            let p = dl.path.lock().unwrap().clone();
            if !p.is_empty() {
                let _ = std::fs::remove_file(format!("{p}.phonebond"));
            }
        }
    });
    Ok(id)
}

struct Probe {
    target: Target,
    total: Option<u64>,
    ranged: bool,
    name: String,
    conn: Option<(Conn, Arc<Link>, Vec<u8>)>, // open reply when the server does not do ranges
}

async fn probe(app: &Arc<App>, dl: &Dl) -> io::Result<Probe> {
    let mut t = parse_url(&dl.url).ok_or_else(|| err(io::ErrorKind::InvalidInput, "bad link"))?;
    let mut last: Option<Arc<Link>> = None;
    for _hop in 0..8 {
        let mut reply = None;
        for _try in 0..3 {
            let Some(link) = choose(app, last.as_ref()) else { return Err(err(io::ErrorKind::NotConnected, "no phone available")) };
            let r: io::Result<(Conn, RHead, Vec<u8>)> = async {
                let mut c = open(app, &link, &t).await?;
                send_get(&mut c, &t, Some((0, 0))).await?;
                let mut buf = Vec::new();
                let hl = read_head_io(&mut c, &mut buf).await?;
                Ok((c, parse_resp(&buf[..hl])?, buf[hl..].to_vec()))
            }
            .await;
            match r {
                Ok(x) => {
                    reply = Some((x, link));
                    break;
                }
                Err(e) => {
                    link.mark_fail();
                    if _try == 2 {
                        return Err(e);
                    }
                    last = Some(link);
                }
            }
        }
        let ((c, h, pre), link) = reply.ok_or_else(|| err(io::ErrorKind::Other, "no reply"))?;
        match h.status {
            301 | 302 | 303 | 307 | 308 => {
                let loc = hget(&h.headers, "location").ok_or_else(|| err(io::ErrorKind::InvalidData, "redirect without a location"))?;
                t = redirect_target(&t, loc).ok_or_else(|| err(io::ErrorKind::InvalidData, "bad redirect"))?;
            }
            206 => {
                let total = hget(&h.headers, "content-range").and_then(|v| v.rsplit('/').next()).and_then(|v| v.parse::<u64>().ok());
                return Ok(Probe { name: file_name(&t, &h), target: t, total, ranged: total.is_some(), conn: None });
            }
            200 => {
                let total = hget(&h.headers, "content-length").and_then(|v| v.parse::<u64>().ok());
                return Ok(Probe { name: file_name(&t, &h), target: t, total, ranged: false, conn: Some((c, link, pre)) });
            }
            s => return Err(err(io::ErrorKind::Other, &format!("The server said HTTP {s}"))),
        }
    }
    Err(err(io::ErrorKind::Other, "too many redirects"))
}

struct Shared {
    app: Arc<App>,
    dl: Arc<Dl>,
    t: Target,
    total: u64,
    piece: u64,
    n: usize,
    state: Vec<AtomicU8>, // 0 waiting, 1 being fetched, 2 done
    dups: Vec<AtomicU8>,
    cursor: AtomicUsize,
    file: Arc<std::fs::File>,
}

impl Shared {
    fn range(&self, i: usize) -> (u64, u64) {
        let a = i as u64 * self.piece;
        (a, (a + self.piece).min(self.total) - 1)
    }
    fn take(&self) -> Option<usize> {
        loop {
            let i = self.cursor.fetch_add(1, Relaxed);
            if i >= self.n {
                break;
            }
            if self.state[i].compare_exchange(0, 1, Relaxed, Relaxed).is_ok() {
                return Some(i);
            }
        }
        for i in 0..self.n {
            if self.state[i].compare_exchange(0, 1, Relaxed, Relaxed).is_ok() {
                return Some(i);
            }
        }
        // Nothing left to start: race a piece that is still on its way, on a different phone.
        for i in 0..self.n {
            if self.state[i].load(Relaxed) == 1 && self.dups[i].compare_exchange(0, 1, Relaxed, Relaxed).is_ok() {
                return Some(i);
            }
        }
        None
    }
}

#[cfg(windows)]
fn write_at(f: &std::fs::File, buf: &[u8], off: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0usize;
    while done < buf.len() {
        done += f.seek_write(&buf[done..], off + done as u64)?;
    }
    Ok(())
}
#[cfg(not(windows))]
fn write_at(f: &std::fs::File, buf: &[u8], off: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.write_all_at(buf, off)
}

async fn run(app: &Arc<App>, dl: &Arc<Dl>) -> io::Result<()> {
    let p = probe(app, dl).await?;
    *dl.name.lock().unwrap() = p.name.clone();
    let path = unique_path(&p.name);
    *dl.path.lock().unwrap() = path.to_string_lossy().to_string();
    let part = format!("{}.phonebond", path.display());
    let file = Arc::new(std::fs::File::create(&part)?);
    dl.total.store(p.total.unwrap_or(0), Relaxed);
    let links: Vec<Arc<Link>> = app.links_vec().into_iter().filter(|l| l.live()).collect();
    app.log(format!("Fast download: {} ({})", p.name, p.total.map_or("unknown size".into(), |t| format!("{:.1} MB", t as f64 / 1048576.0))));

    if !p.ranged {
        // The server cannot give parts of the file: one stream over the best phone.
        dl.set("downloading", "This server sends the file in one piece, so only one phone is used");
        let (mut c, link, pre) = match p.conn {
            Some(x) => x,
            None => return Err(err(io::ErrorKind::Other, "no connection")),
        };
        let _g = StreamGuard::new(&link);
        let mut off = 0u64;
        write_at(&file, &pre, 0)?;
        off += pre.len() as u64;
        dl.live_add(&link, pre.len());
        let mut buf = vec![0u8; 128 * 1024];
        let sup = tokio::spawn(speed_loop(dl.clone()));
        loop {
            if dl.cancel.load(Relaxed) {
                sup.abort();
                return Err(err(io::ErrorKind::Other, "cancelled"));
            }
            if p.total.map_or(false, |t| off >= t) {
                break;
            }
            let n = match with_timeout(app.cfg.stall, c.read(&mut buf)).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    sup.abort();
                    return Err(e);
                }
            };
            link.bytes.fetch_add(n as u64, Relaxed);
            dl.live_add(&link, n);
            write_at(&file, &buf[..n], off)?;
            off += n as u64;
            dl.done.store(off, Relaxed);
        }
        sup.abort();
        drop(file);
        std::fs::rename(&part, &path)?;
        dl.done.store(off, Relaxed);
        dl.total.store(off, Relaxed);
        dl.set("done", "Done");
        return Ok(());
    }

    let total = p.total.unwrap_or(0);
    if total == 0 {
        return Err(err(io::ErrorKind::InvalidData, "empty file"));
    }
    file.set_len(total)?;
    let piece = (total / 48).clamp(1 << 20, 8 << 20);
    let n = total.div_ceil(piece) as usize;
    let sh = Arc::new(Shared {
        app: app.clone(),
        dl: dl.clone(),
        t: p.target.clone(),
        total,
        piece,
        n,
        state: (0..n).map(|_| AtomicU8::new(0)).collect(),
        dups: (0..n).map(|_| AtomicU8::new(0)).collect(),
        cursor: AtomicUsize::new(0),
        file,
    });
    dl.set("downloading", format!("{} phone(s), {} pieces", links.len(), n));
    let per = std::env::var("PB_CONNS").ok().and_then(|v| v.parse().ok()).unwrap_or(if total > 64 << 20 { 8 } else { 4 });
    let mut tasks = Vec::new();
    for l in &links {
        // Connections open on this phone. Shrinks by itself when the server says "too many".
        let open_conns = Arc::new(AtomicUsize::new(per));
        for _ in 0..per {
            tasks.push(tokio::spawn(worker(sh.clone(), l.clone(), open_conns.clone())));
        }
    }
    let sup = tokio::spawn(speed_loop(dl.clone()));
    for t in tasks {
        let _ = t.await;
    }
    sup.abort();
    if dl.cancel.load(Relaxed) {
        return Err(err(io::ErrorKind::Other, "cancelled"));
    }
    if dl.done.load(Relaxed) < total {
        return Err(err(io::ErrorKind::Other, "The download stopped early: the server refused the phones or kept failing"));
    }
    drop(sh);
    std::fs::rename(&part, &path)?;
    dl.set("done", "Done");
    app.log(format!("Fast download finished: {}", p.name));
    Ok(())
}

async fn speed_loop(dl: Arc<Dl>) {
    let mut last = 0u64;
    let mut t = tokio::time::interval(Duration::from_millis(500));
    loop {
        t.tick().await;
        let now = dl.live.load(Relaxed);
        let inst = (now - last) as f64 * 2.0;
        last = now;
        let mut s = dl.speed.lock().unwrap();
        *s = *s * 0.6 + inst * 0.4;
    }
}

enum PieceErr {
    Superseded,
    /// HTTP 429/503: the server wants fewer connections from this address.
    Busy(u64),
    Failed(io::Error),
}

async fn worker(sh: Arc<Shared>, link: Arc<Link>, open_conns: Arc<AtomicUsize>) {
    let mut conn: Option<Conn> = None;
    let mut fails = 0u32;
    loop {
        if sh.dl.cancel.load(Relaxed) {
            return;
        }
        let Some(i) = sh.take() else { return };
        let (a, b) = sh.range(i);
        match fetch_piece(&sh, &link, &mut conn, i, a, b).await {
            Ok(data) => {
                fails = 0;
                if sh.state[i].swap(2, Relaxed) != 2 {
                    let f = sh.file.clone();
                    let len = data.len() as u64;
                    let ok = tokio::task::spawn_blocking(move || write_at(&f, &data, a)).await.map_or(false, |r| r.is_ok());
                    if ok {
                        sh.dl.done.fetch_add(len, Relaxed);
                    } else {
                        sh.dl.cancel.store(true, Relaxed);
                        sh.dl.set("error", "Could not write the file (disk full?)");
                        return;
                    }
                }
            }
            Err(PieceErr::Superseded) => conn = None,
            Err(PieceErr::Busy(wait)) => {
                conn = None;
                let _ = sh.state[i].compare_exchange(1, 0, Relaxed, Relaxed);
                // Too many connections for this server: close this one for good, but always keep two.
                if open_conns.load(Relaxed) > 2 {
                    open_conns.fetch_sub(1, Relaxed);
                    return;
                }
                tokio::time::sleep(Duration::from_secs(wait.clamp(1, 15))).await;
            }
            Err(PieceErr::Failed(e)) => {
                conn = None;
                fails += 1;
                let _ = sh.state[i].compare_exchange(1, 0, Relaxed, Relaxed);
                if fails >= 6 {
                    open_conns.fetch_sub(1, Relaxed);
                    sh.app.log(format!("{}: giving up on this download ({e})", link.name));
                    return;
                }
                tokio::time::sleep(Duration::from_millis(400 * fails as u64)).await;
            }
        }
    }
}

async fn fetch_piece(sh: &Shared, link: &Arc<Link>, conn: &mut Option<Conn>, i: usize, a: u64, b: u64) -> Result<Vec<u8>, PieceErr> {
    let want = (b - a + 1) as usize;
    let _g = StreamGuard::new(link);
    for attempt in 0..2 {
        let fresh = conn.is_none();
        if fresh {
            *conn = Some(open(&sh.app, link, &sh.t).await.map_err(PieceErr::Failed)?);
        }
        let c = conn.as_mut().unwrap();
        let r: Result<(Vec<u8>, bool), PieceErr> = async {
            send_get(c, &sh.t, Some((a, b))).await.map_err(PieceErr::Failed)?;
            let mut buf = Vec::new();
            let hl = read_head_io(c, &mut buf).await.map_err(PieceErr::Failed)?;
            let h = parse_resp(&buf[..hl]).map_err(PieceErr::Failed)?;
            let len = hget(&h.headers, "content-length").and_then(|v| v.parse::<usize>().ok());
            if h.status == 429 || h.status == 503 {
                let wait = hget(&h.headers, "retry-after").and_then(|v| v.parse::<u64>().ok()).unwrap_or(2);
                return Err(PieceErr::Busy(wait));
            }
            if h.status != 206 || len != Some(want) {
                return Err(PieceErr::Failed(err(io::ErrorKind::InvalidData, &format!("unexpected reply HTTP {}", h.status))));
            }
            let keep = !hget(&h.headers, "connection").map_or(false, |v| v.eq_ignore_ascii_case("close"));
            let mut body = Vec::with_capacity(want);
            body.extend_from_slice(&buf[hl..]);
            link.bytes.fetch_add(body.len() as u64, Relaxed);
            sh.dl.live_add(link, body.len());
            let mut tmp = vec![0u8; 128 * 1024];
            while body.len() < want {
                if sh.dl.cancel.load(Relaxed) || sh.state[i].load(Relaxed) == 2 {
                    return Err(PieceErr::Superseded);
                }
                let k = (want - body.len()).min(tmp.len());
                let n = with_timeout(sh.app.cfg.stall, c.read(&mut tmp[..k])).await.map_err(PieceErr::Failed)?;
                if n == 0 {
                    return Err(PieceErr::Failed(err(io::ErrorKind::UnexpectedEof, "closed mid-piece")));
                }
                link.bytes.fetch_add(n as u64, Relaxed);
                sh.dl.live_add(link, n);
                body.extend_from_slice(&tmp[..n]);
            }
            body.truncate(want);
            Ok((body, keep))
        }
        .await;
        match r {
            Ok((body, keep)) => {
                if !keep {
                    *conn = None;
                }
                return Ok(body);
            }
            Err(PieceErr::Failed(e)) => {
                *conn = None;
                if fresh || attempt == 1 {
                    return Err(PieceErr::Failed(e));
                }
            }
            Err(e @ (PieceErr::Superseded | PieceErr::Busy(_))) => return Err(e),
        }
    }
    Err(PieceErr::Failed(err(io::ErrorKind::Other, "no attempt")))
}
