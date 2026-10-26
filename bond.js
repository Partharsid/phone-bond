#!/usr/bin/env node
// phone-bond: spreads downloads across several internet connections (e.g.
// USB-tethered phones) so multi-connection downloads like Steam use all of
// them at once. Zero dependencies. Double-click (or no arguments) opens the app.
//
//   phone-bond.exe / node bond.js     the app: dashboard window + Steam mode
//   node bond.js --steam              command line: Steam cache mode on port 80 (needs admin)
//   node bond.js --system-proxy       command line: HTTP proxy for other apps
//   node bond.js 192.168.42.10 ...    use only these local IPs
//   node bond.js --off                undo everything if bond was killed (proxy, and hosts line when admin)
//   node bond.js --list               show connections that would be used
//
// Other options: --port 8118  --ui-port 8120  --restart-steam  --stall <seconds, default 8>  --no-ui

'use strict';
const http = require('http');
const https = require('https');
const net = require('net');
const os = require('os');
const fs = require('fs');
const path = require('path');
const dns = require('dns');
const { execSync, execFile, spawn } = require('child_process');

const sea = (() => { try { return require('node:sea'); } catch { return null; } })();
const IS_SEA = !!(sea && sea.isSea());

const args = process.argv.slice(IS_SEA ? 1 : 2).filter((a) => a !== process.execPath);
const flag = (name) => args.includes(name);
const opt = (name, def) => {
  const i = args.indexOf(name);
  return i >= 0 && args[i + 1] ? args[i + 1] : def;
};
const PORT = Number(opt('--port', 8118));
const UI_PORT = Number(opt('--ui-port', 8120));
const HOST = '127.0.0.1';
const STALL_MS = Number(opt('--stall', 8)) * 1000; // no data this long = phone is gone
const MAX_RESUMES = 5;
const SCAN_MS = 4000;
const explicitIps = args.filter((a) => net.isIPv4(a));
const isWin = process.platform === 'win32';
// The app (dashboard) runs unless one of the command-line modes is asked for.
const CLI_MODES = ['--steam', '--system-proxy', '--list', '--off'];
const APP_MODE = flag('--app') || flag('--engine') || !args.some((a) => CLI_MODES.includes(a));

// ---------- connection discovery ----------
// A connection is usable when it has an IPv4 address and a default gateway.
// Rescanned every few seconds, so phones can be plugged in, unplugged or
// re-tethered (new IP) while downloading.

const VIRTUAL = /vEthernet|VirtualBox|VMware|WSL|Hyper-V|Loopback|Tailscale|ZeroTier|Bluetooth/i;
const SCAN_PS =
  "Get-NetIPConfiguration | Where-Object { $_.IPv4DefaultGateway -and $_.NetAdapter.Status -eq 'Up' } | " +
  "ForEach-Object { $_.InterfaceAlias + '|' + @($_.IPv4Address)[0].IPAddress + '|' + @($_.IPv4DefaultGateway)[0].NextHop + '|' + $_.InterfaceDescription }";

function parseScan(out) {
  return out.split(/\r?\n/).map((l) => l.trim().split('|')).filter((p) => p.length >= 3 && net.isIPv4(p[1]))
    .map(([name, ip, gw, desc]) => ({ name, ip, gw: net.isIPv4(gw) ? gw : null, desc: desc || '' }));
}

function fallbackScan() {
  const out = [];
  for (const [name, addrs] of Object.entries(os.networkInterfaces())) {
    for (const a of addrs || []) {
      if (a.family === 'IPv4' && !a.internal) out.push({ name, ip: a.address, gw: null, desc: '' });
    }
  }
  return out;
}

function filterScan(found) {
  return found.filter((a) => !a.ip.startsWith('169.254.') && !VIRTUAL.test(a.name) && !VIRTUAL.test(a.desc) &&
    (!explicitIps.length || explicitIps.includes(a.ip)));
}

function scanSync() {
  if (!isWin) return filterScan(fallbackScan());
  try {
    return filterScan(parseScan(execSync(`powershell -NoProfile -Command "${SCAN_PS}"`, { encoding: 'utf8', windowsHide: true })));
  } catch { return filterScan(fallbackScan()); }
}

function scanAsync(cb) {
  if (!isWin) return cb(filterScan(fallbackScan()));
  execFile('powershell', ['-NoProfile', '-Command', SCAN_PS], { encoding: 'utf8', windowsHide: true, timeout: 15000 },
    (err, out) => { if (!err) cb(filterScan(parseScan(out))); });
}

// ---------- hosts file (Steam cache mode) ----------
// Steam ignores proxies, but sends all game downloads to a local "lancache" if
// lancache.steamcontent.com resolves to one. Writing the hosts file needs admin.

