# udm — References

Standards, libraries and documents this project builds on, with a note on
*why each one matters here*.

## 1. Protocols & standards

| Reference | Relevance to udm |
|---|---|
| RFC 9000 — QUIC: A UDP-Based Multiplexed and Secure Transport | Transport under HTTP/3; why UDP egress can matter (§ OPERATIONS 7) |
| RFC 9114 — HTTP/3 | Wire protocol implemented via `h3` + `quinn` |
| RFC 7838 — HTTP Alternative Services (Alt-Svc) | How HTTP/3 endpoints are *learned*; basis of `net/altsvc.rs` and the `alt_svc.v1` table |
| RFC 9110 — HTTP Semantics (Range, ETag, Last-Modified, If-Range, 416) | Segmented downloads and safe resume: `If-Range` validators, `416` handling (ENGINE §2–5) |
| RFC 9112 / 9113 — HTTP/1.1, HTTP/2 | Fallback paths negotiated by `reqwest` via ALPN |
| RFC 6249 etc. (hash prefixes) — convention only | `sha256:…`-style expected-hash syntax accepted by the Add form |
| FIPS 180-4 (SHA-1/256/512), RFC 1321 (MD5), RFC 7693 (BLAKE3) | Checksum algorithms offered for verification (`hash.rs`) |
| Microsoft application manifest docs — `requestedExecutionLevel`, Common-Controls v6 assembly | `asInvoker` + comctl32 v6 dependency; the missing v6 dependency is the classic `TaskDialogIndirect` *“Entry Point Not Found”* startup crash (BUILD §5) |
| Microsoft PerMonitorV2 DPI docs | DPI awareness block in `app.manifest` |
| Taskbar/Explorer icon resources (RT_GROUP_ICON, `.rc`) | `udm.rc` + `embed-resource` embedding |

## 2. Design references

| Reference | Relevance |
|---|---|
| Apple Human Interface Guidelines (macOS Sonoma/Sequoia): dark-mode surfaces, source lists, unified toolbars, progress indicators | The visual language adopted in `DESIGN.md`: depth by lightness, one accent, thin linear progress, quiet tables |
| Apple HIG — Color: system blue `#0A84FF` (dark), semantic green/orange/red | Exact token values in `theme.slint` |
| Microsoft Fluent 2 (dark) | Cross-check for Windows-native feel; Segoe UI Variable as the family |
| Anti-references: “Dribbble IDM” style (gauges, gradients, promo cards) | Explicitly rejected — see DESIGN §2.1 |

## 3. Rust crates (pinned via `Cargo.lock`)

| Crate | Version line | Role |
|---|---|---|
| `slint` / `slint-build` / `i-slint-compiler` | 1.18.1 (pinned; generated code is version-coupled) | Declarative UI, software renderer + winit backend; build-time `.slint` compiler |
| `quinn` | 0.11.x | QUIC client endpoint (shared per process) |
| `h3` + `h3-quinn` | 0.0.10 | HTTP/3 request/response on top of quinn |
| `reqwest` | 0.13.x (rustls, no default TLS) | HTTP/2 & HTTP/1.1 transfers, proxy support |
| `rustls` + `ring` + `rustls-native-certs` | current | TLS with the OS trust store; no OpenSSL anywhere |
| `redb` | 4.x | Embedded, ACID, single-file store; `Durability::None/Immediate` batching (ARCHITECTURE §5) |
| `tokio` | 1.x | Engine runtime (multi-thread) |
| `blake3`, `sha2`, `sha1`, `md-5` | current | The five verification algorithms |
| `rfd` | 0.16.x (`common-controls-v6`, xdg-portal, tokio) | Native folder pickers / message dialogs — the crate whose `TaskDialogIndirect` import makes the v6 manifest dependency mandatory |
| `clap` | 4.x | CLI surface |
| `tracing` + `tracing-subscriber` | current | Local file/console logging (no remote sinks) |
| `embed-resource` | 3.x | `.rc`/manifest/icon embedding from `build.rs` (note its `CompilationResult` API) |
| `bincode`-style codecs | in-repo (`store/records.rs`) | Record serialization for redb values |
| `windows-sys` (0.52, `Win32_Storage_FileSystem`) / `libc` | target-gated | One FFI call each: `GetDiskFreeSpaceExW` / `statvfs` for the sidebar storage widget |

Policy: `deny.toml` + `tests/no_telemetry.rs` reject analytics,
crash-reporting and updater crates at build time (PRIVACY.md).

## 4. Tooling

| Tool | Role |
|---|---|
| MinGW-w64 (`x86_64-w64-mingw32-gcc`) | GNU cross-linker for the Linux→Windows build |
| NSIS 3 | Per-user installer (`installer.nsi`: HKCU-only, no UAC) |
| GitHub Actions (`ci.yml`) | Tests on Linux, MSVC + GNU Windows artifacts, release publishing |
| nightly `rustc` + `rustc_codegen_cranelift` | *Build-environment workaround only* (never shipped): compiles `i-slint-compiler` under ~1 GB RAM for code regeneration; the `pulp` crate must stay on LLVM because cranelift drops `global_asm!` (BUILD §3.2) |
| `jemalloc` (LD_PRELOAD) | Allocator headroom on 1 GB build boxes |

## 5. Internal documents

`README.md` (overview) · `DESIGN.md` (product/UI) · `ARCHITECTURE.md`
(system) · `ENGINE.md` (download pipeline) · `OPERATIONS.md` (running &
troubleshooting) · `BUILD.md` (build & ship) · `PRIVACY.md` (the
zero-telemetry contract).
