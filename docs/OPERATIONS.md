# Run, maintain and troubleshoot UDM

UDM runs under your own Windows account. You do not need administrator
rights or a background service. This guide covers the code at `241f550`;
see [Build](BUILD.md) to produce the binaries and packages.

## Choose an installation

| Form | How to run it | Default application data |
|---|---|---|
| Per-user installer | Run `udm-0.1.0-setup.exe`; it installs under `%LOCALAPPDATA%\Programs\udm` and creates shortcuts. | `%LOCALAPPDATA%\udm` |
| Portable ZIP | Extract the archive, then run `udm.exe` inside the extracted program folder with its `udm.portable` marker. | `data\` beside the executable |
| Bare executable | Put `udm.exe` in a user-writable location and run it without a portable marker. | `%LOCALAPPDATA%\udm` |

Portable mode changes application data, not the download destination.
Downloads normally use your Windows Downloads folder or the folder chosen
in Settings/Add Download. Partials are saved in that destination as
`<filename>.udm.part`, not in the application's `tmp\` directory.

The current packages contain the GUI and CLI. They omit the browser host
and extension; [browser setup](../extension/README.md) requires a separate
`udm-host` build and explicit registration. The host does not launch UDM
when the desktop app is closed.

## Everyday desktop use

Paste an HTTP/HTTPS file link into the toolbar and press Enter. The full
Add Download form accepts multiple links, a folder, checksum and task
options. Use the library views and text filter to find tasks, and their
available row/menu actions to pause, resume, cancel, retry, inspect or remove.

Closing the main window hides it to the tray by default. Double-click the
tray icon to show it; a single click does nothing. Open **Active downloads**
from the tray menu or sidebar for a small popup with totals and pause/resume
controls. It also includes paused tasks. Use **Exit** in the tray menu to
stop the app and release its database lock.

Settings offers Dark/Light/Auto, English/Vietnamese and accent colours.
Auto follows the Windows app theme. Close to tray is on by default;
minimize to tray, start minimized and start with Windows default off.

**Start with Windows** writes the `UDM` value under
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, using the quoted
current executable path and `--minimized` when selected. Disabling the option
removes it. The setting is synchronized on launch, so disable it in UDM
before removing or moving the program permanently. Prefer a stable program
folder; Settings warns when the executable is under Temp or Downloads.

## Locate your data

`UDM_DATA_DIR` overrides the application data root. `UDM_PORTABLE=1` forces
portable mode; `UDM_PORTABLE=0` selects per-user mode instead of the marker.
`UDM_DOWNLOAD_DIR` overrides the default download destination when no
explicit saved/task destination applies. `udm-cli --data-dir <dir>` selects
a CLI data root. Use `udm-cli doctor` to inspect resolved paths after exiting
the GUI.

| Location | Contents |
|---|---|
| `udm.redb` | Tasks, segments, settings, counters and Alt-Svc cache; schema marker 2. |
| `logs\udm.log` | Local desktop diagnostic log, with size-based truncation. |
| `tmp\` | Application scratch directory; not the download-partial directory. |
| `handoff\records.json` | Durable browser transaction journal. |
| `handoff\callers.json` | Registered browser callers. |
| `handoff\pairing.key` | Local pairing secret replaced at coordinator start. |
| `handoff\host-manifest-<browser>.json` | Native Messaging registration manifest. |
| `handoff\host.log` | Append-only coarse host diagnostics; not governed by the desktop log cap. |

The registered host also has `udm-host.caller` beside its executable. Keep
pairing material private. URLs, filenames, paths and errors in local data
may contain sensitive information; review them before sharing.

Pre-rename `quget.portable`, `QUGET_*` environment fallbacks and existing
`quget` data directories remain supported. The database filename migrates
from `quget.redb` to `udm.redb` when possible. Check the resolved directory
before assuming an upgrade started with an empty history.

## Back up or move an installation

1. Disable browser capture temporarily and exit UDM from the tray. Close CLI
   processes and let native host activity finish.
2. Copy the application data directory, including the database and handoff
   journal. Copy any partial and completed files you need from their actual
   download destinations.
3. Preserve the backup unchanged before testing a restored copy.

Tasks store destination paths. Copying the database or choosing a new default
download directory does not rewrite those existing paths. Resume also needs
a matching partial-file identity and checkpoint; copying a partial to another
file or disk can invalidate that proof and require re-downloading its bytes.

Native Messaging manifests and HKCU startup values contain executable paths.
After moving a host, unregister/re-register it deliberately using the same
data root as UDM. A pairing secret from an old session is not a portable
credential; UDM replaces it on startup. Do not open the original and restored
copies against the same downloads concurrently.

## Command-line operations

The CLI opens the engine/database itself. It does not send commands to a
running GUI. The database is exclusively locked: exit the desktop app before
using commands on the same data root. A different `--data-dir` creates a
separate task store, not another view of the existing one.

```powershell
udm-cli get https://example.com/file.iso --segments 8 --dir D:\Downloads
udm-cli get https://example.com/file.iso --paused
udm-cli probe https://example.com/file.iso --headers
udm-cli probe https://example.com/file.iso --h3
udm-cli import urls.txt
udm-cli list
udm-cli watch
udm-cli pause 3
udm-cli resume 3
udm-cli cancel 3
udm-cli retry 3
udm-cli remove 3
udm-cli settings --list
udm-cli settings segments_per_task=12 max_concurrent_tasks=3
udm-cli doctor
```

Replace example URLs and IDs with real ones. Supply an expected checksum
with `get --hash sha256:<full-digest>`. Import files contain one URL per
line and support `#` comments. `watch` resumes queued/paused tasks and follows
them; it skips every terminal state: Completed, Failed and Canceled. Use
an explicit retry for failed tasks. `watch` is an active operation. `get --no-wait` shuts
down its engine after enqueueing; it does not leave a background daemon.

