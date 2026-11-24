//! Shared state: links (one per phone), counters, events, and the dashboard snapshot.
use crate::engine::Chunk;
use crate::net::Up;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub struct Cfg {
    pub port: u16,
    pub ui_port: u16,
    pub socks_port: u16,
    pub background: bool,
    pub explicit_ips: Vec<Ipv4Addr>,
    pub allow_loopback: bool,
    pub no_ui: bool,
    pub stall: Duration,
}

#[derive(Default)]
pub struct LinkStat {
    pub rate: f64,
    pub now: u64,
    pub peak: u64,
    pub history: VecDeque<u64>,
    pub down_until: u64,
    pub fails: u32,
    pub carrier: String,
    pub test: Option<Value>,
    pub dns_ok: bool,
    pub last_bytes: u64,
    pub gw: Option<Ipv4Addr>,
}

pub struct Link {
    pub ip: Ipv4Addr,
    pub name: String,
    pub desc: String,
    pub enabled: AtomicBool,
    pub gone: AtomicBool,
    pub bytes: AtomicU64,
    pub test_bytes: AtomicU64,
    pub streams: AtomicI64,
    pub reserved: AtomicI64,
    /// Bytes still to come for downloads that are not split into ranges.
    pub extra: AtomicI64,
    pub st: Mutex<LinkStat>,
    pub pool: Mutex<Vec<Up>>,
    pub dns: Mutex<HashMap<String, (Ipv4Addr, Instant)>>,
}

impl Link {
    pub fn new(ip: Ipv4Addr, name: String, desc: String, gw: Option<Ipv4Addr>) -> Arc<Link> {
        Arc::new(Link {
            ip,
            name,
            desc,
            enabled: AtomicBool::new(true),
            gone: AtomicBool::new(false),
            bytes: AtomicU64::new(0),
            test_bytes: AtomicU64::new(0),
            streams: AtomicI64::new(0),
            reserved: AtomicI64::new(0),
            extra: AtomicI64::new(0),
            st: Mutex::new(LinkStat { dns_ok: true, gw, ..Default::default() }),
            pool: Mutex::new(Vec::new()),
            dns: Mutex::new(HashMap::new()),
        })
    }
    pub fn live(&self) -> bool {
        !self.gone.load(Relaxed) && self.enabled.load(Relaxed)
    }
    pub fn is_down(&self) -> bool {
        self.st.lock().unwrap().down_until > now_ms()
    }
    pub fn mark_fail(&self) {
        let mut s = self.st.lock().unwrap();
        s.fails += 1;
        s.down_until = now_ms() + (2000 * s.fails as u64).min(30000);
    }
    pub fn mark_ok(&self) {
        let mut s = self.st.lock().unwrap();
        s.fails = 0;
        s.down_until = 0;
    }
    /// What this phone can deliver, in bytes/s: the smoothed rate, or most of its recent best
    /// when it has been idle (so a fast phone that just paused is not mistaken for a slow one).
    pub fn eff_rate(&self) -> f64 {
        if self.bytes.load(Relaxed) == 0 {
            return 1.5e6;
        }
        let s = self.st.lock().unwrap();
        // Optimistic on purpose: a phone that was handed little work has a low *measured* speed,
        // and trusting that would starve it forever. Best second of the last two minutes counts.
        let hi = s.history.iter().copied().max().unwrap_or(0) as f64;
        s.rate.max(0.8 * hi).max(1.0e6)
    }
}

pub struct SteamState {
    pub on: bool,
    pub error: String,
    pub listener: Option<tokio::task::JoinHandle<()>>,
}
pub struct ProxyState {
    pub on: bool,
    pub saved: Option<(Option<String>, Option<String>)>,
}
#[derive(Default)]
pub struct TestState {
    pub running: bool,
    pub phase: String,
    pub combined: Option<f64>,
    pub error: String,
}
#[derive(Default)]
pub struct Cached {
    pub steam_running: bool,
    pub hosts_line: bool,
    pub admin: bool,
}
#[derive(Default)]
pub struct Totals {
    pub now: u64,
    pub peak: u64,
    pub bytes: u64,
    pub last: u64,
    pub history: VecDeque<u64>,
}

