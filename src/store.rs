//! Persist synchronization records, mutation intent journals, and archive metadata in SQLite.
//!
//! Record file states under relation name, item identifier, and replica reference.
//! Stage filesystem modifications in the intent table before writing to disk.
//! Remove committed intents after successful writes to isolate interrupted operations across runs.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::identifier::{Identifier, identifier_from_string, identifier_to_string};
use crate::status::Snapshot;

/// Schema version written to the `setting` table on open. Bump it when a migration becomes necessary.
pub const SCHEMA_VERSION: u32 = 1;

/// Every record of one relation: identifier string, then ref id, then the recorded snapshot (None for a recorded deletion).
pub type RecordMap = BTreeMap<String, BTreeMap<String, Option<Snapshot>>>;

/// Manage SQLite storage for synchronization records, unresolved conflicts, mutation intents, and archive metadata.
pub struct Store {
    conn: Connection,
}

/// Convert a system time to milliseconds since the Unix epoch for storage.
pub fn to_epoch_ms(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_millis() as i64
}

/// Convert stored milliseconds since the Unix epoch back to a system time.
pub fn from_epoch_ms(ms: i64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64)
}

impl Store {
    /// Create any missing parent directories, connect to the SQLite database at path, and apply schema definitions.
    /// Use write-ahead logging so the dashboard's reads never block the engine's writes and small commits avoid rewriting the rollback journal.
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS setting (
                key     TEXT PRIMARY KEY,
                value   TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS record (
                rel     TEXT NOT NULL,
                ident   TEXT NOT NULL,
                ref     TEXT NOT NULL,
                mtime   INTEGER,
                size    INTEGER,
                hash    BLOB,
                PRIMARY KEY (rel, ident, ref)
            );
            CREATE TABLE IF NOT EXISTS unresolved (
                rel     TEXT NOT NULL,
                ident   TEXT NOT NULL,
                ref     TEXT NOT NULL,
                reason  TEXT NOT NULL,
                PRIMARY KEY (rel, ident, ref)
            );
            CREATE TABLE IF NOT EXISTS intent (
                rel     TEXT NOT NULL,
                ident   TEXT NOT NULL,
                detail  TEXT NOT NULL,
                started INTEGER NOT NULL,
                PRIMARY KEY (rel, ident)
            );
            CREATE TABLE IF NOT EXISTS archive (
                id      INTEGER PRIMARY KEY AUTOINCREMENT,
                rel     TEXT NOT NULL,
                ident   TEXT NOT NULL,
                ref     TEXT NOT NULL,
                path    TEXT NOT NULL,
                hash    TEXT NOT NULL,
                size    INTEGER NOT NULL,
                reason  TEXT NOT NULL,
                at      INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS archive_hash_size ON archive (hash, size);
            CREATE INDEX IF NOT EXISTS archive_rel_size ON archive (rel, size);
            CREATE INDEX IF NOT EXISTS archive_rel_at_id ON archive (rel, at, id);",
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO setting (key, value) VALUES ('schema_version', ?1)",
            params![SCHEMA_VERSION.to_string()],
        )?;
        let store = Store { conn };
        let found = store.schema_version()?;
        if found > SCHEMA_VERSION {
            anyhow::bail!("{} was written by a newer replica (schema version {found}, this binary knows {SCHEMA_VERSION})", path.display());
        }
        Ok(store)
    }

    /// Read the schema version recorded when the database was first created.
    pub fn schema_version(&self) -> Result<u32> {
        let v: String = self.conn.query_row("SELECT value FROM setting WHERE key='schema_version'", [], |r| r.get(0))?;
        Ok(v.parse().unwrap_or(0))
    }

