use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::identifier::{Identifier, identifier_from_string, identifier_to_string};
use crate::status::Snapshot;

// The state store: what is on record from past resolutions, plus an intent journal. One record row per (relation, identifier, ref); intents are written before mutating and cleared after commit, so a surviving intent row is evidence of an unfinished operation.
pub struct Store {
    conn: Connection,
}

impl Store {
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
            );",
        )?;
        Ok(Store { conn })
    }

    // Record row: None row means nothing on record; a row with NULL snapshot means recorded-as-absent.
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

    // Identifiers with any record row for a relation: how deletions at every ref are still noticed.
    pub fn known_identifiers(&self, relation: &str) -> Result<Vec<Identifier>> {
        let mut stmt = self.conn.prepare("SELECT DISTINCT ident FROM record WHERE rel=?1")?;
        let idents = stmt
            .query_map(params![relation], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(idents.iter().map(|i| identifier_from_string(i)).collect())
    }

    // Whether a ref has ever committed anything: distinguishes a fresh root that may
    // be created from a recorded root that has gone missing (likely unmounted).
    pub fn ref_has_records(&self, relation: &str, rf: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM record WHERE rel=?1 AND ref=?2 AND mtime IS NOT NULL",
            params![relation, rf],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    // Rows for identifiers absent everywhere are dropped once resolution confirms it.
    pub fn forget(&self, relation: &str, ident: &Identifier) -> Result<()> {
        self.conn.execute("DELETE FROM record WHERE rel=?1 AND ident=?2", params![relation, identifier_to_string(ident)])?;
        Ok(())
    }

    pub fn write_intent(&self, relation: &str, ident: &Identifier, detail: &str) -> Result<()> {
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_millis() as i64;
        self.conn.execute(
            "INSERT INTO intent (rel, ident, detail, started) VALUES (?1,?2,?3,?4)
             ON CONFLICT (rel, ident) DO UPDATE SET detail=?3, started=?4",
            params![relation, identifier_to_string(ident), detail, now],
        )?;
        Ok(())
    }

    pub fn clear_intent(&self, relation: &str, ident: &Identifier) -> Result<()> {
        self.conn.execute("DELETE FROM intent WHERE rel=?1 AND ident=?2", params![relation, identifier_to_string(ident)])?;
        Ok(())
    }

    // Row counts for the dashboard: (record rows, intent rows).
    pub fn stats(&self) -> Result<(u64, u64)> {
        let records: i64 = self.conn.query_row("SELECT COUNT(*) FROM record", [], |r| r.get(0))?;
        let intents: i64 = self.conn.query_row("SELECT COUNT(*) FROM intent", [], |r| r.get(0))?;
        Ok((records as u64, intents as u64))
    }

    // Intents surviving a restart: the operation did not finish. The engine logs them and resolves those identifiers fresh rather than replaying stale plans.
    pub fn stale_intents(&self) -> Result<Vec<(String, Identifier, String)>> {
        let mut stmt = self.conn.prepare("SELECT rel, ident, detail FROM intent")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.into_iter().map(|(rel, i, d)| (rel, identifier_from_string(&i), d)).collect())
    }
}