const HOSTS = path.join(process.env.SystemRoot || 'C:\\Windows', 'System32', 'drivers', 'etc', 'hosts');
const HOSTS_TAG = '# phone-bond';
const HOSTS_LINE = `127.0.0.1 lancache.steamcontent.com ${HOSTS_TAG}`;

function flushDns() { try { execSync('ipconfig /flushdns', { stdio: 'ignore', windowsHide: true }); } catch {} }
function hostsText() { try { return fs.readFileSync(HOSTS, 'utf8'); } catch { return ''; } }
function hostsHasLine() { return /^[ \t]*127\.0\.0\.1[ \t]+lancache\.steamcontent\.com/m.test(hostsText()); }

function addHostsLine() {
  if (hostsHasLine()) return true;
  try {
    const t = hostsText();
    fs.appendFileSync(HOSTS, (t && !t.endsWith('\n') ? '\r\n' : '') + HOSTS_LINE + '\r\n');
    flushDns();
    return true;
  } catch { return false; }
}

function removeHostsLine() {
  const t = hostsText();
  if (!t.includes(HOSTS_TAG)) return true;
  try {
    fs.writeFileSync(HOSTS, t.split(/\r?\n/).filter((l) => !l.includes(HOSTS_TAG)).join('\r\n'));
    flushDns();
    return true;
  } catch { return false; }
}

let adminCache = null;
function isAdmin() {
  if (adminCache === null) {
    try { execSync('net session', { stdio: 'ignore', windowsHide: true }); adminCache = true; } catch { adminCache = false; }
  }
  return adminCache;
}

// ---------- Windows system proxy ----------

const REG = 'HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings';
function regGet(name) {
  try {
    const m = execSync(`reg query "${REG}" /v ${name}`, { encoding: 'utf8', windowsHide: true }).match(/REG_\w+\s+(.*)/);
    return m ? m[1].trim() : null;
  } catch { return null; }
}
function regSet(name, type, value) {
  execSync(`reg add "${REG}" /v ${name} /t ${type} /d "${value}" /f`, { stdio: 'ignore', windowsHide: true });
}

// ---------- links ----------

const links = [];
const counters = { resumes: 0, rescues: 0 };
const events = []; // recent happenings, shown in the app
const log = (msg) => {
  events.unshift({ t: Date.now(), msg });
  events.length = Math.min(events.length, 30);
  process.stdout.write(`\r${msg.padEnd(110)}\n`);
};

// DNS through the phone's own gateway: each carrier then gets the CDN edge
// closest to it, not the one nearest to whichever phone Windows prefers.
function makeLookup(link) {
  const resolver = new dns.Resolver({ timeout: 1500, tries: 1 });
  if (link.gw) {
    resolver.setServers([link.gw]);
    try { resolver.setLocalAddress(link.ip); } catch {}
  }
  const cache = new Map();
  return (host, opts, cb) => {
    if (typeof opts === 'function') { cb = opts; opts = {}; }
    const done = (addr) => {
      if (addr.startsWith('127.')) return cb(Object.assign(new Error('loops back to us'), { code: 'ELOOP' }));
      return opts.all ? cb(null, [{ address: addr, family: 4 }]) : cb(null, addr, 4);
    };
    if (net.isIPv4(host)) return done(host);
    const hit = cache.get(host);
    if (hit && hit.exp > Date.now()) return done(hit.addr);
    const save = (addr, ttl) => { cache.set(host, { addr, exp: Date.now() + ttl * 1000 }); done(addr); };
    const viaSystem = () => dns.lookup(host, { family: 4 }, (e, a) => (e ? cb(e) : save(a, 60)));
    if (!link.gw || !link.dnsOk) return viaSystem();
    resolver.resolve4(host, { ttl: true }, (e, recs) => {
      if (e || !recs.length) {
        if (e && (e.code === 'ETIMEOUT' || e.code === 'ECONNREFUSED')) link.dnsOk = false; // phone has no DNS relay
        return viaSystem();
      }
      const r = recs[Math.floor(Math.random() * recs.length)];
      save(r.address, Math.max(30, Math.min(r.ttl, 300)));
    });
  };
}

const newAgent = (ip) => new http.Agent({ keepAlive: true, localAddress: ip, maxSockets: Infinity, scheduling: 'lifo' });

