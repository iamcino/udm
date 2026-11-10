//! Per-task execution: probe -> plan -> transfer -> verify -> rename.
//!
//! One [`run`] call owns a task from `Queued` to `Completed`/`Failed`. Inside
//! it, one Tokio task per segment writes to its own file offset, so a stall on
//! one connection never blocks the others.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncSeek, AsyncSeekExt, AsyncWrite, AsyncWriteExt};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::config::{ConflictPolicy, Settings, TransportPref};
use crate::engine::{stop_reason, Event, LogLevel, SharedState, TaskCell};
use crate::error::{Error, Result};
use crate::hash;
use crate::net::stack::{FamilyPaths, FamilyRoute};
use crate::net::stack::{Opened, ProbeInfo, RequestSpec};
use crate::store::records::{RouteState, SegmentState, TaskState, OPEN_ENDED};
use crate::util;

/// What the task runner is allowed to touch.
pub(super) struct RunCtx {
    pub state: Arc<SharedState>,
    pub settings: Settings,
    /// The run generation granted by `spawn_task` (Usagi PR 3).
    pub generation: u64,
}

/// Everything a segment worker needs, computed once per attempt.
#[derive(Clone, Debug)]
struct Plan {
    family_paths: Option<FamilyPaths>,
    route: Option<FamilyRoute>,
    allocation_epoch: u64,
    request_url: String,
    part_path: PathBuf,
    final_path: PathBuf,
    total: Option<u64>,
    /// Strong ETag sent as `If-Range`, or nothing. Never `Last-Modified`,
    /// never a weak tag.
    if_range: Option<String>,
    pref: TransportPref,
    /// Bound this plan may acknowledge. `None` is the single-segment path.
    bound: Option<crate::store::records::RepresentationBound>,
    /// Coverage is bound only for GET. Segment workers set this because
    /// they open the request with [`RequestSpec::get`].
    method_get: bool,
}

/// Count response lifetimes, including error/cancel exits, rather than derive
/// live activity from persisted completion milestones.
struct ActiveRequest(Arc<TaskCell>);

impl ActiveRequest {
    fn new(cell: &Arc<TaskCell>) -> Self {
        cell.active_requests.fetch_add(1, Ordering::Relaxed);
        Self(cell.clone())
    }
}

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        self.0.active_requests.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Debug, PartialEq)]
enum Outcome {
    Finished,
    Stopped,
}

/// What we remembered before probing, used to detect a changed remote file.
#[derive(Clone, Debug, Default)]
struct Previous {
    part_exists: bool,
}

/// Runs a task until it completes, is stopped, or gives up.
pub(super) async fn run(ctx: Arc<RunCtx>, id: u64, token: CancellationToken) {
    let _guard = RunningGuard(ctx.state.clone(), id);
    let Some(cell) = ctx.state.cell(id) else {
        return;
    };
    let mut attempt = cell.record.read().attempts;
    if attempt == 0 {
        cell.record.write().started_at = Some(util::now_millis());
    }
    cell.record.write().transport_summary = crate::store::records::TransportSummary::Unknown;
    cell.uses_family_paths.store(false, Ordering::Release);
    cell.lanes.start_run(ctx.generation);

    loop {
        if token.is_cancelled() {
            stopped(&ctx, &cell);
            return;
        }
        let mut result = execute(&ctx, &cell, &token).await;
        if let Some((generation, error)) = cell.storage_failure.lock().take() {
            if generation == ctx.generation {
                result = Err(Error::Storage(error));
            }
        }
        // A superseded run publishes nothing: no state, error or retry. Its
        // coverage and finalize writes were already refused by the fences.
        if let Err(stale) = cell.fence(ctx.generation) {
            tracing::warn!("#{id}: {stale}; exiting without publishing");
            return;
        }
        match result {
            Ok(Outcome::Finished) => return,
            Ok(Outcome::Stopped) => {
                stopped(&ctx, &cell);
                return;
            }
            Err(e) => {
                // A representation mismatch is not a network hiccup and does
                // not consume `attempts`. The budget is `representation_restarts`
                // on the task: one restart, then Failed. Pause, resume, watch,
                // and this loop do not put it back to zero.
                let was_representation = matches!(e, Error::RemoteChanged | Error::RangeIgnored);
                if was_representation && !attempt_relinquished(&ctx, &cell, &token) {
                    let restarts = cell.record.read().representation_restarts;
                    if restarts == 0 {
                        if let Err(clear_error) = reset_progress(&ctx, &cell, true).await {
                            if matches!(clear_error, Error::StaleGeneration { .. }) {
                                return;
                            }
                            fail(&ctx, &cell, &clear_error);
                            return;
                        }
                        ctx.state.log(
                            LogLevel::Warn,
                            format!(
                                "#{} {}: {} - restarting from offset 0",
                                id,
                                filename_of(&cell),
                                e.short()
                            ),
                        );
                        continue;
                    }
                }

                let next_attempt = attempt.saturating_add(1);
                // Fatal errors, and a retryable error whose budget is spent,
                // are definitive. Pause or shutdown must not drop them:
                // publication step 5 wins over step 6. Cancel still wins
                // inside `publish_exit`.
                let storage_failure = ctx.settings.dual_stack_paths
                    && matches!(e, Error::Io(_) | Error::IoPath { .. } | Error::Storage(_));
                let definitive =
                    storage_failure || e.is_fatal() || next_attempt > ctx.settings.max_retries;
                if definitive {
                    attempt = next_attempt;
                    cell.record.write().attempts = attempt;
                    fail(&ctx, &cell, &e);
                    return;
                }
                // A retry that will sleep is not a publication. Read the
                // token, the generation, and the record before sleeping, and
                // again after the sleep returns, before `execute` opens
                // another request. A lost task returns without `fail()`.
                if attempt_relinquished(&ctx, &cell, &token) {
                    publish_exit(&ctx, &cell, ExitKind::Discarded(&e));
                    return;
                }
                attempt = next_attempt;
                cell.record.write().attempts = attempt;
                let delay =
                    util::backoff(attempt, Duration::from_millis(700), Duration::from_secs(30));
                ctx.state.log(
                    LogLevel::Warn,
                    format!(
                        "#{} {}: {} (retry {}/{} in {})",
                        id,
                        filename_of(&cell),
                        e.short(),
                        attempt,
                        ctx.settings.max_retries,
                        util::human_duration_ms(delay.as_millis() as i64)
                    ),
                );
                cell.dirty.store(true, Ordering::Relaxed);
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = token.cancelled() => {}
                }
                if attempt_relinquished(&ctx, &cell, &token) {
                    publish_exit(&ctx, &cell, ExitKind::Discarded(&e));
                    return;
                }
            }
        }
    }
}

/// Clears `running` even if a worker panics.
struct RunningGuard(Arc<SharedState>, u64);

impl Drop for RunningGuard {
    fn drop(&mut self) {
        if let Some(cell) = self.0.cell(self.1) {
            cell.running.store(false, Ordering::Release);
        }
    }
}

// ---------------------------------------------------------------------------
// main attempt
// ---------------------------------------------------------------------------

async fn execute(
    ctx: &Arc<RunCtx>,
    cell: &Arc<TaskCell>,
    token: &CancellationToken,
) -> Result<Outcome> {
    if cell.record.read().finalization.is_some() {
        let part = cell.record.read().part_path();
        let final_path = ctx.state.recover_finalization(cell, ctx.generation)?;
        return completed(ctx, cell, &part, &final_path);
    }
    let legacy = {
        let record = cell.record.read();
        (
            !record.logical_url_identity,
            record.needs_logical_url_discard(),
            record.segments_planned,
        )
    };
    if legacy.0 {
        if legacy.1 {
            // Discard first: never pair a rewritten identity with old coverage
            // in a periodic checkpoint. A crash here retains an unbound row.
            reset_progress(ctx, cell, false).await?;
        }
        let mut record = cell.record.write();
        let _segments = cell.segments.write();
        cell.fence(ctx.generation)?;
        if legacy.1 {
            record.final_url = record.url.clone();
        }
        record.logical_url_identity = true;
        record.segments_planned = legacy.2;
        cell.bump_persist_epoch();
    }
    set_state(ctx, cell, TaskState::Probing);

    let (request_url, pref, previous) = {
        let record = cell.record.read();
        let url = if record.final_url.trim().is_empty() {
            record.url.clone()
        } else {
            record.final_url.clone()
        };
        // A task whose route is not durably locked owns no partial, so its
        // provisional path is never even stat'ed here (Zed G2 review, P2):
        // `part_exists` stays false and the remote-changed reset below can
        // never act on it.
        let part_exists = !record.route_unlocked() && record.part_path().exists();
        let previous = Previous { part_exists };
        (url, record.transport_pref, previous)
    };

    let probe = ctx.state.net.probe(&request_url, pref).await?;
    if !(200..300).contains(&probe.status) {
        return Err(Error::HttpStatus {
            status: probe.status,
            url: probe.final_url.clone(),
        });
    }

    // G2: a task whose route is not durably locked owns no partial, so a
    // changed remote has nothing to discard — and `reset_progress` must
    // never act on a provisional path. The route locks below, before any
    // partial exists. `Unpersisted` (a previous attempt's lock write failed)
    // takes the same path: the lock is re-run from the snapshot.
    let route_pending = cell.record.read().route_unlocked();

    if !crate::net::content_coding_is_identity(
        crate::net::content_encoding(&probe.headers).as_deref(),
    ) {
        return Err(Error::UnsupportedCoding);
    }

    if route_pending {
        lock_route(ctx, cell, &probe).await?;
    } else {
        apply_probe_hints(ctx, cell, &probe)?;
    }
    reconcile_representation(ctx, cell, &probe, &previous).await?;
    let plan = build_plan(ctx, cell, &probe).await?;

    // Nothing to do: an empty remote file.
    if plan.total == Some(0) {
        tokio::fs::write(&plan.part_path, [])
            .await
            .map_err(|e| Error::io(plan.part_path.clone(), e))?;
        return finish(ctx, cell, &plan, token).await;
    }

    set_state(ctx, cell, TaskState::Downloading);
    ctx.state.log(
        LogLevel::Info,
        format!(
            "#{} {} - {} over {}, {} segment(s){}",
            cell.id,
            filename_of(cell),
            util::human_bytes_opt(plan.total),
            probe.transport.as_str(),
            cell.segments.read().len(),
            match cell.received.load(Ordering::Relaxed) {
                0 => String::new(),
                done => format!(", resuming at {}", util::human_bytes(done as f64)),
            }
        ),
    );

    let outcome = transfer(ctx, cell, &plan, token).await?;
    if outcome == Outcome::Stopped {
        return Ok(Outcome::Stopped);
    }
    finish(ctx, cell, &plan, token).await
}

