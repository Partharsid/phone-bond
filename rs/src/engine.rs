//! The download engine.
//!
//! Every Steam chunk is fetched into memory by one or more *workers*, each owning a byte range
//! `[cur, end)` of the chunk over its own connection on one phone. That one idea gives:
//!  * **splitting**   - a new chunk is cut in half across phones while total streams < target;
//!  * **tail racing** - a phone that runs out of work steals half of the biggest backlog;
//!  * **rescue**      - a crawling phone's remaining bytes are stolen by a fast one;
//!  * **resume**      - a worker that fails is retried from its last byte on another phone.
//! New requests go to the phone that would finish them soonest (pending bytes / measured speed).
use crate::app::*;
use crate::net::*;
use std::io;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::Notify;

const EST: f64 = 600e3; // assumed size of a request whose length we do not know yet
const MIN_STEAL: usize = 160 * 1024; // do not bother splitting less than this
const MAX_BUF: usize = 48 * 1024 * 1024;
const MAX_WORKERS: usize = 4;

#[derive(Clone)]
pub struct Req {
    pub method: String,
    pub host: String,
    pub port: u16,
    pub host_hdr: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub has_range: bool,
}

pub struct W {
    pub cur: usize,
    pub end: usize,
    pub link: Arc<Link>,
}
pub struct CI {
    pub buf: Vec<u8>,
    pub w: Vec<W>,
    pub failed: bool,
}
pub struct Chunk {
    pub ci: Mutex<CI>,
    pub notify: Notify,
    pub req: Arc<Req>,
}

fn all_done(ci: &CI) -> bool {
    ci.w.iter().all(|w| w.cur >= w.end)
}

pub struct StreamGuard(Arc<Link>);
impl StreamGuard {
    pub fn new(l: &Arc<Link>) -> Self {
        l.streams.fetch_add(1, Relaxed);
        StreamGuard(l.clone())
    }
}
impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.0.streams.fetch_sub(1, Relaxed);
    }
}

struct Registered(Arc<App>, Arc<Chunk>);
impl Drop for Registered {
    fn drop(&mut self) {
        self.0.chunks.lock().unwrap().retain(|c| !Arc::ptr_eq(c, &self.1));
    }
}

// ---------- choosing a phone ----------

/// Bytes still to come over each phone: unfinished worker ranges, plus a guess for requests
/// that are still waiting for their first reply.
fn pending(app: &App, links: &[Arc<Link>], with_reserved: bool) -> Vec<f64> {
    let mut p: Vec<f64> = links
        .iter()
        .map(|l| {
            l.extra.load(Relaxed).max(0) as f64 + if with_reserved { l.reserved.load(Relaxed).max(0) as f64 * EST } else { 0.0 }
        })
        .collect();
    for ch in app.chunks.lock().unwrap().iter() {
        let ci = ch.ci.lock().unwrap();
        for w in &ci.w {
            if w.cur < w.end {
                if let Some(i) = links.iter().position(|l| Arc::ptr_eq(l, &w.link)) {
                    p[i] += (w.end - w.cur) as f64;
                }
            }
        }
    }
    p
}

/// The phone that would finish a new request soonest.
pub fn choose(app: &App, exclude: Option<&Arc<Link>>) -> Option<Arc<Link>> {
    let links = app.links_vec();
    let pend = pending(app, &links, true);
    let ex = |l: &Arc<Link>| exclude.map_or(false, |e| Arc::ptr_eq(e, l));
    let pick = |ok: &dyn Fn(&Arc<Link>) -> bool| -> Option<usize> {
        let mut best: Option<(usize, f64)> = None;
        for (i, l) in links.iter().enumerate() {
            if !ok(l) {
                continue;
            }
            let score = (pend[i] + EST) / l.eff_rate();
            if best.map_or(true, |(_, s)| score < s) {
                best = Some((i, score));
            }
        }
        best.map(|b| b.0)
    };
    let i = pick(&|l| l.live() && !l.is_down() && !ex(l))
        .or_else(|| pick(&|l| l.live() && !ex(l)))
        .or_else(|| pick(&|l| l.live()))?;
    Some(links[i].clone())
}

// ---------- one HTTP exchange ----------

struct Opened {
    up: Up,
    pre: Vec<u8>,
    body_len: usize,
    keep: bool,
}

fn keeps_alive(h: &RHead) -> bool {
    !hget(&h.headers, "connection").map_or(false, |c| c.eq_ignore_ascii_case("close"))
}