// Which mobile network a connection is on, from its public IP's owner.
function findCarrier(link) {
  const req = https.get('https://ipinfo.io/json', { localAddress: link.ip, lookup: link.lookup, timeout: 8000, family: 4 }, (r) => {
    let body = '';
    r.on('data', (c) => { body += c; });
    r.on('end', () => {
      try {
        const org = (JSON.parse(body).org || '').replace(/^AS\d+\s+/, '');
        const known = [[/jio/i, 'Jio'], [/airtel/i, 'Airtel'], [/vodafone idea|\bvi\b/i, 'Vi'], [/bsnl|bharat sanchar/i, 'BSNL']];
        const hit = known.find(([re]) => re.test(org));
        link.carrier = hit ? hit[1] : org
          .replace(/\b(Infocomm|Limited|Ltd\.?|Pvt\.?|Private|Corporation|Corp\.?|of India|Telecommunications?|Networks?|Communications?|Inc\.?)\b/gi, '')
          .replace(/[,.\s]+$/, '').replace(/\s{2,}/g, ' ').trim();
      } catch {}
    });
  });
  req.on('timeout', () => req.destroy());
  req.on('error', () => {});
}

function addLink(a) {
  const link = {
    ...a, active: 0, bytes: 0, lastBytes: 0, rate: 0, now: 0, peak: 0, downUntil: 0, fails: 0,
    gone: false, dnsOk: true, enabled: true, carrier: '', history: [], since: Date.now(), test: null,
    agent: newAgent(a.ip),
  };
  link.lookup = makeLookup(link);
  links.push(link);
  findCarrier(link);
  return link;
}

function applyScan(found, quiet) {
  for (const a of found) {
    const l = links.find((x) => x.ip === a.ip);
    if (!l) {
      addLink(a);
      if (!quiet) log(`${a.name} connected (${a.ip})`);
    } else if (l.gone) {
      Object.assign(l, { gone: false, fails: 0, downUntil: 0, gw: a.gw, dnsOk: true });
      log(`${l.name} is back`);
    }
  }
  for (const l of links) {
    if (!l.gone && !found.some((a) => a.ip === l.ip)) {
      l.gone = true;
      l.agent.destroy(); // its sockets are dead; stalled downloads move to other phones
      l.agent = newAgent(l.ip);
      log(`${l.name} disconnected: its downloads moved to the others`);
    }
  }
}

// ---------- balancing ----------
// Least active requests: a faster phone finishes its chunks sooner, frees
// slots, and so naturally gets more of the work. Speed only breaks ties —
// weighting by it starves a link that hasn't been measured yet.

function choose(exclude) {
  const now = Date.now();
  const live = links.filter((l) => !l.gone && l.enabled);
  let usable = live.filter((l) => l.downUntil < now && l !== exclude);
  if (!usable.length) usable = live.filter((l) => l !== exclude);
  if (!usable.length) usable = live;
  if (!usable.length) return null;
  return usable.reduce((best, l) =>
    l.active < best.active || (l.active === best.active && l.rate > best.rate) ? l : best);
}

function markFail(l) {
  l.fails++;
  l.downUntil = Date.now() + Math.min(30000, 2000 * l.fails);
}

// Count a link busy from the moment it's picked, not once connected:
// otherwise a burst of simultaneous requests all lands on the same link.
function claim(link) {
  link.active++;
  let done = false;
  return () => { if (!done) { done = true; link.active--; } };
}

// ---------- HTTP (Steam cache requests and plain proxy requests) ----------

const seenHosts = new Set();
const HOP = ['proxy-connection', 'proxy-authorization', 'connection', 'keep-alive', 'te', 'trailer', 'upgrade'];
const inflight = new Set(); // downloads in progress, watched by the slow-chunk rescue

function targetOf(req) {
  if (!req.url.startsWith('/')) return new URL(req.url); // proxy request: absolute URL
  // Steam cache request: path only, real CDN server in the Host header.
  const host = req.headers.host || '';
  if (!host || /^(lancache\.steamcontent\.com|localhost|127\.)/i.test(host)) return null; // would loop to us
  const url = new URL(`http://${host}${req.url}`);
  if (!seenHosts.has(url.hostname)) { seenHosts.add(url.hostname); log(`Steam is downloading from ${url.hostname}`); }
  return url;
}