Pause and cancel keep the partial for later use. **Remove** deletes task
records; with the default `delete_part_on_remove=true`, it also deletes an
owned partial even without `--files`. **Remove `--files`** additionally
requests deletion of a completed output. Do not use removal to preserve
resume information. Ownership checks protect unrelated or ambiguously
claimed partials from deletion.

Settings use positional `key=value` pairs, not `--set`, `--show` or a settings
file. `--list` prints them and `--reset` restores defaults. Not every stored
field has a CLI setter: in particular, handoff lease and lifecycle/startup
fields are not accepted by `Settings::apply_pair` at this revision. Use the
desktop controls where provided; do not assume an undocumented JSON config
file exists.

| Outcome | Status | Where the message goes |
|---|---|---|
| Success | 0 | stdout |
| Top-level error, including a locked database | 1 | stderr (`udm-cli: …`) |
| `probe` failure | 2 | stdout (`probe failed: …`) |
| `get` / `import` / `watch` task failed or canceled | 3 | Failure line on stderr for Failed; closing summary on stdout, including Canceled. |
| Ctrl-C during a wait | 130 | stdout |

The GUI separately exits with code 2 for a locked database.

## After a crash or interrupted download

Restart UDM. It reconciles finalization intents and recovers interrupted
active tasks as **Paused**; they do not silently resume just because the app
reopened. Resume a task to probe the server again and reuse eligible progress.

Usagi requires a valid local checkpoint and a matching strong-ETag
representation binding. Weak ETags, Last-Modified alone, changed final URLs
or incompatible metadata may cause a restart from zero. The default
checkpoint cadence is one second, but disk failures or load can delay it;
there is no fixed maximum number of bytes lost after a power cut.

Completion includes a local recovery digest even without a publisher
checksum, so **Verifying** can be normal for a large file with no supplied
hash. A failed checksum comparison is an error, not successful completion.
See [Engine](ENGINE.md) for checkpoints and rename reconciliation.

## Browser recovery

Fresh settings use a 60-second prepare lease. The coordinator also requires
registered callers; an existing explicit `null` lease remains disabled.
Restart UDM after registration so it can read the caller list.

