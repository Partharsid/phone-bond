#!/usr/bin/env node
// phone-bond: spreads downloads across several internet connections (e.g.
// USB-tethered phones) so multi-connection downloads like Steam use all of
// them at once. Zero dependencies.
//
//   node bond.js --steam              act as Steam's local cache on port 80 (needs admin, see steam-turbo.bat)
//   node bond.js --system-proxy       HTTP proxy for other apps; points Windows' proxy at us (restored on exit)
//   node bond.js 192.168.42.10 ...    use only these local IPs
//   node bond.js --off                undo everything if bond was killed (proxy, and hosts line when admin)
//   node bond.js --list               show connections that would be used
//
// Other options: --port 8118  --restart-steam  --stall <seconds, default 8>

'use strict';
const http = require('http');
const net = require('net');
const os = require('os');
const fs = require('fs');
const path = require('path');
const dns = require('dns');
const { execSync, execFile, spawn } = require('child_process');

const args = process.argv.slice(2);
const flag = (name) => args.includes(name);
const opt = (name, def) => {
  const i = args.indexOf(name);
  return i >= 0 && args[i + 1] ? args[i + 1] : def;
};
const PORT = Number(opt('--port', 8118));
const HOST = opt('--host', '127.0.0.1');
const STALL_MS = Number(opt('--stall', 8)) * 1000; // no data this long = phone is gone
const MAX_RESUMES = 5;
const SCAN_MS = 4000;
const explicitIps = args.filter((a) => net.isIPv4(a));
const isWin = process.platform === 'win32';

// ---------- connection discovery ----------
// A connection is usable when it has an IPv4 address and a default gateway.
// Rescanned every few seconds, so phones can be plugged in, unplugged or
// re-tethered (new IP) while downloading.

const VIRTUAL = /vEthernet|VirtualBox|VMware|WSL|Hyper-V|Loopback|Tailscale|ZeroTier|Bluetooth/i;
const SCAN_PS =
  "Get-NetIPConfiguration | Where-Object { $_.IPv4DefaultGateway -and $_.NetAdapter.Status -eq 'Up' } | " +
  "ForEach-Object { $_.InterfaceAlias + '|' + @($_.IPv4Address)[0].IPAddress + '|' + @($_.IPv4DefaultGateway)[0].NextHop }";

function parseScan(out) {
  return out.split(/\r?\n/).map((l) => l.trim().split('|')).filter((p) => p.length === 3 && net.isIPv4(p[1]))
    .map(([name, ip, gw]) => ({ name, ip, gw: net.isIPv4(gw) ? gw : null }));
}

function fallbackScan() {
  const out = [];
  for (const [name, addrs] of Object.entries(os.networkInterfaces())) {
    for (const a of addrs || []) {
      if (a.family === 'IPv4' && !a.internal) out.push({ name, ip: a.address, gw: null });
    }
  }
  return out;
}

function filterScan(found) {
  return found.filter((a) => !a.ip.startsWith('169.254.') && !VIRTUAL.test(a.name) &&
    (!explicitIps.length || explicitIps.includes(a.ip)));
}

function scanSync() {
  if (!isWin) return filterScan(fallbackScan());
  try {
    return filterScan(parseScan(execSync(`powershell -NoProfile -Command "${SCAN_PS}"`, { encoding: 'utf8' })));
  } catch { return filterScan(fallbackScan()); }
}

function scanAsync(cb) {
  if (!isWin) return cb(filterScan(fallbackScan()));
  execFile('powershell', ['-NoProfile', '-Command', SCAN_PS], { encoding: 'utf8', windowsHide: true, timeout: 15000 },
    (err, out) => { if (!err) cb(filterScan(parseScan(out))); });
}

if (flag('--list')) {
  const found = scanSync();
  for (const a of found) console.log(`  ${a.ip.padEnd(16)} gw ${String(a.gw).padEnd(16)} ${a.name}`);
  console.log(found.length ? `\n${found.length} connection(s) will be used.` : 'No usable connections found.');
  process.exit(0);
}

// ---------- hosts file (Steam cache mode) ----------
// Steam ignores proxies, but sends all game downloads to a local "lancache" if
// lancache.steamcontent.com resolves to one. Writing the hosts file needs admin.

const HOSTS = path.join(process.env.SystemRoot || 'C:\\Windows', 'System32', 'drivers', 'etc', 'hosts');
const HOSTS_TAG = '# phone-bond';
const HOSTS_LINE = `127.0.0.1 lancache.steamcontent.com ${HOSTS_TAG}`;

function flushDns() { try { execSync('ipconfig /flushdns', { stdio: 'ignore' }); } catch {} }
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

// ---------- Windows system proxy ----------

const REG = 'HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings';
function regGet(name) {
  try {
    const m = execSync(`reg query "${REG}" /v ${name}`, { encoding: 'utf8' }).match(/REG_\w+\s+(.*)/);
    return m ? m[1].trim() : null;
  } catch { return null; }
}
function regSet(name, type, value) {
  execSync(`reg add "${REG}" /v ${name} /t ${type} /d "${value}" /f`, { stdio: 'ignore' });
}