function handle(req, res) {
  let url;
  try { url = targetOf(req); } catch { url = undefined; }
  if (!url) { res.writeHead(url === null ? 404 : 400).end('phone-bond\n'); return; }

  const headers = { ...req.headers };
  for (const h of HOP) delete headers[h];
  headers.host = url.host;
  const safe = req.method === 'GET' || req.method === 'HEAD';
  // Resume mid-download on another phone only when we know the full length and
  // the client didn't ask for a range itself (then bytes=N- is exact).
  const state = { sent: 0, total: null, started: false, resumed: 0, resumable: req.method === 'GET' && !req.headers.range };
  let current = null;
  // Client (Steam) went away: stop the upstream download too.
  res.on('close', () => { if (current && !current.over()) current.up.destroy(); });

  const attempt = (link, triesLeft) => {
    if (!link) { if (!res.headersSent) res.writeHead(502).end(); else res.destroy(); return; }
    const release = claim(link);
    let over = false;
    const watch = { link, state, t0: Date.now(), got: 0, slow: false, up: null };
    // Called once per attempt when it ends before the body is complete.
    const fail = () => {
      if (over) return;
      over = true;
      inflight.delete(watch);
      release();
      if (res.destroyed || res.writableEnded) return; // client left; not the phone's fault
      if (!watch.slow) markFail(link);
      if (!state.started) {
        if (triesLeft > 0 && safe) return attempt(choose(link), triesLeft - 1);
        res.writeHead(502).end();
      } else if (state.resumable && state.total !== null && state.resumed < MAX_RESUMES) {
        // Carry on from the byte we got to, on another phone; Steam never notices.
        state.resumed++;
        counters.resumes++;
        attempt(choose(link), 0);
      } else {
        res.destroy();
      }
    };

    const h = { ...headers };
    if (state.started) h.range = `bytes=${state.sent}-`;
    const up = http.request({
      host: url.hostname, port: url.port || 80, method: req.method, path: url.pathname + url.search,
      headers: h, agent: link.agent, lookup: link.lookup, family: 4, timeout: STALL_MS,
    });
    watch.up = up;
    current = { up, over: () => over };
    up.on('timeout', () => up.destroy(new Error('stalled')));
    up.on('error', fail);
    up.on('response', (r) => {
      if (!state.started) {
        // A CDN server error is worth one try on another phone.
        if (r.statusCode >= 502 && r.statusCode <= 504 && triesLeft > 0 && safe) { r.resume(); return fail(); }
        state.started = true;
        const len = Number(r.headers['content-length']);
        if (req.method === 'GET' && r.statusCode === 200 && Number.isFinite(len)) state.total = len;
        else state.resumable = false;
        for (const k of HOP) delete r.headers[k];
        res.writeHead(r.statusCode, r.headers);
      } else if (r.statusCode !== 206) {
        r.resume();
        state.resumable = false; // server won't resume; give up on this response
        return fail();
      }
      link.fails = 0;
      if (state.resumable && state.total) inflight.add(watch);
      r.on('data', (c) => {
        link.bytes += c.length;
        state.sent += c.length;
        watch.got += c.length;
        if (!res.write(c)) { r.pause(); res.once('drain', () => r.resume()); }
      });
      r.on('error', () => {});
      r.on('close', () => {
        if (over) return;
        const complete = r.complete && (state.total === null || state.sent >= state.total);
        if (complete) { over = true; inflight.delete(watch); release(); res.end(); } else fail();
      });
    });
    if (safe) up.end(); else req.pipe(up);
  };
  attempt(choose(), 2);
}

// Slow-chunk rescue: a chunk crawling on a weak phone holds up the end of a
// download. If it's far slower than chunks on other phones, move the rest of
// it to the phone that's doing best (resume from the same byte).
setInterval(() => {
  const live = links.filter((l) => !l.gone && l.enabled);
  if (live.length < 2) return;
  const perChunk = (l) => l.rate / Math.max(1, l.active);
  const best = live.reduce((b, l) => (perChunk(l) > perChunk(b) ? l : b));
  const bestRate = perChunk(best);
  if (bestRate < 100 * 1024) return;
  const now = Date.now();
  for (const w of inflight) {
    const secs = (now - w.t0) / 1000;
    const left = w.state.total - w.state.sent;
    if (secs < 5 || w.slow || left < 256 * 1024 || w.link === best || w.state.resumed >= MAX_RESUMES) continue;
    if (w.got / secs < bestRate * 0.15) {
      w.slow = true;
      counters.rescues++;
      w.up.destroy(new Error('slow'));
    }
  }
}, 1000).unref();

const server = http.createServer(handle);

// HTTPS and anything else tunnelled via CONNECT (system proxy mode)
server.on('connect', (req, client, head) => {
  const [host, port] = req.url.split(':');
  client.on('error', () => {});
  const attempt = (link, retried) => {
    if (!link) return client.end('HTTP/1.1 502 Bad Gateway\r\n\r\n');
    const release = claim(link);
    // family 4: we bind to IPv4 addresses, so an IPv6 answer would fail with EINVAL
    const up = net.connect({ host, port: Number(port) || 443, localAddress: link.ip, family: 4, lookup: link.lookup });
    up.setTimeout(STALL_MS, () => { if (up.connecting) up.destroy(new Error('connect timeout')); });
    up.once('close', release);
    up.once('connect', () => {
      up.setTimeout(0);
      up.setNoDelay(true);
      link.fails = 0;
      client.write('HTTP/1.1 200 Connection Established\r\n\r\n');
      if (head && head.length) up.write(head);
      up.on('data', (c) => { link.bytes += c.length; });
      up.pipe(client);
      client.pipe(up);
    });
    up.once('error', () => {
      release();
      if (up.bytesRead) return client.destroy();
      markFail(link);
      if (!retried) attempt(choose(link), true);
      else client.end('HTTP/1.1 502 Bad Gateway\r\n\r\n');
    });
    client.on('close', () => up.destroy());
  };
  attempt(choose(), false);
});
server.on('clientError', (_e, sock) => sock.destroy());

