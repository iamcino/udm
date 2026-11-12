//! U4 held staging, rename swap, and rollback journal (U2 design REV1
//! sections 2.3, 2.5, and 4).
//!
//! A verified payload (the unit U3 handoff, [`VerifiedPayload`]) is copied
//! into a held staging area inside the install directory. Nothing in the
//! live install changes at staging time. The swap runs only when the
//! caller explicitly applies the staged candidate: there is no
//! auto-install, no silent install, and no forced update. An unverified
//! payload never reaches staging: [`stage`] accepts only the
//! [`VerifiedPayload`] type, whose files unit U3 already checked against
//! the signed manifest, and staging re-verifies every file's size and
//! SHA-256 before and after the copy.
//!
//! The swap uses rename, never overwrite-in-place. Each replaced live
//! file is first renamed to a `.old` sibling (the one retained previous
//! build), then the staged file is renamed into place. There is no
//! directory-level exchange and no single instant at which all files
//! switch; a crash mid-sequence leaves a detectable mix of whole old and
//! whole new files, never a partially written file.
//!
//! The rollback journal is one swap transaction in a dedicated table
//! (`updater_swap.v1`) in the install's `udm.redb`, the same store the
//! PR 2 finalize journal pattern uses: a durable intent record committed
//! before the operation, native renames, a completion commit, and
//! startup recovery reconciliation. The updater opens that file
//! standalone; [`recover_updater`] runs at every startup before the UI
//! launches and before the engine opens its store. The journal records
//! the new build version, the per-file SHA-256 hashes, and the target
//! paths. It does not record an epoch, a previous-build location, or a
//! what-was-where pair.
//!
//! Phase order: `Step::Prepared` (committed durably before any rename),
//! `Step::OldRenamed` (the running exe is renamed to `.old`, the new exe
//! not yet in place), `Step::NewStaged` (all new files in place),
//! `Step::Completed` (the new build has started successfully at least
//! once: it launched, reported its version, and reached ready state).
//! Only at `Step::Completed` are the `.old` files reclaimed, so exactly
//! one previous build is retained, and never two.
//!
//! If renaming the running exe is refused (for example a file lock held
//! by antivirus), [`apply_staged`] returns
//! [`ApplyError::ExeRenameRefused`] with the journal still at
//! `Step::Prepared`; the caller may then spawn the unprivileged helper
//! `udm-updater-helper.exe` via [`spawn_helper`], exit, and let the
//! helper perform the same rename sequence. The helper runs per-user,
//! with no admin rights, no service, and no scheduled task. Staging and
//! rollback are per-user only: no HKLM, no admin, no services, no
//! scheduled tasks. The swap protocol is identical for the per-user
//! installed layout and the portable layout; only the base directory
//! differs.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use redb::{Database, Durability, ReadableDatabase, TableDefinition};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::update_fetch::VerifiedPayload;

/// Journal table in the install's `udm.redb`: one swap transaction.
const SWAP_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("updater_swap.v1");
/// Key of the single in-flight swap transaction.
const JOURNAL_KEY: &str = "current";
/// Metadata file inside the staging area describing the held candidate.
const STAGED_META: &str = "staged.json";
/// The main executable's manifest-relative path.
const EXE_PATH: &str = "udm.exe";
/// The CLI executable's manifest-relative path.
const CLI_PATH: &str = "udm-cli.exe";
/// The trampoline helper shipped in the install directory.
const HELPER_NAME: &str = "udm-updater-helper.exe";
/// User notice for a rollback, worded after the design section 2.1
/// state "Update applied, new build failed to start, rolled back".
pub const ROLLED_BACK_NOTICE: &str = "Update applied, new build failed to start, rolled back";

/// One phase of the swap transaction, in write order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Step {
    /// Intent recorded durably before any rename.
    Prepared,
    /// The running exe has been renamed to `.old`; the new exe is not
    /// yet in place.
    OldRenamed,
    /// All new files are in place; the first successful start has not
    /// yet been recorded.
    NewStaged,
    /// The new build started successfully at least once.
    Completed,
}

/// One file of a staged candidate or swap transaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Manifest-relative path, slash-separated.
    pub path: String,
    /// SHA-256 of the file contents, lowercase hex.
    pub sha256: String,
    /// Size in bytes.
    pub size: u64,
}

/// One file of a swap transaction: the staged entry plus its live
/// target path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalFile {
    /// Manifest-relative path, slash-separated.
    pub path: String,
    /// SHA-256 of the new file contents, lowercase hex.
    pub sha256: String,
    /// Size in bytes.
    pub size: u64,
    /// Absolute target path in the live install.
    pub target: String,
}

/// The one swap transaction held in the journal table.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapJournal {
    /// Record format version.
    pub version: u32,
    /// Version of the new build being applied.
    pub new_version: String,
    /// Current phase.
    pub step: Step,
    /// Every file of the swap, with its expected SHA-256 and target.
    pub files: Vec<JournalFile>,
}

/// A candidate held in the staging area, awaiting an explicit apply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedCandidate {
    /// Version of the staged build.
    pub version: String,
    /// Files of the staged build, as verified at staging time.
    pub files: Vec<FileEntry>,
}

