//! The content-addressed blob store beside the state database.
//!
//! It holds copies of overwritten and deleted files, the content staged for each write in a plan, and the output of transforms. Each blob is a file named by the hex of its BLAKE3 hash. New content is written to a temporary file and renamed into place, so after a crash any blob under its final name is complete.
//! The store keeps only bytes, filed under their hashes. The `archive` table of the state database records where each archived revision came from.

use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::observe::{Hash, from_hex, hex};
use crate::store::Store;

/// The blob directory and the size cap for archived revisions. Old revisions are evicted when the archive grows past the cap.
#[derive(Clone, Debug)]
pub struct Blobs {
    pub dir: PathBuf,
    pub cap: u64,
}

/// A reader wrapper that hashes and counts the bytes read through it.
struct Counting<'a, R: Read> {
    inner: R,
    hasher: &'a mut blake3::Hasher,
    size: u64,
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.size += n as u64;
        Ok(n)
    }
}

impl Blobs {
    /// Path of the blob for a hash.
    pub fn path(&self, hash: &Hash) -> PathBuf {
        self.dir.join(hex(hash))
    }

    /// Path of the blob for a hex digest. Only a well-formed digest is accepted, so a dashboard request cannot point outside the blob directory.
    pub fn path_for_hex(&self, digest: &str) -> Result<PathBuf> {
        let hash = from_hex(digest).with_context(|| format!("not a content hash: {digest:?}"))?;
        Ok(self.path(&hash))
    }

    /// True when the blob for a hash is on disk.
    pub fn has(&self, hash: &Hash) -> bool {
        self.path(hash).is_file()
    }

    /// Open a blob for reading.
    pub fn open(&self, hash: &Hash) -> Result<fs::File> {
        fs::File::open(self.path(hash)).with_context(|| format!("blob {}", hex(hash)))
    }

    /// Copy content into the store and return its hash and size. If a blob with that hash already exists, the existing blob is kept and the new copy is discarded.
    pub fn stage(&self, content: &mut dyn Read) -> Result<(Hash, u64)> {
        fs::create_dir_all(&self.dir).with_context(|| format!("creating blob dir {}", self.dir.display()))?;
        // The hash is unknown until all the bytes are read, so they are written to a temporary file with a unique name first.
        let tmp = self.dir.join(format!(".staging-{}-{}", std::process::id(), unique_suffix()));
        let mut hasher = blake3::Hasher::new();
        let (hash, size) = {
            // Blobs may hold private content, so only the owner can read the file.
            let mut f = {
                use std::os::unix::fs::OpenOptionsExt;
                fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp).with_context(|| format!("creating staging file {}", tmp.display()))?
            };
            let size = {
                let mut counting = Counting { inner: content, hasher: &mut hasher, size: 0 };
                let copied = io::copy(&mut counting, &mut f).with_context(|| format!("writing staging file {}", tmp.display()));
                if let Err(e) = copied {
                    let _ = fs::remove_file(&tmp);
                    return Err(e);
                }
                counting.size
            };
            f.flush()?;
            f.sync_all().with_context(|| format!("syncing staging file {}", tmp.display()))?;
            (*hasher.finalize().as_bytes(), size)
        };
        let blob = self.path(&hash);
        if blob.exists() {
            let _ = fs::remove_file(&tmp);
        } else {
            fs::rename(&tmp, &blob).with_context(|| format!("renaming {} to {}", tmp.display(), blob.display()))?;
            if let Ok(d) = fs::File::open(&self.dir) {
                let _ = d.sync_all();
            }
        }
        Ok((hash, size))
    }

    /// Stage bytes that are already in memory.
    pub fn stage_bytes(&self, bytes: &[u8]) -> Result<(Hash, u64)> {
        self.stage(&mut &bytes[..])
    }

    /// Remove the blob for a hash if no archive row refers to it.
    pub fn sweep(&self, store: &Store, hash: &Hash) -> Result<()> {
        if store.archive_refcount(hash)? == 0 {
            let _ = fs::remove_file(self.path(hash));
        }
        Ok(())
    }

    /// Evict archived revisions until their total size fits under the cap. The oldest row of the relation with the most archived bytes is deleted first, along with its blob if no other row refers to it. Row `keep` is exempt, so the caller can protect the revision it has just archived.
    pub fn evict(&self, store: &Store, keep: i64) -> Result<()> {
        while store.archive_unique_bytes()? > self.cap {
            let Some(relation) = store.archive_largest_relation(keep)? else { break };
            let Some((row, hash)) = store.archive_oldest_row(&relation, keep)? else { break };
            store.archive_delete_row(row)?;
            self.sweep(store, &hash)?;
        }
        Ok(())
    }

    /// Remove staging files left by a crashed process.
    pub fn clean_staging(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else { return };
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(".staging-") {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

/// A number unique within the process, so that concurrent calls to `stage` use different temporary files.
fn unique_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn staging_is_content_addressed_and_idempotent() {
        let tmp = TempDir::new().unwrap();
        let blobs = Blobs { dir: tmp.path().join("blobs"), cap: 1 << 20 };
        let (h1, n1) = blobs.stage_bytes(b"hello").unwrap();
        let (h2, n2) = blobs.stage_bytes(b"hello").unwrap();
        assert_eq!((h1, n1), (h2, n2));
        assert_eq!(n1, 5);
        assert_eq!(h1, *blake3::hash(b"hello").as_bytes());
        assert!(blobs.has(&h1));
        let mut text = String::new();
        blobs.open(&h1).unwrap().read_to_string(&mut text).unwrap();
        assert_eq!(text, "hello");
        assert_eq!(fs::read_dir(&blobs.dir).unwrap().count(), 1, "no staging file is left behind");
        assert!(blobs.path_for_hex("../etc/passwd").is_err());
        assert_eq!(blobs.path_for_hex(&hex(&h1)).unwrap(), blobs.path(&h1));
    }
}
