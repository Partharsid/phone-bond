# phone-bond

Combine the internet of several USB-tethered phones (plus Wi-Fi, if you like) for
faster Steam downloads, with a live dashboard showing what each phone is doing.

## Use it

1. Plug in the phones and turn on **USB tethering** on each one.
2. Double-click **phone-bond.exe** and click **Yes** when Windows asks for admin.

The dashboard opens and Steam restarts on its Downloads page, now downloading
through all your phones. Close the window (or click **Stop & restore**) when you're
done: Steam goes back to normal and is restarted so it notices.

Windows may say "Windows protected your PC" the first time, because the exe isn't
code-signed. Click **More info → Run anyway**.

## The dashboard

- **Total speed** in MB/s and Mbps, peak, data downloaded, and a 2-minute chart
  showing how much each phone contributes.
- **A card per phone:** its carrier (Jio, Airtel, …), current speed and share of the
  total, downloads in progress, data used (useful for daily limits), peak, and a mini graph.
  Status shows *Downloading*, *Idle*, *Retrying* (just failed, skipped for a few seconds),
  *Paused* or *Disconnected*.
- **Switch a phone off or on** without unplugging it.
- **Speed test:** each phone alone, then all together. Use it to find where to put a
  phone for the best signal, and to see how much of the combined speed you're getting.
- **Tips** based on what it sees: a phone much slower than the others, two phones on
  the same carrier, Steam not downloading.
- **Activity:** phones connecting and dropping out, pieces rescued, test results.
- **Modes:** Steam mode (on by default) and System proxy, for other apps.

## Why it's fast

- **Every phone is used.** Steam downloads a game as many small pieces over about
  32 connections. Each piece goes to whichever phone is least busy, so a faster phone
  finishes sooner and gets more. In tests it got 93–100% of the phones' combined speed.
- **Phones can come and go.** It checks every 4 seconds. Plug a phone in mid-download and
  it's used straight away; unplug one and its work moves to the others.
- **Nothing restarts from zero.** If a phone drops or freezes partway through a piece,
  phone-bond continues from the exact byte it got to, on another phone.
- **Slow-piece rescue.** If a piece is crawling on a weak phone (far slower than pieces on
  the best phone), the rest of it moves to the best phone so it doesn't hold up the download.
- **Each phone uses its nearest server.** Server addresses are looked up through each
  phone's own carrier, so each one downloads from the CDN server closest to its network.

The ceiling is the sum of your phones' mobile speeds. To raise it:

- **Signal matters most.** Near a window, standing up, phones apart from each other.
  Run the speed test after moving a phone to see whether it helped.
- **Different carriers** add more than phones on the same carrier and tower.
- **Turn off battery saver / data saver** on each phone. They throttle tethering.
- **Keep them cool and charging,** on USB 3 (blue) ports with good cables.
- **Check your plan's tethering allowance.** Some plans slow down after a daily limit.

## How it works

Steam ignores the Windows proxy setting for game downloads. But it supports a local
game cache ("lancache"): if `lancache.steamcontent.com` points at your own PC, Steam
sends every game download there as plain HTTP. phone-bond adds that one line to the
hosts file (that's why it needs admin), answers on port 80, and passes each request to a
phone. The line is removed again when phone-bond stops.

## If something goes wrong

- **Steam downloads stop after phone-bond crashed or was killed:** run **steam-off.bat**
  as administrator, or `phone-bond.exe --off` from an admin command prompt, then restart Steam.
- **A phone isn't listed:** it needs an internet address from the phone. Turn USB tethering
  off and on again, or replug it.
- **"Steam mode needs admin":** you clicked No. Close phone-bond and open it again.
- **"Port 80 is busy":** another program (a local web server, Skype's old setting, IIS) uses
  port 80. Close it and turn Steam mode on again.

## Command line

Without the app, from the `phone-bond` folder:

```
node bond.js                        the app (same as the exe)
node bond.js --list                 show connections that will be used
node bond.js --steam                Steam cache mode, no window (needs admin)
node bond.js --system-proxy         proxy mode for other apps, no window
node bond.js 10.39.113.222 ...      use only these connections
node bond.js --off                  undo everything after a crash
```

Options: `--restart-steam`, `--port 8118`, `--ui-port 8120`, `--stall 8` (seconds
without data before a download moves to another phone), `--no-ui`.
`steam-turbo.bat` and `start.bat` run the window-less modes.

## Build the exe

Run **build.bat** (needs Node.js 20+ and internet once). It packs `bond.js` and
`ui.html` into `dist\phone-bond.exe` using Node's single-executable feature.