pub struct App {
    pub cfg: Cfg,
    pub links: RwLock<Vec<Arc<Link>>>,
    pub chunks: Mutex<Vec<Arc<Chunk>>>,
    pub resumes: AtomicU64,
    pub steals: AtomicU64,
    pub events: Mutex<VecDeque<(u64, String)>>,
    /// How many parallel downloads to keep running in total; tuned live by a hill-climber.
    pub target: AtomicUsize,
    pub no_range: Mutex<HashSet<String>>,
    pub range_ok: Mutex<HashSet<String>>,
    /// Typical time to fetch one whole chunk (smoothed); slow stragglers are raced at 3x this.
    pub chunk_secs: Mutex<f64>,
    pub seen: Mutex<HashSet<String>>,
    pub totals: Mutex<Totals>,
    pub tx: broadcast::Sender<Arc<String>>,
    pub steam: Mutex<SteamState>,
    pub proxy: Mutex<ProxyState>,
    pub test: Mutex<TestState>,
    pub cache: Mutex<Cached>,
    pub started: Instant,
    pub ui_clients: AtomicUsize,
    pub lonely_since: Mutex<Option<Instant>>,
    pub quitting: AtomicBool,
    /// Keep running when the dashboard window is closed.
    pub background: AtomicBool,
    pub autostart: AtomicBool,
    pub downloads: Mutex<Vec<Arc<crate::fetch::Dl>>>,
    pub next_dl: AtomicU64,
}

impl App {
    pub fn new(cfg: Cfg) -> Arc<App> {
        let (tx, _) = broadcast::channel(16);
        Arc::new(App {
            cfg,
            links: RwLock::new(Vec::new()),
            chunks: Mutex::new(Vec::new()),
            resumes: AtomicU64::new(0),
            steals: AtomicU64::new(0),
            events: Mutex::new(VecDeque::new()),
            target: AtomicUsize::new(64),
            no_range: Mutex::new(HashSet::new()),
            range_ok: Mutex::new(HashSet::new()),
            chunk_secs: Mutex::new(3.0),
            seen: Mutex::new(HashSet::new()),
            totals: Mutex::new(Totals::default()),
            tx,
            steam: Mutex::new(SteamState { on: false, error: String::new(), listener: None }),
            proxy: Mutex::new(ProxyState { on: false, saved: None }),
            test: Mutex::new(TestState::default()),
            cache: Mutex::new(Cached::default()),
            started: Instant::now(),
            ui_clients: AtomicUsize::new(0),
            lonely_since: Mutex::new(None),
            quitting: AtomicBool::new(false),
            background: AtomicBool::new(false),
            autostart: AtomicBool::new(false),
            downloads: Mutex::new(Vec::new()),
            next_dl: AtomicU64::new(1),
        })
    }

    pub fn log(&self, msg: impl Into<String>) {
        let msg = msg.into();
        eprintln!("{msg}");
        let mut e = self.events.lock().unwrap();
        e.push_front((now_ms(), msg));
        e.truncate(30);
    }

    pub fn links_vec(&self) -> Vec<Arc<Link>> {
        self.links.read().unwrap().clone()
    }
    pub fn total_streams(&self) -> i64 {
        self.links_vec().iter().map(|l| l.streams.load(Relaxed)).sum()
    }

