# udm

**UDM — Universal Download Manager.**

The application is **Universal Download Manager (UDM)**, with desktop binary
`udm` and command-line binary `udm-cli`. Its upgraded core engine will be named
**Usagi**. The current core package is `udm-core`.

A download manager for Windows 11 (and Linux) written in Rust, with a Slint
GUI and a [redb](https://github.com/cberner/redb) database. It speaks
**HTTP/3 over QUIC** (`quinn` + `h3`), falls back to HTTP/2 and HTTP/1.1
automatically, and promises two things most download managers cannot:

* **No administrator rights, ever.** Per-user install or portable folder;
  data lives in `%LOCALAPPDATA%\udm` (or beside the executable).
* **Zero telemetry.** No analytics, no crash reporting, no auto-updates.
  Enforced at build time by `crates/core/tests/no_telemetry.rs` and
  `deny.toml`. See [docs/PRIVACY.md](docs/PRIVACY.md).

## Features (v1)

| Feature | How |
|---|---|
| Multi-connection downloads | File probed once, then split into up to 64 ranged segments (`Range: bytes=`), each on its own connection, written positionally into one `.part` file |
| Resume | Segments persisted in redb; restart uses `Range` + `If-Range` (ETag/Last-Modified); interrupted tasks recover as *paused* |
| Pause / cancel / retry | Cooperative stop tokens; retry resets the backoff budget |
| Queue with concurrency limit | Supervisor admits queued tasks up to `max_concurrent_tasks` |
| Progress + speed | Live per-task and aggregate speed/ETA, 150 ms UI refresh |
| HTTP/3 (QUIC) | `Alt-Svc` learning cache in redb with circuit breaker; `TransportPref` per task (auto / prefer / only / never) |
| Integrity | Optional SHA-256 / SHA-512 / SHA-1 / MD5 / BLAKE3 verification before the `.part` file is renamed |
| Persistence | Single redb file; progress is committed durably once per second (configurable), so even a power cut costs at most that much |
| Checksums & conflicts | Unique-suffix / overwrite / refuse policies |

Out of scope for v1 (deliberately): speed limits, scheduler, proxy
configuration UI beyond system-proxy on/off, clipboard watcher, mirror
failover, browser extensions.

## The two binaries

* **`udm`** – the Slint desktop app (dark Fluent-style UI, software
  renderer, works over RDP/VMs without GPU).
* **`udm-cli`** – the same engine, headless:

```text
udm-cli get https://example.com/big.iso -s 16 --hash sha256:deadbeef...
udm-cli probe https://example.com/big.iso        # headers, ranges, Alt-Svc, HTTP/3?
udm-cli import urls.txt                          # batch enqueue
udm-cli list | watch | pause 3 | resume 3 | retry 3 | cancel 3 | remove 3 --files
udm-cli settings --set segments_per_task=12
udm-cli doctor                                   # paths, TLS roots, QUIC, telemetry status
```

## Building

Linux/macOS cross-build (what CI does):

```bash
rustup target add x86_64-pc-windows-gnu
sudo apt install gcc-mingw-w64-x86-64          # linker + windres
./packaging/cross/build-windows.sh             # -> dist/udm.exe, portable zip
cargo test --workspace                         # incl. the no-telemetry guard
```

On Windows:

```powershell
.\packaging\windows\build.ps1                  # -> dist\ + NSIS installer
```

GitHub Actions (`.github/workflows/ci.yml`) defines five jobs - test,
slint-compile, windows-msvc, windows-gnu, release - but while this
repository is private every job is skipped (each carries
`if: ${{ !github.event.repository.private }}`, and release also `needs` the
four skipped jobs), so no push runs the tests and no `v*` tag publishes a
release. The jobs run again automatically once the repo is public; until
then the local/VM gates are the actual gates.

Slint UI codegen: by default (`slint-compile` feature) `udm-gui` compiles
`ui/*.slint` during the build via `slint-build`, which needs ~2 GB of RAM
(`i-slint-compiler` is a big crate). Low-memory machines can pass
`--no-default-features` and use the checked-in
`crates/gui/src/slint_generated.rs` instead — `build-windows.sh` already does
this, `build.ps1` has a `-NoSlintCompile` switch. After editing any `.slint`
file, regenerate it with `packaging/regenerate-slint.sh` (Slint is pinned to
1.18.1 for both the runtime and build compiler).

The installer (`packaging/windows/installer.nsi`) is per-user
(`RequestExecutionLevel user`, installs to `$LOCALAPPDATA\Programs\udm`),
and the executable embeds an `asInvoker` manifest
(`packaging/windows/app.manifest`) so Windows never shows a UAC prompt.

Portable mode: put an empty `udm.portable` file next to `udm.exe` (or set
`UDM_PORTABLE=1`); all data then lives in that folder.

## Architecture

```
crates/core      engine (supervisor + per-task segment workers), net stack
                 (tls / h3 / http12 / altsvc / stack), store (redb), config,
                 hash, paths, util            13,215 lines, 50+ unit tests
crates/gui       Slint UI (ui/*.slint) + thin event-loop glue (src/state.rs)
crates/cli       clap front-end with a live ANSI progress renderer
packaging/       asInvoker manifest, .rc, NSIS per-user installer, build scripts
docs/            design, architecture, engine, operations, build, references,
                 privacy
```

Threading: the engine owns a Tokio multi-thread runtime on its own threads;
the UI thread only polls `Engine::snapshot()` on a timer and sends commands,
so the interface never blocks on I/O. The supervisor task serialises
lifecycle decisions (queue admission, pause/cancel, removal).

Storage: one redb database (`udm.redb`) with tables for tasks, segments,
counters, settings and the Alt-Svc cache. In-flight progress is committed
durably once per `persist_interval_ms` (default 1 s): redb's
`Durability::None` keeps data purely in process memory, so a non-durable
progress commit would vanish on a hard kill - the exact moment resume
matters. Minor in-between record updates still ride on `Durability::None`
and are promoted by the next durable commit.

TLS: rustls with the **ring** provider (never aws-lc-rs: simpler cross-builds)
and the operating-system trust store (`rustls-native-certs`).

## Documentation

| Document | Contents |
|---|---|
| [docs/DESIGN.md](docs/DESIGN.md) | Product principles and the Apple-HIG-derived visual language (tokens, layout, interactions) |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Crate map, layering, threading, the Slint↔Rust contract, persistence schema |
| [docs/ENGINE.md](docs/ENGINE.md) | The download pipeline: probing, transport selection, segmentation, resume, verification |
| [docs/OPERATIONS.md](docs/OPERATIONS.md) | Deploying (portable/installer), data & backups, CLI operations, troubleshooting |
| [docs/BUILD.md](docs/BUILD.md) | Toolchains, the two GUI build modes, regenerating UI code, manifest/icon embedding, CI |
| [docs/REFERENCES.md](docs/REFERENCES.md) | Standards, crates and design references, annotated |
| [docs/PRIVACY.md](docs/PRIVACY.md) | The zero-telemetry contract and how it is enforced |

## Verifying the privacy claims

```bash
cargo test -p udm-core --test no_telemetry   # Cargo.lock + source tripwires
cargo deny check                               # licence/advisory/telemetry bans
```

and at runtime: with an empty queue, udm opens zero sockets.

## License

MIT OR Apache-2.0 — see [LICENSE](LICENSE).

The GUI is built with [Slint](https://slint.dev), used under the Slint
Royalty-free License 2.0.

<p align="center">
  <a href="https://slint.dev">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="assets/icons/made-with-slint-dark.svg">
      <source media="(prefers-color-scheme: light)" srcset="assets/icons/made-with-slint-light.svg">
      <img alt="Made with Slint" src="assets/icons/made-with-slint-light.svg" height="50">
    </picture>
  </a>
</p>