// ---------- Steam ----------
// Steam picks up the cache only when it starts. Started via explorer.exe so it
// never runs as admin, even when bond does.

function steamRunning() {
  try { return /steam\.exe/i.test(execSync('tasklist /FI "IMAGENAME eq steam.exe" /NH', { encoding: 'utf8', windowsHide: true })); }
  catch { return false; }
}

let steamRestarting = false;
function restartSteam(then) {
  if (!isWin || steamRestarting) return then && then();
  const start = () => {
    spawn('explorer.exe', ['steam://open/downloads'], { detached: true, stdio: 'ignore' }).unref();
    steamRestarting = false;
    if (then) then();
  };
  if (!steamRunning()) { steamRestarting = true; return start(); }
  steamRestarting = true;
  log('Restarting Steam...');
  spawn('explorer.exe', ['steam://exit'], { detached: true, stdio: 'ignore' }).unref();
  const until = Date.now() + 30000;
  const wait = () => (steamRunning() && Date.now() < until ? setTimeout(wait, 1000) : setTimeout(start, 1500));
  setTimeout(wait, 2000);
}

const steam = { on: false, server: null, error: '' };

function steamOn(restart) {
  if (steam.on) return true;
  if (!isAdmin()) { steam.error = 'Needs admin: reopen phone-bond and click Yes when Windows asks.'; return false; }
  const enable = () => {
    if (!addHostsLine()) { steam.error = 'Could not edit the hosts file.'; return false; }
    steam.on = true;
    steam.error = '';
    log('Steam mode on: Steam game downloads go through your phones');
    if (restart) restartSteam();
    return true;
  };
  if (steam.server) return enable();
  steam.server = http.createServer(handle)
    .on('clientError', (_e, sock) => sock.destroy())
    .on('error', (e) => {
      steam.error = `Port 80 is busy (${e.code}): close the app using it and try again.`;
      steam.server = null;
      log(steam.error);
    })
    .listen(80, HOST, enable);
  return true;
}

function steamOff(restart) {
  if (!steam.on) return;
  steam.on = false;
  const ok = removeHostsLine();
  log(ok ? 'Steam mode off: Steam downloads directly again' : 'Could not remove the Steam hosts line: run steam-off.bat as admin');
  if (steam.server) { steam.server.close(); steam.server = null; }
  if (restart && steamRunning()) restartSteam();
}

// ---------- system proxy ----------

const proxy = { on: false, saved: null };
function proxyOn() {
  if (proxy.on) return;
  proxy.saved = { enable: regGet('ProxyEnable'), server: regGet('ProxyServer') };
  regSet('ProxyServer', 'REG_SZ', `${HOST}:${PORT}`);
  regSet('ProxyEnable', 'REG_DWORD', '1');
  proxy.on = true;
  log('System proxy on: apps that use it now go through your phones');
}
function proxyOff() {
  if (!proxy.on) return;
  regSet('ProxyEnable', 'REG_DWORD', proxy.saved.enable === '0x1' ? '1' : '0');
  if (proxy.saved.server) regSet('ProxyServer', 'REG_SZ', proxy.saved.server);
  proxy.on = false;
  log('System proxy off');
}

// ---------- speed test ----------
// Downloads from public speed-test servers through each phone alone, then all
// together, so you can see which phone is weak and move it for better signal.

const TEST_URLS = [
  'https://speed.cloudflare.com/__down?bytes=250000000',
  'https://ash-speed.hetzner.com/1GB.bin',
  'https://proof.ovh.net/files/1Gb.dat',
];
const TEST_SECS = 8;
const TEST_CONNS = 6;
const test = { running: false, phase: '', combined: null, error: '' };

function testGet(link, urlIdx, deadline, onDone) {
  if (urlIdx >= TEST_URLS.length || Date.now() >= deadline) return onDone(false);
  let settled = false; // a request can both error and close: report only once
  const next = () => { if (!settled) { settled = true; testGet(link, urlIdx + 1, deadline, onDone); } };
  const end = () => { if (!settled) { settled = true; onDone(true); } };
  const req = https.get(TEST_URLS[urlIdx], { localAddress: link.ip, lookup: link.lookup, family: 4, timeout: 5000 }, (r) => {
    if (r.statusCode !== 200) { r.resume(); return next(); } // e.g. rate-limited: try the next server
    const stop = setTimeout(() => req.destroy(), Math.max(0, deadline - Date.now()));
    r.on('data', (c) => { link.bytes += c.length; link.testBytes += c.length; });
    r.on('error', () => {});
    r.on('close', () => { clearTimeout(stop); end(); });
  });
  req.on('timeout', () => req.destroy());
  req.on('error', next);
}