/// One file to stage: a verified source file on disk plus the hash and
/// size it must match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageInput {
    /// Manifest-relative path, slash-separated.
    pub path: String,
    /// Absolute path of the verified source file.
    pub source: PathBuf,
    /// Expected SHA-256 of the contents, lowercase hex.
    pub sha256: String,
    /// Expected size in bytes.
    pub size: u64,
}

/// Failures of staging, swap, and recovery.
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// A filesystem operation failed.
    #[error("io error at {path}: {source}")]
    Io {
        /// Path the operation acted on.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// The journal store could not be read or written.
    #[error("updater journal error: {0}")]
    Journal(String),
    /// No candidate is held in the staging area.
    #[error("no staged update is held")]
    NotStaged,
    /// A candidate is already staged; staging holds at most one.
    #[error("an update is already staged")]
    AlreadyStaged,
    /// A swap transaction is already in the journal.
    #[error("a swap is already in progress")]
    SwapInFlight,
    /// The file set does not match what the operation requires.
    #[error("file set mismatch: {0}")]
    SetMismatch(String),
    /// A file's contents do not match its recorded SHA-256.
    #[error("sha256 mismatch for {path}: expected {expected}, found {actual}")]
    HashMismatch {
        /// Manifest-relative path of the file.
        path: String,
        /// Recorded SHA-256.
        expected: String,
        /// SHA-256 of the bytes on disk.
        actual: String,
    },
    /// A file's size does not match its recorded size.
    #[error("size mismatch for {path}: expected {expected}, found {actual}")]
    SizeMismatch {
        /// Manifest-relative path of the file.
        path: String,
        /// Recorded size.
        expected: u64,
        /// Size of the file on disk.
        actual: u64,
    },
    /// A manifest-relative path is not a safe relative path.
    #[error("unsafe payload path: {0}")]
    BadPath(String),
    /// Renaming the running exe was refused; the trampoline helper is
    /// the fallback. The journal is still at `Step::Prepared` and no
    /// live file has been replaced.
    #[error("renaming the running executable was refused at {path}: {source}")]
    ExeRenameRefused {
        /// The live exe path.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// The trampoline helper is not present in the install directory.
    #[error("updater helper not found at {0}")]
    HelperMissing(PathBuf),
    /// A recovery or confirmation call met a journal in a step it does
    /// not handle.
    #[error("unexpected journal step {0:?}")]
    UnexpectedStep(Step),
    /// A leftover `.old` file exists with no journal covering it; run
    /// [`recover_updater`] before applying another update.
    #[error("stale .old file at {0} with no swap journal")]
    StaleOldFile(PathBuf),
    /// Test-only fault injection at a named cut point.
    #[error("injected fault")]
    FaultInjected,
}

/// The install directory layout the updater works on. The per-user
/// installed mode and the portable mode differ only in this base
/// directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallLayout {
    base: PathBuf,
}

impl InstallLayout {
    /// A layout rooted at `base` (for example
    /// `%LOCALAPPDATA%\Programs\UDM`, or the portable executable
    /// directory).
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self { base: base.into() }
    }

    /// The install root.
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// The held staging area.
    pub fn staging_dir(&self) -> PathBuf {
        self.base.join("staging")
    }

    /// The retained-previous-build directory named by the layout. The
    /// swap protocol retains the previous build as `.old` siblings in
    /// place (design section 2.3); this directory is part of the
    /// packaging layout (section 2.5) and is not written by the swap.
    pub fn previous_dir(&self) -> PathBuf {
        self.base.join("previous")
    }

    /// The install's `udm.redb`, holding the journal table.
    pub fn db_file(&self) -> PathBuf {
        self.base.join("udm.redb")
    }

    /// Local log every startup reconciliation appends to.
    pub fn recovery_log(&self) -> PathBuf {
        self.base.join("updater-recovery.log")
    }

    /// Live target path for a manifest-relative path.
    pub fn target_for(&self, rel: &str) -> PathBuf {
        join_rel(&self.base, rel)
    }

    /// Staged path for a manifest-relative path.
    pub fn staged_file(&self, rel: &str) -> PathBuf {
        join_rel(&self.staging_dir(), rel)
    }

    /// The trampoline helper's path.
    pub fn helper_path(&self) -> PathBuf {
        self.base.join(HELPER_NAME)
    }
}

/// Join a validated slash-separated relative path onto a base.
fn join_rel(base: &Path, rel: &str) -> PathBuf {
    let mut out = base.to_path_buf();
    for part in rel.split('/') {
        if !part.is_empty() {
            out.push(part);
        }
    }
    out
}

/// Refuse absolute paths, `..`, and empty payload paths.
fn check_rel_path(rel: &str) -> Result<(), ApplyError> {
    if rel.is_empty() || rel.starts_with('/') || rel.contains('\\') {
        return Err(ApplyError::BadPath(rel.to_owned()));
    }
    let path = Path::new(rel);
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            _ => return Err(ApplyError::BadPath(rel.to_owned())),
        }
    }
    Ok(())
}

/// The `.old` sibling retaining the previous build's copy of a file.
fn old_sibling(target: &Path) -> PathBuf {
    let mut name = target.as_os_str().to_owned();
    name.push(".old");
    PathBuf::from(name)
}

