//! The last-sync record and the intent journal.
//!
//! Each (relation, identifier, ref) has a record row.
//! We write an intent before a mutation and clear it after commit. A leftover intent means the last run died mid-write.

use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::identifier::{Identifier, identifier_from_string, identifier_to_string};
use crate::status::Snapshot;

/// The SQLite file that holds the last-sync record, the intent journal, and the archive index.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open the SQLite file (creating it if needed) and make sure the tables exist.
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS record (
                rel     TEXT NOT NULL,
                ident   TEXT NOT NULL,
                ref     TEXT NOT NULL,
                mtime   INTEGER,
                size    INTEGER,
                hash    BLOB,
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
            );",
        )?;
        Ok(Store { conn })
    }

    /// Fetch the record for this slot.
    /// Outer `None` means we have never recorded this slot. Inner `None` means we recorded that the file was absent.
    pub fn record(&self, relation: &str, ident: &Identifier, rf: &str) -> Result<Option<Option<Snapshot>>> {
        let row = self.conn.query_row(
            "SELECT mtime, size, hash FROM record WHERE rel=?1 AND ident=?2 AND ref=?3",
            params![relation, identifier_to_string(ident), rf],
            |r| {
                let mtime: Option<i64> = r.get(0)?;
                let size: Option<i64> = r.get(1)?;
                let hash: Option<Vec<u8>> = r.get(2)?;
                Ok(match (mtime, size, hash) {
                    (Some(m), Some(s), Some(h)) => Some(Snapshot {
                        mtime: SystemTime::UNIX_EPOCH + Duration::from_millis(m as u64),
                        size: s as u64,
                        hash: h.try_into().unwrap_or([0; 32]),
                    }),
                    _ => None,
                })
            },
        ).optional()?;
        Ok(row)
    }

    /// Save the snapshot we just wrote, or record that the file is absent.
    pub fn commit_record(&self, relation: &str, ident: &Identifier, rf: &str, snapshot: Option<&Snapshot>) -> Result<()> {
        let (mtime, size, hash) = match snapshot {
            Some(s) => (
                Some(s.mtime.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_millis() as i64),
                Some(s.size as i64),
                Some(s.hash.to_vec()),
            ),
            None => (None, None, None),
        };
        self.conn.execute(
            "INSERT INTO record (rel, ident, ref, mtime, size, hash) VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT (rel, ident, ref) DO UPDATE SET mtime=?4, size=?5, hash=?6",
            params![relation, identifier_to_string(ident), rf, mtime, size, hash],
        )?;
        Ok(())
    }

    /// Identifiers that have any record for this relation.
    /// We include these so a file deleted at every visible ref is still found.
    pub fn known_identifiers(&self, relation: &str) -> Result<Vec<Identifier>> {
        let mut stmt = self.conn.prepare("SELECT DISTINCT ident FROM record WHERE rel=?1")?;
        let idents = stmt
            .query_map(params![relation], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(idents.iter().map(|i| identifier_from_string(i)).collect())
    }

    /// This ref has committed a present snapshot at some point.
    /// That distinguishes a brand-new root from a recorded root that disappeared (often an unmounted volume).
    pub fn ref_has_records(&self, relation: &str, rf: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM record WHERE rel=?1 AND ref=?2 AND mtime IS NOT NULL",
            params![relation, rf],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Delete the record rows for an identifier that is now missing everywhere.
    pub fn forget(&self, relation: &str, ident: &Identifier) -> Result<()> {
        self.conn.execute("DELETE FROM record WHERE rel=?1 AND ident=?2", params![relation, identifier_to_string(ident)])?;
        Ok(())
    }

    /// Write the operations we are about to run, so a crash leaves a trail.
    pub fn write_intent(&self, relation: &str, ident: &Identifier, detail: &str) -> Result<()> {
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_millis() as i64;
        self.conn.execute(
            "INSERT INTO intent (rel, ident, detail, started) VALUES (?1,?2,?3,?4)
             ON CONFLICT (rel, ident) DO UPDATE SET detail=?3, started=?4",
            params![relation, identifier_to_string(ident), detail, now],
        )?;
        Ok(())
    }

    /// Delete the intent after a successful commit.
    pub fn clear_intent(&self, relation: &str, ident: &Identifier) -> Result<()> {
        self.conn.execute("DELETE FROM intent WHERE rel=?1 AND ident=?2", params![relation, identifier_to_string(ident)])?;
        Ok(())
    }

    /// Record-row count and intent-row count. The dashboard status strip uses these.
    pub fn stats(&self) -> Result<(u64, u64)> {
        let records: i64 = self.conn.query_row("SELECT COUNT(*) FROM record", [], |r| r.get(0))?;
        let intents: i64 = self.conn.query_row("SELECT COUNT(*) FROM intent", [], |r| r.get(0))?;
        Ok((records as u64, intents as u64))
    }

    // The archive index: which filenames held which content, and when.
    // Blob bytes live in the Archive directory, keyed by content hash.
    // These rows are the only map from (relation, file, time) back to a hash, so eviction works row-first.

    /// Record that this content (by hash) was at `path` until now.
    pub fn archive_insert(&self, relation: &str, ident: &Identifier, ref_id: &str, path: &Path, hex: &str, size: u64, reason: &str) -> Result<()> {
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_millis() as i64;
        self.conn.execute(
            "INSERT INTO archive (rel, ident, ref, path, hash, size, reason, at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![relation, identifier_to_string(ident), ref_id, path.display().to_string(), hex, size as i64, reason, now],
        )?;
        Ok(())
    }

    /// Total size of distinct blobs. A shared hash counts once.
    pub fn archive_unique_bytes(&self) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(size),0) FROM (SELECT DISTINCT hash, size FROM archive)", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Count the archive index rows.
    pub fn archive_row_count(&self) -> Result<u64> {
        let n: i64 = self.conn.query_row("SELECT COUNT(*) FROM archive", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// The relation whose archived blobs add up to the most bytes. Eviction starts there.
    pub fn archive_largest_relation(&self) -> Result<Option<String>> {
        Ok(self.conn.query_row(
            "SELECT rel FROM archive GROUP BY rel ORDER BY SUM(size) DESC LIMIT 1", [], |r| r.get(0)).optional()?)
    }

    /// The oldest archive row for this relation: its row id and content hash.
    pub fn archive_oldest_row(&self, relation: &str) -> Result<Option<(i64, String)>> {
        Ok(self.conn.query_row(
            "SELECT id, hash FROM archive WHERE rel=?1 ORDER BY at ASC, id ASC LIMIT 1",
            params![relation], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
    }

    /// Delete this archive index row. We remove the blob file separately, once nothing else still points at it.
    pub fn archive_delete_row(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM archive WHERE id=?1", params![id])?;
        Ok(())
    }

    /// Count the archive rows that still point at this hash.
    pub fn archive_hash_refcount(&self, hex: &str) -> Result<u64> {
        let n: i64 = self.conn.query_row("SELECT COUNT(*) FROM archive WHERE hash=?1", params![hex], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Archived versions for this relation, newest first.
    pub fn archive_entries(&self, relation: &str) -> Result<Vec<ArchiveEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT ident, ref, path, hash, size, reason, at FROM archive WHERE rel=?1 ORDER BY at DESC, id DESC")?;
        let rows = stmt.query_map(params![relation], |r| Ok(ArchiveEntry {
            ident: r.get(0)?,
            ref_id: r.get(1)?,
            path: r.get(2)?,
            hash: r.get(3)?,
            size: r.get::<_, i64>(4)? as u64,
            reason: r.get(5)?,
            at: SystemTime::UNIX_EPOCH + Duration::from_millis(r.get::<_, i64>(6)? as u64),
        }))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Row count and unique-byte total. The status strip uses these.
    pub fn archive_stats(&self) -> Result<(u64, u64)> {
        Ok((self.archive_row_count()?, self.archive_unique_bytes()?))
    }

    /// Intent rows left over after a restart: those operations did not finish.
    /// The engine logs them and resolves those identifiers from scratch, rather than replaying the old plan.
    pub fn stale_intents(&self) -> Result<Vec<(String, Identifier, String)>> {
        let mut stmt = self.conn.prepare("SELECT rel, ident, detail FROM intent")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.into_iter().map(|(rel, i, d)| (rel, identifier_from_string(&i), d)).collect())
    }
}

/// A version we kept. This content (by hash) was at this path, under this relation, until `at`.
pub struct ArchiveEntry {
    pub ident: String,
    pub ref_id: String,
    pub path: String,
    pub hash: String,
    pub size: u64,
    pub reason: String,
    pub at: SystemTime,
}
