# UDM architecture

UDM separates download work from presentation. **Usagi**, packaged as
`usagi` in `crates/core`, owns transfers, task state and persistence. The
desktop app and CLI are front-ends; the browser host bridges a deliberately
narrow local handoff protocol. This describes the implementation at `241f550`.

Read [Engine](ENGINE.md) for transfer safety, [Design](DESIGN.md) for the
interface, and [Build](BUILD.md) for toolchains and generated UI code.

## Components and boundaries

| Component | Responsibility |
|---|---|
| `crates/core` — `usagi` | Engine API, queue admission, segment workers, transports, hashes, category routing, redb storage, handoff coordinator and pipe protocol. |
| `crates/gui` — package `udm-gui`, binary `udm` | Slint window, task/detail views, settings, tray, popup, startup registration and coordinator boot. |
| `crates/cli` — `udm-cli` | Standalone downloads, probes, imports, task operations, settings and diagnostics. |
| `crates/host` — `udm-host` | Browser Native Messaging framing, local pipe forwarding and explicit per-user registration commands. |
| `extension/` | Chrome/Firefox download interception, exclusions, local options and handoff presentation. |
| `packaging/` | Windows resources, per-user installer, build/regeneration scripts and verification campaigns. |

GUI, CLI and host depend on `usagi`; the core does not depend on a front-end.
The GUI owns an `Arc<Engine>`. The host is a separate process launched by
the browser, not a service or a second engine instance.

```text
Browser extension ── Native Messaging ── udm-host
                                             │ paired per-user named pipe
                                             ▼
udm (Slint + tray) ── Engine API       handoff Coordinator
        │                                    │ EngineAdapter
        └────────────────┬───────────────────┘
                         ▼
                     Usagi engine ◄── udm-cli (when DB is free)
                         │
            ┌────────────┼─────────────┐
            ▼            ▼             ▼
       HTTP stack     redb store    download files
     H3 / H2 / H1.1   udm.redb      .udm.part → final
```

The coordinator journal is a separate local JSON file, not a redb table.
The host and GUI must resolve the same data root for pairing and recovery.

## Execution and ownership

The engine runs a Tokio multi-thread runtime and a supervisor that admits
queued work up to `max_concurrent_tasks`. Each admitted task drives its
segment workers. Pause, cancel, retry and removal pass through engine
lifecycle handling; revoking an attempt and draining its workers matters
before files or ownership can be released.

Each run has a generation. File claims, progress mutations, persistence and
finalization check that generation, so an old worker cannot publish into a
new attempt. Persistence also uses an epoch and a shared save lock to keep
task records and segment coverage from different moments out of one commit.
Generation counters are runtime fences, not restart recovery proofs.

The Slint event loop runs on the main thread. `src/state.rs` connects
callbacks to engine actions and refreshes snapshots every 150 ms. A slower
2 s refresh handles auxiliary information such as storage and Auto theme
resolution. Tray summaries are throttled to at least 900 ms and updated
only when their composed text changes. Network transfers run outside the
UI event loop; snapshots are shared state reads, not a second task database.

Engine events provide local feedback such as failure toasts. They are not
an analytics stream. The GUI installs a panic hook that writes diagnostics
and a backtrace to its local log, because release builds have no console.

## UI contract

`crates/gui/ui/app.slint` declares the main window properties and callbacks.
`model.slint` defines row data; `state.rs` maps engine snapshots to it and
implements actions, selection, detail presentation and filtering. The tray,
startup and handoff boot modules own their platform-specific responsibilities.
The popup has its own Slint window and receives its translated data from Rust.

Rows carry action capability flags derived from engine state. The list can
show a record without making it actionable: coordinator recovery rows use
`recovery:<tx>` keys, which do not parse as numeric engine task IDs. They
appear only in **All**, respect the text filter and expose no engine actions.
They are not an additional `TaskState` variant.

