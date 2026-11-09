# Standards and implementation references

Use this page to connect UDM's behaviour to the standards and libraries
behind it. The implementation baseline is `241f550`; `Cargo.lock` is the
source of truth for exact dependency versions. An upstream specification
describes a protocol, not a claim that UDM implements every optional part.

## HTTP, QUIC and safe ranges

| Reference | Why it matters to UDM |
|---|---|
| [RFC 9110 — HTTP Semantics](https://www.rfc-editor.org/rfc/rfc9110) | GET/HEAD, redirects, validators, Range, Content-Range, If-Range and response status semantics. See [Engine](ENGINE.md) for UDM's stricter representation-binding rules. |
| [RFC 9112 — HTTP/1.1](https://www.rfc-editor.org/rfc/rfc9112) | The HTTP/1.1 compatibility transport. |
| [RFC 9113 — HTTP/2](https://www.rfc-editor.org/rfc/rfc9113) | Multiplexed streams on the HTTP/2 path; requests are not physical connections. |
| [RFC 9000 — QUIC](https://www.rfc-editor.org/rfc/rfc9000) | UDP-based transport used for HTTP/3. |
| [RFC 9114 — HTTP/3](https://www.rfc-editor.org/rfc/rfc9114) | HTTP request/response semantics over QUIC streams. |
| [RFC 7838 — HTTP Alternative Services](https://www.rfc-editor.org/rfc/rfc7838) | Alt-Svc service learning and expiry, backed by UDM's local origin cache. |

The relevant implementation is under
[`crates/core/src/net/`](../crates/core/src/net/), with task response checks
in [`engine/task.rs`](../crates/core/src/engine/task.rs). UDM requires a
strong ETag for reusable bound coverage; recording Last-Modified does not
mean it is accepted as a resume validator.

## Checksums and local recovery

[`hash.rs`](../crates/core/src/hash.rs) defines the accepted expected-hash
syntax and supported algorithms: SHA-256, SHA-512, SHA-1, MD5 and BLAKE3.
The `sha256:<hex>` notation is an application convention, not an HTTP or
hash-prefix RFC implemented by UDM.

The [official BLAKE3 repository and specification links](https://github.com/BLAKE3-team/BLAKE3)
describe BLAKE3. RFC 7693 describes **BLAKE2**, not BLAKE3, and should not
be used as the BLAKE3 reference. A checksum supplied by a trusted publisher
and a locally computed recovery digest serve different purposes: the latter
helps reconcile file operations but does not authenticate the publisher.

For UDM's local ordering and recovery rules, read
[`durability.rs`](../crates/core/src/durability.rs),
[`finalize.rs`](../crates/core/src/finalize.rs),
[`store/mod.rs`](../crates/core/src/store/mod.rs) and
[Architecture](ARCHITECTURE.md#storage-and-schema). A durable database commit
alone does not prove that file content was synchronized first.

## Windows and browser integration

| Reference | Implementation connection |
|---|---|
| [Microsoft: application manifests](https://learn.microsoft.com/en-us/windows/win32/sbscs/application-manifests) | `asInvoker`, DPI/long-path declarations and the Common-Controls v6 assembly dependency in `packaging/windows/app.manifest`. |
| [Chrome: Native Messaging](https://developer.chrome.com/docs/extensions/develop/concepts/native-messaging) | Host manifest, pinned extension origin, stdio message framing and per-user host discovery. |
| [Mozilla: Native Messaging](https://developer.mozilla.org/en-US/docs/Mozilla/Add-ons/WebExtensions/Native_messaging) | Firefox host discovery and allowed-extension identity. |

UDM's prepare/commit/abort/query semantics are its own local protocol on top
of these transports. Read [the extension guide](../extension/README.md),
[`handoff.rs`](../crates/core/src/handoff.rs),
[`handoff_pipe.rs`](../crates/core/src/handoff_pipe.rs) and
[`crates/host/src/`](../crates/host/src/). Browser Native Messaging does not
itself provide durable download ownership transfer.

Startup registration is implemented in
[`crates/gui/src/startup.rs`](../crates/gui/src/startup.rs): a disclosed
per-user HKCU Run entry. It is separate from browser host registration and
the NSIS uninstall entry. None is a system service or an elevation mechanism.

## Main Rust dependencies

The versions below identify manifest lines at this baseline, rather than
promising compatibility with future upstream releases. Resolve exact
versions through `Cargo.lock` and inspect features in the crate manifests.

| Dependency | Manifest line | Role |
|---|---|---|
| `slint`, `slint-build` | Exactly 1.18.1 | UI runtime and compiler; generated Rust is version-coupled. |
| `quinn` | 0.11 | QUIC endpoint and connections. |
| `h3`, `h3-quinn` | 0.0.8 / 0.0.10 | HTTP/3 and the Quinn adapter; these are distinct version lines. |
| `reqwest` | 0.13 | HTTP/1.1/2, streaming bodies and system proxy support. |
| `rustls`, `ring`, `rustls-native-certs` | See manifests/lockfile | TLS using ring and OS roots, without OpenSSL/native-tls. |
| `redb` | 4.4 | Embedded transactional database with an exclusive file lock. |
| `tokio`, `tokio-util` | See manifests/lockfile | Async execution, synchronization and cancellation. |
| `sha2`, `sha1`, `md-5`, `blake3` | See manifests/lockfile | Expected-checksum verification and local recovery digests. |
| `serde`, `serde_json` | See manifests/lockfile | Settings, records, handoff journal and wire objects. |
| `clap` | 4.5 | CLI parsing. |
| `rfd` | 0.16 | Native dialogs. |
| `tray-icon`, `muda`, `windows-sys` | See GUI/host manifests and lockfile | Tray/menu and narrowly scoped Windows APIs. |
| `tracing`, `tracing-subscriber` | See manifests/lockfile | Local diagnostics; no remote sink. |
| `embed-resource` | 3 | Windows icon/manifest resource embedding. |

## Interface and design sources

[Design](DESIGN.md) explains the implemented product language. The
vendored cino-ui snapshot supplies shared theme/components and Be Vietnam
Pro; UDM's theme adapter and task views remain local. Read the source tokens
for current colours and fonts rather than treating earlier mockup values
as an API.

The design draws on Apple HIG-style surfaces and source lists, adapted for
Windows. This influence does not imply Apple or Microsoft endorsement or
that every control is a native system widget. The vendored
[cino-ui licence](../crates/gui/vendor/cino-ui/LICENSE) and
[font licence](../crates/gui/vendor/cino-ui/ui/fonts/OFL.txt) travel with
the packaged app.

## Tooling and verification

| Source | What to inspect |
|---|---|
| [Build guide](BUILD.md) | Native MSVC builds, GNU cross-build, Slint regeneration and current package omissions. |
| [`ci.yml`](../.github/workflows/ci.yml) | Actual job commands and private-repository conditions. |
| [`crates/core/tests/`](../crates/core/tests/) | Local network fixtures, integrity/recovery gates, handoff, instrumentation and policy guards. |
| [`no_telemetry.rs`](../crates/core/tests/no_telemetry.rs) | Dependency/source tripwires; the core's separate empty endpoint constant is not directly asserted by this test. |
| [`deny.toml`](../deny.toml) | Explicit dependency bans and licence/advisory/source policy. |
| [`packaging/verification/`](../packaging/verification/) | Longer-running file/finalization campaigns, separate from a routine build. |

Policy checks are useful guardrails, not exhaustive proof of network
behaviour. See [Privacy](PRIVACY.md) for precise claims and runtime inspection.

## Documentation path

Start with the [README](../README.md) or its [Vietnamese companion](../README.vi.md).
Use [Operations](OPERATIONS.md) for day-to-day work and
[the extension guide](../extension/README.md) for browser setup. For changes,
read [Architecture](ARCHITECTURE.md), [Engine](ENGINE.md), [Design](DESIGN.md)
and [Build](BUILD.md) in the order relevant to your work.

The authenticated updater, N2 dual-stack (IPv4/IPv6 selection) and N3
multi-family transfers (splitting one download across address families) are
design-only, not built. They are not implementation references for a shipped
feature at this baseline. N2/N3 implementation remains measurement-gated.
