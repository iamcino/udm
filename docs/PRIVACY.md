# UDM privacy policy

**UDM has no telemetry, advertising or remote crash reporting.** It downloads
files you request and keeps its working data under your control. No account,
licence server or cloud service is needed to use it.

This policy describes the application and browser integration implemented at
`5f08efd`, source version 0.1.0. It distinguishes network downloads from local
diagnostics and the optional Windows registrations.

## What leaves your computer

When you add an HTTP or HTTPS download, UDM can resolve hostnames, establish
TCP/TLS or QUIC connections, probe response metadata and request the file in
one or more byte ranges. It can follow redirects, and use an HTTP/3 service
advertised through Alt-Svc. Those destinations can differ from the hostname
in the link you originally pasted. DNS resolvers, proxies and destination
servers observe the traffic they normally handle.

Requests include a configurable User-Agent (default `udm/<version>`), normal
HTTP headers and, where appropriate, range and strong-ETag validators. Your
URL can itself contain a private token or identifying query parameters.
HTTPS encrypts transport; it does not make the request anonymous to the server.

The system-proxy option defaults on and can be turned off in settings. When
on, system HTTP proxy settings are used for HTTP/1.1 and HTTP/2. HTTP/3 uses
direct QUIC/UDP and bypasses HTTP proxies. Select the separate Never HTTP/3
switch when your network requires proxy-only access.

UDM sends no usage counts, machine fingerprint, feature analytics, advertising
identifiers or remote crash reports. Local task/history/settings management works
offline; downloads and explicit probes require their destination networks.
The built-in updater has its own section below. When update checks are enabled
(the default), the desktop app checks at startup and every 6 hours while it
runs; with checks turned off it opens no new update connection.

## Idle does not mean no socket exists

UDM sends no idle telemetry beacon. The only periodic project traffic is the
update check described below, which runs at startup and every 6 hours while
the desktop app is running and checks are enabled. Its transport stack can
create a local QUIC UDP endpoint during initialization and retain pooled
connections after requests finish. Such endpoints or idle sockets can appear
in operating-system tools even with no active download. Protocol connection
maintenance can also persist while a pool is alive.

The privacy claim is absence of telemetry and of any project-service traffic
beyond the disclosed update checks, not a guarantee of zero socket handles
whenever the queue is empty. `udm-cli doctor` itself initializes a network
stack to check QUIC availability.

## Updates and the built-in updater

The updater is deliberately simple. Every release is published on two
hosts, GitHub (`iamcino/udm`) and GitLab (`cino-group/udm`). A check asks
both hosts at the same time where their "latest release" link points and
reads the version from the answer; the newest version either host has is
the latest. If one host cannot be reached, the other is enough. A check
stores nothing: it is a pure read, so checking again gives the same
answer, and a published version that is not newer than the installed one
is simply reported as already current.

Three surfaces run a check, all only when update checks resolve on: the
desktop app's automatic check (once at startup and every 6 hours while
it runs), the graphical interface's Check for Updates button (and the
tray menu's Check for Updates item), and `udm-cli update check`. Checks
resolve on by default: an absent flag file means on. `udm-cli update
checks on` writes the flag file on the install base, and `udm-cli update
checks off` writes durable opt-out content to that same file rather than
deleting it; the settings page's Enable update checks checkbox reads and
writes the flag directly - it is not a field of the saved settings form.
Turning checks off stops new checks from starting; a check already
running finishes, and no new check opens an update socket while checks
are off.

Applying an update is a separate, explicit action - the graphical
interface's Apply Update button or `udm-cli update apply` - and nothing
is installed until you invoke it. Apply downloads two files from the
host the check found the version on (or the other host if that fails):
`SHA256SUMS.txt` and the portable zip. Acceptance is a fixed gate,
identical for both hosts: the zip's SHA-256 must match `SHA256SUMS.txt`;
the zip is then extracted, and every executable in it (`udm.exe`,
`udm-cli.exe`, `udm-host.exe`) must carry an Authenticode signature that
verifies, whose signer certificate's public key hashes (SHA-256 over
the SubjectPublicKeyInfo) to the pin compiled into the updater. The
signing certificate is self-issued, so the pin - not a public root - is
the trust anchor; a valid RFC 3161 timestamp is also required, and
validity is judged at signing time, so a countersigned build stays
acceptable after the signing certificate expires. A tampered zip served
with recomputed sums still fails the signature gate. Anything that
fails is discarded and nothing is installed.