fn io(path: &Path, source: std::io::Error) -> ApplyError {
    ApplyError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// SHA-256 of a file's contents, lowercase hex, streamed.
fn sha256_file(path: &Path) -> Result<String, ApplyError> {
    let mut file = File::open(path).map_err(|e| io(path, e))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|e| io(path, e))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Check a file on disk against a recorded size and SHA-256.
fn verify_file(path: &Path, entry: &FileEntry) -> Result<(), ApplyError> {
    let actual_size = fs::metadata(path).map_err(|e| io(path, e))?.len();
    if actual_size != entry.size {
        return Err(ApplyError::SizeMismatch {
            path: entry.path.clone(),
            expected: entry.size,
            actual: actual_size,
        });
    }
    let actual = sha256_file(path)?;
    if actual != entry.sha256 {
        return Err(ApplyError::HashMismatch {
            path: entry.path.clone(),
            expected: entry.sha256.clone(),
            actual,
        });
    }
    Ok(())
}

fn sync_file(path: &Path) -> Result<(), ApplyError> {
    // `File::sync_all` on Windows is `FlushFileBuffers`, which requires
    // a handle opened with `GENERIC_WRITE`. `File::open` is read-only,
    // so this call returns `ERROR_ACCESS_DENIED` (os error 5) on every
    // attempt, for any extension. That denial is deterministic: it is
    // not a sharing violation, and it is not a scanner hold a retry
    // can outwait. A read-write open flushes immediately. Unix `fsync`
    // accepts a read-only fd, which is why the same call passes on
    // Linux. This matches `finalize::sync_namespace`, which also opens
    // read-write before `sync_all`.
    File::options()
        .read(true)
        .write(true)
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(|e| io(path, e))
}

/// Directory fsync where the platform supports it. Windows has no
/// directory fsync equivalent in `std` (the same note the PR 2 finalize
/// journal carries); the journal commit and file syncs carry the
/// durability there.
fn sync_dir(path: &Path) {
    #[cfg(unix)]
    {
        if let Ok(dir) = File::open(path) {
            let _ = dir.sync_all();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

// ---------------------------------------------------------------------
// Journal store: one swap transaction in a dedicated redb table.
// ---------------------------------------------------------------------

fn journal_error<E: std::fmt::Display>(error: E) -> ApplyError {
    ApplyError::Journal(error.to_string())
}

/// The journal currently recorded, if any. A missing database file
/// means no swap has ever been journaled here.
pub fn current_journal(layout: &InstallLayout) -> Result<Option<SwapJournal>, ApplyError> {
    let db_path = layout.db_file();
    if !db_path.exists() {
        return Ok(None);
    }
    let db = Database::create(&db_path).map_err(journal_error)?;
    let txn = db.begin_read().map_err(journal_error)?;
    let table = match txn.open_table(SWAP_TABLE) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(error) => return Err(journal_error(error)),
    };
    let Some(entry) = table.get(JOURNAL_KEY).map_err(journal_error)? else {
        return Ok(None);
    };
    let journal: SwapJournal = serde_json::from_slice(entry.value()).map_err(journal_error)?;
    Ok(Some(journal))
}

/// Commit a journal record durably, replacing any previous phase of
/// the same transaction.
fn journal_write(layout: &InstallLayout, journal: &SwapJournal) -> Result<(), ApplyError> {
    if let Some(parent) = layout.db_file().parent() {
        fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
    }
    let db = Database::create(layout.db_file()).map_err(journal_error)?;
    let mut txn = db.begin_write().map_err(journal_error)?;
    txn.set_durability(Durability::Immediate)
        .map_err(journal_error)?;
    {
        let mut table = txn.open_table(SWAP_TABLE).map_err(journal_error)?;
        let bytes = serde_json::to_vec(journal).map_err(journal_error)?;
        table
            .insert(JOURNAL_KEY, bytes.as_slice())
            .map_err(journal_error)?;
    }
    txn.commit().map_err(journal_error)?;
    Ok(())
}

/// Remove the swap transaction from the journal.
fn journal_clear(layout: &InstallLayout) -> Result<(), ApplyError> {
    if !layout.db_file().exists() {
        return Ok(());
    }
    let db = Database::create(layout.db_file()).map_err(journal_error)?;
    let mut txn = db.begin_write().map_err(journal_error)?;
    txn.set_durability(Durability::Immediate)
        .map_err(journal_error)?;
    {
        let mut table = txn.open_table(SWAP_TABLE).map_err(journal_error)?;
        table.remove(JOURNAL_KEY).map_err(journal_error)?;
    }
    txn.commit().map_err(journal_error)?;
    Ok(())
}

// ---------------------------------------------------------------------
// Held staging.
// ---------------------------------------------------------------------

/// Stage a verified payload: copy its files into the held staging
/// area, re-verifying every file against the signed manifest's records
/// before and after the copy. The live install is not touched. Refuses
/// when a swap is in flight, when a candidate is already staged, or
/// when the payload's file set differs from the manifest's.
pub fn stage(
    layout: &InstallLayout,
    payload: &VerifiedPayload,
) -> Result<StagedCandidate, ApplyError> {
    let manifest = payload.manifest.manifest();
    let mut manifest_paths: Vec<&str> = manifest.files.iter().map(|f| f.path.as_str()).collect();
    let mut payload_paths: Vec<&str> = payload.files.iter().map(|f| f.path.as_str()).collect();
    manifest_paths.sort_unstable();
    payload_paths.sort_unstable();
    if manifest_paths != payload_paths {
        return Err(ApplyError::SetMismatch(
            "payload files do not match the manifest file set".to_owned(),
        ));
    }
    for file in &payload.files {
        let entry = manifest
            .files
            .iter()
            .find(|f| f.path == file.path)
            .expect("file sets checked equal above");
        if entry.sha256 != file.sha256 || entry.size != file.size {
            return Err(ApplyError::SetMismatch(format!(
                "payload record for {} differs from the manifest",
                file.path
            )));
        }
    }
    let inputs: Vec<StageInput> = payload
        .files
        .iter()
        .map(|file| StageInput {
            path: file.path.clone(),
            source: file.absolute.clone(),
            sha256: file.sha256.clone(),
            size: file.size,
        })
        .collect();
    stage_files(layout, &manifest.version, &inputs)
}

/// Attempts for the copy half of one file's staging step when a
/// transient `PermissionDenied` hits it: a real-time scanner on
/// Windows can hold a freshly written executable without share-read,
/// so the opens that follow the write can fail until the scanner
/// releases the file. The v1 window (5 attempts, about 185 ms) did
/// not survive the hold on garen-pc: the same 13 update_apply tests
/// still failed there with the denial naming the staged destination.
const STAGE_COPY_ATTEMPTS: u32 = 8;
/// Backoff between copy attempts, in milliseconds. Bounded: about
/// 1.6 s worst case per file. Only a denied file burns budget; on
/// Linux the first attempt succeeds, so Linux gates pay nothing.
const STAGE_COPY_BACKOFF_MS: [u64; 7] = [10, 25, 50, 100, 200, 400, 800];

/// Attempts for the finish half: the sync and the staged-copy
/// verification. A transient sharing denial can still happen while a
/// scanner has the new file open. This half never deletes or rewrites:
/// the staged bytes are already on disk, so it re-opens the same file.
/// The garen-pc failure was not that hold. `sync_file` opened the
/// staged file read-only, and Windows `FlushFileBuffers` rejects that
/// handle with `ERROR_ACCESS_DENIED` on every attempt, so this window
/// ran out deterministically. The sync open is read-write now.
const STAGE_FINISH_ATTEMPTS: u32 = 8;
/// Backoff between finish attempts, in milliseconds. Bounded: about
/// 3.2 s worst case per file.
const STAGE_FINISH_BACKOFF_MS: [u64; 7] = [25, 50, 100, 200, 400, 800, 1600];

/// Whether an error is a transient `PermissionDenied`: the only
/// staging failure retried. Anything else fails at once, and a denial
/// that survives every attempt fails with its original error.
fn is_transient_denied(error: &ApplyError) -> bool {
    matches!(
        error,
        ApplyError::Io { source, .. }
            if source.kind() == std::io::ErrorKind::PermissionDenied
    )
}

/// Stage one file in two halves, each retried on a transient
/// `PermissionDenied`. The copy half verifies the source and copies
/// it; the finish half syncs the staged copy and verifies it. The
/// split is the fix for the v1 failure mode: v1 retried the whole
/// step, deleting the partial destination and rewriting it on every
/// attempt, so each attempt re-armed a fresh scanner hold and the
/// verify kept colliding with it -- and while the scanner still held
/// the file the delete and the re-create failed at once, burning all
/// five attempts in about 185 ms without ever waiting the hold out.
/// The finish half waits instead of rewriting: the staged bytes are
/// already on disk, so it re-opens the same file until the hold
/// clears. A hash or size mismatch still fails at once, as it must:
/// only `PermissionDenied` is retried.
fn stage_one_file(
    source: &Path,
    destination: &Path,
    expected: &FileEntry,
) -> Result<(), ApplyError> {
    stage_copy(source, destination, expected)?;
    stage_finish(destination, expected)
}

/// Copy half: verify the source, then copy it to the destination.
/// Retried as a whole on a transient denial; a failed attempt may
/// have left a partial destination behind, so it is cleared before
/// retrying, where a rewrite is the point.
fn stage_copy(source: &Path, destination: &Path, expected: &FileEntry) -> Result<(), ApplyError> {
    let mut attempt = 0u32;
    loop {
        match stage_copy_once(source, destination, expected) {
            Ok(()) => return Ok(()),
            Err(error) => {
                if is_transient_denied(&error) && attempt + 1 < STAGE_COPY_ATTEMPTS {
                    let _ = fs::remove_file(destination);
                    std::thread::sleep(std::time::Duration::from_millis(
                        STAGE_COPY_BACKOFF_MS[attempt as usize],
                    ));
                    attempt += 1;
                    continue;
                }
                return Err(error);
            }
        }
    }
}

fn stage_copy_once(
    source: &Path,
    destination: &Path,
    expected: &FileEntry,
) -> Result<(), ApplyError> {
    verify_file(source, expected)?;
    copy_file_split(source, destination)
}

/// Finish half: sync the staged copy and verify it. Retried as a
/// whole on a transient denial, with NO delete and NO rewrite: the
/// staged bytes are already on disk, so this half re-opens the same
/// file. The reopen is read-write; a read-only open cannot flush on
/// Windows.
fn stage_finish(destination: &Path, expected: &FileEntry) -> Result<(), ApplyError> {
    let mut attempt = 0u32;
    loop {
        match stage_finish_once(destination, expected) {
            Ok(()) => return Ok(()),
            Err(error) => {
                if is_transient_denied(&error) && attempt + 1 < STAGE_FINISH_ATTEMPTS {
                    std::thread::sleep(std::time::Duration::from_millis(
                        STAGE_FINISH_BACKOFF_MS[attempt as usize],
                    ));
                    attempt += 1;
                    continue;
                }
                return Err(error);
            }
        }
    }
}

fn stage_finish_once(destination: &Path, expected: &FileEntry) -> Result<(), ApplyError> {
    sync_file(destination)?;
    verify_file(destination, expected)
}

/// Copy one file with split error attribution: the source open names
/// the source and the destination create names the destination.
/// `fs::copy` labels every failure with the destination, even a
/// source open failure, so a scanner hold on the source was
/// indistinguishable from a destination failure. Permissions are
/// carried over from the source, as `fs::copy` does.
fn copy_file_split(source: &Path, destination: &Path) -> Result<(), ApplyError> {
    let mut reader = File::open(source).map_err(|e| io(source, e))?;
    let mut writer = File::create(destination).map_err(|e| io(destination, e))?;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer).map_err(|e| io(source, e))?;
        if read == 0 {
            break;
        }
        writer
            .write_all(&buffer[..read])
            .map_err(|e| io(destination, e))?;
    }
    let permissions = reader.metadata().map_err(|e| io(source, e))?.permissions();
    drop(writer);
    fs::set_permissions(destination, permissions).map_err(|e| io(destination, e))?;
    Ok(())
}

/// Stage verified files held on disk. This is the staging core;
/// [`stage`] adapts the unit U3 handoff to it. An unverified payload
/// never reaches staging: every source file must match the size and
/// SHA-256 its caller recorded for it, and the staged copy is verified
/// again after the copy.
pub fn stage_files(
    layout: &InstallLayout,
    version: &str,
    inputs: &[StageInput],
) -> Result<StagedCandidate, ApplyError> {
    if current_journal(layout)?.is_some() {
        return Err(ApplyError::SwapInFlight);
    }
    let staging = layout.staging_dir();
    if staging.join(STAGED_META).exists() {
        return Err(ApplyError::AlreadyStaged);
    }
    for required in [EXE_PATH, CLI_PATH] {
        if !inputs.iter().any(|input| input.path == required) {
            return Err(ApplyError::SetMismatch(format!(
                "candidate is missing {required}"
            )));
        }
    }
    // A staging directory without its metadata is a partial copy from
    // an interrupted stage; no journal points at it, so clear it.
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(|e| io(&staging, e))?;
    }
    fs::create_dir_all(&staging).map_err(|e| io(&staging, e))?;

    let mut entries = Vec::with_capacity(inputs.len());
    for input in inputs {
        check_rel_path(&input.path)?;
        let expected = FileEntry {
            path: input.path.clone(),
            sha256: input.sha256.clone(),
            size: input.size,
        };
        let destination = layout.staged_file(&input.path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
        }
        stage_one_file(&input.source, &destination, &expected)?;
        entries.push(expected);
    }

    let candidate = StagedCandidate {
        version: version.to_owned(),
        files: entries,
    };
    // The metadata lands last: its presence is what makes the staging
    // area a held candidate.
    let meta_tmp = staging.join(format!("{STAGED_META}.tmp"));
    let bytes = serde_json::to_vec(&candidate).map_err(journal_error)?;
    let mut file = File::create(&meta_tmp).map_err(|e| io(&meta_tmp, e))?;
    file.write_all(&bytes).map_err(|e| io(&meta_tmp, e))?;
    file.sync_all().map_err(|e| io(&meta_tmp, e))?;
    drop(file);
    fs::rename(&meta_tmp, staging.join(STAGED_META))
        .map_err(|e| io(&staging.join(STAGED_META), e))?;
    sync_dir(&staging);
    Ok(candidate)
}

