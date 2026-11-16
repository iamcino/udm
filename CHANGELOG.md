# Changelog

All notable changes to UDM are listed here. Vietnamese: [CHANGELOG.vi.md](CHANGELOG.vi.md).

## 0.2.1

- **The browser extension ships with UDM**: the installer and the portable
  zip now carry the extension in the `extension` folder (`chrome` and
  `firefox` subfolders), and each release also attaches it as standalone
  zips. The install guides in Settings and the README were rewritten for
  the bundled layout.
- **About page, trimmed**: the real version, the author and license line,
  project links (GitHub, GitLab) and the data locations. The long texts
  moved out; they live in the README and the privacy document.
- **Data folder renamed**: the per-user data folder no longer uses the
  pre-release codename `quget`; existing installs are migrated to `udm`
  automatically on the first start, with all data (including update
  state) carried over.
- **User-Agent freshness**: a User-Agent still at the factory default now
  follows the current version instead of the version that first installed
  UDM; customized values are untouched.
- **Fixed**: the closing note in **Settings → Browser** could render
  clipped.
- **Fixed**: the Chrome extension mistook current Chrome for Firefox and
  called the wrong native host, so handoffs never connected. The
  extension's options page now shows the extension ID and points to
  registration in **Settings → Browser** instead of a terminal command.

## 0.2.0

- **Browser handoff, built in**: the installer and portable ZIP now ship
  `udm-host.exe`, and each browser is switched on from **Settings → Browser**
  (Chrome and Firefox). Load the extension manually (see the extension setup
  guide), flip the toggle, and downloads you start in the browser are handed
  to UDM; turn the toggle off and the browser keeps its own downloads.
  The Browser settings row shows when the browser last connected.
- **Automatic update checks**: UDM now checks for updates at startup and
  every 6 hours (on by default; **Settings → Enable update checks** turns
  them off). A staged update shows a badge on the tray icon. Applying an
  update is still always your explicit **Apply**.
- **Stronger update verification**: update manifests must pass stricter
  publisher and version checks before anything is staged, so a stale or
  substituted manifest is refused instead of offered.

## 0.1.0

First public release of Universal Download Manager for Windows 11 x64.

- **Usagi download engine**: segmented HTTP and HTTPS downloads over HTTP/1.1, HTTP/2 and
  HTTP/3 (QUIC), using HTTP/3 when available and falling back when the transport policy allows.
- **Careful resume**: synced checkpoints and representation checks reuse eligible partial data
  without mixing different versions of a file.
- **Optional checksums**: SHA-256, SHA-512, SHA-1, MD5 or BLAKE3, checked before a download
  completes.
- **Category routing** (off by default) places downloads into subfolders by file type.
- **Tray icon** with live totals and an active-downloads popup; closing the window keeps
  downloads running in the tray.
- **English and Vietnamese**, Dark/Light/Auto themes and accent colours.
- **Opt-in Start with Windows**, written only to the current user's Run key.
- **Browser handoff** for Chrome and Firefox through a local native messaging host. The host
  and extension are not bundled in this release yet; see docs/BUILD.md.
- **Per-user installer and portable ZIP**: no administrator rights, services or telemetry, and
  no update checks.
- Executables and installer are signed with the self-signed UDM code signing certificate.