/// Copy non-identity probe hints and pick a filename. `final_url`, `etag`,
/// and `total_size` are written later, in the same critical section as the
/// representation decision, so a flush cannot pair a new validator with the
/// previous progress.
fn apply_probe_hints(ctx: &Arc<RunCtx>, cell: &Arc<TaskCell>, probe: &ProbeInfo) -> Result<()> {
    let id = cell.id;
    let mut filename_resolved = false;
    {
        let mut record = cell.record.write();
        record.transport = match record.transport_summary {
            crate::store::records::TransportSummary::Unknown => Some(probe.transport),
            _ => record.transport_summary.legacy(),
        };
        record.accept_ranges = probe.accept_ranges;
        record.last_modified = probe.last_modified.clone();
        record.content_type = probe.content_type.clone();
        if record.filename.trim().is_empty() {
            record.filename = choose_filename(probe, &probe.final_url, id);
            filename_resolved = true;
        }
        record.error = None;
    }

    // A brand new task must not clobber an existing file.
    if filename_resolved || !cell.record.read().part_path().exists() {
        let (dest_dir, filename) = {
            let record = cell.record.read();
            (record.dest_dir(), record.filename.clone())
        };
        if filename.trim().is_empty() {
            let name = choose_filename(probe, &probe.final_url, id);
            cell.record.write().filename = name;
        }
        let candidate = dest_dir.join(&cell.record.read().filename);
        if candidate.exists() {
            match ctx.settings.conflict_policy {
                ConflictPolicy::UniqueSuffix => {
                    let unique = util::unique_path(&dest_dir, &cell.record.read().filename);
                    let new_name = unique
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| cell.record.read().filename.clone());
                    cell.record.write().filename = new_name;
                }
                ConflictPolicy::Overwrite => {}
                ConflictPolicy::Fail => return Err(Error::Conflict(candidate)),
            }
        }
    }
    cell.dirty.store(true, Ordering::Relaxed);
    Ok(())
}

/// G2 route lock. Runs after a successful probe and before `build_plan`
/// takes any path or opens/preallocates a partial (contract sections 1, 3,
/// 6). Resolves the final filename (the record's user-supplied name wins,
/// then the probe's Content-Disposition hint, then the post-redirect URL),
/// classifies the ORIGINAL resolved name with the enqueue-time snapshot
/// rules, verifies/creates the category folder, applies the existing
/// collision policy, and durably publishes filename + destination + locked
/// route. Only a successful durable write lets the task continue: there is
/// no RAM-only route. Any failure is a clear, fatal task error — never a
/// silent fall back to the root or to `Other`.
async fn lock_route(ctx: &Arc<RunCtx>, cell: &Arc<TaskCell>, probe: &ProbeInfo) -> Result<()> {
    // Hints only. Identity fields wait for `reconcile_representation`.
    {
        let mut record = cell.record.write();
        record.transport = Some(probe.transport);
        record.accept_ranges = probe.accept_ranges;
        record.last_modified = probe.last_modified.clone();
        record.content_type = probe.content_type.clone();
        record.error = None;
    }

    let (root, rules) = match &cell.record.read().route {
        RouteState::Pending { root, rules } | RouteState::Unpersisted { root, rules } => {
            (PathBuf::from(root), rules.clone())
        }
        _ => return Ok(()), // route already locked durably; nothing to do
    };

    // The filename the route is classified from: the user's own name when
    // present, otherwise the probe-derived one. Classification happens on
    // THIS name, before any collision suffix exists (Zed G2 re-review,
    // P1): a collision-adjusted `archive.tar (1).gz` must still classify
    // as `.tar.gz`, never re-classify as `.gz` on a later attempt.
    let original_filename = {
        let record = cell.record.read();
        if record.filename.trim().is_empty() {
            choose_filename(probe, &probe.final_url, cell.id)
        } else {
            record.filename.clone()
        }
    };

    let rule = crate::category::classify(&original_filename, &rules).ok_or_else(|| {
        Error::Route("no category rule matched and no `other` category exists".into())
    })?;
    let (category_id, folder) = (rule.id.clone(), rule.folder.clone());

    let dest = crate::category::prepare_category_dir(&root, &folder, &original_filename)?;

    // Collision handling at lock time, per the existing engine policy and
    // inside the locked folder only. A name conflict is a terminal routing
    // outcome, not a retryable failure (Zed G2 re-review, P2).
    let mut filename = original_filename.clone();
    let candidate = dest.join(&filename);
    if candidate.exists() {
        match ctx.settings.conflict_policy {
            ConflictPolicy::UniqueSuffix => {
                let unique = util::unique_path(&dest, &filename);
                filename = unique
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or(filename);
            }
            ConflictPolicy::Overwrite => {}
            ConflictPolicy::Fail => {
                return Err(Error::Route(format!(
                    "destination already exists: {}",
                    crate::paths::display(&candidate)
                )))
            }
        }
    }

    // Publish the lock ATOMICALLY (Zed G2 re-review, P1): the candidate is
    // staged on a clone and saved under the persistence mutex, and only a
    // successful save copies the staged fields into the live record — so
    // no concurrent flush can durably write a speculative `Locked`, and a
    // failed save leaves the live route, destination and filename exactly
    // as they were (the ORIGINAL name included: a retry re-derives the
    // route from it, never from a collision-adjusted or probe-frozen name).
    let root_display = util::display_path(&root);
    let dest_display = util::display_path(&dest);
    ctx.state
        .persist_staged(
            cell,
            Some(ctx.generation),
            |staged| {
                staged.filename = filename.clone();
                staged.dest_dir = dest_display.clone();
                staged.route = RouteState::Locked {
                    category_id: category_id.clone(),
                    root: root_display.clone(),
                };
                Some(())
            },
            |live, staged| {
                live.filename = staged.filename.clone();
                live.dest_dir = staged.dest_dir.clone();
                live.route = staged.route.clone();
            },
        )
        .map_err(|e| Error::Route(format!("could not persist the locked category route: {e}")))?;
    Ok(())
}

fn choose_filename(probe: &ProbeInfo, final_url: &str, id: u64) -> String {
    if let Some(hint) = &probe.filename_hint {
        if !hint.trim().is_empty() {
            return hint.clone();
        }
    }
    if let Ok(url) = Url::parse(final_url) {
        if let Some(name) = util::filename_from_url(&url) {
            return name;
        }
    }
    format!("download-{id}")
}