    /// Run `f` inside one transaction, committing on success and rolling back on error.
    pub fn transaction<T>(&self, f: impl FnOnce(&Store) -> Result<T>) -> Result<T> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let outcome = f(self).and_then(|v| self.conn.execute_batch("COMMIT").map(|_| v).map_err(Into::into));
        if outcome.is_err() {
            // A failed COMMIT leaves the transaction open; roll it back so later statements do not join it.
            let _ = self.conn.execute_batch("ROLLBACK");
        }
        outcome
    }

    /// Load every record of a relation in one query, keyed by identifier string and then by ref id.
    /// A None snapshot is a recorded deletion; an absent key means the ref was never recorded for that identifier.
    pub fn records_for(&self, relation: &str) -> Result<RecordMap> {
        let mut stmt = self.conn.prepare_cached("SELECT ident, ref, mtime, size, hash FROM record WHERE rel=?1")?;
        let rows = stmt.query_map(params![relation], |r| {
            let ident: String = r.get(0)?;
            let rf: String = r.get(1)?;
            let mtime: Option<i64> = r.get(2)?;
            let size: Option<i64> = r.get(3)?;
            let hash: Option<Vec<u8>> = r.get(4)?;
            let snap = match (mtime, size, hash) {
                (Some(m), Some(s), Some(h)) => Some(Snapshot {
                    mtime: from_epoch_ms(m),
                    size: s as u64,
                    hash: h.try_into().unwrap_or([0; 32]),
                }),
                _ => None,
            };
            Ok((ident, rf, snap))
        })?;
        let mut out: RecordMap = BTreeMap::new();
        for row in rows {
            let (ident, rf, snap) = row?;
            out.entry(ident).or_default().insert(rf, snap);
        }
        Ok(out)
    }

    /// Load the identifiers of a relation that carry an unresolved conflict on any ref.
    pub fn unresolved_for(&self, relation: &str) -> Result<BTreeSet<String>> {
        let mut stmt = self.conn.prepare_cached("SELECT DISTINCT ident FROM unresolved WHERE rel=?1")?;
        let rows = stmt.query_map(params![relation], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<BTreeSet<String>>>().map_err(Into::into)
    }

    /// Upsert snapshot metadata into the record table for a replica ref, writing NULL columns when recording an absent file.
    pub fn commit_record(&self, relation: &str, ident: &Identifier, rf: &str, snapshot: Option<&Snapshot>) -> Result<()> {
        let (mtime, size, hash) = match snapshot {
            Some(s) => (Some(to_epoch_ms(s.mtime)), Some(s.size as i64), Some(s.hash.to_vec())),
            None => (None, None, None),
        };
        self.conn.prepare_cached(
            "INSERT INTO record (rel, ident, ref, mtime, size, hash) VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT (rel, ident, ref) DO UPDATE SET mtime=?4, size=?5, hash=?6",
        )?.execute(params![relation, identifier_to_string(ident), rf, mtime, size, hash])?;
        Ok(())
    }

    /// Delete snapshot records and unresolved conflicts for an identifier across all refs in a relation.
    pub fn forget(&self, relation: &str, ident: &Identifier) -> Result<()> {
        let ident = identifier_to_string(ident);
        self.conn.prepare_cached("DELETE FROM record WHERE rel=?1 AND ident=?2")?.execute(params![relation, ident])?;
        self.conn.prepare_cached("DELETE FROM unresolved WHERE rel=?1 AND ident=?2")?.execute(params![relation, ident])?;
        Ok(())
    }

    /// Record an unresolved conflict reason for an identifier on a replica ref.
    pub fn mark_unresolved(&self, relation: &str, ident: &Identifier, rf: &str, reason: &str) -> Result<()> {
        self.conn.prepare_cached(
            "INSERT INTO unresolved (rel, ident, ref, reason) VALUES (?1,?2,?3,?4)
             ON CONFLICT (rel, ident, ref) DO UPDATE SET reason=?4",
        )?.execute(params![relation, identifier_to_string(ident), rf, reason])?;
        Ok(())
    }

    /// Remove the unresolved conflict entry for an identifier and replica ref upon resolution.
    pub fn clear_unresolved(&self, relation: &str, ident: &Identifier, rf: &str) -> Result<()> {
        self.conn.prepare_cached("DELETE FROM unresolved WHERE rel=?1 AND ident=?2 AND ref=?3")?
            .execute(params![relation, identifier_to_string(ident), rf])?;
        Ok(())
    }

    /// Log an intended filesystem mutation with the current timestamp before applying changes.
    pub fn write_intent(&self, relation: &str, ident: &Identifier, detail: &str) -> Result<()> {
        self.conn.prepare_cached(
            "INSERT INTO intent (rel, ident, detail, started) VALUES (?1,?2,?3,?4)
             ON CONFLICT (rel, ident) DO UPDATE SET detail=?3, started=?4",
        )?.execute(params![relation, identifier_to_string(ident), detail, to_epoch_ms(SystemTime::now())])?;
        Ok(())
    }

    /// Remove the logged intent entry for an identifier after completing a filesystem change.
    pub fn clear_intent(&self, relation: &str, ident: &Identifier) -> Result<()> {
        self.conn.prepare_cached("DELETE FROM intent WHERE rel=?1 AND ident=?2")?
            .execute(params![relation, identifier_to_string(ident)])?;
        Ok(())
    }

    /// Query row counts from the record and intent tables, returning both numbers.
    pub fn stats(&self) -> Result<(u64, u64)> {
        let records: i64 = self.conn.query_row("SELECT COUNT(*) FROM record", [], |r| r.get(0))?;
        let intents: i64 = self.conn.query_row("SELECT COUNT(*) FROM intent", [], |r| r.get(0))?;
        Ok((records as u64, intents as u64))
    }

    // Archive storage separates metadata from content files.
    // SQLite tracks paths, timestamps, and content hashes, while the archive directory holds payload files under their hash.
    // During eviction, queries scan index rows chronologically to select candidate hashes for deletion.

    /// Insert revision metadata into the archive table alongside the current timestamp, returning the new row id so eviction can spare it.
    pub fn archive_insert(&self, relation: &str, ident: &Identifier, ref_id: &str, path: &Path, hex: &str, size: u64, reason: &str) -> Result<i64> {
        self.conn.prepare_cached(
            "INSERT INTO archive (rel, ident, ref, path, hash, size, reason, at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        )?.execute(params![relation, identifier_to_string(ident), ref_id, path.display().to_string(), hex, size as i64, reason, to_epoch_ms(SystemTime::now())])?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Calculate total bytes across distinct payload hashes in the archive table.
    pub fn archive_unique_bytes(&self) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(size),0) FROM (SELECT DISTINCT hash, size FROM archive)", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Read the total number of entries in the archive table.
    pub fn archive_row_count(&self) -> Result<u64> {
        let n: i64 = self.conn.query_row("SELECT COUNT(*) FROM archive", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Select the relation with the highest aggregate byte size in the archive for eviction, leaving row `keep` out of the sums.
    pub fn archive_largest_relation(&self, keep: i64) -> Result<Option<String>> {
        Ok(self.conn.prepare_cached(
            "SELECT rel FROM archive WHERE id<>?1 GROUP BY rel ORDER BY SUM(size) DESC LIMIT 1")?
            .query_row(params![keep], |r| r.get(0)).optional()?)
    }

    /// Fetch the earliest row id and content hash for a relation, never returning row `keep`.
    pub fn archive_oldest_row(&self, relation: &str, keep: i64) -> Result<Option<(i64, String)>> {
        Ok(self.conn.prepare_cached(
            "SELECT id, hash FROM archive WHERE rel=?1 AND id<>?2 ORDER BY at ASC, id ASC LIMIT 1")?
            .query_row(params![relation, keep], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
    }

    /// Delete an archive entry by ID, leaving payload file removal on disk to reference count checks.
    pub fn archive_delete_row(&self, id: i64) -> Result<()> {
        self.conn.prepare_cached("DELETE FROM archive WHERE id=?1")?.execute(params![id])?;
        Ok(())
    }

    /// Tally archive entries for a specific content hash to verify reference counts.
    pub fn archive_hash_refcount(&self, hex: &str) -> Result<u64> {
        let n: i64 = self.conn.prepare_cached("SELECT COUNT(*) FROM archive WHERE hash=?1")?
            .query_row(params![hex], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Query archive entries for a relation from newest to oldest.
    pub fn archive_entries(&self, relation: &str) -> Result<Vec<ArchiveEntry>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT ident, path, hash, size, reason, at FROM archive WHERE rel=?1 ORDER BY at DESC, id DESC")?;
        let rows = stmt.query_map(params![relation], |r| Ok(ArchiveEntry {
            ident: r.get(0)?,
            path: r.get(1)?,
            hash: r.get(2)?,
            size: r.get::<_, i64>(3)? as u64,
            reason: r.get(4)?,
            at: from_epoch_ms(r.get::<_, i64>(5)?),
        }))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Gather total archive row count and unique payload bytes.
    pub fn archive_stats(&self) -> Result<(u64, u64)> {
        Ok((self.archive_row_count()?, self.archive_unique_bytes()?))
    }

    /// Load pending intent records from the table so startup recovery can resolve unfinished identifiers.
    pub fn stale_intents(&self) -> Result<Vec<(String, Identifier, String)>> {
        let mut stmt = self.conn.prepare("SELECT rel, ident, detail FROM intent")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.into_iter().map(|(rel, i, d)| (rel, identifier_from_string(&i), d)).collect())
    }
}

/// Metadata record for an archived file revision.
pub struct ArchiveEntry {
    pub ident: String,
    pub path: String,
    pub hash: String,
    pub size: u64,
    pub reason: String,
    pub at: SystemTime,
}
