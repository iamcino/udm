<div align="center">

<img src="crates/gui/assets/icon-256.png" width="96" alt="UDM">

# Universal Download Manager

**Keep your downloads moving, with a native Windows 11 app that puts you in control.**<br>
UDM splits large files into concurrent transfers, keeps recoverable progress on disk, and brings your downloads together in one quiet interface. No account, no telemetry, no administrator rights.

**English** · [Tiếng Việt](README.vi.md)

[![Release](https://img.shields.io/gitlab/v/release/cino-group/udm?label=release&color=0a7aff)](https://gitlab.com/cino-group/udm/-/releases)
![Platform](https://img.shields.io/badge/platform-Windows%2011%20x64-0a7aff)
![Rust](https://img.shields.io/badge/Rust-stable-000000?logo=rust)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-green)](LICENSE)

### ⬇ Download: [installer](https://gitlab.com/cino-group/udm/-/releases/permalink/latest/downloads/udm-setup.exe) · [portable zip](https://gitlab.com/cino-group/udm/-/releases/permalink/latest/downloads/udm-portable.zip)

[All releases](https://gitlab.com/cino-group/udm/-/releases) · [What's new](CHANGELOG.md) · [Report a bug](https://gitlab.com/cino-group/udm/-/issues)

</div>

---

## Up and running in a minute

1. **Download** `udm-<version>-setup.exe` [from GitLab](https://gitlab.com/cino-group/udm/-/releases/permalink/latest/downloads/udm-setup.exe) and run it. It installs for your Windows user only, so there is no administrator prompt.
2. **Paste a link.** Copy a direct HTTP or HTTPS file link, paste it into the toolbar and press Enter.
3. **Watch it go.** The list shows progress, speed, state and the protocol in use. Pause, resume, or open the finished file from its row.

That's it. Closing the window keeps UDM running in the notification area
(bottom right of the taskbar) so downloads carry on; use **Exit** in the tray
menu to quit. If Windows shows a warning, see
[Windows warnings](#windows-and-antivirus-warnings).

> Prefer not to install? Unzip `udm-<version>-portable.zip` into any folder you
> can write to and run `udm.exe`. Its data stays next to the program.

## Why UDM

| | |
|---|---|
| 🚀 **Concurrent transfers** | Large files are fetched as several byte ranges at once, over HTTP/1.1, HTTP/2 or HTTP/3 (QUIC). A separate limit sets how many downloads run together. |
| ↩️ **Careful resume** | Checkpoints are synced to disk and every resume checks it is still the same file, so partial data is reused only when it is safe and versions are never mixed. |
| ✅ **Checksums** | Give the publisher's SHA-256, SHA-512, SHA-1, MD5 or BLAKE3 and UDM checks the file before calling it complete. |
| 🗂️ **Files kept in order** | Optional category routing puts downloads into subfolders by file type. Off by default. |
| 🌐 **Browser handoff** | A Chrome or Firefox extension hands downloads to the running app through a local native messaging host. You opt in. |
| 🔒 **Private and per-user** | No analytics, ads, crash reporting or update checks. No services and no administrator rights; settings and history stay in your user folder. |
| 🪶 **Light and native** | One Rust app with a [Slint](https://slint.dev) interface, plus a command-line tool on the same engine. |

## Features

### Usagi: the download engine

- **Segmented downloads** over HTTP/1.1, HTTP/2 and HTTP/3. UDM uses HTTP/3 when
  the server offers it and falls back when your transport policy allows.
- **Durable progress.** Checkpoints are synced before they count, and partial
  files sit next to the destination as `<filename>.udm.part`.
- **Representation checks.** Strong validators decide whether saved bytes may be
  reused; if the server cannot prove the file is unchanged, the download starts
  over instead of producing a corrupt file.
- **Checksums** are verified before a download is marked complete.

### App

- English or Vietnamese, Dark, Light or Auto (follows Windows) themes, and a
  choice of accent colours.
- **Tray icon** with live totals and an active-downloads popup where rows can be
  paused or resumed.
- **Add Download** for several links at once, a destination, a checksum or
  per-download options.
- **Start with Windows**, start minimized and minimize to tray are separate
  settings, all off by default. Start with Windows writes only UDM's entry in
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` and removes it when you
  turn it off.

### Browser handoff

Follow [the extension setup guide](extension/README.md). The installer and the
portable zip do **not** include `udm-host` or the extension yet; build the host
separately. Handoff uses durable prepare/commit records, and anything that could
not be matched to a task shows as a read-only **Needs recovery** row in the
**All** view.

## Quick tips

| To… | Do this |
|---|---|
| Add many links at once | **Add Download**, one link per line |
| Check a file against the publisher's hash | **Add Download** › checksum, or `udm-cli get <url> --hash sha256:<digest>` |
| Sort downloads into folders | Turn on category routing in settings |
| Use a network that forces an HTTP proxy | Set the transport to **Never HTTP/3** (QUIC uses UDP and skips system proxies) |
| Reopen the window | Double-click the tray icon |
| Quit for real | Tray menu › **Exit** |

## Where your files go

| Mode | Application data |
|---|---|
| Installed, or a bare executable | `%LOCALAPPDATA%\udm` |
| Portable (`udm.portable` beside the executable, or `UDM_PORTABLE=1`) | `data\` beside the executable |
| Explicit override | The directory set by `UDM_DATA_DIR` |

Downloads go to the configured download folder, normally your Windows Downloads
folder. Portable mode moves the application data, **not** your downloads. The
database is `udm.redb`; the log is `logs\udm.log`; browser handoff keeps extra
files under `handoff\`. Older `quget` paths and environment names still work for
upgrades. See [operations and backups](docs/OPERATIONS.md) before moving data.

## Command line

`udm-cli` runs the same Usagi engine without the interface:

```powershell
udm-cli get https://example.com/file.iso --segments 8
udm-cli probe https://example.com/file.iso
udm-cli import urls.txt
udm-cli list
udm-cli settings --list
udm-cli settings segments_per_task=12
udm-cli doctor
```

Run `udm-cli --help` for commands and `udm-cli <command> --help` for options.
The app and the CLI cannot open the same database at once, so exit UDM from the
tray before using the CLI on its data.

## System requirements

- **Windows 11 x64.**
- **No administrator rights.** The installer is per-user, under
  `%LOCALAPPDATA%\Programs\udm`.

## Updates

UDM does not check for updates on its own; this version makes no background
connections. New versions are published on the
[releases page](https://gitlab.com/cino-group/udm/-/releases). Install the new
version over the old one; your settings, history and downloads are kept.

## FAQ

**Does UDM collect any data?**
No. There are no analytics, ads, remote crash reports or update checks. UDM only
contacts the servers of the downloads you create. Logs and download URLs stay on
your machine but can contain sensitive information, so review them before
sharing. See the [privacy policy](docs/PRIVACY.md).

**Is it a torrent client or a video grabber?**
No. UDM downloads files from HTTP and HTTPS links. Browser handoff passes a
download URL to UDM; it does not copy your signed-in browser session.

**Why does a download sometimes restart from zero?**
UDM only reuses saved bytes when the server proves the file has not changed.
Weak ETags or a Last-Modified date alone are not enough.

**How do I uninstall it?**
Turn off **Start with Windows** and unregister any browser host you enabled,
exit UDM from the tray, then use **Settings › Apps › UDM › Uninstall**. The
uninstaller keeps your application data and downloads. A portable copy can simply
be deleted after the same steps. See [removal](docs/OPERATIONS.md#removing-udm).

## Troubleshooting

### Windows and antivirus warnings

UDM releases are digitally signed, but with the project's own certificate rather
than a commercial one, so Windows may show "Windows protected your PC" or
"Unknown publisher" the first time. Click **More info › Run anyway**. To be sure
your download is genuine:

- **Download only** from the [official releases page](https://gitlab.com/cino-group/udm/-/releases).
- **Check the signature:** open PowerShell in the file's folder and run
  `(Get-AuthenticodeSignature .\udm.exe).SignerCertificate.Thumbprint`.
  It must print `9FEE47A048EB37ABD97751E89DD750F75A38C5D4`
  (the certificate is [docs/signing/UDM-codesign.cer](docs/signing/UDM-codesign.cer)).
- **Check the hash** against `SHA256SUMS.txt` from the same release.

### More help

[Operations](docs/OPERATIONS.md) covers backups, recovery after a crash or an
interrupted download, browser recovery and common problems.

## Known limits

- No global speed limit, timed queue scheduling, torrents or media link
  resolving. The queue only limits how many downloads run at once.
- Segments are concurrent requests, not physical connections: HTTP/2 and HTTP/3
  can carry several on one connection.
- No in-app updater, no simultaneous IPv4 and IPv6, and no splitting one
  download across address families in this version.

## For developers

UDM is written in Rust with a [Slint](https://slint.dev) interface, split into
crates: `usagi` (`crates/core`, the engine), `udm-gui` (`crates/gui`, the app),
`udm-cli` (`crates/cli`) and `udm-host` (`crates/host`, the browser native
messaging host).

```
 Browser extension ── Native Messaging ──► udm-host
                                              │ per-user named pipe
                                              ▼
 udm.exe (Slint UI, tray) ───────────► Usagi engine ◄─── udm-cli.exe (when the database is free)
                                              │
                         ┌────────────────────┼────────────────────┐
                         ▼                    ▼                    ▼
                    HTTP/3 · HTTP/2      udm.redb            .udm.part → final file
                    · HTTP/1.1
```

| If you want to… | Read |
|---|---|
| Build the app, the installer and a release | [Build guide](docs/BUILD.md) |
| Understand crates, storage and IPC | [Architecture](docs/ARCHITECTURE.md) |
| Follow a download from URL to final file | [Usagi engine](docs/ENGINE.md) |
| Understand the interface | [Design](docs/DESIGN.md) |
| Run, back up or troubleshoot UDM | [Operations](docs/OPERATIONS.md) |
| Find standards and upstream projects | [References](docs/REFERENCES.md) |

## License and credits

Copyright (c) 2026 UDM contributors

UDM is free, open-source software, dual-licensed under [MIT OR Apache-2.0](LICENSE).
The interface uses [Slint](https://slint.dev) under the Slint Royalty-free
License 2.0, the vendored [cino-ui library](crates/gui/vendor/cino-ui/LICENSE)
and the [Be Vietnam Pro](crates/gui/vendor/cino-ui/ui/fonts/OFL.txt) font (SIL
OFL 1.1). Rust dependencies keep their own licenses.

UDM is provided "as is", without warranty. Only download files you have the
right to download.