/// The candidate currently held in staging, if any.
pub fn staged_candidate(layout: &InstallLayout) -> Result<Option<StagedCandidate>, ApplyError> {
    let meta = layout.staging_dir().join(STAGED_META);
    if !meta.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&meta).map_err(|e| io(&meta, e))?;
    let candidate: StagedCandidate = serde_json::from_slice(&bytes).map_err(journal_error)?;
    Ok(Some(candidate))
}

/// Discard the held candidate without applying it. Refused while a
/// swap journal exists; recovery owns the staging area then.
pub fn discard_staged(layout: &InstallLayout) -> Result<(), ApplyError> {
    if current_journal(layout)?.is_some() {
        return Err(ApplyError::SwapInFlight);
    }
    let staging = layout.staging_dir();
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(|e| io(&staging, e))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Rename swap.
// ---------------------------------------------------------------------

/// Rename `from` onto `to`, replacing an existing destination file.
/// The fallback remove-then-rename is not atomic; it is used only in
/// recovery, where the destination is a file the journal has already
/// condemned, and by the per-file moves where the destination was
/// first backed up to its `.old` sibling.
fn rename_over(from: &Path, to: &Path) -> Result<(), ApplyError> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(first) => {
            if to.is_file() {
                fs::remove_file(to).map_err(|e| io(to, e))?;
                fs::rename(from, to).map_err(|e| io(to, e))
            } else {
                Err(io(to, first))
            }
        }
    }
}