Appearance tokens bridge UDM's `ui/theme.slint` to the vendored cino-ui theme.
Dark and Light are explicit choices; Auto resolves Windows' app colour
preference. Generated Rust is checked in for builds without `slint-compile`;
both paths must stay synchronized. See [Build](BUILD.md#keep-both-ui-build-modes-in-step).

## Storage and schema

`Store` opens `udm.redb` exclusively. **The schema marker is 2.** Table names
retain their `.v1` suffix; that suffix is not the database schema version.
Task, segment, settings and Alt-Svc values are encoded as JSON bytes by the
in-repository codecs. The schema marker is a little-endian `u32`, and the
task-id counter is a `u64`; neither is JSON.

| Table | Key | Stored information |
|---|---|---|
| `tasks.v1` | Task ID (`u64`) | Task identity/state, paths, remote metadata, hash data, route state, representation binding, checkpoint, finalize intent and optional handoff marker. |
| `segments.v1` | Task ID (`u64`) | Segment indices, inclusive ranges, contiguous completed bytes and EOF/finished flags. |
| `meta.v1` | String | Settings JSON and the schema marker. |
| `counters.v1` | String | Next task ID. |
| `alt_svc.v1` | Origin string | Learned HTTP/3 service, expiry and failure state. |

Opening a v1 database retains its tables and records while upgrading the
schema marker. A newer unsupported schema is refused. Schema 2 prevents old
engines from ignoring finalize intents; it does not add renamed `.v2` tables.

Progress crosses several boundaries: received bytes, buffered writes, flushed
coverage and durable coverage are different facts. Before storing a progress
checkpoint, Usagi synchronizes the held partial-file handle and binds the
snapshot to file identity. The periodic progress commit is durable (default
`persist_interval_ms = 1000`); minor intermediate saves may be non-durable.
The interval is a target cadence, not an upper bound on bytes lost or time
to checkpoint under disk failure or load.

Recovery trusts a valid checkpoint, its matching file identity and coverage,
then revalidates the remote representation before reuse. File length alone
does not prove valid segment content. Finalization uses a durable intent
because filesystem rename and database completion cannot be one atomic
transaction. Startup reconciles these intents before recovering interrupted
active tasks as **Paused**. See [Engine](ENGINE.md) for the full sequence.

## Browser handoff

`handoff.rs` implements a serialized coordinator; `handoff_pipe.rs` supplies
versioned JSON framing and the Windows per-user named-pipe boundary. The
host uses a pairing secret and the registered caller identity, then forwards
requests. Its stdout is reserved for Native Messaging frames.

The GUI starts the coordinator only when a positive handoff lease and a
nonempty registered caller list exist. Fresh settings default to 60 seconds;
an explicitly stored `null` remains disabled. Registration remains the
opt-in gate. No companion background service starts UDM on the host's behalf.

1. **Prepare** validates and durably records the intent before acknowledging.
2. The extension confirms cancellation of the browser download.
3. **Commit** durably records the commitment before associating an engine task.
4. A persisted `handoff_tx` marker lets recovery find an already-created task
   instead of creating a duplicate when association recording was interrupted.

`handoff/records.json` stores the coordinator records; `callers.json` is the
allow-list, `pairing.key` is replaced at each coordinator start, and
`host.log` contains coarse local host diagnostics. Pending committed
associations surface through `Coordinator::recovery_entries()` as **Needs
recovery** rows. Expiry before commitment does not authorize an automatic
download; extension-side recovery presentation and GUI pending-association
rows cover different predicates. See [extension details](../extension/README.md).

## Filesystem and Windows integration

`Paths::discover()` resolves application data to `%LOCALAPPDATA%\udm`, or
`data\` beside the executable in portable mode. `UDM_DATA_DIR` overrides the
data root; `UDM_DOWNLOAD_DIR` controls the default download destination.
The selected download directory holds both partial and completed files.
Legacy `quget` names remain fallbacks for upgrades.

The application runs as the current user. The disclosed Windows registry
exceptions are the installer's uninstall entry, explicit Native Messaging
host registration, and the optional `UDM` HKCU Run startup value. Auto theme
reads the Windows app-mode preference. There are no HKLM writes, services,
scheduled tasks or elevation requests. Startup registration is synchronized
from the saved setting when UDM starts, including its current executable path.

The database lock still excludes a second GUI or CLI engine on the same data
root. Browser handoff IPC does not turn the CLI into a remote control client.

## Measurements and scope limits

Task and aggregate speeds, byte counters, request lifetimes and protocol
connection gauges have distinct meanings. HTTP/2 and HTTP/3 multiplex
requests; segment counts cannot stand in for physical connection counts.
The current HTTP/1.1 and HTTP/2 pools do not expose the needed census, so
`EngineConnections` is **Unavailable**, not zero or a derived H3 subtotal.
All instrumentation is local; there is no metric export.

Timed queue scheduling, a global speed limit, torrents and media resolving
are absent. The authenticated updater, N2 dual-stack (IPv4/IPv6 selection)
and N3 multi-family transfers (splitting one download across address families)
are design-only, not built. N2/N3 implementation remains measurement-gated.
