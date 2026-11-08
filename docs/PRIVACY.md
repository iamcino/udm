# udm privacy policy (short version: there is nothing to report)

udm is a download manager. Its only job is to move bytes **you asked for**
between **servers you chose** and **your disk**. This document lists,
exhaustively, what it sends, what it stores, and how that is enforced in the
build.

## What udm never does

* **No analytics.** No usage statistics, no feature counters, no session
  counts, no "first run" ping.
* **No crash reporting.** A panic is written to *your* log file
  (`logs/udm.log` inside the data directory) and nowhere else.
* **No device fingerprinting.** No machine GUID, no hardware IDs, no
  telemetry-grade entropy collection. The only identifier sent anywhere is
  the `User-Agent` header, which you can edit in Settings (and which servers
  need anyway to serve you correctly).
* **No automatic updates.** udm never phones home to check for versions.
  Updating is: you download a new build, you replace the files. There is no
  updater service and no bundled update channel.
* **No advertising SDKs, no attribution SDKs, no A/B testing frameworks.**
* **No background network activity.** When no download is running, udm
  opens zero sockets. The only network traffic is (a) the URLs you enqueue,
  and (b) `Alt-Svc`/DNS/TLS handshakes those URLs require.
* **No administrator rights.** The manifest requests `asInvoker`; the
  installer is per-user (NSIS `RequestExecutionLevel user`); the portable
  build is a single folder. Nothing writes to `HKLM`, `C:\Program Files`,
  `%ProgramData%`, services, or scheduled tasks.

## What udm stores, and where

Everything lives in one directory you control:

* Installed mode: `%LOCALAPPDATA%\udm` (per user; never `%ProgramData%`).
* Portable mode: next to `udm.exe`, triggered by a `udm.portable` file or
  `UDM_PORTABLE=1`.

Inside it:

| File | Contents | Leaves the machine? |
|---|---|---|
| `udm.redb` | task list, resume offsets, settings, Alt-Svc cache | No |
| `logs/udm.log` | local trace log (capped at 8 MiB) | No |
| `tmp/` | `.part` files while downloading | No |

The **Alt-Svc cache** deserves a note: when a server advertises
`Alt-Svc: h3=":443"`, udm remembers *that origin* so the next download can
start on HTTP/3 immediately. This is a local performance cache of information
the server volunteered on a connection you initiated. Entries expire
(`ma=` parameter, capped at 24 h by default) and can be purged by deleting the
database. It is not shared between origins and never uploaded.

## Network behaviour, precisely

For a URL you enqueue, udm may perform: DNS resolution, a TLS handshake
(rustls, system trust store), and HTTP requests to **that origin only**
(HTTP/3 over QUIC when offered, otherwise HTTP/2 or HTTP/1.1), including
redirects the origin returns (capped at `max_redirects`, default 10). System
proxy settings are honoured for HTTP/1.1/2 when enabled (default on), because
that is what a well-behaved desktop app does on a managed network.

Nothing else. No CDN pings, no font fetches, no licence servers, no CRL/OCSP
stapling fetches beyond what rustls does inside your TLS handshake.

## How this is enforced (not just promised)

1. `crates/core/tests/no_telemetry.rs` parses `Cargo.lock` and **fails the
   build** if any dependency name matches an analytics/crash-reporting family
   (`sentry`, `bugsnag`, `*telemetry*`, `*analytics*`, `mixpanel`, …), fails if
   first-party sources contain reporting endpoints, and fails if UI or
   storage code opens sockets.
2. `deny.toml` (cargo-deny) rejects the same families plus licence and
   advisory failures in CI.
3. There is no `reqwest` client outside `crates/core/src/net`; the GUI and the
   store link no HTTP client at all.
4. The release binary contains no update-check URL because no update-check
   code exists.

## Verifying this yourself

* Run `udm-cli doctor`: it prints the data directory, the TLS trust roots in
  use, and confirms QUIC is available.
* Watch the network: with the queue empty, udm is silent (e.g.
  `Get-NetUDPEndpoint`/`Get-NetTCPConnection` filtered by PID show nothing).
* Read `crates/core/src/net/stack.rs`: every outbound connection is created
  there, from a `RequestSpec` built from your URL.

_Last updated: 2026-11-03, applies to udm 0.1.x._

## Installer registration

The installer creates UDM's uninstall entry under your Windows account (HKCU), so you can remove the app from Windows Settings. Uninstall removes that entry and keeps your downloads and application data. The portable build does not register an installer entry. The app does not request administrator rights or create services or scheduled tasks.
