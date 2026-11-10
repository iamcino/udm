# Build UDM from source

Build the desktop app, CLI and browser host from the same checkout. This
guide describes the scripts at `241f550` (version 0.1.0), including what
they currently leave out. For running the results, see [Operations](OPERATIONS.md).

## What you need

| Build | Requirements |
|---|---|
| Windows 11 x64, native | Rust stable with the MSVC toolchain, MSVC C++ build tools and Windows SDK; PowerShell 5+ |
| Windows installer | The native build tools above, plus NSIS 3 (`makensis` on PATH) |
| Windows x64 from Linux | Rust with `x86_64-pc-windows-gnu`, MinGW-w64 GCC and resource compiler, Bash and `zip` |
| Engine/CLI development on Linux or macOS | Rust and the platform linker; GUI builds also need the platform's Slint/winit system dependencies |

The workspace uses Rust edition 2021 and declares Rust **1.92** as its
minimum version. Use the checked-in `Cargo.lock` for reproducible dependency
selection. Product builds do not require nightly Rust.

Run the commands below from the repository root. Building and running UDM
does not require administrator rights; installing a compiler or SDK follows
your machine's own provisioning policy.

## Build on Windows

```powershell
cargo build --release --locked -p udm-gui -p udm-cli -p udm-host
```

The binaries are `target\release\udm.exe`, `udm-cli.exe` and `udm-host.exe`.
To use the checked-in generated UI instead of compiling Slint sources:

```powershell
cargo build --release --locked --no-default-features -p udm-gui -p udm-cli -p udm-host
```

To build and package the desktop app and CLI:

```powershell
.\packaging\windows\build.ps1 -NoSlintCompile
```

