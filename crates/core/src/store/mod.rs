//! Persistence: one [`redb`] file holds every task, every segment, the settings
//! and the learned `Alt-Svc` (HTTP/3) endpoints.
//!
//! Why redb: ACID, crash-safe, zero-copy reads, no server, no external files,
//! and it needs no privileges beyond writing inside the user's data directory.
//!
//! Durability policy
//! * the periodic progress flush (every `persist_interval_ms`, default 1 s)
//!   commits with [`Durability::Immediate`]: redb's [`Durability::None`]
//!   keeps data *only in process memory* until a later durable commit, so a
//!   non-durable progress flush would be lost on any hard kill - defeating
//!   the whole point of resume. One small fsync per second is cheap.
//! * minor in-between updates (e.g. non-terminal state changes) may still
//!   use [`Durability::None`]; the next durable commit promotes them,
//! * everything that changes *what* a task is (creation, completion, deletion,
//!   settings) uses [`Durability::Immediate`].

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

pub mod records;

use crate::config::Settings;
use crate::error::{Error, Result};
use crate::net::AltSvcEntry;

pub use records::{
    is_part_file, RepresentationBound, RouteState, SegmentState, TaskRecord, TaskState,
    TransportKind, HEADER_SCOPE_NONE, OPEN_ENDED,
};

/// Version 2 prevents older engines from ignoring a pending finalize intent.
/// Existing v1 tables/records are retained when the schema marker is upgraded.
pub const SCHEMA_VERSION: u32 = 2;

const TASKS: TableDefinition<u64, &[u8]> = TableDefinition::new("tasks.v1");
const SEGMENTS: TableDefinition<u64, &[u8]> = TableDefinition::new("segments.v1");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta.v1");
const COUNTERS: TableDefinition<&str, u64> = TableDefinition::new("counters.v1");
const ALT_SVC: TableDefinition<&str, &[u8]> = TableDefinition::new("alt_svc.v1");

const SETTINGS_KEY: &str = "settings";
const SCHEMA_KEY: &str = "schema_version";
const TASK_ID_COUNTER: &str = "task_id";

/// A task together with its segment layout, as loaded from disk.
#[derive(Clone, Debug)]
pub struct StoredTask {
    pub record: TaskRecord,
    pub segments: Vec<SegmentState>,
}

/// Summary numbers for the About view / `udm-cli doctor`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StoreStats {
    pub tasks: u64,
    pub active: u64,
    pub completed: u64,
    pub failed: u64,
    pub bytes_on_disk: u64,
    pub db_bytes: u64,
    pub alt_svc_entries: u64,
    pub schema_version: u32,
}

/// Handle to the redb database.
///
/// Cheap to clone-by-`Arc`; all operations are short transactions.
#[derive(Debug)]
pub struct Store {
    db: Database,
    path: PathBuf,
    /// In-memory mirror of the `Alt-Svc` table so transport selection never
    /// blocks on a read transaction.
    alt_svc_cache: Mutex<std::collections::HashMap<String, AltSvcEntry>>,
    /// Fault injection for tests: while non-zero, `save` decrements it and
    /// fails, so fixtures can prove behaviour when a durable write fails
    /// (the G2 route-lock persist is the critical case). Armed only through
    /// [`Store::fail_next_saves`], which integration tests use via the
    /// engine handle; it is never armed in production paths.
    fail_saves: std::sync::atomic::AtomicU32,
    /// Test hook invoked once inside [`Store::save`], before the write, so a
    /// fixture can move a task's persist epoch while a saver is in the
    /// commit it must not leave as the last pair. Production leaves it empty.
    save_observer: Mutex<SaveObserver>,
}

#[derive(Default)]
struct SaveObserver {
    hook: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

impl std::fmt::Debug for SaveObserver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SaveObserver")
            .field("armed", &self.hook.is_some())
            .finish()
    }
}