// Run TEST_CONNS downloads per link for TEST_SECS; resolves bytes/s per link.
function measure(group) {
  return new Promise((resolve) => {
    const deadline = Date.now() + TEST_SECS * 1000;
    for (const l of group) l.testBytes = 0;
    let pending = 0;
    const finish = () => {
      if (--pending > 0) return;
      resolve(group.map((l) => l.testBytes / TEST_SECS));
    };
    for (const l of group) {
      for (let i = 0; i < TEST_CONNS; i++) {
        pending++;
        const loop = () => testGet(l, 0, deadline, (ok) => (ok && Date.now() < deadline - 500 ? loop() : finish()));
        loop();
      }
    }
  });
}

async function runTest(ip) {
  if (test.running) return;
  const group = links.filter((l) => !l.gone && l.enabled && (!ip || l.ip === ip));
  if (!group.length) return;
  Object.assign(test, { running: true, combined: null, error: '' });
  try {
    for (const l of group) {
      test.phase = `Testing ${l.name} alone`;
      l.test = { running: true, rate: null };
      const [r] = await measure([l]);
      l.test = { running: false, rate: r, at: Date.now() };
    }
    if (!ip && group.length > 1) {
      test.phase = 'Testing all phones together';
      const rs = await measure(group);
      test.combined = rs.reduce((a, b) => a + b, 0);
    }
    if (group.every((l) => !l.test.rate)) test.error = 'The speed-test servers could not be reached.';
    log(`Speed test done: ${group.map((l) => `${l.name} ${mb(l.test.rate)} MB/s`).join(', ')}` +
      (test.combined ? `, together ${mb(test.combined)} MB/s` : ''));
  } finally {
    test.running = false;
    test.phase = '';
  }
}

// ---------- stats ----------

const mb = (b) => ((b || 0) / 1048576).toFixed(1);
const totals = { now: 0, peak: 0, bytes: 0, history: [] };
let lastTotal = 0;
const started = Date.now();
setInterval(() => {
  for (const l of links) {
    const delta = l.bytes - l.lastBytes;
    l.lastBytes = l.bytes;
    l.now = delta;
    l.rate = l.rate ? l.rate * 0.6 + delta * 0.4 : delta; // smoothed bytes/s
    l.peak = Math.max(l.peak, delta);
    l.history.push(delta);
    if (l.history.length > 120) l.history.shift();
  }
  totals.bytes = links.reduce((s, l) => s + l.bytes, 0);
  totals.now = totals.bytes - lastTotal;
  lastTotal = totals.bytes;
  totals.peak = Math.max(totals.peak, totals.now);
  totals.history.push(totals.now);
  if (totals.history.length > 120) totals.history.shift();
  const live = links.filter((l) => !l.gone);
  const parts = links.filter((l) => !l.gone || l.active).map((l) => {
    const tag = l.gone ? ' GONE' : !l.enabled ? ' OFF' : l.downUntil > Date.now() ? ' DOWN' : '';
    return `${l.name.replace('Ethernet', 'Eth')} ${mb(l.now)}(${l.active})${tag}`;
  });
  process.stdout.write(`\r${parts.join(' | ')} || ${mb(totals.now)} MB/s  peak ${mb(totals.peak)}  ` +
    `${(totals.bytes / 1073741824).toFixed(2)} GB  ${live.length} conn   `);
  broadcast();
}, 1000).unref();

function snapshot() {
  const now = Date.now();
  return {
    t: now, uptime: now - started, total: totals, counters, events,
    steam: { on: steam.on, error: steam.error, admin: isAdmin(), running: steamRunning.cached, hostsLine: hostsHasLine.cached },
    proxy: { on: proxy.on },
    test: { running: test.running, phase: test.phase, combined: test.combined, error: test.error },
    links: links.map((l) => ({
      ip: l.ip, name: l.name, desc: l.desc, carrier: l.carrier, gw: l.gw, active: l.active, now: l.now,
      peak: l.peak, bytes: l.bytes, history: l.history, enabled: l.enabled, test: l.test,
      status: l.gone ? 'gone' : !l.enabled ? 'off' : l.downUntil > now ? 'down' : l.active ? 'busy' : 'idle',
    })),
  };
}
// Slow checks refreshed in the background so the UI never waits on them.
setInterval(() => { steamRunning.cached = steamRunning(); hostsHasLine.cached = hostsHasLine(); }, 3000).unref();

// ---------- app UI server ----------

