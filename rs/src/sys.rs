//! Windows plumbing: finding phones, the hosts-file trick that points Steam at us, the
//! system proxy, restarting Steam, admin elevation, and opening the dashboard window.
use crate::app::*;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

pub fn cmd(program: &str) -> Command {
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c.stdin(Stdio::null());
    c
}

pub async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.expect("blocking task")
}

pub fn is_admin() -> bool {
    cmd("net").arg("session").stdout(Stdio::null()).stderr(Stdio::null()).status().map_or(false, |s| s.success())
}

// ---------- finding phones ----------

pub struct Found {
    pub name: String,
    pub ip: Ipv4Addr,
    pub gw: Option<Ipv4Addr>,
    pub desc: String,
}

const SCAN_PS: &str = "Get-NetIPConfiguration | Where-Object { $_.IPv4DefaultGateway -and $_.NetAdapter.Status -eq 'Up' } | \
ForEach-Object { $_.InterfaceAlias + '|' + @($_.IPv4Address)[0].IPAddress + '|' + @($_.IPv4DefaultGateway)[0].NextHop + '|' + $_.InterfaceDescription }";

const VIRTUAL: [&str; 9] = ["vethernet", "virtualbox", "vmware", "wsl", "hyper-v", "loopback", "tailscale", "zerotier", "bluetooth"];

pub fn scan_sync(app: &App) -> Vec<Found> {
    if !app.cfg.explicit_ips.is_empty() {
        return app
            .cfg
            .explicit_ips
            .iter()
            .map(|ip| Found { name: format!("test-{ip}"), ip: *ip, gw: None, desc: "explicit address".into() })
            .collect();
    }
    let out = cmd("powershell").args(["-NoProfile", "-Command", SCAN_PS]).output();
    let Ok(out) = out else { return vec![] };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let p: Vec<&str> = l.trim().split('|').collect();
            if p.len() < 3 {
                return None;
            }
            let ip: Ipv4Addr = p[1].parse().ok()?;
            let low = format!("{} {}", p[0], p.get(3).unwrap_or(&"")).to_lowercase();
            if ip.octets()[..2] == [169, 254] || VIRTUAL.iter().any(|v| low.contains(v)) {
                return None;
            }
            Some(Found { name: p[0].to_string(), ip, gw: p[2].parse().ok(), desc: p.get(3).unwrap_or(&"").to_string() })
        })
        .collect()
}

pub fn apply_scan(app: &Arc<App>, found: Vec<Found>, quiet: bool) {
    for f in &found {
        let existing = app.links_vec().into_iter().find(|l| l.ip == f.ip);
        match existing {
            None => {
                let l = Link::new(f.ip, f.name.clone(), f.desc.clone(), f.gw);
                app.links.write().unwrap().push(l.clone());
                if !quiet {
                    app.log(format!("{} connected ({})", f.name, f.ip));
                }
                tokio::spawn(find_carrier(app.clone(), l));
            }
            Some(l) if l.gone.load(Relaxed) => {
                l.gone.store(false, Relaxed);
                {
                    let mut s = l.st.lock().unwrap();
                    s.fails = 0;
                    s.down_until = 0;
                    s.gw = f.gw;
                    s.dns_ok = true;
                }
                l.dns.lock().unwrap().clear();
                app.log(format!("{} is back", l.name));
            }
            Some(_) => {}
        }
    }
    for l in app.links_vec() {
        if !l.gone.load(Relaxed) && !found.iter().any(|f| f.ip == l.ip) {
            l.gone.store(true, Relaxed);
            l.pool.lock().unwrap().clear();
            app.log(format!("{} disconnected: its downloads moved to the others", l.name));
        }
    }
}