/// Build (or reuse) the segment layout, create the `.part` file and
/// pre-allocate it.
async fn build_plan(ctx: &Arc<RunCtx>, cell: &Arc<TaskCell>, probe: &ProbeInfo) -> Result<Plan> {
    let total = probe.size;
    let (part_path, final_path, dest_dir) = {
        let record = cell.record.read();
        (record.part_path(), record.final_path(), record.dest_dir())
    };
    // G2: a locked category route re-verifies its destination before the
    // partial is opened — a folder swapped for a junction after the lock
    // must fail here, not be written through. Legacy/explicit destinations
    // keep the historical behaviour untouched.
    let locked_root = match &cell.record.read().route {
        RouteState::Locked { root, .. } => Some(PathBuf::from(root)),
        _ => None,
    };
    if let Some(root) = locked_root {
        let (folder, filename) = {
            let record = cell.record.read();
            (
                record
                    .dest_dir()
                    .strip_prefix(&root)
                    .ok()
                    .and_then(|rel| rel.file_name())
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
                record.filename.clone(),
            )
        };
        crate::category::prepare_category_dir(&root, &folder, &filename)?;
    } else {
        tokio::fs::create_dir_all(&dest_dir)
            .await
            .map_err(|e| Error::io(dest_dir.clone(), e))?;
    }

    let (existing, bound) = {
        let record = cell.record.read();
        (
            cell.segments.read().clone(),
            record.representation_bound.clone(),
        )
    };
    // Multi-segment planning is allowed only after a full bound exists, and
    // only before those segments receive bytes (a reusable layout already
    // passed that gate on the run that created it). No strong ETag, a weak
    // tag, a bad Vary, or a non-identity coding all stay on one segment.
    // `segments_planned` is ignored on that path.
    let bound_ready = bound.is_some();
    let reusable = bound_ready
        && !existing.is_empty()
        && match total {
            Some(total) => {
                existing.first().map(|s| s.start) == Some(0)
                    && existing.last().map(|s| s.end) == Some(total.saturating_sub(1))
            }
            None => existing.len() == 1 && existing[0].end == OPEN_ENDED,
        };

    let mut segments = if reusable {
        existing
    } else if !bound_ready {
        match total {
            Some(total) if total > 0 => vec![SegmentState::new(0, 0, total.saturating_sub(1))],
            Some(_) => vec![SegmentState::new(0, 0, 0)],
            None => vec![SegmentState::new(0, 0, OPEN_ENDED)],
        }
    } else {
        let count = cell.record.read().segments_planned.max(1);
        match total {
            Some(total) if probe.accept_ranges && total > 0 => {
                util::plan_segments(total, count, ctx.settings.min_segment_bytes)
                    .into_iter()
                    .enumerate()
                    .map(|(i, (start, end))| SegmentState::new(i as u32, start, end))
                    .collect()
            }
            Some(total) => vec![SegmentState::new(0, 0, total.saturating_sub(1))],
            None => vec![SegmentState::new(0, 0, OPEN_ENDED)],
        }
    };

    // Check if the partial file already exists and inspect its actual size
    // BEFORE creating or expanding it. If the file is missing but segments
    // claim progress, the partial file was deleted and progress must reset.
    let (file, initial_disk_len) = ctx
        .state
        .open_partial(cell.id, ctx.generation, &part_path)?;
    let file = tokio::fs::File::from_std(file);

    // If the partial file is missing or empty while we thought we had data,
    // reset all segments to unstarted so we never resume past non-existent bytes.
    if initial_disk_len == 0 {
        for segment in segments.iter_mut() {
            segment.done = 0;
            segment.finished = false;
        }
    }

    // Create / pre-allocate the partial file.
    if let Some(total) = total {
        file.set_len(total)
            .await
            .map_err(|e| Error::io(part_path.clone(), e))?;
    }

    // Validate done counts against the initial on-disk length.
    for segment in segments.iter_mut() {
        if segment.end == OPEN_ENDED {
            segment.done = segment
                .done
                .min(initial_disk_len.saturating_sub(segment.start));
            segment.finished = false;
        } else {
            segment.done = segment
                .done
                .min(initial_disk_len.saturating_sub(segment.start))
                .min(segment.len().unwrap_or(0));
            segment.finished = segment.finished && segment.done >= segment.len().unwrap_or(0);
            // Receiving the last byte is not proof that the response ended
            // correctly. Old interrupted checkpoints must revalidate its tail.
            if !segment.finished && segment.done == segment.len().unwrap_or(0) {
                segment.done = segment.done.saturating_sub(1);
            }
        }
    }

    let pref = {
        let mut record = cell.record.write();
        let mut coverage = cell.segments.write();
        cell.fence(ctx.generation)?;
        *coverage = segments;
        record.segments_planned = coverage.len() as u32;
        cell.bump_persist_epoch();
        record.transport_pref
    };
    let received: u64 = cell.segments.read().iter().map(|s| s.done).sum();
    cell.received.store(received, Ordering::Relaxed);
    ctx.state.persist_owned(cell, true, Some(ctx.generation))?;

    let bound = cell.record.read().representation_bound.clone();
    Ok(Plan {
        family_paths: None,
        route: None,
        allocation_epoch: 0,
        request_url: probe.final_url.clone(),
        part_path,
        final_path,
        total: match &bound {
            Some(bound) => bound.total_size,
            None => total,
        },
        if_range: bound.as_ref().map(|bound| bound.strong_etag.clone()),
        pref,
        bound,
        method_get: true,
    })
}

// ---------------------------------------------------------------------------
// transfer
// ---------------------------------------------------------------------------

async fn transfer(
    ctx: &Arc<RunCtx>,
    cell: &Arc<TaskCell>,
    plan: &Plan,
    token: &CancellationToken,
) -> Result<Outcome> {
    let mut plan = plan.clone();
    let epochs = cell
        .segments
        .read()
        .iter()
        .map(|s| s.allocation_epoch)
        .collect();
    *cell.allocation_epochs.lock() = epochs;
    if ctx.settings.dual_stack_paths {
        let settings = ctx.state.net.settings();
        let eligible = !ctx.settings.use_system_proxy
            && !settings.use_system_proxy
            && ctx.state.net.effective_pref(plan.pref) != TransportPref::Http3Only
            && plan.bound.is_some()
            && plan.total.is_some();
        if eligible {
            let paths = ctx.state.net.family_paths(&plan.request_url, plan.pref)?;
            // Conditional small range probes admit no coverage. All headers,
            // bound and actual peers are checked before any writer is granted.
            let mut admitted = true;
            for route in &paths.0 {
                let spec = RequestSpec::get(&plan.request_url)
                    .with_range(0, Some(0))
                    .with_if_range(plan.if_range.clone())
                    .with_family_route(Some(route.clone()));
                let response = tokio::select! {
                    _ = token.cancelled() => return Ok(Outcome::Stopped),
                    result = ctx.state.net.open(&spec, plan.pref) => result,
                };
                match response {
                    Ok(mut opened) => {
                        if !crate::net::content_coding_is_identity(
                            crate::net::content_encoding(&opened.headers).as_deref(),
                        ) {
                            return Err(Error::UnsupportedCoding);
                        }
                        validate_response(&opened, &plan, 0, 0)?;
                        let mut probe_plan = plan.clone();
                        confirm_segment_representation(ctx, cell, &mut probe_plan, &opened)?;
                        let mut bytes = 0;
                        loop {
                            let chunk = tokio::select! {
                                _ = token.cancelled() => return Ok(Outcome::Stopped),
                                result = tokio::time::timeout(settings.stall_timeout, opened.body.chunk()) => result.map_err(|_| Error::Stall { secs: settings.stall_timeout.as_secs() })??,
                            };
                            let Some(chunk) = chunk else {
                                break;
                            };
                            bytes += chunk.len();
                            cell.lanes.probe_received(chunk.len() as u64);
                            if bytes > 1 {
                                return Err(Error::InvalidResponse(
                                    "excess family probe body".into(),
                                ));
                            }
                        }
                        if bytes != 1 {
                            return Err(Error::InvalidResponse("short family probe body".into()));
                        }
                        cell.fence(ctx.generation)?;
                        opened.body.abort();
                    }
                    Err(e)
                        if matches!(
                            e,
                            Error::Connect(_) | Error::Dns(_) | Error::Timeout { .. }
                        ) =>
                    {
                        ctx.state.log(
                            LogLevel::Info,
                            format!("#{} family paths ineligible: {e}", cell.id),
                        );
                        admitted = false;
                        break;
                    }
                    Err(e) => return Err(e),
                }
            }
            if admitted {
                plan.family_paths = Some(paths);
            }
        } else {
            ctx.state.log(
                LogLevel::Info,
                format!(
                    "#{} family paths ineligible: proxy, H3 policy or unbound representation",
                    cell.id
                ),
            );
        }
    }
    if plan.family_paths.is_some() {
        cell.uses_family_paths.store(true, Ordering::Release);
        return transfer_families(ctx, cell, &plan, token).await;
    }
    let plan = &plan;
    let count = cell.segments.read().len();
    let mut set: JoinSet<Result<()>> = JoinSet::new();
    for index in 0..count {
        let mut attempt = plan.clone();
        attempt.allocation_epoch = cell.allocation_epochs.lock()[index];
        set.spawn(run_segment(
            ctx.clone(),
            cell.clone(),
            attempt,
            token.clone(),
            index,
        ));
    }

    let mut errors: Vec<Error> = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(Ok(())) => {}
            Ok(Err(e)) => errors.push(e),
            Err(join_error) => {
                errors.push(Error::Other(format!("segment task panicked: {join_error}")))
            }
        }
    }

    if token.is_cancelled() {
        return Ok(Outcome::Stopped);
    }

    let all_done = cell.segments.read().iter().all(|s| s.is_done());
    if errors.is_empty() && all_done {
        return Ok(Outcome::Finished);
    }

    // Prefer the most informative error: a fatal one if any segment hit it,
    // otherwise the first error seen, otherwise a generic complaint.
    let err = errors
        .iter()
        .position(|e| e.is_fatal())
        .map(|i| errors.swap_remove(i))
        .or_else(|| errors.pop())
        .unwrap_or_else(|| {
            Error::Other("transfer stopped before the file was complete".to_string())
        });
    Err(err)
}

