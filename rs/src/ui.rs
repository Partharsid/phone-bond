//! The dashboard (ui.html over a local-only HTTP + SSE server), the speed test, and quitting.
use crate::app::*;
use crate::net::*;
use crate::sys;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const UI_HTML: &str = include_str!("../../ui.html");

pub async fn serve(app: Arc<App>) -> std::io::Result<()> {
    let l = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], app.cfg.ui_port))).await?;
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = l.accept().await else { continue };
            tokio::spawn(conn(app.clone(), s));
        }
    });
    Ok(())
}

struct ClientGuard(Arc<App>);
impl Drop for ClientGuard {
    fn drop(&mut self) {
        if self.0.ui_clients.fetch_sub(1, Relaxed) == 1 {
            *self.0.lonely_since.lock().unwrap() = Some(Instant::now());
        }
    }
}

async fn send(s: &mut TcpStream, code: u16, ctype: &str, body: &str) {
    let h = format!(
        "HTTP/1.1 {code} X\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = s.write_all(h.as_bytes()).await;
    let _ = s.write_all(body.as_bytes()).await;
}

async fn conn(app: Arc<App>, mut s: TcpStream) {
    let mut buf = Vec::new();
    let Ok(hl) = read_head(&mut s, &mut buf, Duration::from_secs(10)).await else { return };
    let (method, path, headers) = {
        let mut hs = [httparse::EMPTY_HEADER; 48];
        let mut r = httparse::Request::new(&mut hs);
        if r.parse(&buf[..hl]).is_err() {
            return;
        }
        (
            r.method.unwrap_or("").to_string(),
            r.path.unwrap_or("").to_string(),
            r.headers.iter().map(|h| (h.name.to_string(), String::from_utf8_lossy(h.value).trim().to_string())).collect::<Vec<_>>(),
        )
    };
    // Only our own window may talk to us: reject other sites (DNS rebinding / CSRF).
    let port = app.cfg.ui_port;
    let host = hget(&headers, "host").unwrap_or("");
    if host != format!("127.0.0.1:{port}") && host != format!("localhost:{port}") {
        return send(&mut s, 403, "text/plain", "").await;
    }
    match (method.as_str(), path.as_str()) {
        ("GET", "/") => send(&mut s, 200, "text/html; charset=utf-8", UI_HTML).await,
        ("GET", "/api/ping") => send(&mut s, 200, "text/plain", "phone-bond").await,
        ("GET", "/events") => {
            app.ui_clients.fetch_add(1, Relaxed);
            *app.lonely_since.lock().unwrap() = None;
            let _g = ClientGuard(app.clone());
            let mut rx = app.tx.subscribe();
            let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: keep-alive\r\n\r\n";
            if s.write_all(head.as_bytes()).await.is_err() || s.write_all(format!("data: {}\n\n", app.snapshot()).as_bytes()).await.is_err() {
                return;
            }
            loop {
                match rx.recv().await {
                    Ok(m) => {
                        if s.write_all(format!("data: {m}\n\n").as_bytes()).await.is_err() {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                }
            }
        }
        ("POST", p) if hget(&headers, "x-bond") == Some("1") => {
            let len = hget(&headers, "content-length").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0).min(10_000);
            let mut body = buf[hl..].to_vec();
            while body.len() < len {
                let mut tmp = [0u8; 2048];
                match s.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => body.extend_from_slice(&tmp[..n]),
                }
            }
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let mut extra = json!({});
            match p {
                "/api/link" => {
                    let ip = v["ip"].as_str().unwrap_or("");
                    let links = app.links_vec();
                    if let Some(l) = links.iter().find(|l| l.ip.to_string() == ip) {
                        let want = v["enabled"].as_bool().unwrap_or(false);
                        let others = links.iter().filter(|x| x.ip != l.ip && x.live()).count();
                        if want || others > 0 {
                            l.enabled.store(want, Relaxed);
                            app.log(format!("{} {}", l.name, if want { "turned on" } else { "paused" }));
                        }
                    }
                }
                "/api/steam" => {
                    let r = if v["on"].as_bool().unwrap_or(false) {
                        sys::steam_on(&app, true).await
                    } else {
                        sys::steam_off(&app, true).await;
                        true
                    };
                    extra = json!({ "result": r });
                }
                "/api/proxy" => {
                    if v["on"].as_bool().unwrap_or(false) {
                        sys::proxy_on(&app)
                    } else {
                        sys::proxy_off(&app)
                    }
                }
                "/api/restart-steam" => {
                    let a = app.clone();
                    tokio::spawn(async move { sys::restart_steam(&a).await });
                }
                "/api/test" => {
                    let ip = v["ip"].as_str().map(|x| x.to_string());
                    tokio::spawn(run_test(app.clone(), ip));
                }
                "/api/fetch" => {
                    extra = match crate::fetch::start(&app, v["url"].as_str().unwrap_or("").to_string()) {
                        Ok(id) => json!({ "id": id }),
                        Err(e) => json!({ "error": e }),
                    };
                }
                "/api/fetch-cancel" | "/api/fetch-pause" | "/api/fetch-resume" | "/api/fetch-open" | "/api/fetch-clear" => {
                    let id = v["id"].as_u64().unwrap_or(0);
                    let found = app.downloads.lock().unwrap().iter().find(|d| d.id == id).cloned();
                    if p == "/api/fetch-clear" {
                        app.downloads.lock().unwrap().retain(|d| !matches!(d.status.lock().unwrap().0, "done" | "error" | "cancelled"));
                    } else if let Some(d) = found {
                        match p {
                            "/api/fetch-cancel" => crate::fetch::cancel(&d),
                            "/api/fetch-pause" => crate::fetch::pause(&d),
                            "/api/fetch-resume" => crate::fetch::resume(&app, &d),
                            _ => {
                                let path = d.path.lock().unwrap().clone();
                                let st = d.status.lock().unwrap().0;
                                let target = if st == "done" { path } else { format!("{path}.phonebond") };
                                let _ = sys::cmd("explorer.exe").arg(format!("/select,{target}")).spawn();
                            }
                        }
                    }
                }
                "/api/autostart" => {
                    let on = v["on"].as_bool().unwrap_or(false);
                    extra = match sys::blocking(move || sys::autostart_set(on)).await {
                        Ok(()) => {
                            app.autostart.store(on, Relaxed);
                            app.background.store(on || app.cfg.background, Relaxed);
                            app.log(if on { "Start with Windows: on. phone-bond keeps running when this window is closed" } else { "Start with Windows: off" });
                            json!({ "result": true })
                        }
                        Err(e) => json!({ "error": e }),
                    };
                }
                "/api/quit" => {
                    let mut o = json!({"ok": true});
                    o["ok"] = json!(true);
                    send(&mut s, 200, "application/json", &o.to_string()).await;
                    return quit(&app).await;
                }
                _ => return send(&mut s, 404, "text/plain", "").await,
            }
            let mut o = json!({"ok": true});
            if let (Some(m), Some(e)) = (o.as_object_mut(), extra.as_object()) {
                for (k, x) in e {
                    m.insert(k.clone(), x.clone());
                }
            }
            send(&mut s, 200, "application/json", &o.to_string()).await;
            app.broadcast();
        }
        _ => send(&mut s, 404, "text/plain", "").await,
    }
}

pub async fn quit(app: &Arc<App>) {
    if app.quitting.swap(true, Relaxed) {
        std::process::exit(0);
    }
    sys::proxy_off(app);
    let had_steam = app.steam.lock().unwrap().on;
    sys::steam_off(app, false).await;
    // Steam keeps using the cache until it restarts, so restart it back to normal.
    if had_steam && sys::blocking(sys::steam_running).await {
        sys::restart_steam(app).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    std::process::exit(0);
}

// ---------- speed test ----------
// Downloads from public plain-HTTP speed servers through each phone alone, then all together.

const TEST_URLS: [(&str, &str); 3] = [
    ("speedtest.tele2.net", "/1GB.zip"),
    ("ipv4.download.thinkbroadband.com", "/1GB.zip"),
    ("speed.hetzner.de", "/1GB.bin"),
];
const TEST_SECS: u64 = 8;
const TEST_CONNS: usize = 6;

async fn test_get(app: Arc<App>, link: Arc<Link>, deadline: Instant) {
    while Instant::now() < deadline {
        let mut progressed = false;
        for (host, path) in TEST_URLS {
            if Instant::now() >= deadline {
                return;
            }
            let r: std::io::Result<()> = async {
                let mut up = dial(&app, &link, host, 80).await?;
                up.s.write_all(format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes()).await?;
                let mut buf = Vec::new();
                let hl = read_head(&mut up.s, &mut buf, Duration::from_secs(5)).await?;
                if parse_resp(&buf[..hl])?.status != 200 {
                    return Err(err(std::io::ErrorKind::Other, "not 200"));
                }
                progressed = true;
                let n = (buf.len() - hl) as u64;
                link.test_bytes.fetch_add(n, Relaxed);
                link.bytes.fetch_add(n, Relaxed);
                let mut tmp = vec![0u8; 128 * 1024];
                while Instant::now() < deadline {
                    let left = deadline.saturating_duration_since(Instant::now());
                    let n = with_timeout(left.max(Duration::from_millis(50)), up.s.read(&mut tmp)).await?;
                    if n == 0 {
                        break;
                    }
                    link.test_bytes.fetch_add(n as u64, Relaxed);
                    link.bytes.fetch_add(n as u64, Relaxed);
                }
                Ok(())
            }
            .await;
            if r.is_ok() || progressed {
                break;
            }
        }
        if !progressed {
            return;
        }
    }
}

async fn measure(app: &Arc<App>, group: &[Arc<Link>]) -> Vec<f64> {
    let deadline = Instant::now() + Duration::from_secs(TEST_SECS);
    let mut tasks = Vec::new();
    for l in group {
        l.test_bytes.store(0, Relaxed);
        for _ in 0..TEST_CONNS {
            tasks.push(tokio::spawn(test_get(app.clone(), l.clone(), deadline)));
        }
    }
    for t in tasks {
        let _ = t.await;
    }
    group.iter().map(|l| l.test_bytes.load(Relaxed) as f64 / TEST_SECS as f64).collect()
}

async fn run_test(app: Arc<App>, ip: Option<String>) {
    {
        let mut t = app.test.lock().unwrap();
        if t.running {
            return;
        }
        t.running = true;
        t.combined = None;
        t.error.clear();
    }
    let group: Vec<Arc<Link>> = app
        .links_vec()
        .into_iter()
        .filter(|l| l.live() && ip.as_deref().map_or(true, |i| l.ip.to_string() == i))
        .collect();
    let mut rates = Vec::new();
    for l in &group {
        app.test.lock().unwrap().phase = format!("Testing {} alone", l.name);
        l.st.lock().unwrap().test = Some(json!({"running": true, "rate": null}));
        let r = measure(&app, std::slice::from_ref(l)).await[0];
        l.st.lock().unwrap().test = Some(json!({"running": false, "rate": r, "at": now_ms()}));
        rates.push(r);
    }
    let mut combined = None;
    if ip.is_none() && group.len() > 1 {
        app.test.lock().unwrap().phase = "Testing all phones together".into();
        combined = Some(measure(&app, &group).await.iter().sum::<f64>());
    }
    let mb = |b: f64| format!("{:.1}", b / 1048576.0);
    let mut t = app.test.lock().unwrap();
    t.combined = combined;
    if rates.iter().all(|r| *r == 0.0) {
        t.error = "The speed-test servers could not be reached.".into();
    }
    t.running = false;
    t.phase.clear();
    drop(t);
    let parts: Vec<String> = group.iter().zip(&rates).map(|(l, r)| format!("{} {} MB/s", l.name, mb(*r))).collect();
    app.log(format!("Speed test done: {}{}", parts.join(", "), combined.map_or(String::new(), |c| format!(", together {} MB/s", mb(c)))));
}
