//! The state database: bases per key and replica, unresolved marks, the intent journal, archive rows, persisted settings, relation fingerprints, replica paths.
//!
//! SQLite in write-ahead mode, so dashboard reads never block the engine's writes. Schema version 2; a database from the first design is refused, since its replica identities and record states do not carry over.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

use crate::observe::{Basis, Hash, Snapshot};
use crate::site::Key;

/// Schema version written to the `setting` table when a database is created.
pub const SCHEMA_VERSION: u32 = 2;

/// Bases of one relation: by canonical key text, then by replica id.
pub type RecordMap = BTreeMap<String, BTreeMap<String, Basis>>;

/// The open database.
pub struct Store {
    conn: Connection,
}

/// Milliseconds since the Unix epoch, as stored.
pub fn to_epoch_ms(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_millis() as i64
}

/// A system time from stored milliseconds.
pub fn from_epoch_ms(ms: i64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64)
}

/// Unix seconds now, for `seen` and `started` columns.
fn now_ms() -> i64 {
    to_epoch_ms(SystemTime::now())
}

/// An archived revision as listed on the history page.
pub struct ArchiveEntry {
    pub key: String,
    pub replica: String,
    pub path: String,
    pub hash: Hash,
    pub size: u64,
    pub reason: String,
    pub at: SystemTime,
}

/// An intent row: relation, key, plan text.
pub struct Intent {
    pub relation: String,
    pub key: Key,
    pub detail: String,
}

impl Store {
    /// Open or create the database, creating parent directories, applying the schema, refusing a newer or older schema version.
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let existing: Option<String> = conn.query_row("SELECT value FROM setting WHERE key='schema_version'", [], |r| r.get(0)).optional().unwrap_or(None);
        if let Some(v) = existing {
            let found: u32 = v.parse().unwrap_or(0);
            if found != SCHEMA_VERSION {
                bail!("{} has schema version {found}; this replica uses version {SCHEMA_VERSION}. Start a fresh state directory; the first cycle aligns identical files and holds differing ones", path.display());
            }
        }
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS setting (
                key     TEXT PRIMARY KEY,
                value   TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS relation (
                name        TEXT PRIMARY KEY,
                fingerprint TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS replica (
                rel     TEXT NOT NULL,
                id      TEXT NOT NULL,
                path    TEXT NOT NULL,
                seen    INTEGER NOT NULL,
                PRIMARY KEY (rel, id)
            );
            CREATE TABLE IF NOT EXISTS record (
                rel     TEXT NOT NULL,
                key     TEXT NOT NULL,
                replica TEXT NOT NULL,
                present INTEGER NOT NULL,
                mtime   INTEGER,
                size    INTEGER,
                hash    BLOB,
                PRIMARY KEY (rel, key, replica)
            );
            CREATE TABLE IF NOT EXISTS unresolved (
                rel     TEXT NOT NULL,
                key     TEXT NOT NULL,
                replica TEXT NOT NULL,
                reason  TEXT NOT NULL,
                PRIMARY KEY (rel, key, replica)
            );
            CREATE TABLE IF NOT EXISTS intent (
                rel     TEXT NOT NULL,
                key     TEXT NOT NULL,
                detail  TEXT NOT NULL,
                started INTEGER NOT NULL,
                PRIMARY KEY (rel, key)
            );
            CREATE TABLE IF NOT EXISTS archive (
                id      INTEGER PRIMARY KEY AUTOINCREMENT,
                rel     TEXT NOT NULL,
                key     TEXT NOT NULL,
                replica TEXT NOT NULL,
                path    TEXT NOT NULL,
                hash    BLOB NOT NULL,
                size    INTEGER NOT NULL,
                reason  TEXT NOT NULL,
                at      INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS archive_hash ON archive (hash);
            CREATE INDEX IF NOT EXISTS archive_rel_at_id ON archive (rel, at, id);",
        )?;
        conn.execute("INSERT OR IGNORE INTO setting (key, value) VALUES ('schema_version', ?1)", params![SCHEMA_VERSION.to_string()])?;
        Ok(Store { conn })
    }

    /// Run `f` inside one transaction, committing on success and rolling back on error.
    pub fn transaction<T>(&self, f: impl FnOnce(&Store) -> Result<T>) -> Result<T> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let outcome = f(self).and_then(|v| self.conn.execute_batch("COMMIT").map(|_| v).map_err(Into::into));
        if outcome.is_err() {
            // A failed commit leaves the transaction open, so later statements must not join it.
            let _ = self.conn.execute_batch("ROLLBACK");
        }
        outcome
    }