The swap replaces the install tree file by file: each current file
steps aside to a `.old` sibling, the new file takes its place, and any
failure puts the replaced files back. The graphical interface then
relaunches into the new build; after a command-line apply, restart UDM
yourself. At the next start, UDM deletes the `.old` backups and any
staging remnant. `udm-cli update pin` prints the compiled signing-key
pin so you can compare it with the `update-pin.txt` the installer
writes into the install directory and with the fingerprint published
with the release; it opens no update socket. The CLI itself never
schedules a check; only the desktop app runs the automatic startup and
6-hour checks.

### What an update request looks like

A check sends one HTTPS GET per host to its latest-release address and
reads the redirect target without following it. An apply sends HTTPS
GETs for `SHA256SUMS.txt` and the portable zip; redirects are followed,
so a release CDN can be contacted. A redirect or address that is not
HTTPS is refused. Requests go through WinHTTP (the operating system's
HTTP stack, with its proxy configuration), not the download engine's
HTTP/3 path. The User-Agent is `UDM/<version>`. No nonce, account,
machine fingerprint, usage data or telemetry is added to any update
request.

### What the updater can write

The updater writes only on the install base (the install directory, or
the portable executable's directory): the checks flag
`update-checks-enabled` when you toggle checks; one line per check in
`update-check.log`; the `update-staging` directory while an apply runs
(removed when it finishes); and the `.old` backups of replaced files
until the next start removes them. With checks turned off and no apply
invoked, ordinary use creates none of these. None of this is sent
anywhere. Removing the application data directory does not remove the
install base; removing the install directory, or the portable folder,
is what removes these files.

## What stays on your computer

| Data | Contents and location |
|---|---|
| Task database | `udm.redb`: URLs, file paths/names, task history/state, settings, segment coverage, remote validators, hashes, local recovery metadata and optional browser transaction markers. |
| Alt-Svc cache | In the database: learned HTTP/3 services, expiry and failure state, keyed by origin. It supports transport selection and is not uploaded. |
| Download files | Completed files and `<filename>.udm.part` partials in your selected download destination. |
| Desktop log | `logs\udm.log`: local tracing output, errors and GUI panic diagnostics/backtraces. |
| Browser handoff files | `handoff\records.json`, `callers.json`, `pairing.key`, host manifests and `host.log` under the application data root. |
| Host identity sidecar | `udm-host.caller` beside the registered host executable. It is the bare-launch identity only; browser launches are checked against each browser's own host manifest, so Chrome and Firefox beside one executable are independent. |
| Extension storage | Browser-local capture settings/exclusions and the 20 most recent handoff log entries. |

These records are not an encrypted secret store. URLs, filenames, errors and
paths can reveal private information. The host's log uses coarse operation
and outcome messages rather than request payloads, but the transaction
journal needs the intent URL and hints for recovery. Protect pairing keys
and inspect logs, screenshots or database exports before sharing them.

The desktop log is checked at startup and truncated when already over
approximately 8 MiB; it is not a continuously enforced 8 MiB quota. The
separate host log is append-only and has no size cap in this revision.

## Where application data lives

- **Per-user:** `%LOCALAPPDATA%\udm` on Windows.
- **Portable:** `data\` beside the executable when `udm.portable` exists
  there, or `UDM_PORTABLE=1` is set.
- **Override:** `UDM_DATA_DIR` selects the application data root.

The download destination is independent of portable mode: normally the
user's Downloads folder, a saved default or a per-task choice. The `tmp\`
application directory is scratch space, not the normal partial-file folder.
Development paths use the user's XDG data directory on Linux or
`~/Library/Application Support/udm` on macOS. Legacy `quget` locations and
environment names remain fallbacks for upgrades.

## Browser handoff

The extension uses the browser's `downloads`, `nativeMessaging` and `storage`
permissions. It reads download metadata to decide eligibility, pauses and
cancels eligible browser downloads during handoff, and keeps local settings
and recent outcomes. It has no page content script and does not copy cookies,
authentication headers or page contents to UDM.

The extension sends a GET intent containing URL, filename/size hints and the
browser download ID to the local `udm-host` process. The host uses a paired
Windows named pipe to the running UDM coordinator. The extension makes no
external HTTP requests of its own; UDM requests the file after commitment.
This separation means a URL that needs the browser's login session may fail
in UDM even if the browser could download it.

Capture defaults on while the extension is enabled, subject to its toggle
and exclusions. Host registration is a separate explicit opt-in, done from
Settings → Browser integration (one toggle per browser, applied
immediately, no restart) or with `udm-host --register`. Fresh UDM
settings carry a 60-second prepare lease, but no coordinator starts without
registered callers; an explicitly stored `null` lease remains disabled.
The journal preserves intents and ownership transitions locally for recovery.
See [the extension guide](../extension/README.md) for setup and current limits.

## Windows privileges and registrations

The app manifest requests `asInvoker`. The installer runs per-user. Neither
requires administrator rights, installs a service, creates a scheduled task,
writes HKLM or changes Windows network/security/privacy configuration.

There are three narrow, disclosed registry uses under the current user:

| Integration | What it writes | How to undo it |
|---|---|---|
| Per-user installation | UDM uninstall metadata under HKCU, plus user shortcuts/files. | Run the uninstaller. |
| Native Messaging host | The Chrome or Firefox HKCU host key and local registration files. | Turn the browser off in Settings → Browser integration, run `udm-host --unregister chrome` / `--unregister firefox`, or run the app uninstaller (it sweeps both browsers). |
| Start with Windows | `UDM` under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, pointing to the current executable. | Disable Start with Windows in UDM. |

Startup is off by default. Once selected, the saved setting synchronizes
the entry at app start. The settings view reads registry presence for the
toggle. Auto theme also reads Windows' app-mode registry preference; it
does not change that preference.

Portable mode does not silently register a browser host or enable startup.
The uninstaller sweeps the browser-host registration for both browsers;
undo optional startup before deleting the program.

## Deleting your data

Exit UDM from the tray before copying or deleting its database. The per-user
uninstaller leaves application data and downloaded files intact. To erase
them, remove the data directory and the download files you choose separately.
Deleting a portable folder can include its `data\` directory.

Disable startup first (the uninstaller unregisters the browser hosts for
you). Browser extension settings and recent outcomes live in browser-local
storage; removing desktop data does not clear that storage. Removing task records can also delete an owned
partial under the default removal policy. See [Operations](OPERATIONS.md).

## How to inspect the claim

```sh
cargo test -p usagi --locked --test no_telemetry
cargo deny check
```

The test inspects dependency names, scans first-party Rust sources for known
reporting endpoint patterns and checks selected GUI/store socket patterns.
The core also declares an empty `TELEMETRY_ENDPOINTS` list; the current test
does not directly assert that constant. `deny.toml` adds explicit bans
on telemetry/fingerprinting and selected update/crypto dependencies.

These checks are tripwires, not a proof that any imaginable telemetry could
never be introduced. Source review and runtime inspection remain useful.
Read `crates/core/src/net/` for HTTP/QUIC traffic and `handoff_pipe.rs` plus
`crates/host/` for local IPC. When inspecting traffic, distinguish a bound
local endpoint, an idle pooled connection and an actual outbound request.

## Future designs

The authenticated updater described above is built into the engine; the
user-facing check surfaces (the desktop app's automatic startup and 6-hour
checks, `udm-cli update checks` and `update check`, and the graphical Check
for Updates button) and both apply surfaces (the graphical Apply Update
action and `udm-cli update apply`) are part of this version. N2 dual-stack (IPv4/IPv6 selection) and N3 multi-family
transfers (splitting one download across address families) are
**design-only, not built**. N2/N3 implementation remains measurement-gated.
A future network feature needs its own documented network behaviour and a
policy update alongside implementation; a design does not change what this
version sends.
