#!/usr/bin/env node
// phone-bond: an HTTP proxy that spreads connections across several internet
// interfaces (e.g. USB-tethered phones) so multi-connection downloads like
// Steam use all of them at once. Zero dependencies.
//
//   node bond.js                      auto-detect interfaces that have a gateway
//   node bond.js 192.168.42.10 ...    use these local IPs only
//   node bond.js --system-proxy       also point Windows' proxy at us (restored on exit)
//   node bond.js --off                turn the Windows proxy off (if bond was killed)
//   node bond.js --port 8118 --list

'use strict';
const http = require('http');
const net = require('net');
const os = require('os');
const { execSync } = require('child_process');

const args = process.argv.slice(2);
const flag = (name) => args.includes(name);
const opt = (name, def) => {
  const i = args.indexOf(name);
  return i >= 0 && args[i + 1] ? args[i + 1] : def;
};
const PORT = Number(opt('--port', 8118));
const HOST = opt('--host', '127.0.0.1');
const explicitIps = args.filter((a) => net.isIPv4(a));

// ---------- interface discovery ----------

function allIPv4() {
  const out = [];
  for (const [name, addrs] of Object.entries(os.networkInterfaces())) {
    for (const a of addrs || []) {
      if (a.family === 'IPv4' && !a.internal && !a.address.startsWith('169.254.')) {
        out.push({ name, ip: a.address });
      }
    }
  }
  return out;
}

// Interfaces with a default gateway are the ones that actually reach the internet.
function gatewayIps() {
  if (process.platform !== 'win32') return null;
  try {
    const ps =
      "Get-NetIPConfiguration | Where-Object { $_.IPv4DefaultGateway -ne $null -and $_.NetAdapter.Status -eq 'Up' } | ForEach-Object { $_.IPv4Address.IPAddress }";
    return execSync(`powershell -NoProfile -Command "${ps}"`, { encoding: 'utf8' })
      .split(/\r?\n/).map((s) => s.trim()).filter((s) => net.isIPv4(s));
  } catch {
    return null;
  }
}

function pickInterfaces() {
  const all = allIPv4();
  if (explicitIps.length) {
    return explicitIps.map((ip) => all.find((a) => a.ip === ip) || { name: '?', ip });
  }
  const gw = gatewayIps();
  const virtual = /vEthernet|VirtualBox|VMware|WSL|Hyper-V|Loopback|Tailscale|ZeroTier/i;
  return all.filter((a) => (gw ? gw.includes(a.ip) : true) && !virtual.test(a.name));
}

if (flag('--list')) {
  const gw = gatewayIps() || [];
  for (const a of allIPv4()) console.log(`${gw.includes(a.ip) ? '*' : ' '} ${a.ip.padEnd(16)} ${a.name}`);
  console.log('\n* = has a default gateway (auto-selected)');
  process.exit(0);
}

// Escape hatch if the proxy was killed without restoring settings.
if (flag('--off')) {
  execSync('reg add "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings" /v ProxyEnable /t REG_DWORD /d 0 /f', { stdio: 'ignore' });
  console.log('Windows proxy disabled.');
  process.exit(0);
}

const links = pickInterfaces().map((a) => ({
  ...a, active: 0, bytes: 0, lastBytes: 0, rate: 0, downUntil: 0, fails: 0,
  agent: new http.Agent({ keepAlive: true, localAddress: a.ip, maxSockets: 64 }),
}));
if (!links.length) {
  console.error('No usable interfaces found. Run with --list, then pass IPs explicitly.');
  process.exit(1);
}

// ---------- balancing ----------
// Least active connections, weighted by measured speed: a faster phone finishes
// its chunks sooner, frees slots, and so naturally gets more of the work.

function choose(exclude) {
  const now = Date.now();
  let usable = links.filter((l) => l.downUntil < now && l !== exclude);
  if (!usable.length) usable = links.filter((l) => l !== exclude);
  if (!usable.length) usable = links;
  const maxRate = Math.max(1, ...usable.map((l) => l.rate));
  let best = null, bestScore = Infinity;
  for (const l of usable) {
    const weight = 0.3 + l.rate / maxRate; // floor so idle links still get probed
    const score = (l.active + 1) / weight;
    if (score < bestScore) { bestScore = score; best = l; }
  }
  return best;
}

function markFail(l) {
  l.fails++;
  l.downUntil = Date.now() + Math.min(30000, 2000 * l.fails);
}

function track(link, stream) {
  link.active++;
  let done = false;
  const end = () => { if (!done) { done = true; link.active--; } };
  stream.on('data', (c) => { link.bytes += c.length; });
  stream.on('close', end);
  stream.on('error', end);
}

