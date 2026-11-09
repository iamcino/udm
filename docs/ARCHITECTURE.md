# Universal Download Manager (UDM) — Architecture

**Usagi** is the name of UDM's core engine after the engine safety upgrade.
The current implementation is packaged as `usagi` under `crates/core`.
The desktop application and CLI remain `udm` and `udm-cli`.

How the system is put together. Companion documents: `DESIGN.md` (product &
UI language), `ENGINE.md` (download pipeline in depth), `BUILD.md` (building
and shipping), `OPERATIONS.md` (running it), `REFERENCES.md` (specs & deps).

## 1. Crate map

```
udm/
├── crates/
│   ├── core/   usagi  — the whole product minus pixels (13,215 lines)
│   │   ├── engine/   mod.rs (queue, API, persistence) · task.rs (one
│   │   │             download's state machine) · speed.rs (EWMA meter)
│   │   ├── net/      stack.rs (transport selection, segmented transfer)
│   │   │             h3.rs (quinn+h3) · http12.rs (reqwest) · tls.rs
│   │   │             (rustls+ring, OS trust) · altsvc.rs (cache)
│   │   ├── store/    mod.rs (redb wrapper) · records.rs (codecs, TaskState)
│   │   ├── config.rs  Settings, TransportPref, ConflictPolicy, LogLevel
│   │   ├── hash.rs    ExpectedHash + streaming verifiers (5 algorithms)
│   │   ├── paths.rs   data-dir discovery: portable marker vs %LOCALAPPDATA%
│   │   ├── error.rs   one error enum for the whole core
│   │   └── util.rs    human_bytes/speed/eta, timestamps, fs helpers
│   ├── cli/    udm-cli — headless front-end (clap): get, probe, import,
│   │           list, watch, pause, resume, cancel, retry, remove, settings,
│   │           doctor; global --no-learning
│   └── gui/    udm — Slint 1.18.1 window (software renderer, winit backend)
│       ├── ui/        app.slint (shell + full Rust contract), theme.slint
│       │              (tokens), model.slint (row structs), widgets.slint,
│       │              views/{downloads,add,settings,about}.slint
│       ├── src/state.rs  the only Rust↔Slint glue (150 ms poll loop)
│       ├── src/main.rs   startup: paths → logging → engine → window
│       └── src/slint_generated.rs  checked-in compiler output (see BUILD.md)
├── tests/      integration + policy tests (incl. no_telemetry.rs)
├── packaging/  windows/ (app.manifest, udm.rc, build.ps1, installer.nsi)
│               cross/build-windows.sh · regenerate-slint.sh
└── docs/       this set + PRIVACY.md
```

Dependency rule: `cli` and `gui` depend on `core`; `core` depends on nothing
internal and knows nothing about UI. All three binaries are single-process.

## 2. Layering

```
┌─────────────────────────┐   ┌─────────────────────────┐
│  udm (Slint GUI)      │   │  udm-cli (clap)       │
│  UI thread only —       │   │  one-shot or watch mode │
│  polls, never blocks    │   └───────────┬─────────────┘
└───────────┬─────────────┘               │
            │  Arc<Engine>                │
┌───────────▼─────────────────────────────▼─────────────┐
│ engine      queue scheduler · task cells · events     │
│             persistence policy (flush_dirty/flush_all)│
├───────────────────────────────────────────────────────┤
│ net         NetStack: probe → choose H3/H2/H1.1 →     │
│             segmented ranged transfers → resume       │
├───────────────────────────────────────────────────────┤
│ store       redb: tasks.v1 segments.v1 meta.v1        │
│             counters.v1 alt_svc.v1                    │
└───────────────────────────────────────────────────────┘
```

## 3. Concurrency model

* **Engine**: one Tokio runtime (multi-thread, N worker threads) owns all
  network I/O. Each active task is a supervised Tokio task driving a
  `TaskCell` (in-memory mirror of the DB record + atomics for live stats).
* **API**: `Engine` methods (`add`, `pause`, `resume`, `cancel`, `retry`,
  `remove`, `update_settings`) are cheap, lock-guarded queue mutations —
  safe to call from any thread. Results surface as `Event`s.
* **Events**: a bounded queue drained by the front-ends (`drain_events(32)`
  per GUI tick). The GUI turns `TaskFailed` into a toast; nothing else
  subscribes — there is no telemetry pipeline by construction.