/// One scheduler owns all leases. A family has at most one worker; a failed
/// range is queued to the surviving family only after join and checkpoint read.
async fn transfer_families(
    ctx: &Arc<RunCtx>,
    cell: &Arc<TaskCell>,
    plan: &Plan,
    token: &CancellationToken,
) -> Result<Outcome> {
    use std::collections::VecDeque;
    let paths = plan
        .family_paths
        .as_ref()
        .ok_or_else(|| Error::Other("missing family paths".into()))?;
    let count = cell.segments.read().len();
    let mut queue: VecDeque<_> = (0..count).collect();
    let mut busy = [false; 2];
    let mut alive = [true; 2];
    let mut set = JoinSet::new();
    let drain = token.child_token();
    let mut failure = None;
    loop {
        for family in 0..2 {
            if alive[family] && !busy[family] && failure.is_none() && !token.is_cancelled() {
                if let Some(index) = queue.pop_front() {
                    let mut attempt = plan.clone();
                    attempt.route = Some(paths.0[family].clone());
                    attempt.allocation_epoch = cell.allocation_epochs.lock()[index];
                    cell.lanes.allocation(
                        ctx.generation,
                        index,
                        attempt.allocation_epoch,
                        "grant",
                        cell.segments.read()[index].done,
                    );
                    let ctx = ctx.clone();
                    let cell = cell.clone();
                    let token = drain.clone();
                    busy[family] = true;
                    set.spawn(async move {
                        (
                            family,
                            index,
                            run_segment(ctx, cell, attempt, token, index).await,
                        )
                    });
                }
            }
        }
        let Some(joined) = set.join_next().await else {
            break;
        };
        match joined {
            Ok((family, index, result)) => {
                busy[family] = false;
                let epoch = cell.allocation_epochs.lock()[index];
                let done = cell.segments.read()[index].done;
                cell.lanes
                    .allocation(ctx.generation, index, epoch, "joined", done);
                if let Err(error) = result {
                    // Storage and representation failures are never route loss.
                    let route_loss = matches!(
                        error,
                        Error::Connect(_)
                            | Error::Dns(_)
                            | Error::Timeout { .. }
                            | Error::Stall { .. }
                            | Error::Http3(_)
                    );
                    if route_loss && alive[1 - family] && failure.is_none() && !token.is_cancelled()
                    {
                        alive[family] = false;
                        if let Some(gate) =
                            cell.proof_gate(crate::engine::proof::ProofPoint::BeforeReclaim)
                        {
                            gate.wait_async(index as u64).await;
                        }
                        match ctx.state.reclaim_range(cell, ctx.generation, index) {
                            Ok(_) => queue.push_front(index),
                            Err(e) => failure = Some(e),
                        }
                    } else if failure.is_none() {
                        failure = Some(error);
                    }
                }
            }
            Err(e) => failure = Some(Error::Other(format!("family worker panicked: {e}"))),
        }
        if failure.is_some() {
            drain.cancel();
        }
    }
    if let Some(error) = failure {
        return Err(error);
    }
    if token.is_cancelled() {
        return Ok(Outcome::Stopped);
    }
    if !queue.is_empty() || !cell.segments.read().iter().all(SegmentState::is_done) {
        return Err(Error::Other("family ranges incomplete".into()));
    }
    // All writers drained. Only this synced, committed snapshot can authorize
    // completion, including the withheld last bytes.
    ctx.state.persist_owned(cell, true, Some(ctx.generation))?;
    Ok(Outcome::Finished)
}

enum SegmentOut {
    Plain(tokio::io::BufWriter<tokio::fs::File>),
    Fault(tokio::io::BufWriter<FaultFile>),
}

impl SegmentOut {
    async fn drain(self) -> std::io::Result<()> {
        // Never retry a BufWriter flush after a storage error: that could
        // publish bytes the failing write never accepted. Discard the volatile
        // buffer, but await any Tokio File operation already submitted.
        match self {
            Self::Plain(file) => file.into_inner().flush().await,
            Self::Fault(file) => file.into_inner().inner.flush().await,
        }
    }
    async fn seek_start(&mut self, offset: u64) -> std::io::Result<()> {
        match self {
            Self::Plain(file) => file
                .seek(std::io::SeekFrom::Start(offset))
                .await
                .map(|_| ()),
            Self::Fault(file) => file
                .seek(std::io::SeekFrom::Start(offset))
                .await
                .map(|_| ()),
        }
    }

    async fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self {
            Self::Plain(file) => file.write_all(bytes).await,
            Self::Fault(file) => file.write_all(bytes).await,
        }
    }

    async fn flush_out(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(file) => file.flush().await,
            Self::Fault(file) => file.flush().await,
        }
    }
}

enum WriteInject {
    Short { accept: u64 },
    FailNext,
    StorageFull,
}

struct FaultFile {
    inner: tokio::fs::File,
    inject: Option<WriteInject>,
}

impl FaultFile {
    fn from_fault(inner: tokio::fs::File, fault: super::SegmentWriteFault) -> Self {
        let inject = match fault {
            super::SegmentWriteFault::Short { accept } => WriteInject::Short { accept },
            super::SegmentWriteFault::StorageFull => WriteInject::StorageFull,
        };
        Self {
            inner,
            inject: Some(inject),
        }
    }
}

impl AsyncWrite for FaultFile {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        match this.inject.take() {
            Some(WriteInject::StorageFull) => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "injected ENOSPC",
            ))),
            Some(WriteInject::FailNext) => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "injected short write",
            ))),
            Some(WriteInject::Short { accept }) => {
                if buf.is_empty() {
                    this.inject = Some(WriteInject::Short { accept });
                    return Poll::Ready(Ok(0));
                }
                if accept == 0 {
                    this.inject = Some(WriteInject::FailNext);
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "injected short write",
                    )));
                }
                let n = std::cmp::min(accept as usize, buf.len());
                match Pin::new(&mut this.inner).poll_write(cx, &buf[..n]) {
                    Poll::Ready(Ok(wrote)) => {
                        let left = accept.saturating_sub(wrote as u64);
                        this.inject = if left == 0 {
                            Some(WriteInject::FailNext)
                        } else {
                            Some(WriteInject::Short { accept: left })
                        };
                        Poll::Ready(Ok(wrote))
                    }
                    Poll::Ready(Err(error)) => {
                        this.inject = Some(WriteInject::Short { accept });
                        Poll::Ready(Err(error))
                    }
                    Poll::Pending => {
                        this.inject = Some(WriteInject::Short { accept });
                        Poll::Pending
                    }
                }
            }
            None => Pin::new(&mut this.inner).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

impl AsyncSeek for FaultFile {
    fn start_seek(self: Pin<&mut Self>, position: std::io::SeekFrom) -> std::io::Result<()> {
        Pin::new(&mut self.get_mut().inner).start_seek(position)
    }

    fn poll_complete(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<u64>> {
        Pin::new(&mut self.get_mut().inner).poll_complete(cx)
    }
}

async fn run_segment(
    ctx: Arc<RunCtx>,
    cell: Arc<TaskCell>,
    mut plan: Plan,
    token: CancellationToken,
    index: usize,
) -> Result<()> {
    let (start, end, done) = {
        let segments = cell.segments.read();
        let seg = segments
            .get(index)
            .ok_or_else(|| Error::Other(format!("segment {index} disappeared")))?;
        (seg.start, seg.end, seg.done)
    };
    let length = if end == OPEN_ENDED {
        None
    } else {
        Some(end.saturating_sub(start) + 1)
    };
    // The local length check used to finish the segment before any request.
    // That skip is legal only when a bound exists and still matches this
    // plan. Unbound progress, and a bound that does not match, must not
    // mark the segment finished. They open the request so a 416 can be
    // classified instead of treated as success.
    let locally_complete = length.is_some_and(|len| len == 0 || done >= len);
    if locally_complete && plan_bound_matches(&cell, &plan) {
        return mark_finished_epoch(&ctx, &cell, index, plan.allocation_epoch);
    }

    let opened_file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&plan.part_path)
        .await
        .map_err(|e| Error::io(plan.part_path.clone(), e))?;
    // Unarmed downloads keep `BufWriter<File>`. The wrapper exists only
    // after a test has taken a write fault off the slot.
    let mut file = match ctx.state.take_segment_write_fault() {
        Some(inject) => SegmentOut::Fault(tokio::io::BufWriter::with_capacity(
            ctx.settings.write_buffer_bytes,
            FaultFile::from_fault(opened_file, inject),
        )),
        None => SegmentOut::Plain(tokio::io::BufWriter::with_capacity(
            ctx.settings.write_buffer_bytes,
            opened_file,
        )),
    };