    // Settings: gate flags, pause flag, anything else small the daemon persists.

    /// Read a setting.
    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.prepare_cached("SELECT value FROM setting WHERE key=?1")?.query_row(params![key], |r| r.get(0)).optional()?)
    }

    /// Write a setting.
    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.prepare_cached("INSERT INTO setting (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value=?2")?.execute(params![key, value])?;
        Ok(())
    }

    /// Read a boolean setting, None when unset.
    pub fn flag(&self, key: &str) -> Result<Option<bool>> {
        Ok(self.setting(key)?.map(|v| v == "true"))
    }

    /// Write a boolean setting.
    pub fn set_flag(&self, key: &str, value: bool) -> Result<()> {
        self.set_setting(key, if value { "true" } else { "false" })
    }

    // Relations and replicas.

    /// The stored policy fingerprint of a relation.
    pub fn fingerprint(&self, relation: &str) -> Result<Option<String>> {
        Ok(self.conn.prepare_cached("SELECT fingerprint FROM relation WHERE name=?1")?.query_row(params![relation], |r| r.get(0)).optional()?)
    }

    /// Store the policy fingerprint of a relation.
    pub fn set_fingerprint(&self, relation: &str, fingerprint: &str) -> Result<()> {
        self.conn.prepare_cached("INSERT INTO relation (name, fingerprint) VALUES (?1, ?2) ON CONFLICT (name) DO UPDATE SET fingerprint=?2")?.execute(params![relation, fingerprint])?;
        Ok(())
    }

    /// Record the path behind a replica identity, returning the previously stored path when it differs.
    pub fn note_replica(&self, relation: &str, id: &str, path: &Path) -> Result<Option<String>> {
        let text = path.display().to_string();
        let previous: Option<String> = self.conn.prepare_cached("SELECT path FROM replica WHERE rel=?1 AND id=?2")?.query_row(params![relation, id], |r| r.get(0)).optional()?;
        self.conn.prepare_cached("INSERT INTO replica (rel, id, path, seen) VALUES (?1, ?2, ?3, ?4) ON CONFLICT (rel, id) DO UPDATE SET path=?3, seen=?4")?
            .execute(params![relation, id, text, now_ms()])?;
        Ok(previous.filter(|p| *p != text))
    }

    /// Replica identities with records in a relation.
    pub fn replicas_with_records(&self, relation: &str) -> Result<BTreeSet<String>> {
        let mut stmt = self.conn.prepare_cached("SELECT DISTINCT replica FROM record WHERE rel=?1")?;
        let rows = stmt.query_map(params![relation], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    /// Drop the records, unresolved marks, and path of one replica.
    pub fn forget_replica(&self, relation: &str, id: &str) -> Result<usize> {
        let n = self.conn.prepare_cached("DELETE FROM record WHERE rel=?1 AND replica=?2")?.execute(params![relation, id])?;
        self.conn.prepare_cached("DELETE FROM unresolved WHERE rel=?1 AND replica=?2")?.execute(params![relation, id])?;
        self.conn.prepare_cached("DELETE FROM replica WHERE rel=?1 AND id=?2")?.execute(params![relation, id])?;
        Ok(n)
    }

    // Records.

    /// Load the bases of a relation in one query.
    pub fn records_for(&self, relation: &str) -> Result<RecordMap> {
        let mut stmt = self.conn.prepare_cached("SELECT key, replica, present, mtime, size, hash FROM record WHERE rel=?1")?;
        let rows = stmt.query_map(params![relation], |r| {
            let key: String = r.get(0)?;
            let replica: String = r.get(1)?;
            let present: i64 = r.get(2)?;
            let basis = if present == 0 {
                Basis::Tombstone
            } else {
                let hash: Vec<u8> = r.get(5)?;
                Basis::Snapshot(Snapshot {
                    mtime: from_epoch_ms(r.get(3)?),
                    size: r.get::<_, i64>(4)? as u64,
                    hash: hash.try_into().unwrap_or([0; 32]),
                })
            };
            Ok((key, replica, basis))
        })?;
        let mut out: RecordMap = BTreeMap::new();
        for row in rows {
            let (key, replica, basis) = row?;
            out.entry(key).or_default().insert(replica, basis);
        }
        Ok(out)
    }

    /// Write the basis of a key at a replica. A `None` basis removes the row.
    pub fn commit_record(&self, relation: &str, key: &Key, replica: &str, basis: &Basis) -> Result<()> {
        let key = key.canonical();
        match basis {
            Basis::None => {
                self.conn.prepare_cached("DELETE FROM record WHERE rel=?1 AND key=?2 AND replica=?3")?.execute(params![relation, key, replica])?;
            }
            Basis::Tombstone => {
                self.conn.prepare_cached(
                    "INSERT INTO record (rel, key, replica, present, mtime, size, hash) VALUES (?1,?2,?3,0,NULL,NULL,NULL)
                     ON CONFLICT (rel, key, replica) DO UPDATE SET present=0, mtime=NULL, size=NULL, hash=NULL",
                )?.execute(params![relation, key, replica])?;
            }
            Basis::Snapshot(s) => {
                self.conn.prepare_cached(
                    "INSERT INTO record (rel, key, replica, present, mtime, size, hash) VALUES (?1,?2,?3,1,?4,?5,?6)
                     ON CONFLICT (rel, key, replica) DO UPDATE SET present=1, mtime=?4, size=?5, hash=?6",
                )?.execute(params![relation, key, replica, to_epoch_ms(s.mtime), s.size as i64, s.hash.to_vec()])?;
            }
        }
        Ok(())
    }

    /// Drop the records and unresolved marks of a key at all replicas.
    pub fn forget(&self, relation: &str, key: &Key) -> Result<()> {
        let key = key.canonical();
        self.conn.prepare_cached("DELETE FROM record WHERE rel=?1 AND key=?2")?.execute(params![relation, key])?;
        self.conn.prepare_cached("DELETE FROM unresolved WHERE rel=?1 AND key=?2")?.execute(params![relation, key])?;
        Ok(())
    }

    // Unresolved marks.

    /// Keys of a relation with an unresolved mark on any replica.
    pub fn unresolved_for(&self, relation: &str) -> Result<BTreeSet<String>> {
        let mut stmt = self.conn.prepare_cached("SELECT DISTINCT key FROM unresolved WHERE rel=?1")?;
        let rows = stmt.query_map(params![relation], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    /// Mark a replica of a key unresolved with a reason.
    pub fn mark_unresolved(&self, relation: &str, key: &Key, replica: &str, reason: &str) -> Result<()> {
        self.conn.prepare_cached(
            "INSERT INTO unresolved (rel, key, replica, reason) VALUES (?1,?2,?3,?4) ON CONFLICT (rel, key, replica) DO UPDATE SET reason=?4",
        )?.execute(params![relation, key.canonical(), replica, reason])?;
        Ok(())
    }

    /// Clear the unresolved mark of a replica of a key.
    pub fn clear_unresolved(&self, relation: &str, key: &Key, replica: &str) -> Result<()> {
        self.conn.prepare_cached("DELETE FROM unresolved WHERE rel=?1 AND key=?2 AND replica=?3")?.execute(params![relation, key.canonical(), replica])?;
        Ok(())
    }

    // The intent journal.

    /// Journal a plan before it applies.
    pub fn write_intent(&self, relation: &str, key: &Key, detail: &str) -> Result<()> {
        self.conn.prepare_cached(
            "INSERT INTO intent (rel, key, detail, started) VALUES (?1,?2,?3,?4) ON CONFLICT (rel, key) DO UPDATE SET detail=?3, started=?4",
        )?.execute(params![relation, key.canonical(), detail, now_ms()])?;
        Ok(())
    }

    /// Remove the journal row of a key.
    pub fn clear_intent(&self, relation: &str, key: &Key) -> Result<()> {
        self.conn.prepare_cached("DELETE FROM intent WHERE rel=?1 AND key=?2")?.execute(params![relation, key.canonical()])?;
        Ok(())
    }

    /// Journal rows left behind, meaning a plan did not finish.
    pub fn stale_intents(&self) -> Result<Vec<Intent>> {
        let mut stmt = self.conn.prepare("SELECT rel, key, detail FROM intent ORDER BY started")?;
        let rows = stmt.query_map([], |r| Ok(Intent { relation: r.get(0)?, key: Key::parse(&r.get::<_, String>(1)?), detail: r.get(2)? }))?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    /// Row counts of records and intents, for the dashboard.
    pub fn stats(&self) -> Result<(u64, u64)> {
        let records: i64 = self.conn.query_row("SELECT COUNT(*) FROM record", [], |r| r.get(0))?;
        let intents: i64 = self.conn.query_row("SELECT COUNT(*) FROM intent", [], |r| r.get(0))?;
        Ok((records as u64, intents as u64))
    }

    // Archive rows. The blob store holds the bytes; these rows say which blobs are archived revisions and of what.

    /// Insert an archive row, returning its id so eviction can spare it.
    pub fn archive_insert(&self, relation: &str, key: &Key, replica: &str, path: &Path, hash: &Hash, size: u64, reason: &str) -> Result<i64> {
        self.conn.prepare_cached(
            "INSERT INTO archive (rel, key, replica, path, hash, size, reason, at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        )?.execute(params![relation, key.canonical(), replica, path.display().to_string(), hash.to_vec(), size as i64, reason, now_ms()])?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Bytes held by distinct archived hashes.
    pub fn archive_unique_bytes(&self) -> Result<u64> {
        let n: i64 = self.conn.query_row("SELECT COALESCE(SUM(size),0) FROM (SELECT DISTINCT hash, size FROM archive)", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Archive rows and unique bytes together, for the dashboard.
    pub fn archive_stats(&self) -> Result<(u64, u64)> {
        let rows: i64 = self.conn.query_row("SELECT COUNT(*) FROM archive", [], |r| r.get(0))?;
        Ok((rows as u64, self.archive_unique_bytes()?))
    }

    /// The relation holding the most archived bytes, leaving row `keep` out.
    pub fn archive_largest_relation(&self, keep: i64) -> Result<Option<String>> {
        Ok(self.conn.prepare_cached("SELECT rel FROM archive WHERE id<>?1 GROUP BY rel ORDER BY SUM(size) DESC LIMIT 1")?.query_row(params![keep], |r| r.get(0)).optional()?)
    }

    /// The oldest archive row of a relation other than `keep`, as id and hash.
    pub fn archive_oldest_row(&self, relation: &str, keep: i64) -> Result<Option<(i64, Hash)>> {
        let row: Option<(i64, Vec<u8>)> = self.conn.prepare_cached("SELECT id, hash FROM archive WHERE rel=?1 AND id<>?2 ORDER BY at ASC, id ASC LIMIT 1")?
            .query_row(params![relation, keep], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        Ok(row.map(|(id, h)| (id, h.try_into().unwrap_or([0; 32]))))
    }

    /// Delete an archive row by id.
    pub fn archive_delete_row(&self, id: i64) -> Result<()> {
        self.conn.prepare_cached("DELETE FROM archive WHERE id=?1")?.execute(params![id])?;
        Ok(())
    }

    /// How many archive rows refer to a hash.
    pub fn archive_refcount(&self, hash: &Hash) -> Result<u64> {
        let n: i64 = self.conn.prepare_cached("SELECT COUNT(*) FROM archive WHERE hash=?1")?.query_row(params![hash.to_vec()], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Archive rows of a relation, newest first.
    pub fn archive_entries(&self, relation: &str) -> Result<Vec<ArchiveEntry>> {
        let mut stmt = self.conn.prepare_cached("SELECT key, replica, path, hash, size, reason, at FROM archive WHERE rel=?1 ORDER BY at DESC, id DESC")?;
        let rows = stmt.query_map(params![relation], |r| Ok(ArchiveEntry {
            key: Key::parse(&r.get::<_, String>(0)?).to_string(),
            replica: r.get(1)?,
            path: r.get(2)?,
            hash: r.get::<_, Vec<u8>>(3)?.try_into().unwrap_or([0; 32]),
            size: r.get::<_, i64>(4)? as u64,
            reason: r.get(5)?,
            at: from_epoch_ms(r.get::<_, i64>(6)?),
        }))?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn snap(hash: u8) -> Snapshot {
        Snapshot { mtime: from_epoch_ms(1_700_000_000_000), size: 3, hash: [hash; 32] }
    }

    #[test]
    fn records_round_trip_through_each_basis() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(&tmp.path().join("state.db")).unwrap();
        let key = Key::parse("path=f.txt");
        store.commit_record("r", &key, "a", &Basis::Snapshot(snap(1))).unwrap();
        store.commit_record("r", &key, "b", &Basis::Tombstone).unwrap();
        store.commit_record("r", &key, "c", &Basis::None).unwrap();
        let records = store.records_for("r").unwrap();
        let row = &records[&key.canonical()];
        assert_eq!(row["a"], Basis::Snapshot(snap(1)));
        assert_eq!(row["b"], Basis::Tombstone);
        assert!(!row.contains_key("c"));
        assert_eq!(store.replicas_with_records("r").unwrap(), BTreeSet::from(["a".to_string(), "b".to_string()]));
        store.forget("r", &key).unwrap();
        assert!(store.records_for("r").unwrap().is_empty());
    }

    #[test]
    fn settings_fingerprints_replica_paths_and_intents() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(&tmp.path().join("state.db")).unwrap();
        assert_eq!(store.flag("gate").unwrap(), None);
        store.set_flag("gate", true).unwrap();
        assert_eq!(store.flag("gate").unwrap(), Some(true));
        assert_eq!(store.fingerprint("r").unwrap(), None);
        store.set_fingerprint("r", "abc").unwrap();
        assert_eq!(store.fingerprint("r").unwrap().as_deref(), Some("abc"));
        assert_eq!(store.note_replica("r", "dest", Path::new("/a")).unwrap(), None);
        assert_eq!(store.note_replica("r", "dest", Path::new("/a")).unwrap(), None);
        assert_eq!(store.note_replica("r", "dest", Path::new("/b")).unwrap().as_deref(), Some("/a"));
        let key = Key::parse("path=f.txt");
        store.write_intent("r", &key, "write dest").unwrap();
        let stale = store.stale_intents().unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!((stale[0].relation.as_str(), stale[0].key.clone(), stale[0].detail.as_str()), ("r", key.clone(), "write dest"));
        store.clear_intent("r", &key).unwrap();
        assert!(store.stale_intents().unwrap().is_empty());
        store.mark_unresolved("r", &key, "dest", "conflict").unwrap();
        assert_eq!(store.unresolved_for("r").unwrap(), BTreeSet::from([key.canonical()]));
        store.clear_unresolved("r", &key, "dest").unwrap();
        assert!(store.unresolved_for("r").unwrap().is_empty());
    }

    #[test]
    fn a_foreign_schema_version_is_refused() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("state.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE setting (key TEXT PRIMARY KEY, value TEXT NOT NULL); INSERT INTO setting VALUES ('schema_version', '1');").unwrap();
        }
        let err = match Store::open(&path) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("an old schema was accepted"),
        };
        assert!(err.contains("schema version 1") && err.contains("version 2"), "{err}");
    }
}
