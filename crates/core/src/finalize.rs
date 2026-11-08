//! Local, durable rename transactions. The filesystem namespace and redb cannot
//! commit atomically, so retain an intent and reconcile their states on reopen.
use std::{
    fs::{self, File},
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    config::ConflictPolicy,
    durability::FileIdentity,
    error::{Error, Result},
    hash::{self, ExpectedHash},
    store::{SegmentState, Store, TaskRecord, TaskState},
    util,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Phase {
    Prepared,
    Committed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FinalizeIntent {
    version: u32,
    source: PathBuf,
    destination: PathBuf,
    identity: FileIdentity,
    previous_destination: Option<FileIdentity>,
    length: u64,
    content: ExpectedHash,
    context: String,
    phase: Phase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Prepared,
    Renamed,
    MetadataSynced,
    BeforeCompletion,
}

fn context(record: &TaskRecord) -> Result<String> {
    Ok(blake3::hash(&serde_json::to_vec(&(
        record.id,
        &record.url,
        &record.final_url,
        &record.dest_dir,
        &record.etag,
        &record.last_modified,
        &record.expected_hash,
    ))?)
    .to_hex()
    .to_string())
}

fn open_regular(path: &Path, write: bool) -> Result<File> {
    let metadata = fs::symlink_metadata(path).map_err(|e| Error::io(path, e))?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(Error::Finalize(format!(
                "reparse file refused: {}",
                path.display()
            )));
        }
    }
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(Error::Finalize(format!(
            "not a plain file: {}",
            path.display()
        )));
    }
    File::options()
        .read(true)
        .write(write)
        .open(path)
        .map_err(|e| Error::io(path, e))
}

fn identity_at(path: &Path) -> Result<Option<FileIdentity>> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::io(path, e)),
        Ok(_) => FileIdentity::read(&open_regular(path, false)?)
            .map(Some)
            .map_err(|e| Error::io(path, e)),
    }
}

fn sync_namespace(destination: &Path) -> Result<()> {
    open_regular(destination, true)?
        .sync_all()
        .map_err(|e| Error::io(destination, e))?;
    #[cfg(unix)]
    {
        let parent = destination
            .parent()
            .ok_or_else(|| Error::Finalize("no parent directory".into()))?;
        File::open(parent)
            .and_then(|file| file.sync_all())
            .map_err(|e| Error::io(parent, e))?;
    }
    // Windows std rename uses MoveFileExW(REPLACE_EXISTING). There is no portable
    // directory fsync equivalent in std on Windows. Keep the journal even after
    // completion and validate its final backing on every engine startup.
    Ok(())
}

impl FinalizeIntent {
    pub(crate) fn pending(&self) -> bool {
        self.phase == Phase::Prepared
    }

    fn prepare(
        record: &TaskRecord,
        source: &Path,
        target: &Path,
        policy: ConflictPolicy,
        content: ExpectedHash,
        held_file: &File,
    ) -> Result<Self> {
        if record.part_path() != source || record.final_path() != target {
            return Err(Error::Finalize("task paths changed before finalize".into()));
        }
        let mut destination = target.to_path_buf();
        if identity_at(target)?.is_some() {
            match policy {
                ConflictPolicy::Overwrite => {}
                ConflictPolicy::UniqueSuffix => {
                    destination = util::unique_path(&record.dest_dir(), &record.filename);
                }
                ConflictPolicy::Fail => return Err(Error::Conflict(target.to_path_buf())),
            }
        }
        let parent = source
            .parent()
            .ok_or_else(|| Error::Finalize("no source parent".into()))?;
        if destination.parent() != Some(parent) {
            return Err(Error::Finalize("cross-directory finalize refused".into()));
        }
        let identity = FileIdentity::read(held_file).map_err(|e| Error::io(source, e))?;
        if identity_at(source)?.as_ref() != Some(&identity) {
            return Err(Error::Finalize(
                "source backing changed before finalize".into(),
            ));
        }
        held_file.sync_all().map_err(|e| Error::io(source, e))?;
        let length = held_file
            .metadata()
            .map_err(|e| Error::io(source, e))?
            .len();
        if record.total_size.is_some_and(|expected| expected != length) {
            return Err(Error::Finalize(
                "source length changed before finalize".into(),
            ));
        }
        Ok(Self {
            version: 1,
            source: source.to_path_buf(),
            previous_destination: identity_at(&destination)?,
            destination,
            identity,
            length,
            content,
            context: context(record)?,
            phase: Phase::Prepared,
        })
    }

