//! Versioned progress proofs. A durable DB transaction is necessary in addition
//! to this data-sync receipt. Runtime lease/generation fencing comes in PR 3.
use std::{fs::File, io};

use serde::{Deserialize, Serialize};

use crate::{
    error::{Error, Result},
    store::{SegmentState, TaskRecord},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileIdentity {
    platform: String,
    volume: u64,
    index: u64,
}

impl FileIdentity {
    pub(crate) fn read(file: &File) -> io::Result<Self> {
        #[cfg(windows)]
        {
            let info = winapi_util::file::information(file)?;
            Ok(Self {
                platform: "windows".into(),
                volume: info.volume_serial_number(),
                index: info.file_index(),
            })
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = file.metadata()?;
            Ok(Self {
                platform: "unix".into(),
                volume: metadata.dev(),
                index: metadata.ino(),
            })
        }
        #[cfg(not(any(windows, unix)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "file identity unavailable",
            ))
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProgressCheckpoint {
    version: u32,
    /// In this primitive the backing identity binds a generation. A separate
    /// runtime generation manager is deliberately introduced by the fencing PR.
    identity: FileIdentity,
    snapshot: String,
}

fn fingerprint(record: &TaskRecord, segments: &[SegmentState]) -> Result<String> {
    // Canonical structured serialization avoids ambiguous concatenated fields.
    // Hash input remains local and is never logged.
    let bytes = serde_json::to_vec(&(
        record.id,
        &record.url,
        &record.final_url,
        &record.dest_dir,
        &record.filename,
        record.total_size,
        &record.etag,
        &record.last_modified,
        &record.content_type,
        &record.expected_hash,
        segments,
    ))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

/// Sync the captured snapshot's backing file before authorizing its DB write.
/// Callers must use the held handle from partial claim, never a reopened alias.
pub(crate) fn prepare(
    record: &mut TaskRecord,
    segments: &[SegmentState],
    file: Option<&File>,
    sync: impl FnOnce(&File) -> io::Result<()>,
) -> Result<()> {
    record.progress_checkpoint = None;
    // A prepared finalize intent owns recovery after rename. Do not recreate a
    // progress receipt from its now-missing partial or overwrite that intent.
    if record.finalization.is_some() {
        return Ok(());
    }
    if !segments.iter().any(|s| s.done > 0 || s.finished) {
        return Ok(());
    }
    let path = record.part_path();
    let file = file.ok_or_else(|| Error::Storage("partial has no claimed sync handle".into()))?;
    let identity = FileIdentity::read(file).map_err(|e| Error::io(&path, e))?;
    let current = File::open(&path).map_err(|e| Error::io(&path, e))?;
    if FileIdentity::read(&current).map_err(|e| Error::io(&path, e))? != identity {
        return Err(Error::Storage(
            "partial backing changed before checkpoint".into(),
        ));
    }
    sync(file).map_err(|e| Error::io(&path, e))?;
    record.progress_checkpoint = Some(ProgressCheckpoint {
        version: 1,
        identity,
        snapshot: fingerprint(record, segments)?,
    });
    Ok(())
}

/// Recovery is conservative: old progress remains owned but is re-fetched.
/// A matching identity does not prove absence of external mutation in place.
pub(crate) fn recover(record: &mut TaskRecord, segments: &mut [SegmentState]) -> bool {
    let valid = (|| -> Result<bool> {
        let Some(proof) = &record.progress_checkpoint else {
            return Ok(false);
        };
        if proof.version != 1 || fingerprint(record, segments)? != proof.snapshot {
            return Ok(false);
        }
        let file = File::open(record.part_path()).map_err(|e| Error::io(record.part_path(), e))?;
        let identity = FileIdentity::read(&file).map_err(|e| Error::io(record.part_path(), e))?;
        let len = file
            .metadata()
            .map_err(|e| Error::io(record.part_path(), e))?
            .len();
        Ok(identity == proof.identity
            && segments
                .iter()
                .all(|s| s.start.checked_add(s.done).is_some_and(|end| end <= len)))
    })()
    .unwrap_or(false);
    if valid {
        return false;
    }
    let changed = segments.iter().any(|s| s.done > 0 || s.finished);
    for segment in segments {
        segment.done = 0;
        segment.finished = false;
    }
    record.progress_checkpoint = None;
    record.computed_hash = None;
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Store, TaskState};

    fn fixture() -> (tempfile::TempDir, TaskRecord, Vec<SegmentState>, File) {
        let dir = tempfile::tempdir().unwrap();
        let mut record = TaskRecord::new(
            1,
            "https://example.test/file",
            dir.path(),
            Some("file".into()),
        );
        record.state = TaskState::Paused;
        record.total_size = Some(8);
        std::fs::write(record.part_path(), b"ABCDEFGH").unwrap();
        let file = File::options()
            .read(true)
            .write(true)
            .open(record.part_path())
            .unwrap();
        let mut segment = SegmentState::new(0, 0, 7);
        segment.done = 4;
        (dir, record, vec![segment], file)
    }

    #[test]
    fn legacy_progress_is_reset_without_deleting_partial() {
        let (_dir, mut record, mut segments, _file) = fixture();
        assert!(recover(&mut record, &mut segments));
        assert_eq!(segments[0].done, 0);
        assert_eq!(std::fs::read(record.part_path()).unwrap(), b"ABCDEFGH");
    }

    #[test]
    fn synced_exact_snapshot_survives_store_reopen() {
        let (dir, mut record, segments, file) = fixture();
        prepare(&mut record, &segments, Some(&file), File::sync_all).unwrap();
        let path = dir.path().join("checkpoint.redb");
        {
            let store = Store::open(&path).unwrap();
            store
                .save(&[record.clone()], &[(record.id, segments)], true)
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let mut record = store.get_task(1).unwrap().unwrap();
        let mut segments = store.get_segments(1).unwrap();
        assert!(!recover(&mut record, &mut segments));
        assert_eq!(segments[0].done, 4);
    }

    #[test]
    fn sync_failure_cannot_authorize_a_checkpoint() {
        let (_dir, mut record, segments, file) = fixture();
        let result = prepare(&mut record, &segments, Some(&file), |_| {
            Err(io::Error::other("injected disk sync failure"))
        });
        assert!(result.is_err());
        assert!(record.progress_checkpoint.is_none());
    }

    #[test]
    fn sync_without_database_commit_does_not_advance_recovery() {
        let (dir, mut record, segments, file) = fixture();
        let path = dir.path().join("checkpoint.redb");
        let store = Store::open(&path).unwrap();
        store
            .save(&[record.clone()], &[(1, segments.clone())], true)
            .unwrap();
        prepare(&mut record, &segments, Some(&file), File::sync_all).unwrap();
        store.fail_next_saves(1);
        assert!(store.save(&[record], &[(1, segments)], true).is_err());
        drop(store);
        let store = Store::open(&path).unwrap();
        let mut record = store.get_task(1).unwrap().unwrap();
        let mut segments = store.get_segments(1).unwrap();
        assert!(recover(&mut record, &mut segments));
        assert_eq!(segments[0].done, 0);
    }

    #[test]
    fn later_progress_and_context_cannot_borrow_an_earlier_sync_receipt() {
        let (_dir, mut record, mut segments, file) = fixture();
        prepare(&mut record, &segments, Some(&file), File::sync_all).unwrap();
        let mut changed_context = record.clone();
        changed_context.etag = Some("\"different\"".into());
        assert!(recover(&mut changed_context, &mut segments.clone()));
        segments[0].done = 6;
        assert!(recover(&mut record, &mut segments));
        assert_eq!(segments[0].done, 0);
    }

    #[test]
    fn replaced_backing_cannot_reuse_a_checkpoint_or_sync_handle() {
        let (dir, mut record, mut segments, file) = fixture();
        prepare(&mut record, &segments, Some(&file), File::sync_all).unwrap();
        std::fs::rename(record.part_path(), dir.path().join("old-backing")).unwrap();
        std::fs::write(record.part_path(), b"XXXXXXXX").unwrap();
        assert!(recover(&mut record.clone(), &mut segments));
        assert!(prepare(
            &mut record,
            &[SegmentState {
                done: 4,
                ..SegmentState::new(0, 0, 7)
            }],
            Some(&file),
            File::sync_all
        )
        .is_err());
    }
}