impl Store {
    /// Open (creating if needed) the database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Store> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent.to_path_buf(), e))?;
        }
        let db = match Database::create(&path) {
            Ok(db) => db,
            Err(first) => {
                // A hard process abort (the pre-fix GUI overflowed its main
                // thread stack before the window opened) can leave the file
                // header half-written. Quarantine the unreadable file next to
                // itself and start fresh: a download manager that refuses to
                // launch is worse than one that launches with an empty list.
                let text = first.to_string().to_ascii_lowercase();
                let corrupt = text.contains("magic") || text.contains("not a redb database");
                if !corrupt {
                    return Err(map_db_error(&path, first));
                }
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                let quarantined = path.with_file_name(format!("{name}.corrupt-{stamp}"));
                tracing::warn!(
                    path = %path.display(),
                    quarantined = %quarantined.display(),
                    error = %first,
                    "database header unreadable - moving the damaged file aside and starting fresh"
                );
                fs::rename(&path, &quarantined).map_err(|e| Error::io(path.clone(), e))?;
                Database::create(&path).map_err(|e| map_db_error(&path, e))?
            }
        };
        let store = Store {
            db,
            path,
            alt_svc_cache: Mutex::new(Default::default()),
            fail_saves: std::sync::atomic::AtomicU32::new(0),
            save_observer: Mutex::new(SaveObserver::default()),
        };
        store.migrate()?;
        store.load_alt_svc_cache()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Database file size in bytes.
    pub fn file_size(&self) -> u64 {
        fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }

    /// Reconcile durable local finalization intents before displaying task
    /// status. This performs no network requests and preserves ambiguous files.
    pub fn recover_finalizations(&self) -> Result<()> {
        crate::finalize::recover(self)
    }

    // ---------------------------------------------------------------- schema

    fn migrate(&self) -> Result<()> {
        let txn = self.db.begin_write().map_err(map_txn)?;
        // Make sure every table exists even when empty.
        drop(txn.open_table(TASKS).map_err(map_table)?);
        drop(txn.open_table(SEGMENTS).map_err(map_table)?);
        drop(txn.open_table(COUNTERS).map_err(map_table)?);
        drop(txn.open_table(ALT_SVC).map_err(map_table)?);
        let existing: Option<u32> = {
            let meta = txn.open_table(META).map_err(map_table)?;
            // Bind first: a match-scrutinee temporary in tail position would
            // outlive `meta` (and with it the borrow of the guard).
            let found = meta.get(SCHEMA_KEY).map_err(map_table)?;
            match found {
                Some(guard) => {
                    let raw = guard.value();
                    Some(u32::from_le_bytes(raw.try_into().unwrap_or([0; 4])))
                }
                None => None,
            }
        };
        match existing {
            Some(v) if v > SCHEMA_VERSION => {
                drop(txn);
                return Err(Error::Storage(format!(
                    "database schema version {v} is newer than this build supports ({SCHEMA_VERSION}); \
                     refusing to open it to avoid data loss"
                )));
            }
            Some(v) if v == SCHEMA_VERSION => {
                txn.commit().map_err(map_commit)?;
                return Ok(());
            }
            _ => {}
        }
        {
            let mut meta = txn.open_table(META).map_err(map_table)?;
            meta.insert(SCHEMA_KEY, &SCHEMA_VERSION.to_le_bytes().to_vec()[..])
                .map_err(map_table)?;
        }
        txn.commit().map_err(map_commit)?;
        tracing::info!(version = SCHEMA_VERSION, "database schema initialised");
        Ok(())
    }

    // ---------------------------------------------------------------- ids

    /// Allocate the next task id (monotonic, never reused).
    pub fn next_task_id(&self) -> Result<u64> {
        let txn = self.db.begin_write().map_err(map_txn)?;
        let id = {
            let mut counters = txn.open_table(COUNTERS).map_err(map_table)?;
            let current = counters
                .get(TASK_ID_COUNTER)
                .map_err(map_table)?
                .map(|g| g.value())
                .unwrap_or(0);
            let next = current + 1;
            counters
                .insert(TASK_ID_COUNTER, &next)
                .map_err(map_table)?;
            next
        };
        txn.commit().map_err(map_commit)?;
        Ok(id)
    }

    // ---------------------------------------------------------------- tasks

    /// Insert or update a task (durable).
    pub fn put_task(&self, record: &TaskRecord) -> Result<()> {
        self.save(&[record.clone()], &[], true)
    }

    /// Persist tasks and (optionally) their segment layouts in one transaction.
    ///
    /// `segments` entries whose vector is empty are left untouched, so callers
    /// can update just the records.
    /// Make the next `count` calls to [`Store::save`] fail as if the
    /// durable write had failed, without touching the database. Exists for
    /// failure-injection fixtures; never called by production code.
    pub fn fail_next_saves(&self, count: u32) {
        self.fail_saves
            .store(count, std::sync::atomic::Ordering::SeqCst);
    }

    /// Arm a one-shot hook that runs inside the next [`Store::save`], before
    /// the database write. The hook is taken, so a retry of that save does
    /// not run it again. Production never calls this.
    pub fn arm_save_observer(&self, hook: std::sync::Arc<dyn Fn() + Send + Sync>) {
        self.observer_mut().hook = Some(hook);
    }

    fn observer_mut(&self) -> std::sync::MutexGuard<'_, SaveObserver> {
        match self.save_observer.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub fn save(
        &self,
        records: &[TaskRecord],
        segments: &[(u64, Vec<SegmentState>)],
        durable: bool,
    ) -> Result<()> {
        let mut armed = self.fail_saves.load(std::sync::atomic::Ordering::SeqCst);
        while armed > 0 {
            match self.fail_saves.compare_exchange_weak(
                armed,
                armed - 1,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            ) {
                Ok(_) => {
                    return Err(Error::Storage(
                        "injected save failure (test fault)".into(),
                    ));
                }
                Err(actual) => armed = actual,
            }
        }
        if records.is_empty() && segments.is_empty() {
            return Ok(());
        }
        let observer = self.observer_mut().hook.take();
        if let Some(observer) = observer {
            observer();
        }
        let mut txn = self.db.begin_write().map_err(map_txn)?;
        if !durable {
            // In-memory-only commit (redb: `None` never reaches the file on its
            // own). Only safe for interim updates that the periodic durable
            // progress flush will promote within ~1 s; a hard kill may lose
            // them, which is acceptable for these.
            let _ = txn.set_durability(Durability::None);
        }
        {
            let mut tasks = txn.open_table(TASKS).map_err(map_table)?;
            for record in records {
                let json = serde_json::to_vec(record)?;
                tasks
                    .insert(record.id, &json[..])
                    .map_err(map_table)?;
            }
        }
        if !segments.is_empty() {
            let mut table = txn.open_table(SEGMENTS).map_err(map_table)?;
            for (id, segs) in segments {
                let json = serde_json::to_vec(segs)?;
                table.insert(*id, &json[..]).map_err(map_table)?;
            }
        }
        txn.commit().map_err(map_commit)?;
        Ok(())
    }

    pub fn get_task(&self, id: u64) -> Result<Option<TaskRecord>> {
        let txn = self.db.begin_read().map_err(map_txn)?;
        let table = txn.open_table(TASKS).map_err(map_table)?;
        Ok(match table.get(id).map_err(map_table)? {
            Some(guard) => Some(serde_json::from_slice(guard.value())?),
            None => None,
        })
    }

    pub fn get_segments(&self, id: u64) -> Result<Vec<SegmentState>> {
        let txn = self.db.begin_read().map_err(map_txn)?;
        let table = txn.open_table(SEGMENTS).map_err(map_table)?;
        Ok(match table.get(id).map_err(map_table)? {
            Some(guard) => serde_json::from_slice(guard.value())?,
            None => Vec::new(),
        })
    }

    /// Every stored task, oldest first.
    pub fn list_tasks(&self) -> Result<Vec<StoredTask>> {
        let txn = self.db.begin_read().map_err(map_txn)?;
        let tasks = txn.open_table(TASKS).map_err(map_table)?;
        let segments = txn.open_table(SEGMENTS).map_err(map_table)?;
        let mut out = Vec::new();
        for entry in tasks.iter().map_err(map_table)? {
            let (key, value) = entry.map_err(map_table)?;
            let id = key.value();
            let record: TaskRecord = match serde_json::from_slice(value.value()) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(id, "skipping unreadable task record: {e}");
                    continue;
                }
            };
            let segs = match segments.get(id).map_err(map_table)? {
                Some(guard) => serde_json::from_slice(guard.value()).unwrap_or_default(),
                None => Vec::new(),
            };
            out.push(StoredTask {
                record,
                segments: segs,
            });
        }
        out.sort_by_key(|t| t.record.created_at);
        Ok(out)
    }

    pub fn delete_task(&self, id: u64) -> Result<()> {
        let txn = self.db.begin_write().map_err(map_txn)?;
        {
            let mut tasks = txn.open_table(TASKS).map_err(map_table)?;
            tasks.remove(id).map_err(map_table)?;
            let mut segments = txn.open_table(SEGMENTS).map_err(map_table)?;
            segments.remove(id).map_err(map_table)?;
        }
        txn.commit().map_err(map_commit)?;
        Ok(())
    }

    /// Rewrite the state of every task that was active when udm exited (or
    /// crashed) to `Paused`, so nothing resumes without the user asking.
    pub fn recover_interrupted(&self) -> Result<u32> {
        let mut changed = Vec::new();
        for stored in self.list_tasks()? {
            if stored.record.state.was_interrupted() {
                let mut record = stored.record;
                record.state = TaskState::Paused;
                record.error = Some("interrupted by a previous session".to_string());
                changed.push(record);
            }
        }
        if !changed.is_empty() {
            self.save(&changed, &[], true)?;
        }
        Ok(changed.len() as u32)
    }

    // ---------------------------------------------------------------- settings

    pub fn load_settings(&self) -> Result<Option<Settings>> {
        let txn = self.db.begin_read().map_err(map_txn)?;
        let meta = txn.open_table(META).map_err(map_table)?;
        Ok(match meta.get(SETTINGS_KEY).map_err(map_table)? {
            Some(guard) => Some(Settings::from_json(guard.value())?),
            None => None,
        })
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<()> {
        let json = settings.to_json()?;
        let txn = self.db.begin_write().map_err(map_txn)?;
        {
            let mut meta = txn.open_table(META).map_err(map_table)?;
            meta.insert(SETTINGS_KEY, &json[..]).map_err(map_table)?;
        }
        txn.commit().map_err(map_commit)?;
        Ok(())
    }

    // ---------------------------------------------------------------- alt-svc

    fn load_alt_svc_cache(&self) -> Result<()> {
        let txn = self.db.begin_read().map_err(map_txn)?;
        let table = txn.open_table(ALT_SVC).map_err(map_table)?;
        let mut map = self
            .alt_svc_cache
            .lock()
            .map_err(|_| Error::Storage("alt-svc cache lock poisoned".into()))?;
        map.clear();
        for entry in table.iter().map_err(map_table)? {
            let (key, value) = entry.map_err(map_table)?;
            if let Ok(e) = serde_json::from_slice::<AltSvcEntry>(value.value()) {
                map.insert(key.value().to_string(), e);
            }
        }
        Ok(())
    }

    /// Every cached HTTP/3 endpoint (used by the Settings view).
    pub fn alt_svc_all(&self) -> Result<Vec<AltSvcEntry>> {
        let map = self
            .alt_svc_cache
            .lock()
            .map_err(|_| Error::Storage("alt-svc cache lock poisoned".into()))?;
        let mut out: Vec<AltSvcEntry> = map.values().cloned().collect();
        out.sort_by(|a, b| a.origin.cmp(&b.origin));
        Ok(out)
    }

    /// Remove expired entries from cache and disk; returns how many were dropped.
    pub fn alt_svc_prune(&self, now_millis: i64, max_age_secs: u64) -> Result<usize> {
        let expired: Vec<String> = {
            let map = self
                .alt_svc_cache
                .lock()
                .map_err(|_| Error::Storage("alt-svc cache lock poisoned".into()))?;
            map.iter()
                .filter(|(_, e)| !e.is_valid(now_millis, max_age_secs))
                .map(|(k, _)| k.clone())
                .collect()
        };
        if expired.is_empty() {
            return Ok(0);
        }
        {
            let mut map = self
                .alt_svc_cache
                .lock()
                .map_err(|_| Error::Storage("alt-svc cache lock poisoned".into()))?;
            for key in &expired {
                map.remove(key);
            }
        }
        let txn = self.db.begin_write().map_err(map_txn)?;
        {
            let mut table = txn.open_table(ALT_SVC).map_err(map_table)?;
            for key in &expired {
                table.remove(key.as_str()).map_err(map_table)?;
            }
        }
        txn.commit().map_err(map_commit)?;
        Ok(expired.len())
    }

    // ---------------------------------------------------------------- stats

    pub fn stats(&self) -> Result<StoreStats> {
        let mut stats = StoreStats {
            db_bytes: self.file_size(),
            schema_version: SCHEMA_VERSION,
            ..Default::default()
        };
        let txn = self.db.begin_read().map_err(map_txn)?;
        let tasks = txn.open_table(TASKS).map_err(map_table)?;
        for entry in tasks.iter().map_err(map_table)? {
            let (_, value) = entry.map_err(map_table)?;
            if let Ok(record) = serde_json::from_slice::<TaskRecord>(value.value()) {
                stats.tasks += 1;
                match record.state {
                    TaskState::Completed => stats.completed += 1,
                    TaskState::Failed => stats.failed += 1,
                    s if s.is_active() || s == TaskState::Queued => stats.active += 1,
                    _ => {}
                }
            }
        }
        let segments = txn.open_table(SEGMENTS).map_err(map_table)?;
        for entry in segments.iter().map_err(map_table)? {
            let (_, value) = entry.map_err(map_table)?;
            if let Ok(segs) = serde_json::from_slice::<Vec<SegmentState>>(value.value()) {
                stats.bytes_on_disk += segs.iter().map(|s| s.done).sum::<u64>();
            }
        }
        stats.alt_svc_entries = self.alt_svc_all()?.len() as u64;
        Ok(stats)
    }
}

