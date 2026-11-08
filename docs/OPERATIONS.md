# udm — Operations Document

Running, deploying and troubleshooting udm on Windows 11 (primary
target), with notes for Linux/macOS. No administrator rights are required
for anything in this document.

## 1. Deployment options (Windows)

| Form | What you do | Where data lives |
|---|---|---|
| **Portable zip** (`udm-0.1.0-portable.zip`) | unzip anywhere, run `udm.exe`; the bundled `udm.portable` marker file switches to portable mode | `data\` beside the exe |
| **Per-user installer** (`installer.nsi` → NSIS) | double-click; installs to `%LOCALAPPDATA%\Programs\udm`, Start-Menu shortcut, no UAC prompt | `%LOCALAPPDATA%\udm` |
| **Bare exe** | copy `udm.exe` anywhere and run | `%LOCALAPPDATA%\udm` (no marker file) |

Portable mode is decided at startup: if a file named `udm.portable`
exists next to the executable, everything (DB, logs, partial files' default
destination) is rooted beside it — USB-stick friendly. Delete the marker to
go back to per-user mode.

The manifest embedded in `udm.exe` declares `asInvoker` (never elevates)
and a dependency on **Common-Controls v6**; both matter — see §7.

## 2. Data you own

Under the data directory (`%LOCALAPPDATA%\udm` or `udm-data\`):

```
udm.redb        the database: tasks, segments, settings, Alt-Svc cache
logs\udm.log    capped text log (level via UDM_LOG or Settings → Logging)
tmp\              scratch space for the engine
```

* **Backup** = copy `udm.redb` while udm is not running (the file is
  exclusively locked during operation). Partial `.part` files can be copied
  too; a restored DB + matching `.part` files resumes where it stopped.
* **Migration** = copy the whole directory (or export URLs via
  `udm-cli list` and re-import elsewhere).
* **Uninstall (portable)** = delete the folder. **Uninstall (per-user)** =
  run the NSIS uninstaller or delete `%LOCALAPPDATA%\Programs\udm` and
  `%LOCALAPPDATA%\udm`. Nothing is written anywhere else — no registry
  keys, no services, no scheduled tasks.

## 3. Single-instance behaviour

The database is opened with an exclusive lock. Starting a second copy fails
fast with *“udm is already running (the database … is locked)”* and exit
code 2. This is by design: v1 has no IPC surface, so cross-process control
(e.g. pausing from a second window or from the CLI while the GUI runs) is
intentionally impossible. Stop the GUI first, then use `udm-cli`.

## 4. Everyday CLI operations (`udm-cli.exe`)

```
udm-cli get <url>... [--segments 8] [--dir D:\dl] [--hash sha256:...] [--paused]
udm-cli probe <url>            # size / Range support / HTTP/3 offer, no download
udm-cli import urls.txt        # one URL per line, '#' comments
udm-cli list                   # ids, states, progress from the DB
udm-cli watch                  # resume queued/paused/failed and follow progress
udm-cli pause|resume|cancel|retry <id>...
udm-cli remove <id>... [--files]
udm-cli settings [--show | key=value ...]
udm-cli doctor                 # paths, TLS trust, QUIC availability, telemetry=none
```

Exit codes: 0 success, 1 general failure, 2 database locked. `cancel`
keeps the `.part` file (resume later); `remove --files` deletes data.

## 5. Crash / power-loss behaviour

Nothing special is required: state changes are committed durably, byte
counters are flushed every couple of seconds, and `.part` files are the
second source of truth. After an abnormal exit, start udm again — tasks
re-probe, validate (ETag / Last-Modified) and continue from the last
durable offsets. Worst case a few hundred KB are re-downloaded. A changed
remote file (validator mismatch) restarts that task from zero instead of
producing a corrupt mix — this is deliberate.

## 6. Performance & resource notes

* Concurrency: `max_tasks` (scheduler) × `segments` (connections per task).
  3 × 8 is the default; large numbers mainly help on high-latency links.
* Disk: the STORAGE widget in the sidebar shows free space of the volume
  holding the current download directory (2 s refresh). Downloads pause is
  manual; v1 has no low-disk auto-stop (out of scope).
* Memory: roughly flat; buffers are per-connection and bounded.
* QUIC uses UDP: restrictive networks that break UDP fall back to HTTP/2
  automatically; the row badge shows what is actually in use.

## 7. Troubleshooting

| Symptom | Cause / fix |
|---|---|
| “Entry Point Not Found: TaskDialogIndirect” at startup | An exe built **without** the Common-Controls v6 dependency in its manifest (comctl32 v5 gets loaded and lacks the API — the process dies before `main`). Fixed in this repo since the manifest gained the `<dependency>` block (see BUILD.md §5); rebuild or use current artifacts. |
| Windows SmartScreen warning | The binaries are unsigned (no code-signing certificate in v1). “More info → Run anyway”. Signing is a release-infrastructure task, tracked separately. |
| Second window won’t open | Single instance by design (§3). |
| HTTP/3 never used | Server must advertise Alt-Svc (or set “HTTP/3 only” to force); `udm-cli probe <url>` shows what the origin offers; `doctor` shows whether QUIC is available locally; firewalls that block UDP force fallback. |
| Download stuck at 100 % in “Verifying” | Large file + slow checksum path; the row shows `verifying <speed>`. A mismatch lands the task in Failed with both digests — compare against the publisher’s checksum. |
| Proxy environments | System proxy is honoured for HTTP/1.1–2 (toggle in Settings). QUIC ignores proxies; set “Never HTTP/3” if your network requires proxy-only egress. |
| Log inspection | Settings → Logging sets the level; “Open log file” (About) reveals `udm.log`. |

## 8. Privacy in operation

Zero telemetry is structural, not a setting: no analytics/crash-reporting
crates are linked (enforced by `deny.toml` + `tests/no_telemetry.rs`), no
update checks exist, and the only outbound connections are the downloads
you create plus their probes. `udm-cli doctor` prints the same claim from
the running binary. Details: `PRIVACY.md`.