/// Move one staged file into its live place, retaining any replaced
/// live file as its `.old` sibling. Idempotent: a file already in
/// place with the journal's hash is left alone, so startup recovery
/// can complete a swap forward from any mid-sequence cut point.
fn move_one(layout: &InstallLayout, file: &JournalFile) -> Result<(), ApplyError> {
    let target = PathBuf::from(&file.target);
    let staged = layout.staged_file(&file.path);
    if target.is_file() && sha256_file(&target)? == file.sha256 {
        if staged.exists() {
            fs::remove_file(&staged).map_err(|e| io(&staged, e))?;
        }
        return Ok(());
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
    }
    if target.exists() {
        let old = old_sibling(&target);
        if old.exists() {
            // A backup already exists, so the live copy is a duplicate
            // of the old build left by an interrupted earlier move.
            fs::remove_file(&target).map_err(|e| io(&target, e))?;
        } else {
            fs::rename(&target, &old).map_err(|e| io(&target, e))?;
        }
    }
    fs::rename(&staged, &target).map_err(|e| io(&target, e))?;
    if let Some(parent) = target.parent() {
        sync_dir(parent);
    }
    Ok(())
}

/// Restore every journal file to its previous build: rename each
/// `.old` sibling back over its target, and remove files the update
/// newly added (no `.old` sibling, staged copy already moved in).
fn restore_all(layout: &InstallLayout, journal: &SwapJournal) -> Result<(), ApplyError> {
    for file in &journal.files {
        let target = PathBuf::from(&file.target);
        let old = old_sibling(&target);
        if old.exists() {
            rename_over(&old, &target)?;
        } else if !layout.staged_file(&file.path).exists() && target.exists() {
            fs::remove_file(&target).map_err(|e| io(&target, e))?;
        }
        // Otherwise the file was never moved; the old build's copy is
        // already in place.
    }
    Ok(())
}