// ---------------------------------------------------------------------------
// AltSvcStore implementation (the net layer only knows the trait)
// ---------------------------------------------------------------------------

impl crate::net::AltSvcStore for Store {
    fn get(&self, origin: &str) -> Option<AltSvcEntry> {
        let map = self.alt_svc_cache.lock().ok()?;
        map.get(&origin.to_ascii_lowercase()).cloned()
    }

    fn put(&self, entry: AltSvcEntry) {
        let key = entry.origin.to_ascii_lowercase();
        if let Ok(mut map) = self.alt_svc_cache.lock() {
            map.insert(key.clone(), entry.clone());
        }
        let txn = match self.db.begin_write() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("cannot persist alt-svc entry: {e}");
                return;
            }
        };
        let json = match serde_json::to_vec(&entry) {
            Ok(j) => j,
            Err(_) => return,
        };
        let result = {
            let mut table = match txn.open_table(ALT_SVC) {
                Ok(t) => t,
                Err(_) => return,
            };
            table.insert(key.as_str(), &json[..]).map(|_| ())
        };
        if result.is_err() || txn.commit().is_err() {
            tracing::debug!("alt-svc entry for {key} was not persisted");
        }
    }

    fn record_failure(&self, origin: &str) {
        let key = origin.to_ascii_lowercase();
        let mut entry = match self.get(&key) {
            Some(e) => e,
            None => return,
        };
        entry.record_failure();
        self.put(entry);
    }

    fn record_success(&self, origin: &str) {
        let key = origin.to_ascii_lowercase();
        if let Some(mut entry) = self.get(&key) {
            if entry.failures == 0 {
                return;
            }
            entry.failures = 0;
            self.put(entry);
        }
    }
}

