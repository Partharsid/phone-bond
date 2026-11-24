//! phone-bond: bond several USB-tethered phones into one fast Steam download.
mod app;
mod engine;
mod fetch;
mod flow;
mod net;
mod server;
mod sys;
mod ui;

use app::*;
use std::net::Ipv4Addr;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |n: &str| args.iter().any(|a| a == n);
    let opt = |n: &str, d: u16| args.iter().position(|a| a == n).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(d);
    let explicit_ips: Vec<Ipv4Addr> = args
        .iter()
        .enumerate()
        .filter(|(i, a)| *i > 0 && args[*i - 1] == "--ip" && a.parse::<Ipv4Addr>().is_ok())
        .map(|(_, a)| a.parse().unwrap())
        .collect();
    let cfg = Cfg {
        port: opt("--port", 8118),
        ui_port: opt("--ui-port", 8120),
        socks_port: opt("--socks-port", 1080),
        background: flag("--background"),
        explicit_ips,
        allow_loopback: flag("--allow-loopback-upstream"),
        no_ui: flag("--no-ui"),
        stall: Duration::from_secs(opt("--stall", 8) as u64),
    };
    let app = App::new(cfg);
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");

    if flag("--off") {
        // Escape hatch if phone-bond was killed without cleaning up.
        sys::proxy_force_off();
        println!("Windows proxy disabled.");
        if sys::hosts_has_line() {
            println!(
                "{}",
                if sys::remove_hosts_line() { "Steam hosts line removed. Restart Steam." } else { "Steam hosts line is still there: run this as administrator (or steam-off.bat)." }
            );
        }
        return;
    }
    if flag("--autostart-on") || flag("--autostart-off") {
        let on = flag("--autostart-on");
        match sys::autostart_set(on) {
            Ok(()) => println!("Start with Windows is now {}.", if on { "ON" } else { "OFF" }),
            Err(e) => println!("{e}"),
        }
        return;
    }
    if flag("--list") {
        let found = sys::scan_sync(&app);
        for f in &found {
            println!("  {:<16} gw {:<16} {}  {}", f.ip, f.gw.map_or("-".into(), |g| g.to_string()), f.name, f.desc);
        }
        println!("{}", if found.is_empty() { "No usable connections found.".to_string() } else { format!("\n{} connection(s) will be used.", found.len()) });
        return;
    }
    let engine_mode = flag("--engine") || flag("--no-ui") || !(flag("--steam") || flag("--system-proxy"));
    let app_mode = engine_mode; // the dashboard runs unless a command-line-only mode was asked for
    rt.block_on(async move {
        if app_mode && !flag("--engine") && !flag("--no-ui") {
            launcher(&app).await;
            return;
        }
        run_engine(app.clone(), app_mode, flag("--steam"), flag("--system-proxy"), flag("--restart-steam")).await;
    });
}

async fn ui_alive(port: u16) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Ok(Ok(mut s)) = tokio::time::timeout(Duration::from_millis(700), tokio::net::TcpStream::connect(("127.0.0.1", port))).await else { return false };
    let req = format!("GET /api/ping HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    if s.write_all(req.as_bytes()).await.is_err() {
        return false;
    }
    let mut b = Vec::new();
    let _ = tokio::time::timeout(Duration::from_millis(700), s.read_to_end(&mut b)).await;
    String::from_utf8_lossy(&b).contains("phone-bond")
}