// Escape hatch if bond was killed without cleaning up.
if (flag('--off')) {
  regSet('ProxyEnable', 'REG_DWORD', '0');
  console.log('Windows proxy disabled.');
  if (hostsText().includes(HOSTS_TAG)) {
    console.log(removeHostsLine() ? 'Steam hosts line removed. Restart Steam.'
      : 'Steam hosts line is still there: run this as administrator (or steam-off.bat).');
  }
  process.exit(0);
}

// ---------- links ----------

const links = [];
let resumes = 0;
const log = (msg) => process.stdout.write(`\r${msg.padEnd(110)}\n`);

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

function addLink(a) {
  const link = {
    ...a, active: 0, bytes: 0, lastBytes: 0, rate: 0, downUntil: 0, fails: 0, gone: false, dnsOk: true,
    agent: new http.Agent({ keepAlive: true, localAddress: a.ip, maxSockets: Infinity, scheduling: 'lifo' }),
  };
  link.lookup = makeLookup(link);
  links.push(link);
  return link;
}

function applyScan(found, quiet) {
  for (const a of found) {
    const l = links.find((x) => x.ip === a.ip);
    if (!l) {
      addLink(a);
      if (!quiet) log(`+ ${a.name} (${a.ip}) connected`);
    } else if (l.gone) {
      Object.assign(l, { gone: false, fails: 0, downUntil: 0, gw: a.gw, dnsOk: true });
      log(`+ ${l.name} (${l.ip}) is back`);
    }
  }
  for (const l of links) {
    if (!l.gone && !found.some((a) => a.ip === l.ip)) {
      l.gone = true;
      l.agent.destroy(); // its sockets are dead; stalled downloads move to other phones
      l.agent = new http.Agent({ keepAlive: true, localAddress: l.ip, maxSockets: Infinity, scheduling: 'lifo' });
      log(`- ${l.name} (${l.ip}) disconnected`);
    }
  }
}

// ---------- balancing ----------
// Least active requests: a faster phone finishes its chunks sooner, frees
// slots, and so naturally gets more of the work. Speed only breaks ties —
// weighting by it starves a link that hasn't been measured yet.

function choose(exclude) {
  const now = Date.now();
  const live = links.filter((l) => !l.gone);
  let usable = live.filter((l) => l.downUntil < now && l !== exclude);
  if (!usable.length) usable = live.filter((l) => l !== exclude);
  if (!usable.length) usable = live.length ? live : links;
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

function targetOf(req) {
  if (!req.url.startsWith('/')) return new URL(req.url); // proxy request: absolute URL
  // Steam cache request: path only, real CDN server in the Host header.
  const host = req.headers.host || '';
  if (!host || /^(lancache\.steamcontent\.com|localhost|127\.)/i.test(host)) return null; // would loop to us
  const url = new URL(`http://${host}${req.url}`);
  if (!seenHosts.has(url.hostname)) { seenHosts.add(url.hostname); log(`[steam] downloading from ${url.hostname}`); }
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
    // Called once per attempt when it ends before the body is complete.
    const fail = () => {
      if (over) return;
      over = true;
      release();
      if (res.destroyed || res.writableEnded) return; // client left; not the phone's fault
      markFail(link);
      if (!state.started) {
        if (triesLeft > 0 && safe) return attempt(choose(link), triesLeft - 1);
        res.writeHead(502).end();
      } else if (state.resumable && state.total !== null && state.resumed < MAX_RESUMES) {
        // Carry on from the byte we got to, on another phone; Steam never notices.
        state.resumed++;
        resumes++;
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
      r.on('data', (c) => {
        link.bytes += c.length;
        state.sent += c.length;
        if (!res.write(c)) { r.pause(); res.once('drain', () => r.resume()); }
      });
      r.on('error', () => {});
      r.on('close', () => {
        if (over) return;
        const complete = r.complete && (state.total === null || state.sent >= state.total);
        if (complete) { over = true; release(); res.end(); } else fail();
      });
    });
    if (safe) up.end(); else req.pipe(up);
  };
  attempt(choose(), 2);
}

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

// ---------- Steam restart ----------
// Steam picks up the cache only when it starts. Started via explorer.exe so it
// never runs as admin, even when bond does.

function steamRunning() {
  try { return /steam\.exe/i.test(execSync('tasklist /FI "IMAGENAME eq steam.exe" /NH', { encoding: 'utf8' })); }
  catch { return false; }
}

function restartSteam(then) {
  if (!isWin) return then && then();
  const start = () => { spawn('explorer.exe', ['steam://open/downloads'], { detached: true, stdio: 'ignore' }).unref(); if (then) then(); };
  if (!steamRunning()) return start();
  log('Restarting Steam...');
  spawn('explorer.exe', ['steam://exit'], { detached: true, stdio: 'ignore' }).unref();
  const until = Date.now() + 30000;
  const wait = () => (steamRunning() && Date.now() < until ? setTimeout(wait, 1000) : setTimeout(start, 1500));
  setTimeout(wait, 2000);
}