    fn matches(&self, path: &Path, verify: bool) -> Result<bool> {
        if identity_at(path)?.as_ref() != Some(&self.identity) {
            return Ok(false);
        }
        let mut file = open_regular(path, false)?;
        if file.metadata().map_err(|e| Error::io(path, e))?.len() != self.length {
            return Ok(false);
        }
        if verify {
            let mut digest = hash::hasher(self.content.algo);
            let mut buffer = vec![0; 128 * 1024];
            loop {
                let count = file.read(&mut buffer).map_err(|e| Error::io(path, e))?;
                if count == 0 {
                    break;
                }
                digest.update(&buffer[..count]);
            }
            return Ok(self.content.matches(&digest.finish_hex()));
        }
        Ok(true)
    }

    fn publish(&self, verify: bool) -> Result<()> {
        if self.matches(&self.destination, verify)? {
            return Ok(());
        }
        if !self.matches(&self.source, verify)? {
            return Err(Error::Finalize(
                "neither source nor destination matches prepared data".into(),
            ));
        }
        if identity_at(&self.destination)? != self.previous_destination {
            return Err(Error::Finalize(
                "destination changed since prepare; files retained".into(),
            ));
        }
        // Both names are in the same directory. Native rename replaces directly
        // for Overwrite, preserving the old destination if the operation fails.
        // The engine persistence lock serializes competing task finalizations.
        // External concurrent namespace mutation remains a path-based limitation.
        fs::rename(&self.source, &self.destination).map_err(|e| Error::io(&self.destination, e))
    }

    fn completed_record(&self, record: &TaskRecord) -> Result<TaskRecord> {
        let mut completed = record.clone();
        completed.filename = self
            .destination
            .file_name()
            .ok_or_else(|| Error::Finalize("no final filename".into()))?
            .to_string_lossy()
            .into_owned();
        completed.state = TaskState::Completed;
        completed.finished_at = Some(util::now_millis());
        completed.total_size = Some(self.length);
        completed.error = None;
        completed.attempts = 0;
        completed.progress_checkpoint = None;
        let mut committed = self.clone();
        committed.phase = Phase::Committed;
        completed.finalization = Some(committed);
        Ok(completed)
    }
}

/// Caller holds the shared persistence mutex and live record lock. Publish the
/// prepared intent after its commit, and Completed only after its own commit.
pub(crate) fn complete(
    store: &Store,
    record: &mut TaskRecord,
    segments: &mut Vec<SegmentState>,
    source: &Path,
    target: &Path,
    policy: ConflictPolicy,
    content: ExpectedHash,
    held_file: &File,
    mut hook: impl FnMut(Step) -> Result<()>,
) -> Result<PathBuf> {
    if record.finalization.is_some() {
        return Err(Error::Finalize(
            "existing transaction must be recovered first".into(),
        ));
    }
    let intent = FinalizeIntent::prepare(record, source, target, policy, content, held_file)?;
    let mut prepared = record.clone();
    prepared.finalization = Some(intent.clone());
    prepared.progress_checkpoint = None;
    store.save(&[prepared.clone()], &[(record.id, segments.clone())], true)?;
    *record = prepared;
    hook(Step::Prepared)?;
    intent.publish(false)?;
    hook(Step::Renamed)?;
    sync_namespace(&intent.destination)?;
    hook(Step::MetadataSynced)?;
    let completed = intent.completed_record(record)?;
    hook(Step::BeforeCompletion)?;
    store.save(&[completed.clone()], &[(record.id, Vec::new())], true)?;
    *record = completed;
    segments.clear();
    Ok(intent.destination)
}

