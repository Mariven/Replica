//! Content we keep when about to overwrite or delete a file.
//!
//! Each hash lives in a single blob file. Copies of the same content share it.
//! The store's archive table remembers which path held which hash, and when, so the history page can list versions without storing the bytes twice.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::identifier::Identifier;
use crate::store::Store;

/// We store blobs in `dir`, keyed by content hash, and keep unique content under `cap` bytes.
/// Call [`Archive::preserve`] before overwriting or deleting a file.
#[derive(Clone)]
pub struct Archive {
    pub dir: PathBuf,
    pub cap: u64,
}

impl Archive {
    /// Save these bytes (they are about to be overwritten or deleted), then evict down to the cap.
    /// If the save fails we abort the overwrite or delete, so the only remaining copy stays on disk.
    pub fn preserve(&self, store: &Store, relation: &str, ident: &Identifier, ref_id: &str, path: &Path, bytes: &[u8], reason: &str) -> Result<()> {
        let hex = blake3::hash(bytes).to_hex().to_string();
        fs::create_dir_all(&self.dir).with_context(|| format!("creating archive dir {}", self.dir.display()))?;
        let blob = self.dir.join(&hex);
        if !blob.exists() {
            let tmp = self.dir.join(format!(".{hex}.tmp"));
            fs::write(&tmp, bytes)?;
            fs::rename(&tmp, &blob)?;
        }
        store.archive_insert(relation, ident, ref_id, path, &hex, bytes.len() as u64, reason)?;
        self.evict(store)
    }

    /// While unique blob bytes exceed the cap, delete the oldest archive row of the relation using the most space.
    /// When no row still points at a blob, delete the blob file.
    /// The newest row is always kept, even if that one file is larger than the whole cap.
    fn evict(&self, store: &Store) -> Result<()> {
        while store.archive_unique_bytes()? > self.cap && store.archive_row_count()? > 1 {
            let Some(rel) = store.archive_largest_relation()? else { break };
            let Some((row_id, hex)) = store.archive_oldest_row(&rel)? else { break };
            store.archive_delete_row(row_id)?;
            if store.archive_hash_refcount(&hex)? == 0 {
                let _ = fs::remove_file(self.dir.join(&hex));
            }
        }
        Ok(())
    }

    /// Return the blob bytes for this hex hash.
    /// `hex` comes from query strings, so we only accept 64 hex digits.
    pub fn read_blob(&self, hex: &str) -> Result<Vec<u8>> {
        if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            anyhow::bail!("not a content hash: {hex:?}");
        }
        fs::read(self.dir.join(hex)).with_context(|| format!("archive blob {hex}"))
    }
}
