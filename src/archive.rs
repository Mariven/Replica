//! Content-addressed file archive storing revisions before destructive file modifications or removals.
//! Write revisions to disk indexed by BLAKE3 digest to deduplicate identical content across revisions.
//! Track file paths and content digests in the database archive table to preserve revision history.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::identifier::Identifier;
use crate::store::Store;

/// Content-addressed blob repository managing files in `dir` up to a `cap` byte limit.
/// Invoke [`Archive::preserve`] before modifying or removing target files.
#[derive(Clone)]
pub struct Archive {
    pub dir: PathBuf,
    pub cap: u64,
}

impl Archive {
    /// Save payload bytes into `dir` under a content-addressed filename, recording revision metadata in `store` before evicting records past `cap`.
    /// Return filesystem and database errors immediately, halting execution before destructive file modifications proceed.
    pub fn preserve(&self, store: &Store, relation: &str, ident: &Identifier, ref_id: &str, path: &Path, bytes: &[u8], reason: &str) -> Result<()> {
        // Hash the payload with BLAKE3 to determine the content-addressed filename.
        let hex = blake3::hash(bytes).to_hex().to_string();
        fs::create_dir_all(&self.dir).with_context(|| format!("creating archive dir {}", self.dir.display()))?;
        let blob = self.dir.join(&hex);
        if !blob.exists() {
            // Write to a temporary file, flush it to disk, then rename atomically so a crash never leaves a half-written blob under its final name.
            let tmp = self.dir.join(format!(".{hex}.tmp"));
            // Archived revisions may come from private files, so the blob is readable by the owner alone.
            let mut f = {
                use std::os::unix::fs::OpenOptionsExt;
                fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)
                    .with_context(|| format!("creating archive blob {}", tmp.display()))?
            };
            f.write_all(bytes).with_context(|| format!("writing archive blob {}", tmp.display()))?;
            f.sync_all().with_context(|| format!("syncing archive blob {}", tmp.display()))?;
            drop(f);
            fs::rename(&tmp, &blob).with_context(|| format!("renaming archive blob {} to {}", tmp.display(), blob.display()))?;
            // A directory sync makes the rename durable; filesystems that refuse it still hold the synced file contents.
            if let Ok(d) = fs::File::open(&self.dir) {
                let _ = d.sync_all();
            }
        }
        // Register revision metadata in the store, then prune entries past the byte limit while sparing the row just written.
        let id = store.archive_insert(relation, ident, ref_id, path, &hex, bytes.len() as u64, reason)?;
        self.evict(store, id)
    }

    /// Prune archived revisions while unique blob bytes exceed `cap`, never touching row `keep`, the revision this call just preserved.
    /// Select the relation with highest storage consumption, delete its oldest row, and unlink the backing blob when no database rows reference it.
    fn evict(&self, store: &Store, keep: i64) -> Result<()> {
        while store.archive_unique_bytes()? > self.cap {
            let Some(rel) = store.archive_largest_relation(keep)? else { break };
            let Some((row_id, hex)) = store.archive_oldest_row(&rel, keep)? else { break };
            store.archive_delete_row(row_id)?;
            // Delete the on-disk blob when no database rows reference its digest.
            if store.archive_hash_refcount(&hex)? == 0 {
                let _ = fs::remove_file(self.dir.join(&hex));
            }
        }
        Ok(())
    }

    /// Read blob bytes from disk matching the specified hex digest.
    /// Require 64 hexadecimal ASCII characters in `hex` before reading the file to prevent directory traversal.
    pub fn read_blob(&self, hex: &str) -> Result<Vec<u8>> {
        fs::read(self.blob_path(hex)?).with_context(|| format!("archive blob {hex}"))
    }

    /// Resolve a digest to its blob path, rejecting anything that is not a 64-character hex string.
    pub fn blob_path(&self, hex: &str) -> Result<PathBuf> {
        if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            anyhow::bail!("not a content hash: {hex:?}");
        }
        Ok(self.dir.join(hex))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// The revision being preserved is never the one evicted, even when its relation holds the most bytes and it is that relation's only row.
    #[test]
    fn evict_spares_the_revision_just_preserved() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(&tmp.path().join("state.db")).unwrap();
        let archive = Archive { dir: tmp.path().join("archive"), cap: 50 };
        let ident = vec!["f.txt".to_string()];
        archive.preserve(&store, "small", &ident, "dest", Path::new("/x/f.txt"), b"0123456789", "overwritten").unwrap();
        // 100 bytes for relation `big` pushes the archive past the cap; `big` is the largest relation and this row is its oldest.
        let payload = vec![b'x'; 100];
        archive.preserve(&store, "big", &ident, "dest", Path::new("/y/f.txt"), &payload, "overwritten").unwrap();
        let big = store.archive_entries("big").unwrap();
        assert_eq!(big.len(), 1, "the new revision survives eviction");
        assert_eq!(fs::read(archive.dir.join(&big[0].hash)).unwrap(), payload);
        assert!(store.archive_entries("small").unwrap().is_empty(), "the older revision is the one evicted");
    }
}