    let result = async {
    let offset = start + done;
    file.seek_start(offset)
        .await
        .map_err(|e| Error::io(plan.part_path.clone(), e))?;

    let spec = RequestSpec::get(&plan.request_url)
        .with_range(offset, if end == OPEN_ENDED { None } else { Some(end) })
        .with_if_range(plan.if_range.clone())
        .with_family_route(plan.route.clone());

    // Revocation (pause/cancel/remove/shutdown) must not wait for connect or
    // response headers. Dropping an unopened request touches no file.
    let mut opened = tokio::select! {
        biased;
        _ = token.cancelled() => return Ok(()),
        opened = ctx.state.net.open_observed(&spec, plan.pref, &cell.lanes, cell.id, ctx.generation, index) => opened?,
    };
    let active_request = ActiveRequest::new(&cell);

    if !crate::net::content_coding_is_identity(
        crate::net::content_encoding(&opened.headers).as_deref(),
    ) {
        opened.body.end_network();
        return Err(Error::UnsupportedCoding);
    }
    if opened.is_range_not_satisfiable() {
        let class = classify_416(
            usable_416_total(&opened),
            cell.record.read().representation_bound.as_ref(),
            done,
            length,
            plan_bound_matches(&cell, &plan),
        );
        opened.body.end_network();
        // 416 never adds coverage and never calls `mark_finished`.
        return match class {
            RangeClass::Mismatch => Err(Error::RemoteChanged),
            RangeClass::NotSatisfiable => Err(Error::RangeNotSatisfiable),
        };
    }
    if !opened.is_success() {
        return Err(Error::HttpStatus {
            status: opened.status,
            url: plan.request_url.clone(),
        });
    }
    let response_length = validate_response(&opened, &plan, offset, end)?;
    confirm_segment_representation(&ctx, &cell, &mut plan, &opened)?;

    // Record which transport is actually delivering this task's bytes.
    {
        let mut record = cell.record.write();
        record.transport_summary.observe(opened.transport);
        record.transport = record.transport_summary.legacy();
    }

    let _path_use = plan.route.as_ref().and_then(|route| {
        cell.lanes.admit_path(ctx.generation, route.family, plan.allocation_epoch,
            opened.transport, &opened.final_url, opened.remote_addr)
    });

    let stall = Duration::from_secs(ctx.settings.stall_timeout_secs);
    // Flushed means the OS has accepted the write, not power-loss durability.
    let mut flushed: u64 = done;
    let mut buffered_bytes: usize = 0;
    // EOF is the network terminal. Cancel is too, but it must not be mistaken
    // for a body the caller still has to flush before dropping in-use.
    let mut network_eof = false;
    let max_unflushed_bytes = ctx.settings.write_buffer_bytes.clamp(1, 1024 * 1024);

    loop {
        if token.is_cancelled() {
            break;
        }
        // Only the network read races revocation (F1). File writes below are
        // never raced or dropped mid-flight: a write that was submitted is
        // awaited to completion, so draining a revoked run leaves no unknown
        // write behind it.
        let chunk = tokio::select! {
            biased;
            _ = token.cancelled() => break,
            chunk = tokio::time::timeout(stall, opened.body.chunk()) => chunk
                .map_err(|_| Error::Stall {
                    secs: stall.as_secs(),
                })??,
        };
        let Some(chunk) = chunk else {
            network_eof = true;
            break;
        };
        if chunk.is_empty() {
            continue;
        }

        // Validate chunk against expected length if known.
        let received = flushed - done + buffered_bytes as u64;
        if response_length.is_some_and(|len| chunk.len() as u64 > len.saturating_sub(received)) {
            return Err(Error::InvalidResponse(
                "body exceeds the declared response range".into(),
            ));
        }
        let to_write = &chunk[..];

        if to_write.is_empty() {
            break;
        }

        // Narrow defense: a superseded run writes no further bytes. Overlap is
        // already excluded by drain-before-regrant (`running`).
        cell.fence(ctx.generation)?;
        if let Some(gate) = cell.proof_gate(crate::engine::proof::ProofPoint::BeforeWrite)
            .filter(|g| g.segment.is_none_or(|s| s == index)) {
            gate.wait_async(to_write.len() as u64).await;
            if token.is_cancelled() {
                return Ok(());
            }
        }
        cell.fence(ctx.generation)?;
        cell.allocation_fence(index, plan.allocation_epoch)?;
        file.write_all(to_write)
            .await
            .map_err(|e| Error::io(plan.part_path.clone(), e))?;

        buffered_bytes += to_write.len();
        // Update live UI counter immediately so user sees real-time reception speed.
        cell.received
            .fetch_add(to_write.len() as u64, Ordering::Relaxed);

        // Periodically flush buffer down to disk so resume milestones advance safely
        // without incurring disk flush overhead on every chunk.
        if buffered_bytes >= max_unflushed_bytes {
            file.flush_out()
                .await
                .map_err(|e| Error::io(plan.part_path.clone(), e))?;
            flushed += buffered_bytes as u64;
            buffered_bytes = 0;
            note_flushed_progress(&ctx, &cell, index, flushed, length, plan.allocation_epoch)?;
        }
    }

    // Network terminal is this drop, not the tail flush below and not the
    // `ActiveRequest` guard, which stays alive across that flush. Cancel
    // takes the same drop, so in-use does not wait out `stall` or the write.
    opened.body.end_network();
    if let Some(gate) = cell.proof_gate(crate::engine::proof::ProofPoint::AfterBodyBeforeFlush)
        .filter(|g| g.segment.is_none_or(|s| s == index)) {
        gate.wait_async(flushed + buffered_bytes as u64).await;
    }
    if network_eof && !token.is_cancelled() {
        if let Some(gate) = ctx.state.net.tail_flush_gate() {
            tokio::select! {
                biased;
                _ = token.cancelled() => {}
                _ = gate.wait_for_release() => {}
            }
        }
    }

    // Flush any remaining buffered data before exiting the transfer loop.
    if buffered_bytes > 0 {
        file.flush_out()
            .await
            .map_err(|e| Error::io(plan.part_path.clone(), e))?;
        flushed += buffered_bytes as u64;
        note_flushed_progress(&ctx, &cell, index, flushed, length, plan.allocation_epoch)?;
    }

    // The outer owner drains this handle on every exit, including network errors.
    // Already ended on EOF and on cancel. A path that left the body open
    // still ends it before `ActiveRequest` drops.
    opened.body.end_network();
    drop(active_request);

    if token.is_cancelled() {
        return Ok(());
    }

    if response_length.is_some_and(|len| flushed - done != len) {
        return Err(Error::Other(format!(
            "segment {index} response ended early"
        )));
    }

    if !bound_still_current(&cell, &plan) {
        return Err(Error::RemoteChanged);
    }
    match length {
        Some(len) if flushed == len => mark_finished_epoch(&ctx, &cell, index, plan.allocation_epoch),
        Some(len) if flushed < len => Err(Error::Other(format!(
            "segment {index} ended early ({flushed} of {len} bytes)",
        ))),
        Some(len) => Err(Error::Other(format!(
            "segment {index} exceeded planned length ({flushed} of {len} bytes)",
        ))),
        None => {
            // Open-ended: the server closed the stream, so this is the whole file.
            mark_finished_epoch(&ctx, &cell, index, plan.allocation_epoch)
        }
    }
    }.await;
    // Complete even already-submitted Tokio file operations on every exit.
    // A joined failed worker must leave no write racing its replacement.
    file.drain()
        .await
        .map_err(|e| Error::io(plan.part_path.clone(), e))?;
    result
}

/// Validate the supported single-range response before any bytes reach the file.
/// The returned length bounds the *response*, including open-ended segments.
fn validate_response(opened: &Opened, plan: &Plan, offset: u64, end: u64) -> Result<Option<u64>> {
    let invalid = |message: &str| Error::InvalidResponse(message.to_string());
    let content_length = opened.content_length();
    if opened.headers.contains_key(http::header::CONTENT_LENGTH) && content_length.is_none() {
        return Err(invalid("invalid Content-Length"));
    }
    match opened.status {
        200 => {
            if offset > 0
                || (end != OPEN_ENDED
                    && plan
                        .total
                        .is_some_and(|total| end.checked_add(1) != Some(total)))
            {
                return Err(Error::RangeIgnored);
            }
            if let (Some(expected), Some(actual)) = (plan.total, content_length) {
                if expected != actual {
                    return Err(Error::RemoteChanged);
                }
            }
            Ok(plan.total.or(content_length))
        }
        206 => {
            let header = opened
                .header(&http::header::CONTENT_RANGE)
                .ok_or_else(|| invalid("206 requires Content-Range"))?;
            if header
                .split_whitespace()
                .nth(1)
                .is_some_and(|range| range.starts_with('*'))
            {
                return Err(invalid("206 requires a satisfied byte range"));
            }
            let (first, last, total) = opened
                .content_range()
                .ok_or_else(|| invalid("malformed Content-Range"))?;
            if first != offset {
                return Err(invalid("Content-Range starts at the wrong offset"));
            }
            if let Some(expected) = plan.total {
                match total {
                    Some(actual) if actual != expected => return Err(Error::RemoteChanged),
                    None => return Err(invalid("Content-Range omits the expected total")),
                    _ => {}
                }
            }
            let expected_end = if end == OPEN_ENDED {
                total.and_then(|size| size.checked_sub(1))
            } else {
                Some(end)
            };
            if expected_end.is_some_and(|expected| last != expected) {
                // A shorter range can be legal HTTP, but this worker only supports
                // the exact requested range. Fail explicitly rather than misplace it.
                return Err(invalid("Content-Range does not cover the requested range"));
            }
            let length = last
                .checked_sub(first)
                .and_then(|n| n.checked_add(1))
                .ok_or_else(|| invalid("Content-Range length overflows"))?;
            if content_length.is_some_and(|actual| actual != length) {
                return Err(invalid("Content-Length disagrees with Content-Range"));
            }
            Ok(Some(length))
        }
        _ => Err(invalid("expected HTTP 200 or 206 with a download body")),
    }
}

#[cfg(test)]
fn mark_finished(ctx: &RunCtx, cell: &Arc<TaskCell>, index: usize) -> Result<()> {
    mark_finished_epoch(ctx, cell, index, 0)
}

fn mark_finished_epoch(ctx: &RunCtx, cell: &Arc<TaskCell>, index: usize, epoch: u64) -> Result<()> {
    let _record = cell.record.read();
    let mut segments = cell.segments.write();
    cell.fence(ctx.generation)?;
    cell.allocation_fence(index, epoch)?;
    if let Some(seg) = segments.get_mut(index) {
        if let Some(len) = seg.len() {
            seg.done = len;
        }
        seg.finished = true;
    }
    cell.bump_persist_epoch();
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum RangeClass {
    /// Section 2 restart. Coverage stays where it was. `finish` is not called.
    Mismatch,
    /// Not a representation restart.
    NotSatisfiable,
}

/// One function for every 416. It has no success arm: `done >= len` is not
/// `mark_finished`.
fn classify_416(
    observed_total: Option<u64>,
    bound: Option<&crate::store::records::RepresentationBound>,
    done: u64,
    len: Option<u64>,
    bound_matches_plan: bool,
) -> RangeClass {
    if let (Some(total), Some(bound)) = (observed_total, bound) {
        // `None` is an identity. A present total that is not that same
        // `Some(n)` contradicts the bound, including `None` versus `Some`.
        if bound.total_size != Some(total) {
            return RangeClass::Mismatch;
        }
    }
    let locally_complete = len.is_some_and(|len| done >= len);
    if locally_complete && !bound_matches_plan {
        return RangeClass::Mismatch;
    }
    RangeClass::NotSatisfiable
}

fn usable_416_total(opened: &Opened) -> Option<u64> {
    let header = opened.header(&http::header::CONTENT_RANGE)?;
    let (_, _, total) = util::parse_content_range(&header)?;
    total
}

fn plan_bound_matches(cell: &TaskCell, plan: &Plan) -> bool {
    plan.bound.is_some() && cell.record.read().representation_bound == plan.bound
}

fn bound_still_current(cell: &TaskCell, plan: &Plan) -> bool {
    cell.record.read().representation_bound == plan.bound
}

fn segments_flushed(cell: &TaskCell) -> bool {
    cell.segments
        .read()
        .iter()
        .any(|segment| segment.done > 0 || segment.finished)
}

fn note_flushed_progress(
    ctx: &RunCtx,
    cell: &TaskCell,
    index: usize,
    flushed: u64,
    length: Option<u64>,
    epoch: u64,
) -> Result<()> {
    let _record = cell.record.read();
    let mut segments = cell.segments.write();
    cell.fence(ctx.generation)?;
    cell.allocation_fence(index, epoch)?;
    if let Some(segment) = segments.get_mut(index) {
        // Withhold the final byte from resume checkpoints until EOF is
        // validated, even when the last buffer was already flushed.
        segment.done = length.map_or(flushed, |len| flushed.min(len.saturating_sub(1)));
    }
    cell.bump_persist_epoch();
    Ok(())
}

fn url_for_log(url: &str) -> String {
    match Url::parse(url) {
        Ok(mut parsed) => {
            parsed.set_query(None);
            parsed.set_fragment(None);
            parsed.to_string()
        }
        Err(_) => match url.split(['?', '#']).next() {
            Some(head) => head.to_string(),
            None => url.to_string(),
        },
    }
}

fn etag_difference(stored: &str, observed: &str) -> &'static str {
    if crate::net::is_weak_etag(stored) || crate::net::is_weak_etag(observed) {
        return "weak-prefix difference";
    }
    let stored_body = stored.trim().trim_matches('"');
    let observed_body = observed.trim().trim_matches('"');
    if stored_body == observed_body && stored != observed {
        return "quote-only difference";
    }
    "verbatim difference"
}