const sseClients = new Set();
function broadcast() {
  if (!sseClients.size) return;
  const msg = `data: ${JSON.stringify(snapshot())}\n\n`;
  for (const c of sseClients) c.write(msg);
}

function uiHtml() {
  if (IS_SEA) return sea.getAsset('ui.html', 'utf8');
  return fs.readFileSync(path.join(__dirname, 'ui.html'), 'utf8');
}

let lonelySince = null;
function uiServer() {
  return http.createServer((req, res) => {
    // Only our own window may talk to us: reject other sites (DNS rebinding / CSRF).
    if (req.headers.host !== `${HOST}:${UI_PORT}` && req.headers.host !== `localhost:${UI_PORT}`) return res.writeHead(403).end();
    const u = new URL(req.url, `http://${HOST}`);
    if (req.method === 'GET' && u.pathname === '/') {
      res.writeHead(200, { 'content-type': 'text/html; charset=utf-8', 'cache-control': 'no-store' });
      return res.end(uiHtml());
    }
    if (req.method === 'GET' && u.pathname === '/api/ping') return res.writeHead(200).end('phone-bond');
    if (req.method === 'GET' && u.pathname === '/events') {
      res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-store', connection: 'keep-alive' });
      res.write(`data: ${JSON.stringify(snapshot())}\n\n`);
      sseClients.add(res);
      lonelySince = null;
      req.on('close', () => { sseClients.delete(res); if (!sseClients.size) lonelySince = Date.now(); });
      return;
    }
    if (req.method !== 'POST' || req.headers['x-bond'] !== '1') return res.writeHead(404).end();
    let body = '';
    req.on('data', (c) => { body += c; if (body.length > 10000) req.destroy(); });
    req.on('end', () => {
      let p = {};
      try { p = body ? JSON.parse(body) : {}; } catch {}
      const ok = (extra) => { res.writeHead(200, { 'content-type': 'application/json' }); res.end(JSON.stringify({ ok: true, ...extra })); broadcast(); };
      switch (u.pathname) {
        case '/api/link': {
          const l = links.find((x) => x.ip === p.ip);
          if (l) {
            const others = links.filter((x) => x !== l && !x.gone && x.enabled).length;
            if (p.enabled || others) { l.enabled = !!p.enabled; log(`${l.name} ${l.enabled ? 'turned on' : 'paused'}`); }
          }
          return ok();
        }
        case '/api/steam': return ok({ result: p.on ? steamOn(true) : (steamOff(true), true) });
        case '/api/proxy': p.on ? proxyOn() : proxyOff(); return ok();
        case '/api/restart-steam': restartSteam(); return ok();
        case '/api/test': runTest(p.ip); return ok();
        case '/api/quit': ok(); return quit();
        default: res.writeHead(404).end();
      }
    });
  });
}

// ---------- cleanup ----------

let cleaned = false;
function cleanup() {
  if (cleaned) return;
  cleaned = true;
  proxyOff();
  if (steam.on) steamOff(false);
}
let quitting = false;
function quit() {
  if (quitting) return process.exit(0);
  quitting = true;
  const hadSteam = steam.on;
  cleanup();
  // Steam keeps using the cache until it restarts, so restart it back to normal.
  if (hadSteam && steamRunning()) restartSteam(() => setTimeout(() => process.exit(0), 500));
  else setTimeout(() => process.exit(0), 200);
}
for (const sig of ['SIGINT', 'SIGTERM', 'SIGBREAK']) process.on(sig, quit);
process.on('SIGHUP', () => { cleanup(); process.exit(0); }); // console window closed: no time for Steam restart
process.on('exit', cleanup);

// ---------- app launcher ----------

function selfCommand(extra) {
  // The exe runs itself; plain node runs this script.
  return IS_SEA ? { file: process.execPath, args: extra } : { file: process.execPath, args: [__filename, ...extra] };
}

function uiAlive() {
  return new Promise((resolve) => {
    const req = http.get({ host: HOST, port: UI_PORT, path: '/api/ping', timeout: 700 }, (r) => { r.resume(); resolve(r.statusCode === 200); });
    req.on('timeout', () => req.destroy());
    req.on('error', () => resolve(false));
  });
}

