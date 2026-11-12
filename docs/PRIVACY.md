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
The built-in updater has its own section below; in ordinary use it makes no
connections at all.

## Idle does not mean no socket exists

UDM does not poll a project endpoint or send an idle telemetry beacon. Its
transport stack can create a local QUIC UDP endpoint during initialization
and retain pooled connections after requests finish. Such endpoints or idle
sockets can appear in operating-system tools even with no active download.
Protocol connection maintenance can also persist while a pool is alive.

The privacy claim is absence of telemetry and unsolicited project-service
traffic, not a guarantee of zero socket handles whenever the queue is empty.
`udm-cli doctor` itself initializes a network stack to check QUIC availability.

## Updates and the built-in updater

The engine contains a complete authenticated updater: it can fetch a signed
update manifest and payload, verify them, stage the files and swap them into
the install directory with a rollback journal. **No part of the application
you run invokes it.** The graphical interface has no update control, and the
only update command, `udm-cli update pin`, prints the compiled publisher pin,
the publisher public key and the recovery pin, so you can compare them with a
release you downloaded by hand; it opens no update socket and does not
download, stage or install anything. There is no automatic check, no polling
and no background timer, and the running application opens no update socket
while idle. Updates reach your machine the way they always have: you download
a new version from the release pages and install it yourself.

Rotation and revocation records — the signed records that replace or retire a
publisher key — are verified only when a caller passes their bytes to the
engine. The fetch flow does not download those records, and no command in
this version passes them.

### If the engine's fetch is invoked

The updater's fetch takes exactly two HTTPS manifest addresses from its
caller. It has no built-in origin and no third slot. A caller that follows
the project's release layout passes the GitHub (`iamcino/udm`) and GitLab
(`cino-group/udm`) release manifests, GitHub first; if a fetch succeeds on
the second origin, that origin is tried first for the rest of that session,
in memory only. The running application has no caller, so ordinary use
contacts neither host. The payload addresses are the HTTPS URLs inside the
signed manifest, not a compiled host list; the `sha256` query parameter on
them is stripped before the payload request. HTTPS redirects are followed,
so a release CDN can be contacted. A manifest, payload or redirect address
that is not HTTPS is refused.

A fetch sends one manifest request per origin tried, then the payload
requests. Every manifest request carries a `nonce` query parameter: 16
random bytes written as hex. It is a cache-buster, not an identity, and
payload requests do not carry it. Payload requests send `Range: bytes=N-`
only when a partial file already holds bytes; otherwise they start at
byte 0. The User-Agent is the built-in `udm/<version>` string, not the
configurable download User-Agent. The updater uses the system HTTP proxy and
does not consult the download setting that turns that proxy off. Its TLS is
rustls with the operating-system trust store, over HTTP/1.1 and HTTP/2 only —
not the download engine's HTTP/3 path — and the publisher pin is not a TLS
root. No account, machine fingerprint, usage data or telemetry is added.

Before any payload is downloaded, the engine checks, in order: the manifest
signature against the publisher keys the trust state currently allows (a
revoked key is refused); the manifest epoch, strictly greater than the stored
last-seen epoch; the manifest version, strictly greater than the installed
version; the installed version, at or above the manifest's `min_version`; and
the validity window (not expired, at most 14 days, `issued_at` at most one
day ahead). On acceptance the new epoch is stored before any payload byte is
requested, and a later payload failure does not undo that store. The whole
archive is then checked against the SHA-256 named in the signed payload URLs,
and every extracted file is checked against the manifest's path, hash and
size, both ways. Staging and the rollback journal run only when a caller
invokes them, and startup does not run updater recovery.

### What the updater can write

The engine writes updater files only when a caller invokes it, and neither
the graphical interface nor the command line does, so ordinary use creates
none. When a caller does invoke it: the last-seen epoch is a JSON file at a
path the caller chooses; the trust state is `updater-trust.json` on the
install base (`%LOCALAPPDATA%\Programs\UDM`, or the portable executable's
directory); and the rollback journal is a table in `udm.redb` on that same
install base — not the application database under `%LOCALAPPDATA%\udm`.
Staging files, `updater-recovery.log` and `.old` copies sit on that install
base too, and a payload partial sits in the destination directory the caller
supplied. None of this is sent anywhere. Removing the application data
directory does not remove the install base; removing the install directory,
or the portable folder, is what removes these files.

## What stays on your computer

| Data | Contents and location |
|---|---|
| Task database | `udm.redb`: URLs, file paths/names, task history/state, settings, segment coverage, remote validators, hashes, local recovery metadata and optional browser transaction markers. |
| Alt-Svc cache | In the database: learned HTTP/3 services, expiry and failure state, keyed by origin. It supports transport selection and is not uploaded. |
| Download files | Completed files and `<filename>.udm.part` partials in your selected download destination. |
| Desktop log | `logs\udm.log`: local tracing output, errors and GUI panic diagnostics/backtraces. |
| Browser handoff files | `handoff\records.json`, `callers.json`, `pairing.key`, host manifests and `host.log` under the application data root. |
| Host identity sidecar | `udm-host.caller` beside the registered host executable. |
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
and exclusions. Host registration is a separate explicit opt-in. Fresh UDM
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
| Native Messaging host | The Chrome or Firefox HKCU host key and local registration files. | `udm-host --unregister chrome` or `--unregister firefox`. |
| Start with Windows | `UDM` under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, pointing to the current executable. | Disable Start with Windows in UDM. |

Startup is off by default. Once selected, the saved setting synchronizes
the entry at app start. The settings view reads registry presence for the
toggle. Auto theme also reads Windows' app-mode registry preference; it
does not change that preference.

Portable mode does not silently register a browser host or enable startup.
At this revision the uninstaller only cleans up its own installation entry;
undo optional startup and host registration before deleting the program.

## Deleting your data

Exit UDM from the tray before copying or deleting its database. The per-user
uninstaller leaves application data and downloaded files intact. To erase
them, remove the data directory and the download files you choose separately.
Deleting a portable folder can include its `data\` directory.

Unregister browser hosts and disable startup first. Browser extension settings
and recent outcomes live in browser-local storage; removing desktop data
does not clear that storage. Removing task records can also delete an owned
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

The authenticated updater described above is built into the engine; a
user-facing update check and apply surface is a later package, not part of
this version. N2 dual-stack (IPv4/IPv6 selection) and N3 multi-family
transfers (splitting one download across address families) are
**design-only, not built**. N2/N3 implementation remains measurement-gated.
A future network feature needs its own documented network behaviour and a
policy update alongside implementation; a design does not change what this
version sends.
