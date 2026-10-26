# phone-bond

Combine the internet of several USB-tethered phones (plus Wi-Fi, if you like) for Steam downloads.

## How it works

Windows sends all traffic over one connection. `bond.js` is a local HTTP proxy
that gives each new connection to whichever phone is least busy. A faster phone
finishes its chunks sooner and so gets more of them. Steam downloads a game as
many small chunks over several connections at once, so all the phones end up
downloading together.

It does **not** speed up a single connection, such as one big file in a browser.
A download made of many connections (Steam, a download manager, torrents) does
add up.

## Use

1. Plug in the phones and turn on **USB tethering** on each one.
2. Optional: turn off Wi-Fi, or keep it on as another connection.
3. Check what was found:
   ```
   node bond.js --list
   ```
   Lines starting with `*` will be used.
4. Double-click **start.bat**. It starts the proxy and points Windows' proxy setting at it.
5. **Restart Steam** completely (right-click the tray icon → Exit, then open it again),
   because Steam only reads the proxy setting when it starts.
6. Start the download. The window shows the speed of each phone and the total.
7. When you're done, press **Ctrl+C** in the window. The proxy setting goes back to how it was.

If the internet stops working after a crash, run `node bond.js --off`.

Use only some connections: `node bond.js --system-proxy 192.168.42.129 192.168.43.5`

## Make Steam open more connections (optional)

Each phone needs at least one connection to work on, so more connections help.
Paste `steam://open/console` into Win+R, then run:

```
@nClientDownloadEnableHTTP2PreferenceDefault 0
@fDownloadRateImprovementToAddAnotherConnection 1.0
```

The first command makes Steam use many HTTP/1.1 connections instead of one shared
HTTP/2 connection. The second makes it add connections more readily.

## Troubleshooting

- **Only one phone gets traffic:** check that every phone shows in `--list` with `*`.
  If one has no `*`, its tethering isn't giving the PC a gateway; unplug it and plug it back in.
- **Steam ignores the proxy:** the window shows 0 MB/s while Steam downloads. If that
  happens, use [dispatch](https://github.com/alexkirsz/dispatch) (SOCKS) with
  Proxifier, or the paid app Speedify, which combines connections for every app.
- **Phone shows `DOWN`:** connections to that phone failed, so it is skipped for a
  few seconds and then tried again.