/// Which mobile network a connection is on, from its public IP's owner.
async fn find_carrier(app: Arc<App>, link: Arc<Link>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let r: std::io::Result<String> = async {
        let mut up = crate::net::dial(&app, &link, "ipinfo.io", 80).await?;
        up.s.write_all(b"GET /json HTTP/1.1\r\nHost: ipinfo.io\r\nUser-Agent: curl/8.0\r\nConnection: close\r\n\r\n").await?;
        let mut body = Vec::new();
        crate::net::with_timeout(Duration::from_secs(8), up.s.read_to_end(&mut body)).await?;
        Ok(String::from_utf8_lossy(&body).to_string())
    }
    .await;
    let Ok(text) = r else { return };
    let (Some(a), Some(b)) = (text.find('{'), text.rfind('}')) else { return };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text[a..=b]) else { return };
    let org = v["org"].as_str().unwrap_or("").to_string();
    let low = org.to_lowercase();
    let carrier = if low.contains("jio") {
        "Jio".to_string()
    } else if low.contains("airtel") {
        "Airtel".to_string()
    } else if low.contains("vodafone") || low.split_whitespace().any(|w| w == "vi") {
        "Vi".to_string()
    } else if low.contains("bsnl") || low.contains("bharat sanchar") {
        "BSNL".to_string()
    } else {
        org.split_once(' ').map(|x| x.1.to_string()).unwrap_or(org)
    };
    link.st.lock().unwrap().carrier = carrier;
}

// ---------- hosts file (Steam cache mode) ----------
// Steam ignores proxies, but sends all game downloads to a local "lancache" if
// lancache.steamcontent.com resolves to one.

const HOSTS_TAG: &str = "# phone-bond";

fn hosts_path() -> PathBuf {
    PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into())).join("System32/drivers/etc/hosts")
}
fn hosts_text() -> String {
    std::fs::read_to_string(hosts_path()).unwrap_or_default()
}
pub fn hosts_has_line() -> bool {
    hosts_text().lines().any(|l| {
        let mut w = l.split_whitespace();
        w.next() == Some("127.0.0.1") && w.next() == Some("lancache.steamcontent.com")
    })
}
fn flush_dns() {
    let _ = cmd("ipconfig").arg("/flushdns").stdout(Stdio::null()).status();
}
pub fn add_hosts_line() -> bool {
    if hosts_has_line() {
        return true;
    }
    let mut t = hosts_text();
    if !t.is_empty() && !t.ends_with('\n') {
        t.push_str("\r\n");
    }
    t.push_str(&format!("127.0.0.1 lancache.steamcontent.com {HOSTS_TAG}\r\n"));
    let ok = std::fs::write(hosts_path(), t).is_ok();
    flush_dns();
    ok
}
pub fn remove_hosts_line() -> bool {
    let t = hosts_text();
    if !t.contains(HOSTS_TAG) {
        return true;
    }
    let kept: Vec<&str> = t.lines().filter(|l| !l.contains(HOSTS_TAG)).collect();
    let ok = std::fs::write(hosts_path(), kept.join("\r\n") + "\r\n").is_ok();
    flush_dns();
    ok
}

// ---------- Steam ----------

pub fn steam_running() -> bool {
    cmd("tasklist")
        .args(["/FI", "IMAGENAME eq steam.exe", "/NH"])
        .output()
        .map_or(false, |o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains("steam.exe"))
}

static RESTARTING: AtomicBool = AtomicBool::new(false);

fn explorer(arg: &str) {
    let _ = cmd("explorer.exe").arg(arg).spawn();
}