// ---------- proxy ----------

const server = http.createServer((req, res) => {
  let url;
  try { url = new URL(req.url); } catch {
    res.writeHead(400).end('phone-bond: use me as an HTTP proxy\n');
    return;
  }
  const attempt = (link, retried) => {
    const up = http.request({
      host: url.hostname, port: url.port || 80, method: req.method,
      path: url.pathname + url.search, headers: req.headers, agent: link.agent,
    }, (r) => {
      link.fails = 0;
      res.writeHead(r.statusCode, r.headers);
      track(link, r);
      r.pipe(res);
    });
    up.on('error', () => {
      markFail(link);
      // Only retry before any body was sent upstream (GET/HEAD are safe).
      if (!retried && !res.headersSent && (req.method === 'GET' || req.method === 'HEAD')) {
        attempt(choose(link), true);
      } else if (!res.headersSent) {
        res.writeHead(502).end();
      } else {
        res.destroy();
      }
    });
    if (req.method === 'GET' || req.method === 'HEAD') up.end(); else req.pipe(up);
  };
  delete req.headers['proxy-connection'];
  attempt(choose(), false);
});

// HTTPS and anything else tunnelled via CONNECT
server.on('connect', (req, client, head) => {
  const [host, port] = req.url.split(':');
  const attempt = (link, retried) => {
    const up = net.connect({ host, port: Number(port) || 443, localAddress: link.ip });
    up.once('connect', () => {
      link.fails = 0;
      client.write('HTTP/1.1 200 Connection Established\r\n\r\n');
      if (head && head.length) up.write(head);
      track(link, up);
      up.pipe(client);
      client.pipe(up);
    });
    up.once('error', () => {
      if (!up.connecting && up.bytesRead) return client.destroy();
      markFail(link);
      if (!retried) attempt(choose(link), true);
      else { client.end('HTTP/1.1 502 Bad Gateway\r\n\r\n'); }
    });
    client.on('error', () => up.destroy());
    client.on('close', () => up.destroy());
  };
  attempt(choose(), false);
});
server.on('clientError', (_e, sock) => sock.destroy());

// ---------- Windows system proxy (optional) ----------

const REG = 'HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings';
let savedProxy = null;
function regGet(name) {
  try {
    const m = execSync(`reg query "${REG}" /v ${name}`, { encoding: 'utf8' }).match(/REG_\w+\s+(.*)/);
    return m ? m[1].trim() : null;
  } catch { return null; }
}
function regSet(name, type, value) {
  execSync(`reg add "${REG}" /v ${name} /t ${type} /d "${value}" /f`, { stdio: 'ignore' });
}
function enableSystemProxy() {
  savedProxy = { enable: regGet('ProxyEnable'), server: regGet('ProxyServer') };
  regSet('ProxyServer', 'REG_SZ', `${HOST}:${PORT}`);
  regSet('ProxyEnable', 'REG_DWORD', '1');
  console.log(`Windows proxy -> ${HOST}:${PORT} (restart Steam so it picks this up)`);
}
function restoreSystemProxy() {
  if (!savedProxy) return;
  regSet('ProxyEnable', 'REG_DWORD', savedProxy.enable === '0x1' ? '1' : '0');
  if (savedProxy.server) regSet('ProxyServer', 'REG_SZ', savedProxy.server);
  savedProxy = null;
  console.log('\nWindows proxy settings restored.');
}
for (const sig of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
  process.on(sig, () => { restoreSystemProxy(); process.exit(0); });
}
process.on('exit', restoreSystemProxy);

// ---------- stats ----------

const mb = (b) => (b / 1048576).toFixed(2);
setInterval(() => {
  let total = 0;
  const parts = links.map((l) => {
    const delta = l.bytes - l.lastBytes;
    l.lastBytes = l.bytes;
    const r = delta / 2;
    l.rate = l.rate ? l.rate * 0.6 + r * 0.4 : r; // smoothed bytes/s
    total += r;
    const down = l.downUntil > Date.now() ? ' DOWN' : '';
    return `${l.ip} ${mb(r)}MB/s (${l.active})${down}`;
  });
  process.stdout.write(`\r${parts.join(' | ')} || total ${mb(total)} MB/s   `);
}, 2000).unref();

server.listen(PORT, HOST, () => {
  console.log(`phone-bond proxy on http://${HOST}:${PORT}`);
  for (const l of links) console.log(`  using ${l.ip.padEnd(16)} ${l.name}`);
  if (flag('--system-proxy')) enableSystemProxy();
  console.log('Ctrl+C to stop.\n');
});