/// Remove the retained previous build (every `.old` sibling).
fn reclaim_old(journal: &SwapJournal) -> Result<(), ApplyError> {
    for file in &journal.files {
        let old = old_sibling(Path::new(&file.target));
        if old.exists() {
            fs::remove_file(&old).map_err(|e| io(&old, e))?;
        }
    }
    Ok(())
}

/// Cut points for fault-injection tests, mirroring the design
/// section 4 kill cases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailPoint {
    AfterPrepared,
    AfterOldRenamed,
    AfterExeInPlace,
    AfterCliInPlace,
    DuringAssets { moved: usize },
    AfterNewStaged,
}

fn swap_rank(path: &str) -> u8 {
    match path {
        EXE_PATH => 0,
        CLI_PATH => 1,
        _ => 2,
    }
}

/// Apply the staged candidate. Calling this function IS the explicit
/// user action the design requires; nothing in this module calls it
/// implicitly. On success the journal stands at `Step::NewStaged`:
/// the caller exits and relaunches the new build, whose first
/// successful start is recorded through [`confirm_successful_start`].
pub fn apply_staged(layout: &InstallLayout) -> Result<SwapJournal, ApplyError> {
    apply_staged_at(layout, None)
}

pub(crate) fn apply_staged_at(
    layout: &InstallLayout,
    fail: Option<FailPoint>,
) -> Result<SwapJournal, ApplyError> {
    let candidate = staged_candidate(layout)?.ok_or(ApplyError::NotStaged)?;
    if current_journal(layout)?.is_some() {
        return Err(ApplyError::SwapInFlight);
    }
    // The staged copies must still match what staging verified.
    for entry in &candidate.files {
        verify_file(&layout.staged_file(&entry.path), entry)?;
    }
    // No stale retained files outside a journal: startup recovery
    // owns those, and it runs before any apply.
    let mut files: Vec<JournalFile> = Vec::with_capacity(candidate.files.len());
    for entry in &candidate.files {
        let target = layout.target_for(&entry.path);
        let old = old_sibling(&target);
        if old.exists() {
            return Err(ApplyError::StaleOldFile(old));
        }
        files.push(JournalFile {
            path: entry.path.clone(),
            sha256: entry.sha256.clone(),
            size: entry.size,
            target: target
                .to_str()
                .ok_or_else(|| ApplyError::BadPath(entry.path.clone()))?
                .to_owned(),
        });
    }
    files.sort_by(|a, b| {
        swap_rank(&a.path)
            .cmp(&swap_rank(&b.path))
            .then_with(|| a.path.cmp(&b.path))
    });

    // (1) The intent record is committed durably before any rename.
    let mut journal = SwapJournal {
        version: 1,
        new_version: candidate.version.clone(),
        step: Step::Prepared,
        files,
    };
    journal_write(layout, &journal)?;
    if fail == Some(FailPoint::AfterPrepared) {
        return Err(ApplyError::FaultInjected);
    }

    // (2) Rename the running exe to its `.old` sibling. A refusal
    // here is the trampoline signal: the journal stays at Prepared
    // and no live file has been replaced.
    let exe = &journal.files[0];
    debug_assert_eq!(exe.path, EXE_PATH);
    let exe_target = PathBuf::from(&exe.target);
    if exe_target.exists() {
        fs::rename(&exe_target, old_sibling(&exe_target)).map_err(|e| {
            ApplyError::ExeRenameRefused {
                path: exe_target.clone(),
                source: e,
            }
        })?;
    }

    // Step::OldRenamed: committed once the exe is renamed and before
    // the new exe is in place.
    journal.step = Step::OldRenamed;
    journal_write(layout, &journal)?;
    if fail == Some(FailPoint::AfterOldRenamed) {
        return Err(ApplyError::FaultInjected);
    }

    // (3)-(5) Move the staged files into place: exe first, then the
    // CLI by direct rename, then assets file by file.
    let mut assets_moved = 0usize;
    for file in &journal.files.clone() {
        move_one(layout, file)?;
        match file.path.as_str() {
            EXE_PATH => {
                if fail == Some(FailPoint::AfterExeInPlace) {
                    return Err(ApplyError::FaultInjected);
                }
            }
            CLI_PATH => {
                if fail == Some(FailPoint::AfterCliInPlace) {
                    return Err(ApplyError::FaultInjected);
                }
            }
            _ => {
                assets_moved += 1;
                if fail
                    == Some(FailPoint::DuringAssets {
                        moved: assets_moved,
                    })
                {
                    return Err(ApplyError::FaultInjected);
                }
            }
        }
    }

    // (6) All new files are in place.
    journal.step = Step::NewStaged;
    journal_write(layout, &journal)?;
    if fail == Some(FailPoint::AfterNewStaged) {
        return Err(ApplyError::FaultInjected);
    }

    // The candidate is consumed; its files now live in the install.
    let meta = layout.staging_dir().join(STAGED_META);
    if meta.exists() {
        fs::remove_file(&meta).map_err(|e| io(&meta, e))?;
    }
    sync_dir(&layout.staging_dir());
    Ok(journal)
}