async fn open_full(app: &App, link: &Arc<Link>, req: &Req) -> io::Result<(Up, RHead, Vec<u8>)> {
    let mut last = err(io::ErrorKind::Other, "no attempt");
    for attempt in 0..2 {
        let (mut up, pooled) = connect(app, link, &req.host, req.port, attempt == 1).await?;
        let r: io::Result<(RHead, Vec<u8>)> = async {
            send_request(&mut up, &req.method, &req.path, &req.host_hdr, &req.headers, None).await?;
            let mut buf = Vec::new();
            let hl = read_head(&mut up.s, &mut buf, Duration::from_secs(10)).await?;
            Ok((parse_resp(&buf[..hl])?, buf[hl..].to_vec()))
        }
        .await;
        match r {
            Ok((h, pre)) => return Ok((up, h, pre)),
            Err(e) if pooled && attempt == 0 => last = e, // stale keep-alive: retry on a fresh socket
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

async fn open_range(app: &App, ch: &Chunk, link: &Arc<Link>, start: usize, end: usize) -> io::Result<Opened> {
    let req = &ch.req;
    let mut last = err(io::ErrorKind::Other, "no attempt");
    for attempt in 0..2 {
        let (mut up, pooled) = connect(app, link, &req.host, req.port, attempt == 1).await?;
        let r: io::Result<(RHead, Vec<u8>)> = async {
            send_request(&mut up, "GET", &req.path, &req.host_hdr, &req.headers, Some((start, end - 1))).await?;
            let mut buf = Vec::new();
            let hl = read_head(&mut up.s, &mut buf, Duration::from_secs(10)).await?;
            Ok((parse_resp(&buf[..hl])?, buf[hl..].to_vec()))
        }
        .await;
        match r {
            Ok((h, pre)) => {
                if h.status == 200 {
                    return Err(err(io::ErrorKind::Unsupported, "server ignores Range"));
                }
                let len = hget(&h.headers, "content-length").and_then(|v| v.parse::<usize>().ok());
                if h.status != 206 || len != Some(end - start) {
                    return Err(err(io::ErrorKind::InvalidData, "unexpected range reply"));
                }
                let keep = keeps_alive(&h);
                return Ok(Opened { up, pre, body_len: end - start, keep });
            }
            Err(e) if pooled && attempt == 0 => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

/// Copies body bytes into the chunk buffer, never past this worker's (possibly shrunk) end.
/// Returns how many bytes were taken.
fn deposit(ch: &Chunk, wi: usize, data: &[u8]) -> usize {
    let mut ci = ch.ci.lock().unwrap();
    if ci.failed {
        return 0;
    }
    let (cur, end) = (ci.w[wi].cur, ci.w[wi].end);
    let k = data.len().min(end.saturating_sub(cur));
    ci.buf[cur..cur + k].copy_from_slice(&data[..k]);
    ci.w[wi].cur += k;
    let done = all_done(&ci);
    drop(ci);
    if done {
        ch.notify.notify_waiters();
    }
    k
}

/// Reads one response body into the chunk. Ok(true) = connection can be reused.
async fn stream_body(app: &App, ch: &Chunk, wi: usize, link: &Link, o: &mut Opened) -> io::Result<bool> {
    let mut consumed = 0usize;
    if !o.pre.is_empty() {
        let take = o.pre.len().min(o.body_len);
        link.bytes.fetch_add(take as u64, Relaxed);
        consumed += take;
        if deposit(ch, wi, &o.pre[..take]) < take {
            return Ok(false);
        }
    }
    let mut tmp = vec![0u8; 128 * 1024];
    while consumed < o.body_len {
        let want = (o.body_len - consumed).min(tmp.len());
        let n = with_timeout(app.cfg.stall, tokio::io::AsyncReadExt::read(&mut o.up.s, &mut tmp[..want])).await?;
        if n == 0 {
            return Err(err(io::ErrorKind::UnexpectedEof, "upstream closed"));
        }
        link.bytes.fetch_add(n as u64, Relaxed);
        consumed += n;
        if deposit(ch, wi, &tmp[..n]) < n {
            return Ok(false); // the rest of this range was taken over by another phone
        }
    }
    Ok(o.keep)
}

fn fail_chunk(ch: &Chunk) {
    ch.ci.lock().unwrap().failed = true;
    ch.notify.notify_waiters();
}

async fn worker(app: Arc<App>, ch: Arc<Chunk>, wi: usize, mut first: Option<Opened>) {
    let mut attempts = 0u32;
    loop {
        let (cur, end, link) = {
            let ci = ch.ci.lock().unwrap();
            if ci.failed {
                return;
            }
            (ci.w[wi].cur, ci.w[wi].end, ci.w[wi].link.clone())
        };
        if cur >= end {
            ch.notify.notify_waiters();
            return;
        }
        let f = first.take();
        let r: io::Result<(Opened, bool)> = async {
            let mut o = match f {
                Some(o) => o,
                None => open_range(&app, &ch, &link, cur, end).await?,
            };
            let _g = StreamGuard::new(&link);
            let reuse = stream_body(&app, &ch, wi, &link, &mut o).await?;
            Ok((o, reuse))
        }
        .await;
        match r {
            Ok((o, reuse)) => {
                attempts = 0;
                link.mark_ok();
                if reuse {
                    pool_put(&link, o.up);
                }
            }
            Err(e) => {
                if e.kind() == io::ErrorKind::Unsupported {
                    app.log(format!("{} ignores partial requests: not splitting its chunks", ch.req.host));
                    app.no_range.lock().unwrap().insert(ch.req.host.clone());
                    fail_chunk(&ch); // the caller falls back to a plain single download
                    return;
                }
                attempts += 1;
                link.mark_fail();
                let n = app.resumes.fetch_add(1, Relaxed);
                if n < 40 {
                    app.log(format!("{}: download piece failed ({}) on {}, retrying on another phone", link.name, e, ch.req.host));
                }
                if attempts > 5 {
                    fail_chunk(&ch);
                    return;
                }
                if let Some(l) = choose(&app, Some(&link)) {
                    ch.ci.lock().unwrap().w[wi].link = l;
                }
                tokio::time::sleep(Duration::from_millis(80)).await;
            }
        }
    }
}

/// Gives half of a worker's remaining bytes to a new worker on `link`.
pub fn steal(app: &Arc<App>, ch: &Arc<Chunk>, victim: Option<usize>, link: Arc<Link>) -> bool {
    let idx = {
        let mut ci = ch.ci.lock().unwrap();
        if ci.failed || ci.w.len() >= MAX_WORKERS {
            return false;
        }
        let vi = match victim {
            Some(v) => v,
            None => match ci.w.iter().enumerate().filter(|(_, w)| w.cur < w.end).max_by_key(|(_, w)| w.end - w.cur) {
                Some((i, _)) => i,
                None => return false,
            },
        };
        let rem = ci.w[vi].end.saturating_sub(ci.w[vi].cur);
        if rem < MIN_STEAL {
            return false;
        }
        let split = ci.w[vi].cur + rem / 2;
        let end = ci.w[vi].end;
        ci.w[vi].end = split;
        ci.w.push(W { cur: split, end, link });
        ci.w.len() - 1
    };
    app.steals.fetch_add(1, Relaxed);
    tokio::spawn(worker(app.clone(), ch.clone(), idx, None));
    true
}

/// Right after a chunk's first reply: while there is room under the stream target, cut it up.
fn split_new(app: &Arc<App>, ch: &Arc<Chunk>) {
    if app.no_range.lock().unwrap().contains(&ch.req.host) {
        return;
    }
    let mut spawned = 0i64;
    while spawned < 3 && app.total_streams() + spawned < app.target.load(Relaxed) as i64 {
        let Some(link) = choose(app, None) else { break };
        if !steal(app, ch, None, link) {
            break;
        }
        spawned += 1;
    }
}

/// Every 100 ms: a phone about to run out of work takes half of the biggest backlog on a slower one.
pub async fn scheduler(app: Arc<App>) {
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        tick.tick().await;
        let links = app.links_vec();
        if links.len() < 2 || app.total_streams() > app.target.load(Relaxed) as i64 + 16 {
            continue;
        }
        let pend = pending(&app, &links, false);
        let ect = |i: usize| pend[i] / links[i].eff_rate();
        let chunks: Vec<Arc<Chunk>> = app.chunks.lock().unwrap().clone();
        for (i, l) in links.iter().enumerate() {
            if !l.live() || l.is_down() || ect(i) > 0.25 {
                continue;
            }
            let mut best: Option<(Arc<Chunk>, usize, usize)> = None;
            for ch in &chunks {
                let ci = ch.ci.lock().unwrap();
                for (wi, w) in ci.w.iter().enumerate() {
                    let rem = w.end.saturating_sub(w.cur);
                    if rem < 2 * MIN_STEAL || Arc::ptr_eq(&w.link, l) {
                        continue;
                    }
                    let Some(j) = links.iter().position(|x| Arc::ptr_eq(x, &w.link)) else { continue };
                    if ect(j) < ect(i) + 0.6 {
                        continue;
                    }
                    if best.as_ref().map_or(true, |b| rem > b.2) {
                        best = Some((ch.clone(), wi, rem));
                    }
                }
            }
            if let Some((ch, wi, _)) = best {
                steal(&app, &ch, Some(wi), l.clone());
            }
        }
    }
}

// ---------- serving a client request ----------

async fn respond(client: &mut TcpStream, code: u16, text: &str) {
    let b = format!("HTTP/1.1 {code} {text}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let _ = client.write_all(b.as_bytes()).await;
}

fn head_for_client(h: &RHead, keep: bool, keep_te: bool) -> String {
    let mut s = format!("HTTP/1.1 {} {}\r\n", h.status, h.reason);
    for (k, v) in &h.headers {
        if is_hop(k) && !(keep_te && k.eq_ignore_ascii_case("transfer-encoding")) {
            continue;
        }
        s.push_str(k);
        s.push_str(": ");
        s.push_str(v);
        s.push_str("\r\n");
    }
    s.push_str(if keep { "Connection: keep-alive\r\n\r\n" } else { "Connection: close\r\n\r\n" });
    s
}

struct ExtraGuard(Arc<Link>, i64);
impl ExtraGuard {
    fn new(l: &Arc<Link>, bytes: i64) -> Self {
        l.extra.fetch_add(bytes, Relaxed);
        ExtraGuard(l.clone(), bytes)
    }
    fn sub(&mut self, n: usize) {
        self.0.extra.fetch_sub(n as i64, Relaxed);
        self.1 -= n as i64;
    }
}
impl Drop for ExtraGuard {
    fn drop(&mut self) {
        self.0.extra.fetch_sub(self.1, Relaxed);
    }
}

/// Handles one GET/HEAD for a client. Returns whether the client connection can be reused.
pub async fn serve(app: &Arc<App>, client: &mut TcpStream, req: Arc<Req>) -> bool {
    let plain_get = req.method == "GET" && !req.has_range;
    let known_no_range = app.no_range.lock().unwrap().contains(&req.host);
    let known_range = app.range_ok.lock().unwrap().contains(&req.host);
    // A server we have not met yet gets "Range: bytes=0-" on its first request. A 206 proves it can
    // be split, a 200 proves it cannot, and either way the reply is the whole chunk: nothing wasted.
    let probe = plain_get && !known_no_range && !known_range;
    let open_req: Arc<Req> = if probe {
        let mut r = (*req).clone();
        r.headers.push(("Range".into(), "bytes=0-".into()));
        Arc::new(r)
    } else {
        req.clone()
    };
    let mut opened = None;
    let mut last: Option<Arc<Link>> = None;
    for _ in 0..3 {
        let Some(link) = choose(app, last.as_ref()) else { break };
        link.reserved.fetch_add(1, Relaxed);
        let r = open_full(app, &link, &open_req).await;
        link.reserved.fetch_sub(1, Relaxed);
        match r {
            Ok((_, h, _)) if (502..=504).contains(&h.status) => {
                link.mark_fail();
                last = Some(link);
            }
            Ok(x) => {
                link.mark_ok();
                opened = Some((link, x));
                break;
            }
            Err(e) => {
                if app.resumes.fetch_add(1, Relaxed) < 40 {
                    app.log(format!("{}: could not start a download from {} ({})", link.name, req.host, e));
                }
                link.mark_fail();
                last = Some(link);
            }
        }
    }
    let Some((link, (up, mut h, pre))) = opened else {
        respond(client, 502, "Bad Gateway").await;
        return false;
    };
    let cl = hget(&h.headers, "content-length").and_then(|v| v.parse::<usize>().ok());
    if probe {
        if h.status == 206 {
            let total = hget(&h.headers, "content-range").and_then(|v| v.rsplit('/').next()).and_then(|v| v.parse::<usize>().ok());
            if total.is_some() && total == cl {
                app.range_ok.lock().unwrap().insert(req.host.clone());
                // Steam asked for the whole file, so answer it as a plain 200.
                h.status = 200;
                h.reason = "OK".into();
                h.headers.retain(|(k, _)| !k.eq_ignore_ascii_case("content-range"));
            }
        } else if h.status == 200 {
            let mut nr = app.no_range.lock().unwrap();
            if nr.is_empty() {
                app.log("This download server ignores partial requests: balancing whole chunks and racing slow ones instead");
            }
            nr.insert(req.host.clone());
        }
    }
    let chunked = hget(&h.headers, "transfer-encoding").is_some();
    let whole_ok = plain_get && h.status == 200 && !chunked && matches!(cl, Some(l) if l > 0 && l <= MAX_BUF);
    if whole_ok && app.range_ok.lock().unwrap().contains(&req.host) {
        let l = cl.unwrap();
        let ch = Arc::new(Chunk {
            ci: Mutex::new(CI { buf: vec![0u8; l], w: vec![W { cur: 0, end: l, link: link.clone() }], failed: false }),
            notify: Notify::new(),
            req: req.clone(),
        });
        app.chunks.lock().unwrap().push(ch.clone());
        let _reg = Registered(app.clone(), ch.clone());
        let keep = keeps_alive(&h);
        tokio::spawn(worker(app.clone(), ch.clone(), 0, Some(Opened { up, pre, body_len: l, keep })));
        split_new(app, &ch);
        loop {
            let notified = ch.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let ci = ch.ci.lock().unwrap();
                if ci.failed || all_done(&ci) {
                    break;
                }
            }
            let _ = tokio::time::timeout(Duration::from_secs(2), notified).await;
        }
        let body = {
            let mut ci = ch.ci.lock().unwrap();
            if ci.failed {
                None
            } else {
                Some(std::mem::take(&mut ci.buf))
            }
        };
        return match body {
            Some(b) => {
                let head = head_for_client(&h, true, false);
                client.write_all(head.as_bytes()).await.is_ok() && client.write_all(&b).await.is_ok()
            }
            None => buffered_single(app, client, &req, None).await,
        };
    }
    if whole_ok {
        // Server cannot do ranges: fetch the whole chunk over one phone, race it if it straggles.
        return buffered_single(app, client, &req, Some((link, up, h, pre))).await;
    }
    stream_through(app, client, &req, &link, up, h, pre).await
}

struct Whole {
    h: RHead,
    body: Vec<u8>,
    up: Up,
    link: Arc<Link>,
}

/// One whole 200 response over one phone. `first` is a reply that is already open.
async fn fetch_whole(app: &Arc<App>, req: &Req, link: Arc<Link>, first: Option<(Up, RHead, Vec<u8>)>) -> io::Result<Whole> {
    let (mut up, h, mut body) = match first {
        Some(x) => x,
        None => {
            link.reserved.fetch_add(1, Relaxed);
            let r = open_full(app, &link, req).await;
            link.reserved.fetch_sub(1, Relaxed);
            r?
        }
    };
    let total = hget(&h.headers, "content-length").and_then(|v| v.parse::<usize>().ok());
    let Some(total) = total.filter(|_| h.status == 200) else {
        return Err(err(io::ErrorKind::InvalidData, "unusable reply"));
    };
    let _g = StreamGuard::new(&link);
    let mut extra = ExtraGuard::new(&link, total.saturating_sub(body.len()) as i64);
    link.bytes.fetch_add(body.len() as u64, Relaxed);
    let mut tmp = vec![0u8; 128 * 1024];
    while body.len() < total {
        let want = (total - body.len()).min(tmp.len());
        let n = with_timeout(app.cfg.stall, tokio::io::AsyncReadExt::read(&mut up.s, &mut tmp[..want])).await?;
        if n == 0 {
            return Err(err(io::ErrorKind::UnexpectedEof, "upstream closed"));
        }
        link.bytes.fetch_add(n as u64, Relaxed);
        extra.sub(n);
        body.extend_from_slice(&tmp[..n]);
    }
    Ok(Whole { h, body, up, link })
}

/// Downloads a chunk; if it is taking much longer than chunks usually do, a second copy is raced on the
/// least busy other phone and the first one to finish wins (the loser is simply dropped).
async fn hedged(app: &Arc<App>, req: &Req, link: Arc<Link>, opened: Option<(Up, RHead, Vec<u8>)>) -> io::Result<Whole> {
    let delay = Duration::from_secs_f64((*app.chunk_secs.lock().unwrap() * 3.0).clamp(1.5, 8.0));
    let primary = fetch_whole(app, req, link.clone(), opened);
    tokio::pin!(primary);
    if let Ok(r) = tokio::time::timeout(delay, &mut primary).await {
        return r;
    }
    let other = match choose(app, Some(&link)) {
        Some(o) if !Arc::ptr_eq(&o, &link) && o.streams.load(Relaxed) < link.streams.load(Relaxed) => o,
        _ => return primary.await,
    };
    app.steals.fetch_add(1, Relaxed);
    let secondary = fetch_whole(app, req, other, None);
    tokio::pin!(secondary);
    let (mut pd, mut sd) = (false, false);
    let mut last_err;
    loop {
        tokio::select! {
            r = &mut primary, if !pd => { pd = true; match r { Ok(w) => return Ok(w), Err(e) => last_err = e } }
            r = &mut secondary, if !sd => { sd = true; match r { Ok(w) => return Ok(w), Err(e) => last_err = e } }
        }
        if pd && sd {
            return Err(last_err);
        }
    }
}

/// One whole download at a time, retried on the other phones, answered only when complete,
/// so Steam never sees a chunk cut short.
async fn buffered_single(
    app: &Arc<App>,
    client: &mut TcpStream,
    req: &Req,
    mut first: Option<(Arc<Link>, Up, RHead, Vec<u8>)>,
) -> bool {
    let mut last: Option<Arc<Link>> = None;
    for _ in 0..4 {
        let (link, opened) = match first.take() {
            Some((l, u, h, p)) => (l, Some((u, h, p))),
            None => {
                let Some(l) = choose(app, last.as_ref()) else { break };
                (l, None)
            }
        };
        let t0 = std::time::Instant::now();
        match hedged(app, req, link.clone(), opened).await {
            Ok(w) => {
                {
                    let mut c = app.chunk_secs.lock().unwrap();
                    *c = *c * 0.9 + t0.elapsed().as_secs_f64() * 0.1;
                }
                w.link.mark_ok();
                let sent = client.write_all(head_for_client(&w.h, true, false).as_bytes()).await.is_ok()
                    && client.write_all(&w.body).await.is_ok();
                if keeps_alive(&w.h) {
                    pool_put(&w.link, w.up);
                }
                return sent;
            }
            Err(e) => {
                link.mark_fail();
                if app.resumes.fetch_add(1, Relaxed) < 40 {
                    app.log(format!("{}: a download failed ({}), retrying on another phone", link.name, e));
                }
                last = Some(link);
            }
        }
    }
    respond(client, 502, "Bad Gateway").await;
    false
}

async fn stream_through(app: &App, client: &mut TcpStream, req: &Req, link: &Arc<Link>, mut up: Up, h: RHead, pre: Vec<u8>) -> bool {
    let cl = hget(&h.headers, "content-length").and_then(|v| v.parse::<usize>().ok());
    let chunked = hget(&h.headers, "transfer-encoding").is_some();
    let no_body = req.method == "HEAD" || h.status == 204 || h.status == 304 || (100..200).contains(&h.status);
    let keep_up = keeps_alive(&h);
    let _g = StreamGuard::new(link);
    if no_body {
        let ok = client.write_all(head_for_client(&h, true, false).as_bytes()).await.is_ok();
        if keep_up {
            pool_put(link, up);
        }
        return ok;
    }
    let client_keep = cl.is_some() && !chunked;
    if client.write_all(head_for_client(&h, client_keep, chunked).as_bytes()).await.is_err() {
        return false;
    }
    let mut sent = 0usize;
    if !pre.is_empty() {
        let take = cl.map_or(pre.len(), |c| pre.len().min(c));
        link.bytes.fetch_add(take as u64, Relaxed);
        if client.write_all(&pre[..take]).await.is_err() {
            return false;
        }
        sent += take;
    }
    let mut tmp = vec![0u8; 128 * 1024];
    loop {
        if let Some(c) = cl {
            if sent >= c {
                if keep_up {
                    pool_put(link, up);
                }
                return client_keep;
            }
        }
        let want = cl.map_or(tmp.len(), |c| (c - sent).min(tmp.len()));
        let n = match with_timeout(app.cfg.stall, tokio::io::AsyncReadExt::read(&mut up.s, &mut tmp[..want])).await {
            Ok(0) => return false,
            Ok(n) => n,
            Err(_) => {
                link.mark_fail();
                return false;
            }
        };
        link.bytes.fetch_add(n as u64, Relaxed);
        if client.write_all(&tmp[..n]).await.is_err() {
            return false;
        }
        sent += n;
    }
}