Before a durable prepare acknowledgement, an unavailable host/app leaves
the browser download in charge. After confirmed cancellation, do not assume
the browser still has a resumable file. The extension can show a recovery
outcome for an expired or unresolved commit. UDM's **Needs recovery** rows
show committed records whose engine association is still pending; they are
read-only, appear in **All**, and are not repaired by normal task actions.

Check the task list and extension history before manually adding the same
URL again. Keep the journal for diagnosis rather than deleting it to hide
the warning. The current extension has a post-cancellation failure-reporting
limitation described in [its guide](../extension/README.md#recovery-and-current-limits).

## Troubleshooting

| Symptom | What to check |
|---|---|
| Another copy cannot open the database | Exit UDM from the tray, not just the window. Close CLI processes using the same root. |
| `TaskDialogIndirect` entry-point error before the window appears | The Windows build needs the embedded Common-Controls v6 manifest. Rebuild with resources enabled; see [Build](BUILD.md#windows-resources-that-must-survive-packaging). |
| Process exits before the first frame | Inspect the local log and build resources/stack settings. A pre-logging loader failure may have no app log. |
| HTTP/3 is not used | Check the server's offer with `probe`; use `--h3` to try without a cached offer. H3 targeting requires HTTPS with a DNS hostname. For eligible targets, HTTP/3-only returns an H3 failure; plain HTTP/IP-literal URLs and globally disabled H3 still use TCP. See [Engine](ENGINE.md#3-choose-a-transport). |
| Downloads fail on a proxy-only network | The system-proxy option defaults on and can be turned off; check that it is on. Separately select Never HTTP/3 because QUIC bypasses HTTP proxies. |
| A resumed download starts from zero | The local checkpoint or remote representation did not meet reuse requirements. Last-Modified alone is insufficient. |
| Progress is at 100% but task is not completed | EOF validation, disk sync, hashing or finalization may still be running. Check the state/error before treating the file as finished. |
| Category routing did not move an old task | Routing is captured for eligible new tasks; existing/explicit destinations keep their route. |
| Browser capture does nothing | Check separately built host, registration sidecar, caller list, matching data root and running GUI; see [browser setup](../extension/README.md). |

The About actions open the data directory and log. At this revision the
desktop logger initializes from `UDM_LOG` at launch (default `info`); although
Settings saves a log-level field, it does not reload or configure that logger.
CLI `--log` controls console logging. When reporting a problem, include version,
task state, transport, reproduction steps and a redacted log excerpt; never
include pairing keys or private URLs.

## Removing UDM

1. Turn off **Start with Windows** in UDM while its current executable still
   exists. This removes the per-user Run value.
2. If registered, run `udm-host --unregister chrome` and/or
   `udm-host --unregister firefox` using the registered host/data root.
   Disable or remove the extension in the browser.
3. Exit UDM and other processes using its data.
4. Run the per-user uninstaller, or delete the portable/program folder.

At this revision the NSIS uninstaller removes its own app files, shortcuts
and uninstall entry. It does **not** undo the separately configured startup
or Native Messaging registrations. It deliberately keeps application data
and downloaded files. Delete those separately only if you no longer need
history, recovery or downloaded content. Removing a portable folder can
also remove its `data\` directory, so back it up first if needed.

## Privacy and current limits

UDM is offline-first: local history/settings need no remote service. It has
no telemetry or version checks. Downloads, probes, redirects and their
network setup still contact external servers. Idle transport pools can
retain sockets; absence of telemetry does not mean there can never be an
idle UDP endpoint. See [Privacy](PRIVACY.md).

A global speed limit, timed queue scheduling, torrents and media resolving
are not implemented. The authenticated updater, N2 dual-stack (IPv4/IPv6
selection) and N3 multi-family transfers (splitting one download across
address families) are design-only, not built. Update by replacing the program
from a trusted build after exiting it. N2/N3 implementation remains
measurement-gated.
