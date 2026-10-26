# phone-bond

Combine the internet of several USB-tethered phones (plus Wi-Fi, if you like) for Steam downloads.
One small Node.js file, no dependencies.

## Steam: one click

1. Plug in the phones and turn on **USB tethering** on each one.
2. Double-click **steam-turbo.bat** and click **Yes** when Windows asks for admin.

That's it. It points Steam at phone-bond, restarts Steam on its Downloads page, and
shows the speed of each phone. Press **Ctrl+C** when you're done: Steam goes back to
downloading normally, and is restarted so it notices.

```
Eth 2 6.1(11) | Eth 5 4.2(10) | Eth 6 5.8(11) || 16.1 MB/s  peak 17.3  2.41 GB  3 conn  2 resumed
```

Each entry is a phone: MB/s, then how many downloads are in progress on it.
`DOWN` means its last download failed, so it's skipped for a few seconds;
`GONE` means it was unplugged.

If the window was killed and Steam downloads stop working, run **steam-off.bat** as administrator.

## Why it's fast

- **Every phone is used.** Steam downloads a game as many small chunks over about
  32 connections. Each chunk goes to whichever phone is least busy, so a faster phone
  finishes sooner and gets more of them. In tests it got 93–100% of the phones' combined speed.
- **Phones can come and go.** It rescans every 4 seconds. Plug a phone in mid-download and
  it's used straight away; unplug one and its work moves to the others.
- **Nothing restarts from zero.** If a phone drops or freezes partway through a chunk,
  phone-bond carries on from the exact byte it got to, on another phone. Steam never notices.
- **Each phone uses its nearest server.** Server addresses are looked up through each phone's
  own carrier, so every phone downloads from the CDN server closest to its network.

The ceiling is the sum of your phones' mobile speeds: software can't go past that.
To raise it:

- **Use phones on different carriers.** Two phones on the same tower and carrier share its capacity.
- **Signal matters most.** Put the phones near a window, standing up, not stacked on each other.
  4G/5G signal at 3–4 bars is often several times faster than at 1 bar.
- **Turn off battery saver / data saver** on each phone. They throttle tethering.
- **Keep them cool and charging.** Hot phones slow their modem down. Use USB 3 (blue) ports.
- **Check how much tethering data your plan allows.** Some plans slow tethering down after a daily amount.

## How it works

Steam ignores the Windows proxy setting for game downloads. But it has built-in support
for a local game cache ("lancache"): if the name `lancache.steamcontent.com` points at
your own PC, Steam sends every game download there as plain HTTP. steam-turbo.bat adds
that one line to the hosts file and phone-bond answers on port 80, passing each download
to a phone. The line is removed again when phone-bond stops.

## Other apps (system proxy)

Double-click **start.bat**. It points Windows' proxy setting at phone-bond, which helps
apps that use the proxy and download over several connections (download managers,
browsers fetching many files). Press Ctrl+C to restore the proxy setting. If the internet
stops working after a crash, run `node bond.js --off`.

## Commands

```
node bond.js --list                 show connections that will be used
node bond.js --steam                Steam cache mode (needs admin for the hosts line)
node bond.js --system-proxy         proxy mode for other apps
node bond.js 10.39.113.222 ...      use only these connections
node bond.js --off                  undo everything after a crash
```

Options: `--restart-steam`, `--port 8118`, `--stall 8` (seconds without data before a
download is moved to another phone).

## Troubleshooting

- **A phone isn't listed:** it needs an internet address from the phone. Turn USB tethering
  off and on again on that phone, or replug it. `169.254.x` addresses mean it didn't get one.
- **Steam downloads stop after closing the window:** run steam-off.bat as administrator and restart Steam.
- **The window shows 0 MB/s while Steam downloads:** Steam wasn't restarted after the mode was turned on.
  Exit Steam from the tray icon and open it again.
