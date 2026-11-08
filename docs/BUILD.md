# udm — Build Document

Everything needed to build, package and ship udm, including the
low-memory workarounds this repo relies on. Companion: `OPERATIONS.md`
(running it), `ARCHITECTURE.md` (what the pieces are).

## 1. Toolchains

| Target | Toolchain | Extra tools |
|---|---|---|
| Windows x64 (native, primary) | Rust stable `x86_64-pc-windows-msvc` | NSIS 3 (installer), PowerShell 5+ |
| Windows x64 (cross from Linux/macOS) | Rust stable + `x86_64-pc-windows-gnu` | `gcc-mingw-w64-x86-64`, `zip` |
| Linux / macOS (dev & tests) | Rust stable | — |

Edition 2021; no nightly features are used by the product itself.

## 2. The two GUI build modes

`udm-gui` can obtain its Slint code two ways (feature `slint-compile`,
default **on**):

1. **Compile `.slint` at build time** — `build.rs` runs `slint-build`
   (which embeds `i-slint-compiler`) over `ui/app.slint`, style `fluent`,
   `EmbedResourcesKind::EmbedFiles`. Needs ~2 GB RAM for the compiler
   dependency graph; this is the normal developer path.
2. **Checked-in generated code** — `cargo build --no-default-features`
   uses `crates/gui/src/slint_generated.rs` verbatim (build.rs only exports
   `SLINT_INCLUDE_GENERATED`). This is what CI and 1 GB build boxes use.

**Rule: whenever anything under `crates/gui/ui/` changes,
`slint_generated.rs` must be regenerated and committed** — otherwise
`--no-default-features` builds (CI, cross-builds, installers) ship the old
UI or fail to compile against changed contracts.

## 3. Regenerating `slint_generated.rs`

### 3.1 Normal machine (≥2 GB RAM)

```sh
./packaging/regenerate-slint.sh      # builds with slint-compile, copies out/app.rs
```

### 3.2 Low-memory machine / sandbox (the recipe actually used here)

`i-slint-compiler` cannot be compiled by LLVM under ~1 GB. Workaround that
works in this repo's 1 GB sandbox:

1. A **standalone driver crate** (outside the workspace) depending only on
   `slint-build = "=1.18.1"` whose `main()` calls
   `compile_with_config("ui/app.slint", CompilerConfiguration::new()
   .embed_resources(EmbedFiles).with_style("fluent"))` with
   `CARGO_MANIFEST_DIR=<repo>/crates/gui`, `OUT_DIR=<tmp>` faked in the env.
2. Compile it with **nightly rustc + `-Zcodegen-backend=cranelift`**
   (via a `RUSTC=` wrapper), `CARGO_BUILD_JOBS=1`, jemalloc preloaded.
3. **Except the `pulp` crate**: cranelift drops `global_asm!` and leaves
   `\x01`-prefixed undefined symbol references (`libpulp_v0_22_3_*`) that no
   stub can satisfy. The wrapper must route `--crate-name pulp` to plain
   nightly-LLVM rustc; the crate then carries its own asm definitions and
   the link is clean.
4. Run the driver, prepend the standard 3-line header, overwrite
   `crates/gui/src/slint_generated.rs`, commit.

Keep the slint pin (currently **1.18.1**) identical between `slint`,
`slint-build` and the regeneration environment — generated code is
version-coupled.

## 4. Native Windows build & packaging

```powershell
./packaging/windows/build.ps1                  # full build (needs ~2 GB RAM)
./packaging/windows/build.ps1 -NoSlintCompile  # uses checked-in generated code
```

Produces `dist\udm.exe`, `dist\udm-cli.exe`,
`dist\udm-0.1.0-portable.zip` (adds the `udm.portable` marker,
`LICENSE.txt`, `PRIVACY.md`) and, with NSIS on PATH,
`dist\udm-0.1.0-setup.exe` from `packaging/windows/installer.nsi`
(per-user: `RequestExecutionLevel user`, installs under
`%LOCALAPPDATA%\Programs`, HKCU-only registry writes).

