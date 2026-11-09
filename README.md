<div align="center">

<img src="crates/gui/assets/icon-256.png" width="96" alt="UDM rabbit icon">

# Universal Download Manager

**Keep your downloads moving, with a native Windows 11 app that puts you in control.**<br>
UDM splits large files into concurrent transfers, keeps recoverable progress on disk, and brings your downloads together in one quiet interface.

**English** · [Tiếng Việt](README.vi.md)

Rust · Slint · Windows 11 x64 · [MIT OR Apache-2.0](LICENSE)

[Getting started](#getting-started) · [Browser setup](extension/README.md) · [Build from source](docs/BUILD.md) · [Privacy](docs/PRIVACY.md)

</div>

---

## What UDM does

UDM downloads files from HTTP and HTTPS links. Its engine, **Usagi** (the
`usagi` Rust package), supports segmented transfers over HTTP/1.1, HTTP/2
and HTTP/3 over QUIC. It uses HTTP/3 when available and falls back when the
selected transport policy allows it.

UDM is a file download manager, not a torrent client or a tool for extracting
video links from websites. A browser handoff passes a download URL to UDM;
it does not copy your signed-in browser session.

The current source version is **0.1.0**. This README describes the code at
`241f550`; it does not announce a public release or an available download.
Windows 11 x64 is the desktop target. Linux and macOS paths support
development and testing; Windows integration is platform-specific.

## Why UDM

| What you get | What it means for you |
|---|---|
| **Concurrent transfers** | Large files can use several byte-range requests, with a separate limit on how many downloads run at once. Servers and your connection still determine the speed. |
| **Careful resume** | Synced checkpoints and representation checks let UDM reuse eligible partial data without combining different versions of a file. |
| **Optional checksums** | Supply a publisher's checksum to check the file before completion: SHA-256, SHA-512, SHA-1, MD5 or BLAKE3. |
| **Files kept in order** | Optional category routing places downloads into subfolders by filename suffix. It is off by default. |
| **A useful tray icon** | See live totals, open the active-downloads popup, pause or resume its rows, or return to the main window. |
| **Your preferred appearance** | English or Vietnamese, Dark/Light/Auto themes, and a choice of accent colours. Auto follows Windows' app theme. |
| **Browser handoff** | A Chrome or Firefox extension hands eligible downloads to the running app through a local native messaging host. Setup is an explicit opt-in. |
| **Private and per-user** | No telemetry, administrator rights or services. Settings and history stay in user directories. There are no update checks in this version. |

## Getting started

1. **Build UDM** using [the Windows build guide](docs/BUILD.md), or use a
   build supplied to you. The packaging scripts create a per-user installer
   and a portable ZIP.
2. **Run `udm.exe`.** Paste a direct HTTP or HTTPS file link into the toolbar
   and press Enter. Use **Add Download** for several links, a destination,
   a checksum or per-download options.
3. **Follow the download.** The list shows progress, speed, state and the
   transport in use. Pause and resume from the row controls; open a completed
   file or its folder from the list.

Closing the window hides UDM to the tray by default, so downloads can
continue. Double-click the tray icon to reopen it; use the tray menu's
**Exit** to quit. Minimize to tray, start minimized and start with Windows
are separate settings and are off by default.

**Start with Windows** is a disclosed, per-user option. Enabling it writes
only UDM's entry in `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`;
disabling it removes that entry. Keep the executable in a stable folder.

For browser downloads, follow [the extension setup guide](extension/README.md).
The current installer and portable ZIP do **not** bundle `udm-host` or the
extension; build the host separately. Handoff uses durable prepare/commit
records. Unresolved committed associations appear as read-only **Needs
recovery** rows in the **All** task view; they are not ordinary downloads
with a Retry button.

## Where your files go

| Mode | Application data |
|---|---|
| Per-user installation or bare executable | `%LOCALAPPDATA%\udm` |
| Portable (`udm.portable` beside the executable, or `UDM_PORTABLE=1`) | `data\` beside the executable |
| Explicit override | The directory set by `UDM_DATA_DIR` |

Downloaded files use the configured download folder, normally your Windows
Downloads folder. Portable mode changes the application data location; it
**does not automatically move downloads beside the executable**. Partial
files sit in the download destination as `<filename>.udm.part`.

The database is `udm.redb`; the desktop log is `logs\udm.log`. Browser
handoff keeps additional local files under `handoff\`. Older `quget`
paths and environment names remain supported for upgrades. See
[operations and backups](docs/OPERATIONS.md) before moving existing data.

## Command-line use

`udm-cli` runs the same Usagi engine without the desktop interface:

```powershell
udm-cli get https://example.com/file.iso --segments 8
udm-cli probe https://example.com/file.iso
udm-cli import urls.txt
udm-cli list
udm-cli settings --list
udm-cli settings segments_per_task=12
udm-cli doctor
```

Replace the example URL with a real download link. Use
`get --hash sha256:<digest>` with the publisher's full digest when available.
Run `udm-cli --help` for commands and `udm-cli <command> --help` for options.

The GUI and CLI cannot open the same database at the same time. Exit UDM
from the tray before using the CLI on its data. Browser IPC is a separate,
narrow handoff interface, not remote CLI control.

## Limits and future work

- Resume depends on what the server proves. Weak ETags or Last-Modified
  alone do not permit reuse of saved bytes; some downloads restart from zero.
- QUIC uses UDP and bypasses system HTTP proxies. Use **Never HTTP/3** on
  networks that require all traffic to go through an HTTP proxy.
- Segments are concurrent requests, not a count of physical connections:
  HTTP/2 and HTTP/3 can multiplex them. An engine-wide connection total is
  unavailable in this version.
- There is no global speed limit, timed queue scheduling, torrent support
  or media resolving. The existing queue only limits concurrent downloads.
- The authenticated updater, dual-stack N2 and multi-family N3 are
  **design-only, not built**. This version has no in-app updater, does not
  use IPv4 and IPv6 together, and does not split one download across address
  families. N2/N3 implementation remains gated on measurements.

## Privacy and removal

UDM has no analytics, advertising or remote crash reporting. It contacts
servers for downloads you create, their probes, redirects and transport
setup. It works offline for local task/history management. It has no
background version check. Local logs and download URLs can still contain
sensitive information; review them before sharing. Read the
[privacy policy](docs/PRIVACY.md) for storage and registration details.

Before removing UDM, disable **Start with Windows** and unregister any
browser host you enabled. Then exit the app. The per-user uninstaller
removes the installed app and its uninstall entry, and keeps application
data and downloads. A portable copy can be deleted after the same optional
registrations have been undone. See [removal instructions](docs/OPERATIONS.md#removing-udm).

## Read next

| If you want to… | Read |
|---|---|
| Run, back up or troubleshoot UDM | [Operations](docs/OPERATIONS.md) |
| Hand browser downloads to UDM | [Browser extension](extension/README.md) |
| Build the app and understand packaging | [Build guide](docs/BUILD.md) |
| Understand crates, storage and IPC | [Architecture](docs/ARCHITECTURE.md) |
| Follow a download from URL to final file | [Usagi engine](docs/ENGINE.md) |
| Understand the interface and interaction choices | [Design](docs/DESIGN.md) |
| Check privacy claims | [Privacy](docs/PRIVACY.md) |
| Find standards and upstream projects | [References](docs/REFERENCES.md) |

## License and credits

UDM is dual-licensed under [MIT OR Apache-2.0](LICENSE).

The interface uses [Slint](https://slint.dev) under the Slint Royalty-free
License 2.0, the vendored [cino-ui library](crates/gui/vendor/cino-ui/LICENSE),
and [Be Vietnam Pro](crates/gui/vendor/cino-ui/ui/fonts/OFL.txt). Rust
dependencies retain their respective licences.