* **GUI**: the Slint event loop runs on the main thread. A 150 ms timer
  pulls `engine.snapshot()` / `engine.totals()` (lock-free reads of the
  task cells) and patches the `VecModel` row-by-row when lengths match, so
  scrolling and selection state survive refreshes. A 2 s slow tick handles
  DB stats, the Alt-Svc table, and disk free space.
* **CLI**: `get`/`watch` block on the same snapshot loop with ANSI output;
  one-shot verbs (`pause 42`) mutate and exit.
* **Connection totals (owner decision 2026-11-08):** the engine reports no
  connection total for v0.1.0 - `EngineConnections` is `Unavailable` and the
  H1/H2 gauges read `Unknown`, because the pinned reqwest/hyper-util stack
  exposes no pool observability. Totals must never be derived from
  per-protocol counts, and no pool is owned or replaced in v1. Reopen only if
  the owner approves an N1 connection budget, N2/N3 multi-path starts,
  upstream ships pool observability, or a real incident traces to the
  missing count.

**Single instance**: redb opens the database file exclusively. A second
process gets `Error::DatabaseLocked` and exits with code 2 (GUI shows the
message). This is deliberate: cross-process control would need an IPC
surface, which v1 does not want.

## 4. The Slint ↔ Rust contract

`ui/app.slint` declares the whole interface; `src/state.rs` implements it
and nothing else touches Slint types.

* **Window → Rust (callbacks)**: `pause/resume/cancel/retry/remove/
  open-file/open-folder (float id)`, `add-submitted`, `add-pick-dir`,
  `settings-pick-dir`, `settings-saved`, `settings-reset`,
  `open-data-dir`, `open-log`, `clear-finished`, `add-from-clipboard`.
* **Rust → window (properties)**: `tasks: [TaskRow]`, `status: StatusInfo`
  (counts, speed, db, mode, data dir, `disk-text`, `disk-fraction`),
  `endpoints: [EndpointRow]`, `version-text`, `http3-available`, plus the
  `add-*` / `set-*` form mirrors and `toast`/`toast-error`.
* **Purely-UI state**: `view` (0 downloads / 1 add / 2 settings / 3 about),
  `library` (0 all / 1 active / 2 completed / 3 failed) and `filter`
  (substring). `library`/`filter` are read by `state.rs` each tick and
  applied when mapping snapshots → rows, so the Rust side owns filtering.
* **Row shape** (`model.slint TaskRow`): id, name, url, state + label,
  received/total text, percent (−1 = unknown), speed/eta text, transport
  (H3/H2/H1/?) + label, segments text, error text, path, created text,
  hash text, `glyph` (category emoji derived from the extension in Rust),
  and the `can-*` capability flags that drive which row actions render.

Capability flags come from the task state machine (`state.rs::row_from`),
never from the view — the UI cannot invent an illegal action.

## 5. Persistence (redb)

| Table | Key → value | Contents |
|---|---|---|
| `tasks.v1` | id → bincode-ish record | url, filename, state, sizes, etag/last-modified, error, timestamps, hash expectation/result, transport pref |
| `segments.v1` | id → record | per-segment start/end/written |
| `meta.v1` | str → record | settings blob, schema version |
| `counters.v1` | str → u64 | next task id |
| `alt_svc.v1` | origin → record | learned HTTP/3 endpoints, failures, max-age |

Writes are batched: task cells accumulate byte counts in memory;
`flush_dirty()` commits every ~2 s with `Durability::None` (memory-only,
cheap) and periodic/exit paths commit `Durability::Immediate` so the DB
survives power loss. `.part` files are the second source of truth: on
restart, `written` counters are re-derived from segment metadata and
validated against the file length, so a crash between flushes costs at most
the un-flushed delta, which is re-downloaded.

## 6. Paths & privileges

`paths.rs::discover()`: if a `udm.portable` marker sits beside the exe,
everything lives in `./udm-data/` next to it; otherwise
`%LOCALAPPDATA%\udm` (Windows) / `~/.local/share/udm` (XDG). No
registry keys, no services, no HKLM, no elevation prompt — the Windows
manifest requests `asInvoker` (see BUILD.md for the Common-Controls v6
dependency, without which the process dies before `main`).

## 7. Error strategy

One `usagi::error::Error` enum; the GUI maps failures to toasts and
row error text, the CLI to stderr + exit codes (1 general, 2 DB locked).
Task failures never kill the engine: each task is `JoinHandle`-supervised,
its error is persisted to the record, and `retry` re-queues from the last
durable segment offsets.
