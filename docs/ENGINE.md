# Usagi: how a download works

**Usagi** is UDM's download engine, packaged as `usagi` in `crates/core`.
It owns the path from a requested URL to a completed file: probing,
transport choice, category routing, transfer, recovery and finalization.
This guide describes the safety rules implemented at `241f550`.

[Architecture](ARCHITECTURE.md) covers storage and process boundaries.
[Operations](OPERATIONS.md) explains the controls available to users.

## 1. Admit a task

`Engine::add` creates a persisted task. Unless added paused, it waits in
**Queued** until the supervisor admits it. `max_concurrent_tasks` limits
active downloads; `segments_per_task` bounds the requested segmentation
within a download. Defaults are 3 tasks and 8 segments. This is concurrency
control, not timed queue scheduling or a global speed limiter.

The normal path is:

```text
Queued → Probing → Downloading → Verifying → Completed
                    │                │
                    └── failure ─────┴──→ Failed

Pause/cancel revoke the current run; resume/retry queue another attempt.
An interrupted active task reopens as Paused after startup recovery.
```

Live received bytes, flushed segment coverage and durable progress are
separate measurements. A progress bar reaching its planned size does not
authorize completion: workers must validate response EOF, finish their
writes and pass the remaining checks.

## 2. Probe the resource

`NetStack::probe` obtains response metadata using HEAD with GET-based
fallback where needed. Useful results include the final URL after
redirects, size, range support, ETag, Last-Modified, filename hint,
content type, transport and Alt-Svc advertisement.

Metadata is evidence, not a promise that every range response will match.
The worker checks the responses that actually deliver bytes. Unknown-size
resources use a single open-ended stream; servers without usable range
support do not get a multi-range plan.

Requests ask for identity content coding. Non-identity content coding is
rejected rather than writing decoded bytes against encoded byte offsets.

## 3. Choose a transport

The HTTP/3 path uses `quinn`, `h3` and `h3-quinn`; HTTP/2 and HTTP/1.1 use
`reqwest`. TLS uses rustls with ring and the operating-system trust roots.
Alt-Svc entries are cached locally by origin, with expiry and failure state.

| Preference | Behaviour |
|---|---|
| Auto | Try HTTP/3 when a usable learned service is available; otherwise use the HTTP/2 or HTTP/1.1 path. |
| Prefer HTTP/3 | Can attempt HTTP/3 without a learned advertisement, with fallback. |
| HTTP/3 only | For an eligible HTTPS hostname, an attempted H3 failure is returned rather than converted to an HTTP/2 download. See the targeting limit below. |
| Never HTTP/3 | Use HTTP/2 or HTTP/1.1. |

**Targeting limit:** the current H3 selector only targets HTTPS URLs with
DNS hostnames. Plain HTTP and IP-literal URLs take the HTTP/1.1/2 path even
when a task requested HTTP/3-only; disabling HTTP/3 globally also forces that
path. The preference is therefore not an absolute prohibition on TCP for
every possible input URL.

The global HTTP/3 switch also constrains the effective preference. HTTP/2
is negotiated with the server on the TCP/TLS path; HTTP/1.1 remains the
compatibility path. Failures on a fallback-enabled H3 attempt update the
learned-service failure state and allow the HTTP path to continue.

System HTTP proxies apply to HTTP/1.1 and HTTP/2 when enabled. QUIC uses
direct UDP and does not use that proxy. The task badge shows the protocol
used; it does not prove a particular physical connection count.

## 4. Bind bytes to one representation

Before Usagi permits multi-segment planning or reuse of saved coverage, it
needs a `RepresentationBound`. The binding includes:

- the exact final URL;
- a strong ETag, preserved verbatim;
- total size when known;
- the current header scope (`HEADER_SCOPE_NONE`), with acceptable `Vary`
  semantics and identity content coding.

A weak ETag is not promoted to a strong one. Last-Modified remains metadata
and is **never** used as the `If-Range` validator. A new probe must match the
binding, and each bound GET response is checked again for final URL, ETag,
length and header scope before its bytes can extend coverage.

Without a binding, a download can proceed as one stream, but its old bytes
are discarded on a later attempt. Legacy progress without a binding is
also restarted; an old stored ETag by itself is insufficient proof.
An unknown length may be upgraded to a known length only before coverage
has been flushed under the old binding.

Representation mismatch has one persisted restart allowance per task,
independent of the ordinary retry budget, even if `max_retries` is zero.
Pause/resume and watch do not replenish it. An explicit **Retry** resets both
the network attempts and representation-restart counter. Repeated incompatible
changes within that budget fail the task instead of endlessly restarting it.

## 5. Lock the destination

Category routing is optional and defaults off. If enabled when a task is
added without an explicit destination, the task snapshots the download
root and validated rules. The worker chooses the sanitized filename,
classifies its suffix and durably locks filename and destination before
claiming a partial file. A failed route save cannot publish a speculative
route to another concurrent saver.