/// Resolve journaled local operations before interrupted tasks are loaded.
/// Unrelated files are retained on every ambiguous/error path.
pub(crate) fn recover(store: &Store) -> Result<()> {
    for stored in store.list_tasks()? {
        if stored.record.finalization.is_some() {
            recover_record(store, &stored.record)?;
        }
    }
    Ok(())
}

pub(crate) fn recover_record(store: &Store, record: &TaskRecord) -> Result<TaskRecord> {
    let Some(intent) = record.finalization.clone() else {
        return Ok(record.clone());
    };
    let already_completed = intent.phase == Phase::Committed
        && record.state == TaskState::Completed
        && record.error.is_none();
    let result = (|| -> Result<TaskRecord> {
        if intent.version != 1 || context(record)? != intent.context {
            return Err(Error::Finalize(
                "unsupported or mismatched finalize context".into(),
            ));
        }
        if intent.source.parent() != Some(record.dest_dir().as_path())
            || intent.destination.parent() != intent.source.parent()
        {
            return Err(Error::Finalize(
                "journal paths do not match task destination".into(),
            ));
        }
        if (intent.pending() && record.part_path() != intent.source)
            || (!intent.pending() && record.final_path() != intent.destination)
            || record.total_size.is_some_and(|size| size != intent.length)
        {
            return Err(Error::Finalize(
                "task artifact no longer matches journal".into(),
            ));
        }
        if let crate::store::RouteState::Locked { root, .. } = &record.route {
            let root = Path::new(root);
            let dest = record.dest_dir();
            let relative = dest
                .strip_prefix(root)
                .map_err(|_| Error::Finalize("route root mismatch".into()))?;
            let folder = relative
                .to_str()
                .ok_or_else(|| Error::Finalize("invalid category path".into()))?;
            crate::category::prepare_category_dir(root, folder, &record.filename)?;
        }
        if intent.phase == Phase::Committed {
            if !intent.matches(&intent.destination, false)? {
                return Err(Error::Finalize(
                    "completed artifact missing or replaced".into(),
                ));
            }
            return if record.state == TaskState::Completed {
                Ok(record.clone())
            } else {
                intent.completed_record(record)
            };
        }
        intent.publish(true)?;
        sync_namespace(&intent.destination)?;
        intent.completed_record(record)
    })();
    match result {
        Ok(record) if record.state == TaskState::Completed => {
            if already_completed {
                // Normal startup does not need another durable DB transaction
                // for every completed history entry. Retry from a failed state
                // still commits its recovered Completed state below.
                return Ok(record);
            }
            store.save(&[record.clone()], &[(record.id, Vec::new())], true)?;
            Ok(record)
        }
        Ok(record) => Ok(record),
        Err(error) => {
            let mut failed = record.clone();
            failed.state = TaskState::Failed;
            failed.error = Some(error.short());
            tracing::warn!(id = failed.id, error = %error, "finalize recovery retained ambiguous files");
            store.save(&[failed.clone()], &[], true)?;
            Ok(failed)
        }
    }
}