fn bindable_bound(probe: &ProbeInfo) -> Option<crate::store::records::RepresentationBound> {
    if !crate::net::content_coding_is_identity(
        crate::net::content_encoding(&probe.headers).as_deref(),
    ) {
        return None;
    }
    if !crate::net::vary_allows_none_scope(crate::net::vary(&probe.headers).as_deref()) {
        return None;
    }
    let strong = crate::net::strong_etag(probe.etag.as_deref()?)?;
    Some(crate::store::records::RepresentationBound::none_scope(
        probe.final_url.clone(),
        strong,
        probe.size,
    ))
}

enum BoundCompare {
    Same,
    Upgrade(u64),
    Mismatch(&'static str),
}

fn compare_bound(
    bound: &crate::store::records::RepresentationBound,
    probe: &ProbeInfo,
    flushed: bool,
) -> BoundCompare {
    if bound.header_scope != crate::store::records::HEADER_SCOPE_NONE
        || !crate::net::vary_allows_none_scope(crate::net::vary(&probe.headers).as_deref())
    {
        return BoundCompare::Mismatch("header scope");
    }
    if bound.final_url != probe.final_url {
        return BoundCompare::Mismatch("final_url");
    }
    match probe.etag.as_deref() {
        Some(raw) => match crate::net::strong_etag(raw) {
            Some(strong) if strong == bound.strong_etag => {}
            _ => return BoundCompare::Mismatch(etag_difference(&bound.strong_etag, raw)),
        },
        None => return BoundCompare::Mismatch("verbatim difference"),
    }
    match (bound.total_size, probe.size) {
        (Some(expected), Some(actual)) if expected == actual => BoundCompare::Same,
        (Some(_), Some(_)) => BoundCompare::Mismatch("length"),
        (None, None) => BoundCompare::Same,
        (None, Some(size)) if !flushed => BoundCompare::Upgrade(size),
        (None, Some(_)) => BoundCompare::Mismatch("length"),
        (Some(_), None) => BoundCompare::Mismatch("length"),
    }
}

fn commit_identity(
    ctx: &RunCtx,
    cell: &TaskCell,
    probe: &ProbeInfo,
    new_bound: Option<crate::store::records::RepresentationBound>,
) -> Result<()> {
    let mut record = cell.record.write();
    let segments = cell.segments.write();
    cell.fence(ctx.generation)?;
    let flushed = segments
        .iter()
        .any(|segment| segment.done > 0 || segment.finished);
    if let Some(bound) = &new_bound {
        let existing = record.representation_bound.clone();
        if existing.is_none() && flushed {
            return Err(Error::RemoteChanged);
        }
        if let Some(existing) = &existing {
            if existing.total_size.is_none() && bound.total_size.is_some() && flushed {
                return Err(Error::RemoteChanged);
            }
        }
        record.representation_bound = Some(bound.clone());
    }
    record.logical_url_identity = true;
    record.final_url = probe.final_url.clone();
    record.etag = probe.etag.clone();
    record.last_modified = probe.last_modified.clone();
    record.accept_ranges = probe.accept_ranges;
    record.transport = match record.transport_summary {
        crate::store::records::TransportSummary::Unknown => Some(probe.transport),
        _ => record.transport_summary.legacy(),
    };
    record.content_type = probe.content_type.clone();
    if let Some(size) = probe.size {
        record.total_size = Some(size);
    } else if new_bound
        .as_ref()
        .and_then(|bound| bound.total_size)
        .is_some()
    {
        record.total_size = new_bound.as_ref().and_then(|bound| bound.total_size);
    }
    record.error = None;
    cell.bump_persist_epoch();
    Ok(())
}

async fn reconcile_representation(
    ctx: &Arc<RunCtx>,
    cell: &Arc<TaskCell>,
    probe: &ProbeInfo,
    previous: &Previous,
) -> Result<()> {
    if !crate::net::content_coding_is_identity(
        crate::net::content_encoding(&probe.headers).as_deref(),
    ) {
        return Err(Error::UnsupportedCoding);
    }
    let bound = cell.record.read().representation_bound.clone();
    if bound.is_none() && segments_flushed(cell) {
        ctx.state.log(
            LogLevel::Warn,
            format!(
                "#{} {}: discarding unbound progress{}; a stored validator is not a bound",
                cell.id,
                filename_of(cell),
                if previous.part_exists {
                    " and the partial"
                } else {
                    ""
                }
            ),
        );
        // Legacy and unbound bytes restart from offset 0. This is not the
        // representation-mismatch budget: the next run of an origin with no
        // strong ETag discards again instead of failing the task.
        reset_progress(ctx, cell, false).await?;
    }

    let flushed = segments_flushed(cell);
    let bound = cell.record.read().representation_bound.clone();
    match bound {
        Some(bound) => match compare_bound(&bound, probe, flushed) {
            BoundCompare::Same => commit_identity(ctx, cell, probe, None),
            BoundCompare::Upgrade(size) => {
                let mut updated = bound;
                updated.total_size = Some(size);
                commit_identity(ctx, cell, probe, Some(updated))
            }
            BoundCompare::Mismatch(note) => {
                tracing::warn!(
                    difference = note,
                    url = %url_for_log(&probe.final_url),
                    "representation mismatch"
                );
                Err(Error::RemoteChanged)
            }
        },
        None => {
            if let Some(bound) = bindable_bound(probe) {
                commit_identity(ctx, cell, probe, Some(bound))
            } else {
                commit_identity(ctx, cell, probe, None)
            }
        }
    }
}

fn confirm_segment_representation(
    ctx: &RunCtx,
    cell: &TaskCell,
    plan: &mut Plan,
    opened: &Opened,
) -> Result<()> {
    if !plan.method_get {
        return Err(Error::InvalidResponse(
            "coverage is bound only for GET".into(),
        ));
    }
    let live = cell.record.read().representation_bound.clone();
    let Some(bound) = live else {
        if plan.bound.is_some() {
            return Err(Error::RemoteChanged);
        }
        return Ok(());
    };
    if plan.bound.is_some() && plan.bound.as_ref() != Some(&bound) {
        return Err(Error::RemoteChanged);
    }
    if opened.final_url != bound.final_url {
        tracing::warn!(
            url = %url_for_log(&opened.final_url),
            "representation final_url mismatch"
        );
        return Err(Error::RemoteChanged);
    }
    if bound.header_scope != crate::store::records::HEADER_SCOPE_NONE
        || !crate::net::vary_allows_none_scope(crate::net::vary(&opened.headers).as_deref())
    {
        tracing::warn!("representation header scope mismatch");
        return Err(Error::RemoteChanged);
    }
    match crate::net::etag(&opened.headers).as_deref() {
        Some(raw) => match crate::net::strong_etag(raw) {
            Some(strong) if strong == bound.strong_etag => {}
            _ => {
                tracing::warn!(
                    difference = etag_difference(&bound.strong_etag, raw),
                    "representation validator mismatch"
                );
                return Err(Error::RemoteChanged);
            }
        },
        None => {
            tracing::warn!("representation validator missing on the segment response");
            return Err(Error::RemoteChanged);
        }
    }
    let declared = match opened.status {
        206 => opened.content_range().and_then(|(_, _, total)| total),
        200 => opened.content_length(),
        _ => None,
    };
    match (bound.total_size, declared) {
        (None, Some(size)) => {
            let updated = {
                let mut record = cell.record.write();
                let segments = cell.segments.write();
                cell.fence(ctx.generation)?;
                if segments
                    .iter()
                    .any(|segment| segment.done > 0 || segment.finished)
                {
                    return Err(Error::RemoteChanged);
                }
                let Some(mut live) = record.representation_bound.clone() else {
                    return Err(Error::RemoteChanged);
                };
                if live.total_size.is_none() {
                    live.total_size = Some(size);
                    record.total_size = Some(size);
                    record.representation_bound = Some(live.clone());
                    cell.bump_persist_epoch();
                }
                live
            };
            plan.bound = Some(updated);
            plan.total = updated_total(plan);
            ctx.state.persist_owned(cell, true, Some(ctx.generation))?;
        }
        (Some(expected), Some(actual)) if expected != actual => {
            tracing::warn!("representation length mismatch");
            return Err(Error::RemoteChanged);
        }
        _ => {}
    }
    Ok(())
}

fn updated_total(plan: &Plan) -> Option<u64> {
    plan.bound.as_ref().and_then(|bound| bound.total_size)
}

// ---------------------------------------------------------------------------
// completion
// ---------------------------------------------------------------------------

async fn finish(
    ctx: &Arc<RunCtx>,
    cell: &Arc<TaskCell>,
    plan: &Plan,
    token: &CancellationToken,
) -> Result<Outcome> {
    // Optional integrity check before the file becomes visible under its final
    // name.
    // Completion has an empty segment layout. Sync the full backing before
    // rename so its final tail is not exempt from the progress ordering rule.
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&plan.part_path)
        .await
        .map_err(|e| Error::io(plan.part_path.clone(), e))?;
    if ctx.state.take_finish_sync_fault() {
        drop(file);
        return Err(Error::io(
            plan.part_path.clone(),
            std::io::Error::new(std::io::ErrorKind::Other, "injected disk sync failure"),
        ));
    }
    file.sync_all()
        .await
        .map_err(|e| Error::io(plan.part_path.clone(), e))?;
    drop(file);
    let expected = cell.record.read().expected_hash.clone();
    if let Some(expected) = expected.filter(|_| ctx.settings.verify_checksums) {
        set_state(ctx, cell, TaskState::Verifying);
        cell.verify_progress.store(0, Ordering::Relaxed);
        ctx.state.log(
            LogLevel::Info,
            format!(
                "#{} verifying {} ({})",
                cell.id,
                expected.algo.as_str(),
                filename_of(cell)
            ),
        );
        let progress = Arc::new(AtomicU64::new(0));
        let watcher = {
            let cell = cell.clone();
            let progress = progress.clone();
            let token = token.clone();
            tokio::spawn(async move {
                while !token.is_cancelled() {
                    let v = progress.load(Ordering::Relaxed);
                    cell.verify_progress.store(v, Ordering::Relaxed);
                    if v == 0 {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        continue;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            })
        };
        let result = hash::hash_file(
            &plan.part_path,
            expected.algo,
            Some(progress.clone()),
            Some(token.clone()),
        )
        .await;
        watcher.abort();
        let actual = result?;
        cell.record.write().computed_hash = Some(actual.clone());
        if !expected.matches(&actual) {
            cell.verify_progress.store(0, Ordering::Relaxed);
            return Err(Error::ChecksumMismatch {
                algo: expected.algo.as_str().to_string(),
                expected: expected.hex.clone(),
                actual,
            });
        }
        cell.verify_progress.store(0, Ordering::Relaxed);
        ctx.state.log(
            LogLevel::Info,
            format!("#{} checksum verified ({})", cell.id, expected.display()),
        );
    }

    // G2: re-verify a locked category destination before the rename, and
    // hold partial ownership until the rename and its persist complete
    // (the persist below already precedes release_partial).
    if let RouteState::Locked { root, .. } = &cell.record.read().route {
        let root = PathBuf::from(root);
        let (folder, filename) = {
            let record = cell.record.read();
            (
                record
                    .dest_dir()
                    .strip_prefix(&root)
                    .ok()
                    .and_then(|rel| rel.file_name())
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
                record.filename.clone(),
            )
        };
        crate::category::prepare_category_dir(&root, &folder, &filename)?;
    }

    // A locally recorded content digest lets recovery verify a rename whose
    // completion commit was interrupted, even when user checksums are optional.
    let verified = if ctx.settings.verify_checksums {
        cell.record.read().expected_hash.clone()
    } else {
        None
    };
    let content = match verified {
        Some(expected) => expected,
        None => {
            set_state(ctx, cell, TaskState::Verifying);
            let digest = hash::hash_file(
                &plan.part_path,
                crate::hash::HashAlgo::Blake3,
                Some(cell.verify_progress.clone()),
                Some(token.clone()),
            )
            .await;
            cell.verify_progress.store(0, Ordering::Relaxed);
            crate::hash::ExpectedHash::new(crate::hash::HashAlgo::Blake3, digest?)?
        }
    };
    if token.is_cancelled() {
        return Ok(Outcome::Stopped);
    }
    let final_path = ctx.state.finalize_file(
        cell,
        ctx.generation,
        &plan.part_path,
        &plan.final_path,
        content,
        ctx.settings.conflict_policy,
    )?;
    completed(ctx, cell, &plan.part_path, &final_path)
}

fn completed(
    ctx: &Arc<RunCtx>,
    cell: &Arc<TaskCell>,
    part: &std::path::Path,
    final_path: &std::path::Path,
) -> Result<Outcome> {
    // Finalize (or recovery) has returned after its durable completion save.
    // The write handle follows the rename on Windows and must be closed before
    // publishing completion so consumers can open/execute the finished file.
    cell.checkpoint_file.lock().take();
    if let Some(total) = cell.record.read().total_size {
        cell.received.store(total, Ordering::Relaxed);
    }
    ctx.state.release_partial(cell.id, part);
    ctx.state.emit(Event::TaskStateChanged {
        id: cell.id,
        state: TaskState::Completed,
    });
    ctx.state.emit(Event::TaskCompleted {
        id: cell.id,
        path: util::display_path(&final_path),
    });
    ctx.state.emit(Event::QueueChanged);
    ctx.state.log(
        LogLevel::Info,
        format!(
            "#{} completed: {}",
            cell.id,
            util::display_path(&final_path)
        ),
    );
    Ok(Outcome::Finished)
}

// ---------------------------------------------------------------------------
// terminal helpers
// ---------------------------------------------------------------------------

fn set_state(ctx: &Arc<RunCtx>, cell: &Arc<TaskCell>, state: TaskState) {
    let previous = {
        let mut record = cell.record.write();
        let previous = record.state;
        record.state = state;
        if state == TaskState::Downloading && record.started_at.is_none() {
            record.started_at = Some(util::now_millis());
        }
        previous
    };
    if previous != state {
        cell.dirty.store(true, Ordering::Relaxed);
        ctx.state
            .emit(Event::TaskStateChanged { id: cell.id, state });
        let durable = state.is_terminal() || state == TaskState::Downloading;
        if let Err(e) = ctx.state.persist_owned(cell, durable, Some(ctx.generation)) {
            ctx.state.log(
                LogLevel::Error,
                format!("#{} cannot persist state: {e}", cell.id),
            );
        }
    }
}

/// What a draining run is trying to publish. A retry that will sleep is not
/// one of these: that path returns without calling [`publish_exit`].
#[derive(Debug)]
pub(super) enum ExitKind<'a> {
    /// Pause, shutdown, or a clean stop. No error is stored.
    Clean,
    /// Representation budget, unsupported coding, a non-restart 416, any
    /// error already classified fatal, or a retryable error whose attempt
    /// budget is exhausted. Slice 4a has the last two; the first three arrive
    /// with slice 4b and publish through this same arm.
    Definitive(&'a Error),
    /// A retryable error dropped because this run lost the task before the
    /// next attempt. Not stored as `Failed`.
    Discarded(&'a Error),
}

/// A draining run may open another request only while it still owns the task.
///
/// Callers read in this order: the cancel token, `TaskCell::fence`, then the
/// record. The booleans are the results of those reads.
pub(super) fn owns_attempt(token_cancelled: bool, generation_ok: bool, state: TaskState) -> bool {
    if token_cancelled {
        return false;
    }
    if !generation_ok {
        return false;
    }
    matches!(
        state,
        TaskState::Probing | TaskState::Downloading | TaskState::Verifying
    )
}

fn attempt_relinquished(ctx: &RunCtx, cell: &TaskCell, token: &CancellationToken) -> bool {
    let token_cancelled = token.is_cancelled();
    let generation_ok = cell.fence(ctx.generation).is_ok();
    let state = cell.record.read().state;
    !owns_attempt(token_cancelled, generation_ok, state)
}

fn stopped(ctx: &Arc<RunCtx>, cell: &Arc<TaskCell>) {
    publish_exit(ctx, cell, ExitKind::Clean);
}

fn fail(ctx: &Arc<RunCtx>, cell: &Arc<TaskCell>, error: &Error) {
    publish_exit(ctx, cell, ExitKind::Definitive(error));
}

/// Publish the outcome of a current-generation run, under the record lock.
///
/// Order (amended PR 4 spec, section 5; Cino: `Failed` is sticky):
/// 1. A stale generation publishes nothing.
/// 2. `Queued`, `Completed`, and `Canceled` stay as they are.
/// 3. `Failed` stays as it is.
/// 4. `CANCEL` or `REMOVE` publishes `Canceled`.
/// 5. A definitive error publishes `Failed` and keeps `record.error`.
/// 6. `PAUSE`, `SHUTDOWN`, or a clean stop publishes `Paused`.
///
/// Success does not come through here. `completed()` publishes it, and
/// `finalize_file` already refuses a stale generation.
pub(super) fn publish_exit(ctx: &Arc<RunCtx>, cell: &Arc<TaskCell>, kind: ExitKind<'_>) {
    let reason = cell.stop_reason.load(Ordering::SeqCst);
    let definitive = match &kind {
        ExitKind::Definitive(error) => Some(error.short()),
        ExitKind::Clean | ExitKind::Discarded(_) => None,
    };
    let discarded = match &kind {
        ExitKind::Definitive(error) | ExitKind::Discarded(error) => Some(error.short()),
        ExitKind::Clean => None,
    };
    let mut record = cell.record.write();
    // 1.
    if cell.generation.load(Ordering::Acquire) != ctx.generation {
        let current = cell.generation.load(Ordering::Acquire);
        drop(record);
        tracing::warn!(
            "#{}: superseded task run (generation {}, current {current}); exiting without publishing",
            cell.id,
            ctx.generation
        );
        return;
    }
    let prior = record.state;
    // 2 and 3. The draining run does not own these.
    if matches!(
        prior,
        TaskState::Queued | TaskState::Completed | TaskState::Canceled | TaskState::Failed
    ) {
        drop(record);
        if prior == TaskState::Queued {
            if let Some(text) = discarded {
                ctx.state.log(
                    LogLevel::Warn,
                    format!("#{} discarded error while state is queued: {text}", cell.id),
                );
            }
        }
        announce_exit(ctx, cell, prior, None);
        return;
    }
    // 4, then 5, then 6.
    // G2: cancelling a task must NOT clear its routing metadata (Zed G2
    // review, P2). `Canceled` is resumable in this engine, and clearing
    // the snapshot here would silently turn a resumed task into a legacy
    // task routed by whatever the record's provisional fields happen to
    // hold. The route metadata — pending snapshot or durable lock — is
    // part of the task and survives cancel exactly like its destination.
    // Paused likewise keeps it (it resumes into the lock, per the
    // contract's paused-enqueue case).
    let failed = if reason == stop_reason::CANCEL || reason == stop_reason::REMOVE {
        record.state = TaskState::Canceled;
        None
    } else if let Some(text) = definitive {
        record.state = TaskState::Failed;
        record.error = Some(text.clone());
        record.finished_at = Some(util::now_millis());
        Some(text)
    } else {
        record.state = TaskState::Paused;
        None
    };
    let final_state = record.state;
    drop(record);
    announce_exit(ctx, cell, final_state, failed);
}

fn announce_exit(
    ctx: &Arc<RunCtx>,
    cell: &Arc<TaskCell>,
    state: TaskState,
    failed: Option<String>,
) {
    cell.speed.write().reset();
    cell.dirty.store(true, Ordering::Relaxed);
    if let Err(e) = ctx.state.persist_owned(cell, true, Some(ctx.generation)) {
        let what = if failed.is_some() { "failure" } else { "state" };
        ctx.state.log(
            LogLevel::Error,
            format!("#{} cannot persist {what}: {e}", cell.id),
        );
    }
    ctx.state
        .emit(Event::TaskStateChanged { id: cell.id, state });
    if let Some(error) = failed {
        ctx.state.emit(Event::TaskFailed {
            id: cell.id,
            error: error.clone(),
        });
        ctx.state.log(
            LogLevel::Error,
            format!("#{} {}: {error}", cell.id, filename_of(cell)),
        );
    } else {
        let resumed_from = util::human_bytes(cell.received.load(Ordering::Relaxed) as f64);
        ctx.state.log(
            LogLevel::Info,
            format!(
                "#{} {} ({resumed_from} kept for resume)",
                cell.id,
                state.label()
            ),
        );
    }
    ctx.state.emit(Event::QueueChanged);
}

/// Throw away partial progress. The cleared bound, validator, segments, and
/// size are one mutation and are durable before the partial file is deleted.
/// `spend_restart` records the one task-level representation restart.
async fn reset_progress(
    ctx: &Arc<RunCtx>,
    cell: &Arc<TaskCell>,
    spend_restart: bool,
) -> Result<()> {
    // G2: a task whose route is not durably locked owns no partial; never
    // touch its provisional path (Zed G2 review, P2). In-memory progress
    // still resets so a fallback restart begins clean.
    let route_unlocked = cell.record.read().route_unlocked();
    let part = cell.record.read().part_path();
    {
        let mut record = cell.record.write();
        let mut segments = cell.segments.write();
        // A superseded run must not reset, or delete, the current run's data.
        if let Err(stale) = cell.fence(ctx.generation) {
            tracing::warn!("#{}: {stale}; progress reset refused", cell.id);
            return Err(stale);
        }
        record.representation_bound = None;
        record.etag = None;
        record.last_modified = None;
        record.total_size = None;
        record.accept_ranges = false;
        record.segments_planned = 1;
        if spend_restart {
            record.representation_restarts = record.representation_restarts.saturating_add(1);
        }
        *segments = Vec::new();
        cell.received.store(0, Ordering::Relaxed);
        cell.speed.write().reset();
        cell.bump_persist_epoch();
    }
    ctx.state.persist_owned(cell, true, Some(ctx.generation))?;
    if route_unlocked || !ctx.state.owns_partial(cell.id, &part) {
        return Ok(());
    }
    if let Err(e) = tokio::fs::remove_file(&part).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            ctx.state.log(
                LogLevel::Warn,
                format!(
                    "#{} cannot delete {}: {e}",
                    cell.id,
                    util::display_path(&part)
                ),
            );
        }
    }
    Ok(())
}

fn filename_of(cell: &Arc<TaskCell>) -> String {
    let record = cell.record.read();
    if record.filename.trim().is_empty() {
        record.url.clone()
    } else {
        record.filename.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{AddRequest, EngineBuilder};
    use crate::paths::Paths;

    /// Usagi PR 3: segment coverage from a superseded run is refused, and a
    /// stale run cannot reset (or delete) the current run's progress.
    #[tokio::test(flavor = "multi_thread")]
    async fn superseded_generation_cannot_cover_or_reset() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let engine = EngineBuilder::new()
            .paths(Paths {
                root: root.into(),
                db_file: root.join("test.redb"),
                log_file: root.join("logs/test.log"),
                temp_dir: root.join("tmp"),
                default_download_dir: root.join("downloads"),
                portable: true,
                exe_dir: Some(root.into()),
            })
            .idle(true)
            .build()
            .unwrap();
        let id = engine
            .add(AddRequest::new("http://127.0.0.1:9/fenced.bin").start_paused(true))
            .unwrap();
        let cell = engine.state.cell(id).unwrap();
        *cell.segments.write() = vec![SegmentState::new(0, 0, 9), SegmentState::new(1, 10, 19)];
        cell.segments.write()[1].done = 4;
        cell.generation.store(3, Ordering::Release);
        let stale = RunCtx {
            state: engine.state.clone(),
            settings: engine.settings(),
            generation: 2,
        };
        let current = RunCtx {
            state: engine.state.clone(),
            settings: engine.settings(),
            generation: 3,
        };

        assert!(matches!(
            mark_finished(&stale, &cell, 0),
            Err(Error::StaleGeneration {
                stale: 2,
                current: 3
            })
        ));
        assert!(
            !cell.segments.read()[0].finished,
            "stale run published coverage"
        );
        let stale = Arc::new(stale);
        let refused = reset_progress(&stale, &cell, false).await;
        assert!(
            matches!(
                refused,
                Err(Error::StaleGeneration {
                    stale: 2,
                    current: 3
                })
            ),
            "stale reset must be refused, got {refused:?}"
        );
        assert_eq!(
            cell.segments.read().len(),
            2,
            "stale run reset current progress"
        );
        assert_eq!(cell.segments.read()[1].done, 4);

        mark_finished(&current, &cell, 0).unwrap();
        assert!(cell.segments.read()[0].finished);
        engine.shutdown();
    }

    /// A resume that lands while the revoked run is still draining must
    /// survive that run's own stop publication (garen-pc 2/20 race).
    #[tokio::test(flavor = "multi_thread")]
    async fn resume_during_drain_is_not_overwritten_by_stop() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let engine = EngineBuilder::new()
            .paths(Paths {
                root: root.into(),
                db_file: root.join("test.redb"),
                log_file: root.join("logs/test.log"),
                temp_dir: root.join("tmp"),
                default_download_dir: root.join("downloads"),
                portable: true,
                exe_dir: Some(root.into()),
            })
            .idle(true)
            .build()
            .unwrap();
        let id = engine
            .add(AddRequest::new("http://127.0.0.1:9/drain.bin").start_paused(true))
            .unwrap();
        let cell = engine.state.cell(id).unwrap();
        let ctx = Arc::new(RunCtx {
            state: engine.state.clone(),
            settings: engine.settings(),
            generation: 0,
        });
        // The revoked run is still draining, so no new run may be admitted.
        cell.running.store(true, Ordering::SeqCst);
        for reason in [stop_reason::PAUSE, stop_reason::CANCEL] {
            // Pause/cancel revoked the run; resume_task then queued the task
            // and reset the reason, all before the run drained.
            cell.stop_reason.store(reason, Ordering::SeqCst);
            cell.record.write().state = TaskState::Queued;
            cell.stop_reason.store(stop_reason::NONE, Ordering::SeqCst);
            stopped(&ctx, &cell);
            assert_eq!(
                cell.state(),
                TaskState::Queued,
                "resume lost (reason {reason})"
            );
        }
        // Without a resume, the stop is still published.
        cell.record.write().state = TaskState::Downloading;
        cell.stop_reason.store(stop_reason::PAUSE, Ordering::SeqCst);
        stopped(&ctx, &cell);
        assert_eq!(cell.state(), TaskState::Paused);
        // Cino: Failed is sticky. A pause publication must not hide it, and
        // fail() must not take a task that a resume already re-queued.
        {
            let mut record = cell.record.write();
            record.state = TaskState::Failed;
            record.error = Some("kept".into());
        }
        cell.stop_reason.store(stop_reason::PAUSE, Ordering::SeqCst);
        stopped(&ctx, &cell);
        assert_eq!(cell.state(), TaskState::Failed);
        assert_eq!(cell.record.read().error.as_deref(), Some("kept"));
        cell.record.write().state = TaskState::Queued;
        cell.record.write().error = None;
        fail(&ctx, &cell, &Error::Other("dropped".into()));
        assert_eq!(cell.state(), TaskState::Queued);
        assert!(cell.record.read().error.is_none());
        cell.running.store(false, Ordering::SeqCst);
        engine.shutdown();
    }
}

#[cfg(test)]
#[path = "representation_tests.rs"]
mod representation_tests;