/// Spawn the trampoline helper shipped in the install directory. The
/// helper waits for this process to exit, performs the same rename
/// sequence, relaunches the new build, and exits. It runs per-user,
/// unprivileged.
pub fn spawn_helper(layout: &InstallLayout) -> Result<std::process::Child, ApplyError> {
    let helper = layout.helper_path();
    if !helper.is_file() {
        return Err(ApplyError::HelperMissing(helper));
    }
    std::process::Command::new(&helper)
        .arg(layout.base())
        .spawn()
        .map_err(|e| io(&helper, e))
}

// ---------------------------------------------------------------------
// First-start confirmation and startup recovery.
// ---------------------------------------------------------------------

/// What [`confirm_successful_start`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfirmOutcome {
    /// No swap was awaiting a first-start receipt.
    NothingPending,
    /// The new build's first successful start is recorded; the
    /// previous build has been reclaimed.
    Confirmed {
        /// Version of the build now installed.
        version: String,
    },
}

/// Record the new build's first successful start. The caller invokes
/// this only once the new binary has launched, reported its version,
/// and reached ready state without crashing. This writes
/// `Step::Completed` durably, reclaims the retained previous build,
/// and closes the transaction. Called at `Step::NewStaged` only; a
/// journal at `Step::Completed` (a crash between the completion commit
/// and the reclaim) is finished here too.
pub fn confirm_successful_start(layout: &InstallLayout) -> Result<ConfirmOutcome, ApplyError> {
    let Some(mut journal) = current_journal(layout)? else {
        return Ok(ConfirmOutcome::NothingPending);
    };
    match journal.step {
        Step::NewStaged => {
            // The receipt is earned by the files on disk: every live
            // file must still match the journal's hashes.
            for file in &journal.files {
                let entry = FileEntry {
                    path: file.path.clone(),
                    sha256: file.sha256.clone(),
                    size: file.size,
                };
                verify_file(Path::new(&file.target), &entry)?;
            }
            journal.step = Step::Completed;
            journal_write(layout, &journal)?;
        }
        Step::Completed => {}
        step => return Err(ApplyError::UnexpectedStep(step)),
    }
    reclaim_old(&journal)?;
    journal_clear(layout)?;
    Ok(ConfirmOutcome::Confirmed {
        version: journal.new_version,
    })
}

/// What [`recover_updater`] found and did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// No swap journal: nothing to reconcile.
    NoJournal,
    /// The swap never renamed anything; the journal was discarded and
    /// the old build kept.
    DiscardedPrepared {
        /// Version of the update that was not applied.
        version: String,
    },
    /// An interrupted swap was completed forward; the new files are in
    /// place and verified, awaiting the first-successful-start
    /// receipt.
    CompletedForward {
        /// Version of the new build now in place.
        version: String,
    },
    /// The new build is in place and verified, still awaiting its
    /// first-successful-start receipt.
    AwaitingFirstStart {
        /// Version of the new build in place.
        version: String,
    },
    /// The swap was rolled back to the previous build.
    RolledBack {
        /// Version of the update that was rolled back.
        version: String,
    },
    /// A completed swap's retained previous build was reclaimed.
    Reclaimed {
        /// Version of the build that completed.
        version: String,
    },
}