/// Steam picks the cache up only when it starts. Started via explorer so it never runs as admin.
pub async fn restart_steam(app: &Arc<App>) {
    if RESTARTING.swap(true, Relaxed) {
        return;
    }
    if blocking(steam_running).await {
        app.log("Restarting Steam...");
        explorer("steam://exit");
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if !blocking(steam_running).await {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    explorer("steam://open/downloads");
    RESTARTING.store(false, Relaxed);
}

pub async fn steam_on(app: &Arc<App>, restart: bool) -> bool {
    if app.steam.lock().unwrap().on {
        return true;
    }
    if !app.cache.lock().unwrap().admin {
        app.steam.lock().unwrap().error = "Needs admin: reopen phone-bond and click Yes when Windows asks.".into();
        return false;
    }
    let need_listener = app.steam.lock().unwrap().listener.is_none();
    if need_listener {
        match crate::server::bind(([127, 0, 0, 1], 80).into()).await {
            Ok(l) => {
                let a = app.clone();
                app.steam.lock().unwrap().listener = Some(tokio::spawn(crate::server::accept_loop(a, l)));
            }
            Err(e) => {
                let msg = format!("Port 80 is busy ({:?}): close the app using it and try again.", e.kind());
                app.steam.lock().unwrap().error = msg.clone();
                app.log(msg);
                return false;
            }
        }
    }
    if !blocking(add_hosts_line).await {
        app.steam.lock().unwrap().error = "Could not edit the hosts file.".into();
        return false;
    }
    {
        let mut s = app.steam.lock().unwrap();
        s.on = true;
        s.error.clear();
    }
    app.log("Steam mode on: Steam game downloads go through your phones");
    if restart {
        restart_steam(app).await;
    }
    true
}

pub async fn steam_off(app: &Arc<App>, restart: bool) {
    {
        let mut s = app.steam.lock().unwrap();
        if !s.on {
            return;
        }
        s.on = false;
    }
    let ok = blocking(remove_hosts_line).await;
    app.log(if ok {
        "Steam mode off: Steam downloads directly again"
    } else {
        "Could not remove the Steam hosts line: run steam-off.bat as admin"
    });
    if let Some(h) = app.steam.lock().unwrap().listener.take() {
        h.abort();
    }
    if restart && blocking(steam_running).await {
        restart_steam(app).await;
    }
}

// ---------- Windows system proxy ----------

const REG: &str = "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings";

fn reg_get(name: &str) -> Option<String> {
    let o = cmd("reg").args(["query", REG, "/v", name]).output().ok()?;
    let t = String::from_utf8_lossy(&o.stdout);
    let line = t.lines().find(|l| l.contains("REG_"))?;
    let it = line.split_whitespace().skip(2);
    Some(it.collect::<Vec<_>>().join(" "))
}
fn reg_set(name: &str, ty: &str, v: &str) {
    let _ = cmd("reg").args(["add", REG, "/v", name, "/t", ty, "/d", v, "/f"]).stdout(Stdio::null()).status();
}

pub fn proxy_on(app: &Arc<App>) {
    let mut p = app.proxy.lock().unwrap();
    if p.on {
        return;
    }
    p.saved = Some((reg_get("ProxyEnable"), reg_get("ProxyServer")));
    reg_set("ProxyServer", "REG_SZ", &format!("127.0.0.1:{}", app.cfg.port));
    reg_set("ProxyEnable", "REG_DWORD", "1");
    p.on = true;
    drop(p);
    app.log("System proxy on: apps that use it now go through your phones");
}
pub fn proxy_off(app: &Arc<App>) {
    let mut p = app.proxy.lock().unwrap();
    if !p.on {
        return;
    }
    if let Some((en, srv)) = p.saved.take() {
        reg_set("ProxyEnable", "REG_DWORD", if en.as_deref() == Some("0x1") { "1" } else { "0" });
        if let Some(s) = srv {
            reg_set("ProxyServer", "REG_SZ", &s);
        }
    }
    p.on = false;
    drop(p);
    app.log("System proxy off");
}
pub fn proxy_force_off() {
    reg_set("ProxyEnable", "REG_DWORD", "0");
}

// ---------- window + elevation ----------

pub fn open_ui(ui_port: u16) {
    let url = format!("http://127.0.0.1:{ui_port}/");
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let candidates = [
        format!("{}\\Microsoft\\Edge\\Application\\msedge.exe", std::env::var("ProgramFiles(x86)").unwrap_or("C:\\Program Files (x86)".into())),
        format!("{}\\Microsoft\\Edge\\Application\\msedge.exe", env("ProgramFiles")),
        format!("{}\\Google\\Chrome\\Application\\chrome.exe", env("ProgramFiles")),
        format!("{}\\Google\\Chrome\\Application\\chrome.exe", env("LOCALAPPDATA")),
    ];
    let profile = std::env::temp_dir().join("phone-bond-ui");
    if let Some(exe) = candidates.iter().find(|p| std::path::Path::new(p).exists()) {
        let _ = cmd(exe)
            .args([format!("--app={url}"), "--window-size=1200,820".into(), format!("--user-data-dir={}", profile.display()), "--no-first-run".into()])
            .spawn();
    } else {
        let _ = cmd("cmd").args(["/c", "start", "", &url]).spawn();
    }
}

/// Asks Windows for admin and runs ourselves there as the engine (no console, no window).
pub fn elevate(extra: &[&str]) -> bool {
    let exe = std::env::current_exe().unwrap_or_default();
    let q = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let args = extra.join(" ");
    let c = format!(
        "Start-Process -FilePath {} -ArgumentList {} -WorkingDirectory {} -Verb RunAs -WindowStyle Hidden",
        q(&exe.to_string_lossy()),
        q(&args),
        q(&exe.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default())
    );
    cmd("powershell").args(["-NoProfile", "-Command", &c]).status().map_or(false, |s| s.success())
}