/// Double-click path: reuse a running engine, or ask Windows for admin and start one.
async fn launcher(app: &Arc<App>) {
    let port = app.cfg.ui_port;
    if ui_alive(port).await {
        sys::open_ui(port);
        return;
    }
    let admin = sys::blocking(sys::is_admin).await;
    if !admin {
        println!("Asking Windows for admin (needed so Steam downloads go through your phones)...");
        if sys::blocking(|| sys::elevate(&["--engine"])).await {
            for _ in 0..60 {
                if ui_alive(port).await {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            sys::open_ui(port);
            return;
        }
        println!("No admin: Steam mode is unavailable, the system proxy still works.");
    }
    run_engine(app.clone(), true, false, false, false).await;
}

async fn run_engine(app: Arc<App>, with_ui: bool, steam_cli: bool, system_proxy: bool, restart_steam: bool) {
    let admin = sys::blocking(sys::is_admin).await;
    app.cache.lock().unwrap().admin = admin;
    let autostart = sys::blocking(sys::autostart_enabled).await;
    app.autostart.store(autostart, Relaxed);
    // Started by Windows, or "start with Windows" is on: stay alive when the window closes.
    let background = app.cfg.background || autostart;
    app.background.store(background, Relaxed);
    fetch::load_unfinished(&app);
    let a = app.clone();
    let found = sys::blocking(move || sys::scan_sync(&a)).await;
    sys::apply_scan(&app, found, true);
    if app.links_vec().is_empty() {
        println!("No connections yet: plug in a phone and turn on USB tethering. Waiting...");
    }
    for l in app.links_vec() {
        println!("  using {:<16} {}", l.ip, l.name);
    }
    {
        // Hot-plug: rescan every 4 s, so phones can come and go while downloading.
        let app_scan = app.clone();
        tokio::spawn(async move {
            let app = app_scan;
            let mut t = tokio::time::interval(Duration::from_secs(4));
            t.tick().await;
            loop {
                t.tick().await;
                let a = app.clone();
                let found = sys::blocking(move || sys::scan_sync(&a)).await;
                if !found.is_empty() || app.cfg.explicit_ips.is_empty() {
                    sys::apply_scan(&app, found, false);
                }
                steam_guard(&app).await;
            }
        });
        let app = app.clone();
        tokio::spawn(async move {
            let mut round = 0u32;
            loop {
                let (r, h) = sys::blocking(|| (sys::steam_running(), sys::hosts_has_line())).await;
                {
                    let mut c = app.cache.lock().unwrap();
                    c.steam_running = r;
                    c.hosts_line = h;
                }
                round += 1;
                if round % 5 == 0 {
                    let a = sys::blocking(sys::autostart_enabled).await;
                    app.autostart.store(a, Relaxed);
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        });
    }
    tokio::spawn(stats_loop(app.clone()));
    tokio::spawn(engine::scheduler(app.clone()));

    match server::bind(([127, 0, 0, 1], app.cfg.port).into()).await {
        Ok(l) => {
            println!("phone-bond on http://127.0.0.1:{}", app.cfg.port);
            tokio::spawn(server::accept_loop(app.clone(), l));
        }
        Err(e) => {
            eprintln!("Can't listen on port {} ({e})", app.cfg.port);
            std::process::exit(1);
        }
    }
    match flow::socks_listen(app.clone(), app.cfg.socks_port).await {
        Ok(()) => println!("SOCKS5 on 127.0.0.1:{} (torrent clients and other apps)", app.cfg.socks_port),
        Err(e) => app.log(format!("SOCKS5 port {} is busy ({e}); other apps cannot use it", app.cfg.socks_port)),
    }
    if system_proxy {
        sys::proxy_on(&app);
    }
    if with_ui {
        if let Err(e) = ui::serve(app.clone()).await {
            eprintln!("Can't open the app on port {} ({e})", app.cfg.ui_port);
            std::process::exit(1);
        }
        println!("App: http://127.0.0.1:{}/  (closing the app window stops phone-bond)", app.cfg.ui_port);
        if admin {
            sys::steam_on(&app, !background).await;
        }
        if !app.cfg.no_ui && !std::env::args().any(|a| a == "--engine") {
            sys::open_ui(app.cfg.ui_port);
        }
        if !app.cfg.no_ui {
            // Closing the app window = quit (after a grace period, so a reload doesn't).
            *app.lonely_since.lock().unwrap() = Some(Instant::now() + Duration::from_secs(60));
            let app = app.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    let since = *app.lonely_since.lock().unwrap();
                    if !app.background.load(Relaxed) && app.ui_clients.load(Relaxed) == 0 && since.map_or(false, |t| t.elapsed() > Duration::from_secs(8)) {
                        ui::quit(&app).await;
                    }
                }
            });
        }
    } else if steam_cli {
        sys::steam_on(&app, restart_steam).await;
        if !admin {
            println!("[steam] Not admin, so Steam is NOT using phone-bond. Start phone-bond.exe instead.");
        }
    }
    println!("Ctrl+C to stop.");
    let _ = tokio::signal::ctrl_c().await;
    ui::quit(&app).await;
}

/// Steam mode points Steam at us, so with no phone connected Steam would have no internet at all.
/// When the last phone goes away Steam goes direct again; when one is back, the cache returns.
async fn steam_guard(app: &Arc<App>) {
    if !app.steam.lock().unwrap().on {
        return;
    }
    let live = app.links_vec().iter().filter(|l| l.live()).count();
    let has = sys::blocking(sys::hosts_has_line).await;
    if live == 0 && has {
        sys::blocking(sys::remove_hosts_line).await;
        app.log("No phone connected: Steam downloads directly until one is back");
    } else if live > 0 && !has {
        sys::blocking(sys::add_hosts_line).await;
        app.log("Phone connected again: Steam downloads go through your phones");
    }
}