Classification uses only the filename, case-insensitively, longest suffix
first. It does not inspect MIME types, query strings or file contents.
Built-in folders are Documents, Music, Videos, Images, Archives, Programs,
Disk images and Other. The rule validator confines a category folder to one
direct child of the root; routing checks reject unsafe symlink/reparse
destinations. A locked route is rechecked before opening and finalizing.

Later settings changes do not move existing tasks. Explicit destinations
and legacy tasks retain their existing routing behaviour.

## 6. Claim and write the partial

The partial file is `<filename>.udm.part` in the destination directory.
New ownership uses atomic `create_new`; a matching filename is not treated
as proof that an unrelated existing partial belongs to this task. A uniquely
owned persisted partial can be reused. Ambiguous legacy claims are refused
for both writing and deletion.

Eligible known-size ranged downloads split into disjoint inclusive ranges,
subject to the minimum segment size (default 2 MiB). Each segment tracks
contiguous `done` bytes relative to its start. Its next offset is
`start + done`. Concurrent range requests write to the corresponding file
positions; HTTP/2 and HTTP/3 can multiplex requests on shared connections.

Range responses are validated rather than accepted by status alone:

- A ranged `206` needs a valid `Content-Range` matching the requested range
  and compatible total. Invalid metadata and excess bytes are rejected.
- A `200` that ignores a required range cannot be appended as if it were
  the requested segment. The protocol/representation restart path handles it.
- A `416` is accepted only in the specifically validated end-of-resource
  case; it is not a blanket declaration that a segment is complete.
- The final byte is withheld from interrupted checkpoints until EOF is
  validated. A worker error cannot be hidden by counters already at 100%.

Buffered writes periodically expose flushed coverage. That flush alone is
not a power-loss guarantee; checkpoint persistence adds the data-sync step.

## 7. Persist and recover progress

A durable checkpoint synchronizes the held, claimed file handle, records
file identity and fingerprints the matching task/segment snapshot before
the durable database commit. Runtime generation checks reject writes from
superseded workers. A persistence epoch keeps changed representation or
coverage state from being saved as a mismatched pair.

The default checkpoint interval is 1 second. Actual work lost after failure
depends on the last successful checkpoint, storage behaviour and remote
revalidation; there is no fixed byte-loss guarantee. Startup checks the
checkpoint version, snapshot, file identity and coverage against file length.
Missing or invalid proof resets reusable coverage conservatively.

Interrupted Probing, Downloading and Verifying tasks become **Paused**.
On resume, Usagi probes again and compares the representation before
reusing eligible ranges. File length alone is never enough: preallocation
or an interrupted write can leave a long file without valid coverage.

Pause and cancel keep the partial. Removal revokes and drains the worker
before releasing its ownership or removing records/files. File deletion is
controlled by the remove request and `delete_part_on_remove`; see
[CLI removal details](OPERATIONS.md#command-line-operations).

## 8. Verify and finalize

Before finalization, Usagi synchronizes the full partial. If an expected
checksum was supplied and `verify_checksums` is enabled, it hashes the file
and compares the result. Supported algorithms are SHA-256, SHA-512, SHA-1,
MD5 and BLAKE3. Mismatch fails the task and retains the error information.

Without an enabled publisher-checksum comparison, Usagi still computes a
local BLAKE3 digest for finalization recovery. Therefore **Verifying can
appear even when no expected checksum was supplied**. This local digest
proves continuity for the journal; it does not authenticate a publisher.

Filesystem rename and redb completion cannot commit atomically. Usagi writes
a durable finalize intent containing source/destination, identities, length,
content digest and task context. It performs the rename under the selected
conflict policy (unique suffix, overwrite or fail), synchronizes the result
and commits completion. Recovery validates the files and intent rather than
guessing from which filename exists.

Unix can synchronize the parent directory. Windows has no equivalent
portable directory fsync in this implementation, so the committed intent
is retained and the final backing is checked on subsequent starts. This is
reconciliation, not a claim that every filesystem or storage device provides
identical power-loss semantics. Schema version 2 protects these records from
older engines that would ignore the intent.

## Failures, feedback and measurements

Transient network failures have bounded retries and backoff. Exhausted
attempts remain **Failed**, with error text and eligible partial data.
Generation fencing ensures late worker success/failure cannot overwrite a
new attempt's state. UI actions use engine capabilities rather than guessing
which transitions are valid.

Speed and ETA use local measurements; byte, request and connection counters
have separate lifetimes. An active request can outlive its network body
while disk writes drain. H1/H2 pool connection census is unavailable in this
version, so the engine total is **Unavailable**, not a segment-derived count.
There is no metric export or telemetry.

Browser transactions are handled by the separate coordinator and associated
using `handoff_tx`; **Needs recovery** is coordinator presentation, not an
engine task state. See [Architecture](ARCHITECTURE.md#browser-handoff).

There is no global speed limit, timed queue scheduling, torrent engine or
media resolver. The authenticated updater, N2 dual-stack (IPv4/IPv6 selection)
and N3 multi-family transfers (splitting one download across address families)
are design-only, not built. N2/N3 implementation remains measurement-gated.
