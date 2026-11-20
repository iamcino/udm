# Changelog

All notable changes to UDM are listed here. Vietnamese: [CHANGELOG.vi.md](CHANGELOG.vi.md).

## 0.3.0

- **Categories in the main window**: the sidebar now shows the download categories with a count for each. Selecting a category filters the task list, combined with the status filter and search; Finished and Unfinished views are included.
- **Browser downloads ask first**: when the extension hands a download over, UDM now shows a confirmation dialog with the file name, size and source. Choose the folder and the category, then download now, add the download paused, or discard it. If the dialog expires unanswered, a notice in the main window keeps the handoff from being lost.

## 0.2.4

- **Update checks, fixed**: after a successful check, checking again no longer fails with a "manifest epoch" refusal. A check is now a pure read — repeating it gives the same answer, and "Already current" is reported correctly.
- **Updater rebuilt on the release hosts**: UDM now asks GitHub and GitLab for their latest release in parallel and takes the newest. An update is accepted only when the archive's SHA-256 matches the published `SHA256SUMS.txt` and every program file carries a valid UDM code signature.

## 0.2.3

- **Close to tray, fixed**: closing the window with "Close to tray" on now keeps UDM running in the tray; the app no longer exits when the window is hidden.
- **Minimize to tray, fixed**: minimizing with "Minimize to tray" on no longer brings the window straight back.
- **Settings from the command line**: `udm-cli settings set` now accepts `close_to_tray`, `minimize_to_tray`, `start_minimized` and `start_with_windows`.
- **Browser handoff, more reliable**: the extension cancels the browser's own download first, so Chrome's Save As dialog no longer blocks the handoff; a download the browser keeps is resumed in the browser instead of being dropped.
- **Tray**: **Check for Updates** now restores the main window before it starts checking, so the result is visible.
- **Alignment fixes**: the About page pills, the brand button icon and label, and the browser status dots in Settings are centred on their rows.
- **Tray menu follows the theme**: the tray menu is now dark when the app is dark, and light when the app is light.

## 0.2.2

- **Downloads survive stalls**: when one connection of a multi-part
  download stalls, UDM retries just that part instead of restarting
  the whole attempt, and the task row shows the retry while it
  happens. A download that previously could burn all its attempts on
  a few stalled connections and fail now recovers on its own.
- **Browser handoff, more reliable**: Chrome cancellations are
  confirmed before ownership moves, and a download the browser keeps
  is resumed in the browser on every exit path.
- **Firefox extension as an installable file**: the installer and the
  portable zip bundle the Firefox extension as a zip file in the
  `extension` folder — select that file under **Load Temporary
  Add-on**. All bundled zips now write their entry names portably,
  so the extension icon resolves in Firefox.
- **Tray**: a **Check for Updates** item joins the tray menu, and the
  About page's GitHub/GitLab links carry their brand marks.
- **Fixed**: the downloads popup could open straddling two monitors.
- **Fixed**: timeout errors said "timed out after 0s"; they now show
  the measured wait.

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
