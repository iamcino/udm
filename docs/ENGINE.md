# Universal Download Manager (UDM) — Core Engine

The core engine will be named **Usagi** after the engine safety upgrade.
This document describes the current `usagi` implementation. The upgrade
contract is tracked separately while development is in progress.

How a download actually happens, from `Engine::add` to the renamed final
file. Code map: `crates/core/src/engine/{mod,task,speed}.rs`,
`crates/core/src/net/{stack,h3,http12,altsvc,tls}.rs`,
`crates/core/src/store/`, `crates/core/src/hash.rs`.

## 1. Task lifecycle

```
            add()                        scheduler slot
  Queued ──────────► Probing ──────────────────────────► Downloading
     ▲                  │  (HEAD/GET, size + Range +        │
     │                  │   ETag/Last-Modified + Alt-Svc)   │ all bytes in
     │                  ▼                                   ▼
     │              (no slot yet → stays Queued)        Verifying
     │                                                     │  checksum ok → rename .part → final
   Paused ◄──── pause() (keeps .part + segment map)       ▼
     │                                                 Completed
   Failed ◄──── transport/IO/verify error (keeps .part when resumable)
     │
   retry() ────► re-probe (validator check) ─► Queued/Downloading
   Canceled ─── cancel() (keeps .part) ── resume() ──► Queued
```

`remove(id, delete_files)` drops the DB records and optionally the files.
Every transition is persisted (durable commit for state changes; byte
counters ride the cheap batched commits — see ARCHITECTURE.md §5).

## 2. Probing

Before any transfer the task learns, in one round trip where possible:

* **size** (`Content-Length` / `Content-Range` total),
* **Range support** (`Accept-Ranges`, or empirically from a ranged GET —
  the only fully reliable signal),
* **validator**: strong `ETag` preferred, else `Last-Modified`, kept as the
  `If-Range` value for every later resume,
* **Alt-Svc** headers → learned HTTP/3 endpoints (`net/altsvc.rs`,
  persisted in `alt_svc.v1` with failure counters and max-age).

Probe outcome decides the plan: `segments = min(settings.segments,
max(1, size / min_segment_bytes))`; unknown size ⇒ single stream, no
segmentation, indeterminate progress (`percent = -1`).

## 3. Transport selection

Order per connection attempt (`net/stack.rs`), honouring the per-task
`TransportPref` (Auto / Prefer H3 / H3 only / Never H3) and the global
HTTP/3 switch:

1. **HTTP/3** via `quinn` + `h3` when (a) enabled, (b) the origin is in the
   Alt-Svc cache (or pref forces an attempt), (c) QUIC is available on this
   machine. One shared `quinn::Endpoint` per process; rustls with the ring
   provider and the OS trust store (`net/tls.rs`, `rustls-native-certs`).
2. **HTTP/2 then HTTP/1.1** via `reqwest` (ALPN-negotiated), system proxy
   respected for HTTP paths when enabled; QUIC ignores the proxy by design
   (documented, not configurable in v1).
3. H3 failures increment the endpoint's failure counter; after repeated
   failures the Alt-Svc entry is evicted and the task silently continues on
   HTTP/2 — a fallback is never a user-visible error.

The transport actually used is recorded per task and shown as the row badge
(`H3` green / `H2` / `H1`).

## 4. Segmented transfer

* The partial file is `<name>.udm.part`; segments are disjoint byte ranges
  `[start, end]` written at **relative offsets** (`written` counts bytes of
  *this segment*, so a resumed segment continues where it stopped without
  re-reading absolute positions).
* Each segment is an independent HTTP request with `Range` +
  `If-Range: <validator>`; a `200 OK` (validator changed / range ignored)
  aborts the segment so a stale `.part` can never be silently corrupted.
* `416 Range Not Satisfiable` is treated as "segment already complete" when
  our math says we asked beyond EOF, else as a hard error.
* Segments run concurrently within the task, bounded by `segments`; the
  global scheduler bounds concurrent *tasks* (`max_tasks`).
* Writes go through a buffered writer, flushed periodically and at segment end.
  Live byte counters serve the UI; resume counters only advance after flush.
  The last byte is withheld from interrupted checkpoints until response EOF
  has been validated. Flush here means OS acceptance, not power-loss durability.
* A single-range `206` must contain a valid `Content-Range` covering the requested
  bytes. Invalid metadata and excess body bytes are rejected. Worker errors
  prevent completion even if progress counters already reach the planned size.
* One protocol restart (Range ignored / remote changed) is allowed independently
  of the ordinary network retry budget, including when `max_retries` is zero.
* Speed/ETA come from an EWMA meter (`engine/speed.rs`) per task plus an
  aggregate over active tasks.

## 5. Resume

Partial files have exclusive ownership within the engine. New claims use atomic
`create_new`; an unclaimed existing file is preserved and reported as a conflict.
For existing databases, a partial with exactly one persisted segment-map owner
is reused without renaming. If several old tasks claim the same path, neither
may write or delete it. Removing an unstarted duplicate does not remove the
other task's partial. Worker cancellation must finish before removal drops its
DB record or releases ownership. These checks preserve the legacy filename
format without assuming that any matching file belongs to a new task.

Resume = re-probe → compare validators (`etag_equal` handles weak/strong
forms; changed validator ⇒ restart from zero, `.part` truncated) → rebuild
the segment map from `segments.v1` → re-issue `Range: bytes=<written+start>-`
requests for unfinished segments only. Crash-resume follows the same path:
whatever was not durably flushed is simply re-downloaded. Verified by E2E
tests that `SIGKILL`/`SIGTERM` mid-download and restart produces a
byte-identical final file.

## 6. Verification & completion

When the last byte lands and an expected hash was supplied (`sha256:…`,
`sha512:…`, `sha1:…`, `md5:…`, `blake3:…`, or bare hex disambiguated by
length), the task enters `Verifying` and streams the `.part` file through
the matching hasher (`hash.rs`; reported as `verifying <speed>`). Mismatch
⇒ `Failed` with both digests in the error text; match ⇒ rename to the final
name, applying the conflict policy (rename-unique / overwrite / refuse).
With no expected hash, completion skips verification (checksum is optional
by design — see Settings → Integrity).

## 7. Retry & failure policy

* Transport/IO errors: automatic bounded retries with backoff inside a task
  attempt; when exhausted the task parks in `Failed` (partial data kept).
* `retry()` clears the error and re-enters probing; nothing is re-downloaded
  that the validator still accepts.
* `TaskFailed` events are the only engine→UI push; the GUI surfaces them as
  danger-coloured toasts and inline row errors.

## 8. Settings that change engine behaviour

`config.rs::Settings`: `max_tasks` (scheduler), `segments` (per-task
connection cap), `transport_pref`, `http3_enabled`, `use_system_proxy`
(HTTP/1.1–2 only), `verify_before_rename`, `conflict_policy`,
`default_download_dir`, `user_agent`, `log_level`. Persisted in `meta.v1`,
applied live via `update_settings` (running tasks keep their plan; new
tasks and resumes pick up changes).