Omit `-NoSlintCompile` for a normal source-compiled UI build. Use
`-SkipInstaller` if NSIS is unavailable, or `-Profile debug` for a debug
package. The script reads the version from the workspace manifest and
stages these outputs under `dist\`:

- `udm.exe` and `udm-cli.exe`;
- `udm-0.1.0-setup.exe` unless the installer is skipped;
- `udm-0.1.0-portable.zip`, containing the executables, `udm.portable`,
  privacy documentation and bundled licence notices.

**Packaging gap:** neither `build.ps1`, `installer.nsi` nor the cross-build
script includes `udm-host` or the extension at this revision. Building the
host with Cargo does not add it to those packages. For browser testing,
place the separately built host in a stable user-writable directory and
follow [extension setup](../extension/README.md). Portable mode requires
the app and host to resolve the same data directory.

The installer is per-user, under `%LOCALAPPDATA%\Programs\udm`. It creates
shortcuts and an HKCU uninstall entry. It does not register the browser
host or enable startup. Its uninstaller keeps application data and downloads;
undo optional startup/host registration first, as explained in
[Operations](OPERATIONS.md#removing-udm).

## Keep both UI build modes in step

`udm-gui` pins `slint` and `slint-build` to **1.18.1**. Its default
`slint-compile` feature compiles `crates/gui/ui/app.slint` with the Fluent
style, embedded resources and the vendored cino-ui import mapping.

With `--no-default-features`, the build uses
`crates/gui/src/slint_generated.rs`. This reduces compiler memory use, but
it makes regeneration part of every Slint source or embedded UI asset change.
Commit the generated file alongside the source change.

On Windows:

```powershell
.\packaging\regenerate-slint.ps1
```

The PowerShell script runs Cargo with `--locked --offline`, so dependencies
must already be cached. It rewrites embedded asset includes to be relative
to `CARGO_MANIFEST_DIR`, then writes strict UTF-8 output. An ordinary
feature-enabled build alone does not refresh the checked-in file.

On a Unix development machine:

```sh
./packaging/regenerate-slint.sh
```

Review the generated includes for relocatability before moving the checkout
or cross-building. The shell script still prints a legacy `1.6.0` header;
the actual compiler comes from the crate's 1.18.1 pin. Unlike the PowerShell
script, it copies output without rewriting asset includes. Check its chosen
`out/app.rs` and resulting diff carefully. Keep compiler and runtime pins identical.

## Cross-build for Windows

On a Debian/Ubuntu build machine, provision MinGW-w64 and `zip`, then run:

```sh
rustup target add x86_64-pc-windows-gnu
./packaging/cross/build-windows.sh
```

The script uses `--no-default-features`, builds the GUI and CLI, and creates
the portable ZIP in `dist/`. It does not create an NSIS installer or build
the host. It uses `set -euo pipefail`: a failed Cargo build stops the script
before copying new binaries. Existing files in `dist/` can still be stale,
so inspect exit status and output timestamps before distributing anything.

## Windows resources that must survive packaging

`crates/gui/build.rs` reads `CARGO_CFG_TARGET_OS`, so resource embedding
follows the target even when the build host is Linux. It embeds
`packaging/windows/udm.rc`, the rabbit icon and `app.manifest`.

| Resource or linker setting | Why it matters |
|---|---|
| `asInvoker` | Runs with the current user's rights without requesting elevation. |
| Common-Controls v6 dependency | Provides `TaskDialogIndirect` for native dialogs; omitting it can prevent the process from reaching `main`. |
| PerMonitorV2 and long-path declarations | Windows DPI and path behaviour. |
| 8 MiB main-thread stack | Accommodates initialization of the generated Slint window. |
| MSVC `/MANIFEST:NO` after successful embedding | Avoids a second default manifest; this flag is not sent to GNU ld. |

`UDM_EMBED_RC=0` skips resource embedding for diagnosis. Do not treat such a
build as a normal Windows deliverable. Check a candidate's PE architecture,
icon and embedded manifest, then launch it as an ordinary Windows user.
For a quick marker check on Unix:

```sh
file dist/udm.exe
strings dist/udm.exe | rg 'asInvoker|Microsoft.Windows.Common-Controls|6595b64144ccf1df'
```

Marker checks supplement a native launch; they do not prove that dialogs,
tray behaviour or startup registration work.

## Memory-constrained builds

Compiling the Slint compiler can require a few GiB of RAM. First use the
generated-code build and `CARGO_BUILD_JOBS=1`. The workspace already disables
development debug information and gives the GUI more codegen units.

The release profile uses optimization, thin LTO and one codegen unit. It can
still exceed a 1 GiB machine's memory even without Slint compilation. Prefer
a machine with enough RAM for release artifacts. Historical nightly
Cranelift/jemalloc regeneration recipes are environment workarounds, not
requirements or a portable replacement for the supported regeneration scripts.

## Verification

```powershell
cargo test -p usagi --locked
cargo test --workspace --locked --no-default-features
cargo check --workspace --all-targets --locked --no-default-features
cargo check -p udm-gui --locked --features slint-compile
node extension/tests/handoff.test.js
```

Add `--offline` only when dependencies are cached. Engine tests include local
HTTP/TLS fixtures, checksum and resume paths, durability, finalization,
generation fencing, representation checks, handoff and policy guards. Some
tests are platform-gated; a passing non-Windows run does not exercise Windows
named pipes or registry integration. Review GUI changes visually with seeded
tasks as well as compiling them.

`cargo deny check` is an additional dependency-policy check when cargo-deny
is installed. It is not invoked by the current CI workflow.

## CI and publication

UDM is published on GitLab ([cino-group/udm](https://gitlab.com/cino-group/udm))
and GitHub ([iamcino/udm](https://github.com/iamcino/udm)); both carry the same
commits and tags.

- [`.gitlab-ci.yml`](../.gitlab-ci.yml) runs the tests and a packaged build on
  every push. Its manual `release` job runs `tools/release-gitlab.ps1`, which
  builds, signs (pinned certificate `docs/signing/UDM-codesign.cer`), packages
  and publishes the GitLab release and creates the `v<version>` tag.
- [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) runs Linux tests, a
  Slint source check, Windows MSVC packaging and a GNU cross-build.
  [`release.yml`](../.github/workflows/release.yml) runs when a `v*` tag reaches
  GitHub: `tools/release-github.ps1` downloads the GitLab release, checks the
  hashes and signatures, and publishes the same files on GitHub.

A release is: publish the source snapshot (`tools/publish-gitlab-snapshot.ps1
-Push`, which also mirrors GitLab to GitHub), run the GitLab `release` job, then
run the snapshot script with `-Push` again to carry the new tag to GitHub.

The workflows describe automation, not release approval. It neither packages
the browser integration nor implements an authenticated app updater. No
updater is built by any command in this guide.

Continue with [Architecture](ARCHITECTURE.md) for component boundaries or
[Operations](OPERATIONS.md) for deployment, logs and recovery.