/// A recovery result. `notice` is the user-facing text when the
/// reconciliation is one the design surfaces to the user (a rollback).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryReport {
    /// What recovery did.
    pub outcome: RecoveryOutcome,
    /// User notice, when the outcome carries one.
    pub notice: Option<String>,
}

fn log_recovery(layout: &InstallLayout, now_unix: i64, journal: &SwapJournal, tag: &str) {
    let line = format!(
        "{now_unix} recover step={:?} outcome={tag} version={}\n",
        journal.step, journal.new_version
    );
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(layout.recovery_log())
    {
        let _ = file.write_all(line.as_bytes());
        let _ = file.sync_all();
    }
}

/// Reconcile the swap journal at startup, before the UI launches.
/// Every reconciliation is logged locally; a rollback also carries
/// the user notice. The end state is one working install: either the
/// new build (swap completed forward, hashes verified) or the
/// previous build (rollback, `.old` files restored), never a corrupted
/// or half-swapped install.
pub fn recover_updater(
    layout: &InstallLayout,
    now_unix: i64,
) -> Result<RecoveryReport, ApplyError> {
    let Some(journal) = current_journal(layout)? else {
        return Ok(RecoveryReport {
            outcome: RecoveryOutcome::NoJournal,
            notice: None,
        });
    };
    let report = match journal.step {
        Step::Prepared => {
            // No rename was committed. If a crash landed between the
            // exe rename and the next journal write, the live exe may
            // be missing with its `.old` sibling present; restoring it
            // is what "keeps the old build" means on disk.
            for file in &journal.files {
                let target = PathBuf::from(&file.target);
                let old = old_sibling(&target);
                if !target.exists() && old.exists() {
                    fs::rename(&old, &target).map_err(|e| io(&target, e))?;
                }
            }
            journal_clear(layout)?;
            RecoveryReport {
                outcome: RecoveryOutcome::DiscardedPrepared {
                    version: journal.new_version.clone(),
                },
                notice: None,
            }
        }
        Step::OldRenamed => {
            // The swap can complete forward only if every new file is
            // still available and verified: staged, or already in
            // place from a partially completed sequence.
            let mut forward = true;
            for file in &journal.files {
                let entry = FileEntry {
                    path: file.path.clone(),
                    sha256: file.sha256.clone(),
                    size: file.size,
                };
                let staged = layout.staged_file(&file.path);
                let target = PathBuf::from(&file.target);
                let available = (staged.is_file() && verify_file(&staged, &entry).is_ok())
                    || (target.is_file() && verify_file(&target, &entry).is_ok());
                if !available {
                    forward = false;
                    break;
                }
            }
            if forward {
                for file in &journal.files.clone() {
                    move_one(layout, file)?;
                }
                let mut advanced = journal.clone();
                advanced.step = Step::NewStaged;
                journal_write(layout, &advanced)?;
                let meta = layout.staging_dir().join(STAGED_META);
                if meta.exists() {
                    fs::remove_file(&meta).map_err(|e| io(&meta, e))?;
                }
                RecoveryReport {
                    outcome: RecoveryOutcome::CompletedForward {
                        version: journal.new_version.clone(),
                    },
                    notice: None,
                }
            } else {
                restore_all(layout, &journal)?;
                journal_clear(layout)?;
                RecoveryReport {
                    outcome: RecoveryOutcome::RolledBack {
                        version: journal.new_version.clone(),
                    },
                    notice: Some(ROLLED_BACK_NOTICE.to_owned()),
                }
            }
        }
        Step::NewStaged => {
            let intact = journal.files.iter().all(|file| {
                let entry = FileEntry {
                    path: file.path.clone(),
                    sha256: file.sha256.clone(),
                    size: file.size,
                };
                verify_file(Path::new(&file.target), &entry).is_ok()
            });
            if intact {
                RecoveryReport {
                    outcome: RecoveryOutcome::AwaitingFirstStart {
                        version: journal.new_version.clone(),
                    },
                    notice: None,
                }
            } else {
                restore_all(layout, &journal)?;
                journal_clear(layout)?;
                RecoveryReport {
                    outcome: RecoveryOutcome::RolledBack {
                        version: journal.new_version.clone(),
                    },
                    notice: Some(ROLLED_BACK_NOTICE.to_owned()),
                }
            }
        }
        Step::Completed => {
            reclaim_old(&journal)?;
            journal_clear(layout)?;
            RecoveryReport {
                outcome: RecoveryOutcome::Reclaimed {
                    version: journal.new_version.clone(),
                },
                notice: None,
            }
        }
    };
    let tag = match &report.outcome {
        RecoveryOutcome::NoJournal => "no_journal",
        RecoveryOutcome::DiscardedPrepared { .. } => "discarded_prepared",
        RecoveryOutcome::CompletedForward { .. } => "completed_forward",
        RecoveryOutcome::AwaitingFirstStart { .. } => "awaiting_first_start",
        RecoveryOutcome::RolledBack { .. } => "rolled_back",
        RecoveryOutcome::Reclaimed { .. } => "reclaimed",
    };
    log_recovery(layout, now_unix, &journal, tag);
    Ok(report)
}

#[cfg(test)]
#[path = "update_apply_tests.rs"]
mod tests;