// Opens the dashboard as its own app window (Edge/Chrome app mode, no tabs or address bar).
function openUi() {
  const url = `http://${HOST}:${UI_PORT}/`;
  const pf86 = process.env['ProgramFiles(x86)'] || 'C:\\Program Files (x86)';
  const pf = process.env.ProgramFiles || 'C:\\Program Files';
  const local = process.env.LOCALAPPDATA || '';
  const browsers = [
    path.join(pf86, 'Microsoft\\Edge\\Application\\msedge.exe'),
    path.join(pf, 'Microsoft\\Edge\\Application\\msedge.exe'),
    path.join(pf, 'Google\\Chrome\\Application\\chrome.exe'),
    path.join(local, 'Google\\Chrome\\Application\\chrome.exe'),
  ];
  const exe = browsers.find((b) => { try { return fs.existsSync(b); } catch { return false; } });
  const profile = path.join(os.tmpdir(), 'phone-bond-ui');
  if (exe) {
    spawn(exe, [`--app=${url}`, '--window-size=1200,820', `--user-data-dir=${profile}`, '--no-first-run'],
      { detached: true, stdio: 'ignore' }).unref();
  } else {
    spawn('cmd', ['/c', 'start', '', url], { detached: true, stdio: 'ignore', windowsHide: true }).unref();
  }
}

// Ask Windows for admin (needed for Steam mode) and run the engine there.
function elevate(extra) {
  return new Promise((resolve) => {
    const { file, args: a } = selfCommand(extra);
    const q = (s) => `'${String(s).replace(/'/g, "''")}'`;
    const argList = a.map((x) => (/\s/.test(x) ? `"${x}"` : x)).join(' ');
    const cmd = `Start-Process -FilePath ${q(file)} -ArgumentList ${q(argList)} -WorkingDirectory ${q(path.dirname(file))} -Verb RunAs -WindowStyle Minimized`;
    execFile('powershell', ['-NoProfile', '-Command', cmd], { windowsHide: true }, (err) => resolve(!err));
  });
}

async function launcher() {
  if (await uiAlive()) { openUi(); return process.exit(0); } // already running: just show it
  if (!isAdmin() && isWin) {
    console.log('Asking Windows for admin (needed so Steam downloads go through your phones)...');
    if (await elevate(['--engine'])) {
      for (let i = 0; i < 60 && !(await uiAlive()); i++) await new Promise((r) => setTimeout(r, 500));
      openUi();
      return process.exit(0);
    }
    console.log('No admin: Steam mode is unavailable, the system proxy still works.');
  }
  startEngine(true);
}

// ---------- start ----------

function startEngine(app) {
  applyScan(scanSync(), true);
  if (!links.length) console.log('No connections yet: plug in a phone and turn on USB tethering. Waiting...');
  setInterval(() => scanAsync((found) => applyScan(found, false)), SCAN_MS).unref();
  steamRunning.cached = steamRunning();
  hostsHasLine.cached = hostsHasLine();

  server.listen(PORT, HOST, () => {
    console.log(`phone-bond on http://${HOST}:${PORT}`);
    for (const l of links) console.log(`  using ${l.ip.padEnd(16)} ${l.name}`);
    if (flag('--system-proxy')) proxyOn();
  });

  if (app) {
    uiServer().on('error', (e) => { console.error(`Can't open the app on port ${UI_PORT} (${e.code})`); process.exit(1); })
      .listen(UI_PORT, HOST, () => {
        console.log(`App: http://${HOST}:${UI_PORT}/  (closing the app window stops phone-bond)`);
        if (isAdmin()) steamOn(true);
        if (!flag('--engine') && !flag('--no-ui')) openUi();
      });
    // Closing the app window = quit (after a grace period, so a reload doesn't).
    if (!flag('--no-ui')) {
      lonelySince = Date.now() + 60000; // give the window time to open
      setInterval(() => { if (lonelySince && !sseClients.size && Date.now() - lonelySince > 8000) quit(); }, 2000).unref();
    }
  } else if (flag('--steam')) {
    steamOn(flag('--restart-steam'));
    if (!isAdmin()) console.log('[steam] Not admin, so Steam is NOT using phone-bond. Start phone-bond.exe or steam-turbo.bat instead.');
    else if (!flag('--restart-steam')) console.log('[steam] Restart Steam fully (tray icon > Exit) so it starts using it.');
  }
  console.log('Ctrl+C to stop.\n');
}

if (flag('--list')) {
  const found = scanSync();
  for (const a of found) console.log(`  ${a.ip.padEnd(16)} gw ${String(a.gw).padEnd(16)} ${a.name}  ${a.desc}`);
  console.log(found.length ? `\n${found.length} connection(s) will be used.` : 'No usable connections found.');
  process.exit(0);
} else if (flag('--off')) {
  // Escape hatch if bond was killed without cleaning up.
  regSet('ProxyEnable', 'REG_DWORD', '0');
  console.log('Windows proxy disabled.');
  if (hostsText().includes(HOSTS_TAG)) {
    console.log(removeHostsLine() ? 'Steam hosts line removed. Restart Steam.'
      : 'Steam hosts line is still there: run this as administrator (or steam-off.bat).');
  }
  process.exit(0);
} else if (APP_MODE && !flag('--engine')) {
  launcher();
} else {
  startEngine(APP_MODE);
}