// ---------- cleanup ----------

let savedProxy = null;
let ownHosts = false;
function cleanup() {
  if (savedProxy) {
    regSet('ProxyEnable', 'REG_DWORD', savedProxy.enable === '0x1' ? '1' : '0');
    if (savedProxy.server) regSet('ProxyServer', 'REG_SZ', savedProxy.server);
    savedProxy = null;
    log('Windows proxy settings restored.');
  }
  if (ownHosts) {
    ownHosts = false;
    log(removeHostsLine() ? 'Steam hosts line removed: Steam downloads directly again.'
      : 'Could not remove the Steam hosts line: run steam-off.bat as administrator.');
  }
}
let quitting = false;
for (const sig of ['SIGINT', 'SIGTERM', 'SIGBREAK']) {
  process.on(sig, () => {
    if (quitting) process.exit(0);
    quitting = true;
    const hadHosts = ownHosts;
    cleanup();
    if (hadHosts && flag('--restart-steam') && steamRunning()) restartSteam(() => setTimeout(() => process.exit(0), 500));
    else process.exit(0);
  });
}
process.on('SIGHUP', () => { cleanup(); process.exit(0); }); // console window closed: no time for Steam restart
process.on('exit', cleanup);

// ---------- stats ----------

const mb = (b) => (b / 1048576).toFixed(1);
let peak = 0, totalBytes = 0, lastTotal = 0;
setInterval(() => {
  let total = 0;
  const parts = links.filter((l) => !l.gone || l.active).map((l) => {
    const delta = l.bytes - l.lastBytes;
    l.lastBytes = l.bytes;
    const r = delta / 2;
    l.rate = l.rate ? l.rate * 0.6 + r * 0.4 : r; // smoothed bytes/s
    total += r;
    const tag = l.gone ? ' GONE' : l.downUntil > Date.now() ? ' DOWN' : '';
    return `${l.name.replace('Ethernet', 'Eth')} ${mb(r)}(${l.active})${tag}`;
  });
  totalBytes = links.reduce((s, l) => s + l.bytes, 0);
  const now = (totalBytes - lastTotal) / 2;
  lastTotal = totalBytes;
  peak = Math.max(peak, now);
  const live = links.filter((l) => !l.gone).length;
  process.stdout.write(`\r${parts.join(' | ')} || ${mb(now)} MB/s  peak ${mb(peak)}  ${(totalBytes / 1073741824).toFixed(2)} GB  ` +
    `${live} conn${resumes ? `  ${resumes} resumed` : ''}   `);
}, 2000).unref();

// ---------- start ----------

applyScan(scanSync(), true);
if (!links.length) console.log('No connections yet: plug in a phone and turn on USB tethering. Waiting...');
setInterval(() => scanAsync((found) => applyScan(found, false)), SCAN_MS).unref();

server.listen(PORT, HOST, () => {
  console.log(`phone-bond on http://${HOST}:${PORT}`);
  for (const l of links) console.log(`  using ${l.ip.padEnd(16)} ${l.name}`);
  if (flag('--system-proxy')) {
    savedProxy = { enable: regGet('ProxyEnable'), server: regGet('ProxyServer') };
    regSet('ProxyServer', 'REG_SZ', `${HOST}:${PORT}`);
    regSet('ProxyEnable', 'REG_DWORD', '1');
    console.log(`Windows proxy -> ${HOST}:${PORT} (restart apps so they pick this up)`);
  }
  if (!flag('--steam')) console.log('Ctrl+C to stop.\n');
});

if (flag('--steam')) {
  http.createServer(handle)
    .on('clientError', (_e, sock) => sock.destroy())
    .on('error', (e) => { console.error(`[steam] can't listen on ${HOST}:80 (${e.code}): is something else using port 80?`); process.exit(1); })
    .listen(80, HOST, () => {
      if (addHostsLine()) {
        ownHosts = isAdmin(); // we can remove it on exit only if we can write the hosts file
        console.log(`[steam] cache mode on http://${HOST}:80 — Steam game downloads go through phone-bond`);
        if (!ownHosts) console.log('[steam] (hosts line was added earlier; run steam-off.bat as admin when done)');
        if (flag('--restart-steam')) restartSteam();
        else console.log('[steam] Restart Steam fully (tray icon > Exit) so it starts using it.');
      } else {
        console.log('[steam] Not admin, so Steam is NOT using phone-bond. Start it with steam-turbo.bat instead.');
      }
      console.log(ownHosts ? 'Ctrl+C to stop (puts Steam back to normal).\n' : 'Ctrl+C to stop.\n');
    });
}

function isAdmin() {
  try { execSync('net session', { stdio: 'ignore' }); return true; } catch { return false; }
}