    pub fn snapshot(&self) -> String {
        let now = now_ms();
        let links = self.links_vec();
        let t = self.totals.lock().unwrap();
        let c = self.cache.lock().unwrap();
        let st = self.steam.lock().unwrap();
        let ts = self.test.lock().unwrap();
        let ev: Vec<Value> =
            self.events.lock().unwrap().iter().map(|(t, m)| json!({"t": t, "msg": m})).collect();
        let ls: Vec<Value> = links
            .iter()
            .map(|l| {
                let s = l.st.lock().unwrap();
                let active = l.streams.load(Relaxed).max(0);
                let status = if l.gone.load(Relaxed) {
                    "gone"
                } else if !l.enabled.load(Relaxed) {
                    "off"
                } else if s.down_until > now {
                    "down"
                } else if active > 0 {
                    "busy"
                } else {
                    "idle"
                };
                json!({
                    "ip": l.ip.to_string(), "name": l.name, "desc": l.desc, "carrier": s.carrier,
                    "gw": s.gw.map(|g| g.to_string()), "active": active, "now": s.now, "peak": s.peak,
                    "bytes": l.bytes.load(Relaxed), "history": s.history, "enabled": l.enabled.load(Relaxed),
                    "test": s.test, "status": status,
                })
            })
            .collect();
        json!({
            "t": now, "uptime": self.started.elapsed().as_millis() as u64,
            "total": {"now": t.now, "peak": t.peak, "bytes": t.bytes, "history": t.history},
            "counters": {"resumes": self.resumes.load(Relaxed), "rescues": self.steals.load(Relaxed)},
            "events": ev,
            "steam": {"on": st.on, "error": st.error, "admin": c.admin, "running": c.steam_running, "hostsLine": c.hosts_line},
            "proxy": {"on": self.proxy.lock().unwrap().on},
            "test": {"running": ts.running, "phase": ts.phase, "combined": ts.combined, "error": ts.error},
            "links": ls,
            "streams": self.total_streams(), "target": self.target.load(Relaxed), "socks": self.cfg.socks_port, "autostart": self.autostart.load(Relaxed), "background": self.background.load(Relaxed),
            "downloads": self.downloads.lock().unwrap().iter().map(|d| d.json()).collect::<Vec<_>>(),
        })
        .to_string()
    }

    pub fn broadcast(&self) {
        if self.tx.receiver_count() > 0 {
            let _ = self.tx.send(Arc::new(self.snapshot()));
        }
    }
}

/// One tick per second: per-phone speeds, totals, history, and the live concurrency tuner.
pub async fn stats_loop(app: Arc<App>) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut window: VecDeque<u64> = VecDeque::new();
    let mut dir: i64 = 12;
    let mut prev_avg = 0.0f64;
    let mut n = 0u32;
    loop {
        tick.tick().await;
        let links = app.links_vec();
        let mut total = 0u64;
        for l in &links {
            let b = l.bytes.load(Relaxed);
            let mut s = l.st.lock().unwrap();
            let delta = b.saturating_sub(s.last_bytes);
            s.last_bytes = b;
            s.now = delta;
            s.rate = if s.rate > 0.0 { s.rate * 0.6 + delta as f64 * 0.4 } else { delta as f64 };
            s.peak = s.peak.max(delta);
            s.history.push_back(delta);
            if s.history.len() > 120 {
                s.history.pop_front();
            }
            total += b;
        }
        {
            let mut t = app.totals.lock().unwrap();
            t.bytes = total;
            t.now = total.saturating_sub(t.last);
            t.last = total;
            t.peak = t.peak.max(t.now);
            let now = t.now;
            t.history.push_back(now);
            if t.history.len() > 120 {
                t.history.pop_front();
            }
            window.push_back(now);
        }
        // Hill-climb the number of parallel downloads: more helps until the phones saturate,
        // after that it only adds overhead. Step up/down every 8 s depending on what paid off.
        n += 1;
        if n >= 8 {
            n = 0;
            let avg = window.iter().sum::<u64>() as f64 / window.len().max(1) as f64;
            window.clear();
            if avg > 2e6 {
                if avg < prev_avg * 0.97 {
                    dir = -dir;
                }
                let t = (app.target.load(Relaxed) as i64 + dir).clamp(32, 192);
                app.target.store(t as usize, Relaxed);
            }
            prev_avg = avg;
        }
        app.broadcast();
    }
}