#[cfg(test)]
#[path = "finalize_campaign_tests.rs"]
mod campaign_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (
        tempfile::TempDir,
        Store,
        TaskRecord,
        Vec<SegmentState>,
        File,
        ExpectedHash,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("state.redb")).unwrap();
        let mut record = TaskRecord::new(
            1,
            "https://example.test/file",
            dir.path(),
            Some("file.bin".into()),
        );
        record.state = TaskState::Downloading;
        record.total_size = Some(8);
        fs::write(record.part_path(), b"new-data").unwrap();
        fs::write(record.final_path(), b"old-data").unwrap();
        let file = open_regular(&record.part_path(), true).unwrap();
        let segments = vec![SegmentState {
            done: 8,
            finished: true,
            ..SegmentState::new(0, 0, 7)
        }];
        store
            .save(&[record.clone()], &[(1, segments.clone())], true)
            .unwrap();
        let digest = ExpectedHash::new(
            hash::HashAlgo::Blake3,
            blake3::hash(b"new-data").to_hex().to_string(),
        )
        .unwrap();
        (dir, store, record, segments, file, digest)
    }

    fn run(
        store: &Store,
        record: &mut TaskRecord,
        segments: &mut Vec<SegmentState>,
        file: &File,
        digest: ExpectedHash,
        hook: impl FnMut(Step) -> Result<()>,
    ) -> Result<PathBuf> {
        complete(
            store,
            record,
            segments,
            &record.part_path(),
            &record.final_path(),
            ConflictPolicy::Overwrite,
            digest,
            file,
            hook,
        )
    }

    #[test]
    fn overwrite_commits_without_deleting_destination_first() {
        let (_dir, store, mut record, mut segments, file, digest) = fixture();
        let target = run(
            &store,
            &mut record,
            &mut segments,
            &file,
            digest,
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(fs::read(target).unwrap(), b"new-data");
        assert!(!record.part_path().exists());
        assert_eq!(
            store.get_task(1).unwrap().unwrap().state,
            TaskState::Completed
        );
        assert!(store.get_segments(1).unwrap().is_empty());
        let finished = record.finished_at;
        recover(&store).unwrap();
        recover(&store).unwrap();
        assert_eq!(store.get_task(1).unwrap().unwrap().finished_at, finished);
    }

    #[test]
    fn failed_prepare_commit_preserves_both_files_and_live_record() {
        let (_dir, store, mut record, mut segments, file, digest) = fixture();
        store.fail_next_saves(1);
        assert!(run(
            &store,
            &mut record,
            &mut segments,
            &file,
            digest,
            |_| Ok(())
        )
        .is_err());
        assert!(record.finalization.is_none());
        assert_eq!(fs::read(record.final_path()).unwrap(), b"old-data");
        assert_eq!(fs::read(record.part_path()).unwrap(), b"new-data");
    }

    #[test]
    fn reopen_recovers_every_prepare_rename_metadata_and_completion_cut() {
        for cut in [
            Step::Prepared,
            Step::Renamed,
            Step::MetadataSynced,
            Step::BeforeCompletion,
        ] {
            let (dir, store, mut record, mut segments, file, digest) = fixture();
            assert!(
                run(&store, &mut record, &mut segments, &file, digest, |step| {
                    if step == cut {
                        return Err(Error::Storage("injected crash boundary".into()));
                    }
                    Ok(())
                })
                .is_err()
            );
            assert_ne!(record.state, TaskState::Completed);
            drop(store);
            let reopened = Store::open(dir.path().join("state.redb")).unwrap();
            recover(&reopened).unwrap();
            recover(&reopened).unwrap();
            assert_eq!(
                reopened.get_task(1).unwrap().unwrap().state,
                TaskState::Completed,
                "{cut:?}"
            );
            assert_eq!(fs::read(record.final_path()).unwrap(), b"new-data");
        }
    }

    #[test]
    fn completion_save_failure_never_publishes_completed_in_memory() {
        let (dir, store, mut record, mut segments, file, digest) = fixture();
        assert!(
            run(&store, &mut record, &mut segments, &file, digest, |step| {
                if matches!(step, Step::BeforeCompletion) {
                    store.fail_next_saves(1);
                }
                Ok(())
            })
            .is_err()
        );
        assert_eq!(record.state, TaskState::Downloading);
        assert!(record.finalization.as_ref().unwrap().pending());
        assert_eq!(fs::read(record.final_path()).unwrap(), b"new-data");
        drop(store);
        let store = Store::open(dir.path().join("state.redb")).unwrap();
        recover(&store).unwrap();
        assert_eq!(
            store.get_task(1).unwrap().unwrap().state,
            TaskState::Completed
        );
    }

    #[test]
    fn changed_destination_after_prepare_is_preserved() {
        let (dir, store, mut record, mut segments, file, digest) = fixture();
        let target = record.final_path();
        assert!(
            run(&store, &mut record, &mut segments, &file, digest, |step| {
                if matches!(step, Step::Prepared) {
                    fs::rename(&target, dir.path().join("original-destination")).unwrap();
                    fs::write(&target, b"unrelated").unwrap();
                }
                Ok(())
            })
            .is_err()
        );
        recover(&store).unwrap();
        assert_eq!(store.get_task(1).unwrap().unwrap().state, TaskState::Failed);
        assert_eq!(fs::read(target).unwrap(), b"unrelated");
        assert_eq!(fs::read(record.part_path()).unwrap(), b"new-data");
    }

    #[test]
    fn altered_renamed_content_is_not_adopted() {
        let (_dir, store, mut record, mut segments, file, digest) = fixture();
        let target = record.final_path();
        assert!(
            run(&store, &mut record, &mut segments, &file, digest, |step| {
                if matches!(step, Step::Renamed) {
                    fs::write(&target, b"bad-data").unwrap();
                    return Err(Error::Storage("cut after external mutation".into()));
                }
                Ok(())
            })
            .is_err()
        );
        recover(&store).unwrap();
        assert_eq!(store.get_task(1).unwrap().unwrap().state, TaskState::Failed);
        assert_eq!(fs::read(target).unwrap(), b"bad-data");
    }

    #[test]
    fn committed_missing_artifact_is_reported_failed_on_reopen() {
        let (_dir, store, mut record, mut segments, file, digest) = fixture();
        let target = run(
            &store,
            &mut record,
            &mut segments,
            &file,
            digest,
            |_| Ok(()),
        )
        .unwrap();
        fs::remove_file(target).unwrap();
        recover(&store).unwrap();
        let failed = store.get_task(1).unwrap().unwrap();
        assert_eq!(failed.state, TaskState::Failed);
        assert!(failed.error.unwrap().contains("missing or replaced"));
    }

    #[test]
    fn collision_policies_preserve_existing_files() {
        for policy in [ConflictPolicy::Fail, ConflictPolicy::UniqueSuffix] {
            let (_dir, store, mut record, mut segments, file, digest) = fixture();
            let original = record.final_path();
            let source = record.part_path();
            let result = complete(
                &store,
                &mut record,
                &mut segments,
                &source,
                &original,
                policy,
                digest,
                &file,
                |_| Ok(()),
            );
            assert_eq!(fs::read(&original).unwrap(), b"old-data");
            match policy {
                ConflictPolicy::Fail => {
                    assert!(result.is_err());
                    assert!(record.finalization.is_none());
                }
                _ => {
                    let actual = result.unwrap();
                    assert_ne!(actual, original);
                    assert_eq!(fs::read(actual).unwrap(), b"new-data");
                    recover(&store).unwrap();
                    assert_eq!(
                        store.get_task(1).unwrap().unwrap().state,
                        TaskState::Completed
                    );
                }
            }
        }
    }

    #[test]
    fn unknown_length_completion_remains_recoverable() {
        let (_dir, store, mut record, mut segments, file, digest) = fixture();
        record.total_size = None;
        run(
            &store,
            &mut record,
            &mut segments,
            &file,
            digest,
            |_| Ok(()),
        )
        .unwrap();
        recover(&store).unwrap();
        let completed = store.get_task(1).unwrap().unwrap();
        assert_eq!(completed.state, TaskState::Completed);
        assert_eq!(completed.total_size, Some(8));
    }

    #[test]
    fn engine_startup_adopts_renamed_file_without_network() {
        let (dir, store, mut record, mut segments, file, digest) = fixture();
        assert!(
            run(&store, &mut record, &mut segments, &file, digest, |step| {
                if step == Step::Renamed {
                    return Err(Error::Storage("cut".into()));
                }
                Ok(())
            })
            .is_err()
        );
        drop(store);
        let root = dir.path();
        let paths = crate::Paths {
            root: root.into(),
            db_file: root.join("state.redb"),
            log_file: root.join("logs/test.log"),
            temp_dir: root.join("tmp"),
            default_download_dir: root.into(),
            portable: true,
            exe_dir: Some(root.into()),
        };
        let engine = crate::EngineBuilder::new()
            .paths(paths)
            .idle(true)
            .settings(crate::Settings {
                http3_enabled: false,
                use_system_proxy: false,
                ..crate::Settings::default()
            })
            .build()
            .unwrap();
        assert_eq!(engine.task(1).unwrap().state, TaskState::Completed);
        assert_eq!(
            fs::read(engine.task(1).unwrap().final_path).unwrap(),
            b"new-data"
        );
        engine.shutdown();
    }

    #[test]
    fn changed_request_context_blocks_recovery_without_file_changes() {
        let (_dir, store, mut record, mut segments, file, digest) = fixture();
        assert!(
            run(&store, &mut record, &mut segments, &file, digest, |step| {
                if step == Step::Prepared {
                    return Err(Error::Storage("cut".into()));
                }
                Ok(())
            })
            .is_err()
        );
        record.final_url = "https://other.test/different".into();
        store.save(&[record.clone()], &[], true).unwrap();
        recover(&store).unwrap();
        assert_eq!(store.get_task(1).unwrap().unwrap().state, TaskState::Failed);
        assert_eq!(fs::read(record.final_path()).unwrap(), b"old-data");
        assert_eq!(fs::read(record.part_path()).unwrap(), b"new-data");
    }

    // Child entry point for the crash driver below. Exit skips Rust destructors,
    // including Store::drop, so this exercises redb reopen after abrupt exit.
    #[test]
    fn finalize_cut_child() {
        let Ok(cut) = std::env::var("UDM_TEST_FINALIZE_CUT") else {
            return;
        };
        let root = PathBuf::from(std::env::var_os("UDM_TEST_FINALIZE_ROOT").unwrap());
        let store = Store::open(root.join("state.redb")).unwrap();
        let mut record = store.get_task(1).unwrap().unwrap();
        let mut segments = store.get_segments(1).unwrap();
        let file = open_regular(&record.part_path(), true).unwrap();
        let digest = ExpectedHash::new(
            hash::HashAlgo::Blake3,
            blake3::hash(b"new-data").to_hex().to_string(),
        )
        .unwrap();
        run(&store, &mut record, &mut segments, &file, digest, |step| {
            if format!("{step:?}") == cut {
                std::process::exit(86);
            }
            Ok(())
        })
        .unwrap();
        panic!("requested crash boundary was never reached");
    }

    #[test]
    fn abrupt_child_exit_recovers_all_finalize_boundaries() {
        for cut in [
            Step::Prepared,
            Step::Renamed,
            Step::MetadataSynced,
            Step::BeforeCompletion,
        ] {
            let (dir, store, record, _segments, file, _digest) = fixture();
            drop(store);
            drop(file);
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args([
                    "--exact",
                    "finalize::tests::finalize_cut_child",
                    "--nocapture",
                ])
                .env("UDM_TEST_FINALIZE_CUT", format!("{cut:?}"))
                .env("UDM_TEST_FINALIZE_ROOT", dir.path());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                child.creation_flags(0x08000000);
            }
            let output = child.output().unwrap();
            assert_eq!(
                output.status.code(),
                Some(86),
                "{cut:?}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let store = Store::open(dir.path().join("state.redb")).unwrap();
            store.recover_finalizations().unwrap();
            assert_eq!(
                store.get_task(1).unwrap().unwrap().state,
                TaskState::Completed,
                "{cut:?}"
            );
            assert_eq!(fs::read(record.final_path()).unwrap(), b"new-data");
        }
    }

    #[cfg(windows)]
    #[test]
    fn sharing_violation_on_source_rename_preserves_old_destination() {
        use std::os::windows::fs::OpenOptionsExt;
        let (_dir, store, mut record, mut segments, file, digest) = fixture();
        let source = record.part_path();
        let mut blocker = None;
        assert!(
            run(&store, &mut record, &mut segments, &file, digest, |step| {
                if matches!(step, Step::Prepared) {
                    // Deny delete/rename on the source, while leaving the old final
                    // free to delete. Delete-then-rename would lose the old final.
                    blocker = Some(
                        File::options()
                            .read(true)
                            .share_mode(3)
                            .open(&source)
                            .unwrap(),
                    );
                }
                Ok(())
            })
            .is_err()
        );
        assert_eq!(fs::read(record.final_path()).unwrap(), b"old-data");
        assert_eq!(fs::read(source).unwrap(), b"new-data");
        drop(blocker);
        recover(&store).unwrap();
        assert_eq!(
            store.get_task(1).unwrap().unwrap().state,
            TaskState::Completed
        );
    }
}
