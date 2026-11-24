# phone-bond

**Plug in two (or more) phones, turn on USB tethering, and use all of their mobile data at once.**
Steam, Chrome downloads, torrents, any file link: one connection per phone is slow, all of them together are not.

Built for the very real situation of "my broadband is slow / down, but I have two phones with decent 4G/5G".
Windows only. Free. About 2 MB, no installer, nothing runs in the cloud, no account.

| | Steam download, two phones (Airtel + Jio, India) |
|---|---|
| One phone | ~10–25 MB/s |
| Windows alone (spreads a little over IPv6) | ~14 MB/s |
| **phone-bond** | **46–64 MB/s** |

Your numbers depend on your phones and signal. phone-bond can only add up what your phones can each do.

## What it speeds up

| Use | How | Needs |
|---|---|---|
| **Steam** game downloads | Steam is pointed at phone-bond; every chunk goes to the phone that finishes it soonest, slow ones are raced on the other phone | nothing: switch is automatic (Steam restarts once) |
| **Any file link** (ISO, game launcher, installer, video) | paste the link in the **Fast download** box; the file is cut into pieces pulled over all phones at once, HTTPS included, with pause / resume | nothing |
| **Chrome, Edge and other apps** | turn on **System proxy**; every connection is spread over the phones | one toggle |
| **Torrents** (qBittorrent, Transmission…) | SOCKS5 proxy `127.0.0.1:1080` (TCP and UDP); every peer connection is spread over the phones | set the proxy in the client |

## Use it

1. Plug each phone in with USB, turn on **USB tethering** (Settings → Network → Hotspot & tethering).
2. Run `phone-bond.exe`. Windows asks for admin once (it edits the hosts file for Steam mode).
3. The dashboard opens. Steam restarts into Steam mode; resume your download.

Close the window and phone-bond stops and puts everything back (Steam, hosts file, Windows proxy).
Turn on **Start with Windows** in the dashboard and it instead keeps running quietly in the background
(it also restarts itself if it ever crashes). Phones can be plugged and unplugged while it runs.

Windows SmartScreen will say "unrecognised app" because the exe is not code-signed. **More info → Run anyway.**
The source is all here; build it yourself if you prefer (below).

## Why it is fast

- **Every phone is used, in proportion to its speed.** New requests go to the phone with the smallest
  `bytes waiting / measured speed`; a phone that finishes early takes work from a slower one.
- **Stragglers are raced.** A chunk taking 3× longer than usual is requested again on the other phone;
  first copy wins. Phones that drop or freeze never stall a download: work moves to the others.
- **Servers are probed on the first request.** If a download server supports `Range`, chunks are split across
  phones; if not (Steam's CDN does not), whole chunks are balanced and raced instead. No wasted bytes.
- **Each phone resolves DNS through its own carrier,** so it talks to the CDN edge nearest to *that* network.
- **Live tuning.** The number of parallel streams is hill-climbed while downloading.
- **Safe by default.** With no phone connected, Steam goes back to your normal internet automatically.

## Honest limits

- Windows 10/11 only. IPv4 only (phones are bound by their IPv4 address).
- One big download over HTTPS in a normal browser still uses **one** phone (a browser cannot split it).
  Paste the link into **Fast download**, or enable `chrome://flags/#enable-parallel-downloading`.
- Fast download: some links are tied to your IP or login and refuse a second phone. Those fall back to one phone.
- It does not bond a single TCP connection (that needs a server on the other end). Many connections, yes.
- Mind your plan: tethering allowances and fair-use limits still apply, and heavy use heats phones.

## Build

```
cd rs
cargo build --release          # needs Rust + MSVC build tools; output: rs\target\release\phone-bond.exe
cargo test
```

`bond.js` (repo root) is the original Node.js prototype, kept for reference; the Rust app in `rs/` replaces it.

Command line: `--list` (show phones), `--off` (undo hosts/proxy if it was killed),
`--autostart-on` / `--autostart-off`, `--socks-port N`, `--port N`.

## Licence

MIT, see [LICENSE](LICENSE).