## 5. Windows resources — manifest & icon (read this before touching)

`crates/gui/build.rs` embeds `packaging/windows/udm.rc` via
`embed-resource` when `CARGO_CFG_TARGET_OS == "windows"` (**not**
`#[cfg(target_os)]` — in a build script that describes the *host*, which
silently skips embedding when cross-compiling). The `.rc` pulls in
`assets/icon.ico` and `packaging/windows/app.manifest`, which declares:

* `requestedExecutionLevel level="asInvoker"` — never elevate;
* PerMonitorV2 DPI awareness + longPath awareness;
* **`Microsoft.Windows.Common-Controls` version 6.0.0.0 dependency** —
  mandatory. The `rfd` dialog crate imports `TaskDialogIndirect`, which
  exists only in comctl32 **v6**. Without this manifest block the OS loader
  binds comctl32 v5 and the process dies with *“Entry Point Not Found:
  TaskDialogIndirect”* **before `main` runs**. This exact bug shipped once
  and is the reason `build.rs` has
  `cargo:rerun-if-changed=../../packaging/windows`.

Verify a built exe (should all be present):

```sh
python3 - <<'EOF'
d = open('dist/udm.exe','rb').read()
for s in [b'asInvoker', b'Microsoft.Windows.Common-Controls', b'6595b64144ccf1df']:
    print(s, s in d)
EOF
```

MSVC only: `build.rs` adds `/MANIFEST:NO` so link.exe does not append a
second default manifest (that flag is invalid for GNU ld — the code guards
on `CARGO_CFG_TARGET_ENV`).

## 6. Cross-building from Linux

```sh
./packaging/cross/build-windows.sh    # x86_64-pc-windows-gnu, --no-default-features
```

Outputs `dist/udm.exe`, `dist/udm-cli.exe`, `dist/udm-0.1.0-portable.zip`
(with embedded `.rsrc`: manifest + icon — verify with `strings`/`wrestool`
as above). **Memory guard for ≤1 GB boxes:** the release profile is
`opt-level=3, lto="thin", codegen-units=1`, which OOMs on the 54k-line
generated GUI crate. Temporarily append, build, then revert:

```toml
[profile.release.package.udm-gui]
opt-level = 0
codegen-units = 256
```

(The shipped exe is opt-3 for *core/cli*; the GUI crate's own codegen level
has no measurable effect on a UI that is I/O-bound on its 150 ms poll.)
Note the script copies `dist/` artifacts even when compilation fails —
always check timestamps/strings of what you ship.

## 7. CI (`.github/workflows/ci.yml`)

Five jobs: (1) Linux test — `cargo test --workspace --no-default-features`
(includes the `no_telemetry` guard) + `cargo check --workspace
--all-targets`; (2) Slint source compile — `cargo check -p udm-gui
--features slint-compile`; (3) Windows MSVC — `build.ps1 -NoSlintCompile` +
NSIS installer, artifacts uploaded; (4) Linux→Windows GNU cross-build via
`build-windows.sh`, artifacts uploaded; (5) release — attaches `dist/`
artifacts to GitHub Releases on `v*` tags. While the repository is private,
no job runs: every job carries `if: ${{ !github.event.repository.private }}`
(and release `needs` the four skipped jobs), so the local/VM gates are the
actual gates until the repo goes public.

## 8. Tests

```sh
cargo test --workspace --no-default-features
```

Covers: engine unit/integration (probe/segment/resume/hash paths), store
round-trips, config, CLI parsing, E2E downloads against a local test server
with SHA-256 verification — including kill-9/SIGTERM mid-download resume —
plus policy tests (`no_telemetry.rs` rejects analytics/crash crates;
`deny.toml` encodes the same at the dependency level).