// ---------------------------------------------------------------------------
// error mapping
// ---------------------------------------------------------------------------

fn map_db_error(path: &Path, e: impl std::fmt::Display) -> Error {
    let text = e.to_string();
    if text.to_ascii_lowercase().contains("lock") {
        Error::DatabaseLocked(dunce::simplified(path).to_path_buf())
    } else {
        Error::Storage(text)
    }
}

fn map_txn(e: impl std::fmt::Display) -> Error {
    Error::Storage(e.to_string())
}

fn map_table(e: impl std::fmt::Display) -> Error {
    Error::Storage(e.to_string())
}

fn map_commit(e: impl std::fmt::Display) -> Error {
    Error::Storage(e.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn v1_upgrade_preserves_tasks_and_future_schema_is_refused() {
        use super::*;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("schema.redb");
        let store = Store::open(&path).unwrap();
        let record = TaskRecord::new(1, "https://example.test/file", dir.path(), Some("file".into()));
        store.save(&[record.clone()], &[(1, vec![SegmentState::new(0, 0, 7)])], true).unwrap();
        let transaction = store.db.begin_write().unwrap();
        {
            let mut meta = transaction.open_table(META).unwrap();
            meta.insert(SCHEMA_KEY, 1u32.to_le_bytes().as_slice()).unwrap();
        }
        transaction.commit().unwrap();
        drop(store);
        let upgraded = Store::open(&path).unwrap();
        assert_eq!(upgraded.get_task(1).unwrap().unwrap().url, record.url);
        assert_eq!(upgraded.get_segments(1).unwrap().len(), 1);
        let transaction = upgraded.db.begin_write().unwrap();
        {
            let mut meta = transaction.open_table(META).unwrap();
            assert_eq!(meta.get(SCHEMA_KEY).unwrap().unwrap().value(), &2u32.to_le_bytes());
            meta.insert(SCHEMA_KEY, 3u32.to_le_bytes().as_slice()).unwrap();
        }
        transaction.commit().unwrap();
        drop(upgraded);
        assert!(matches!(Store::open(&path), Err(Error::Storage(message)) if message.contains("newer than this build")));
    }

    use super::*;
    use crate::net::AltSvcStore;
    use crate::util;

    fn tmp_db() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("test.redb")).unwrap();
        (dir, store)
    }

    #[test]
    fn round_trips_tasks_and_segments() {
        let (_dir, store) = tmp_db();
        let id = store.next_task_id().unwrap();
        assert_eq!(id, 1);
        let mut record = TaskRecord::new(id, "https://x.example/f.bin", Path::new("/tmp"), Some("f.bin".into()));
        record.total_size = Some(300);
        store.put_task(&record).unwrap();
        store
            .save(
                &[],
                &[(
                    id,
                    vec![
                        SegmentState::new(0, 0, 99),
                        SegmentState::new(1, 100, 199),
                        SegmentState::new(2, 200, 299),
                    ],
                )],
                true,
            )
            .unwrap();

        let loaded = store.get_task(id).unwrap().unwrap();
        assert_eq!(loaded.filename, "f.bin");
        assert_eq!(loaded.total_size, Some(300));
        assert_eq!(store.get_segments(id).unwrap().len(), 3);
        assert_eq!(store.list_tasks().unwrap().len(), 1);

        store.delete_task(id).unwrap();
        assert!(store.get_task(id).unwrap().is_none());
        assert!(store.get_segments(id).unwrap().is_empty());
    }

    #[test]
    fn ids_are_monotonic_across_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");
        let first = {
            let s = Store::open(&path).unwrap();
            s.next_task_id().unwrap()
        };
        let second = {
            let s = Store::open(&path).unwrap();
            s.next_task_id().unwrap()
        };
        assert_eq!((first, second), (1, 2));
    }

    #[test]
    fn settings_round_trip() {
        let (_dir, store) = tmp_db();
        assert!(store.load_settings().unwrap().is_none());
        let mut s = Settings::default();
        s.segments_per_task = 4;
        store.save_settings(&s).unwrap();
        assert_eq!(store.load_settings().unwrap().unwrap().segments_per_task, 4);
    }

    #[test]
    fn interrupted_tasks_are_recovered_as_paused() {
        let (_dir, store) = tmp_db();
        let id = store.next_task_id().unwrap();
        let mut record = TaskRecord::new(id, "u", Path::new("/tmp"), Some("f".into()));
        record.state = TaskState::Downloading;
        store.put_task(&record).unwrap();
        assert_eq!(store.recover_interrupted().unwrap(), 1);
        assert_eq!(
            store.get_task(id).unwrap().unwrap().state,
            TaskState::Paused
        );
    }

    #[test]
    fn alt_svc_cache_persists_and_prunes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.redb");
        let now = util::now_millis();
        {
            let store = Store::open(&path).unwrap();
            store.put(AltSvcEntry {
                origin: "cdn.example:443".into(),
                host: "cdn.example".into(),
                port: 443,
                learned_at: now,
                max_age_secs: 3600,
                failures: 0,
            });
            assert!(store.get("cdn.example:443").is_some());
            assert_eq!(store.alt_svc_all().unwrap().len(), 1);
        }
        {
            let store = Store::open(&path).unwrap();
            // reloaded from disk
            assert!(store.get("cdn.example:443").is_some());
            // way in the future -> expired
            let pruned = store
                .alt_svc_prune(now + 3600 * 1000 + 10, 3600)
                .unwrap();
            assert_eq!(pruned, 1);
            assert!(store.get("cdn.example:443").is_none());
        }
    }

    #[test]
    fn stats_are_reported() {
        let (_dir, store) = tmp_db();
        let id = store.next_task_id().unwrap();
        let mut record = TaskRecord::new(id, "u", Path::new("/tmp"), Some("f".into()));
        record.state = TaskState::Completed;
        store.put_task(&record).unwrap();
        store
            .save(&[], &[(id, vec![{ let mut s = SegmentState::new(0,0,99); s.done = 42; s }])], true)
            .unwrap();
        let stats = store.stats().unwrap();
        assert_eq!(stats.tasks, 1);
        assert_eq!(stats.completed, 1);
        assert_eq!(stats.bytes_on_disk, 42);
        assert!(stats.db_bytes > 0);
    }
}
