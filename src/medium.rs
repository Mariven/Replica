//! The medium: how the engine lists, reads, writes, removes files. The local filesystem is the only implementation; the trait keeps a remote one possible without changes to the engine.
//!
//! Writes go to a temporary sibling in the target directory, are synced, then renamed over the target, so a crash leaves either the old file or the new one.
//! Marker files placed beside a file about to be created or deleted carry a note for whoever finds them after a crash; walks skip them.

use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};

/// Suffix of the temporary sibling a write stages through.
pub const TEMP_SUFFIX: &str = ".replica-write";
/// Suffix of the marker placed beside a file during a create or delete.
pub const MARKER_SUFFIX: &str = ".replica.tmp";
/// Longest file name most filesystems accept, in bytes.
const NAME_MAX: usize = 255;

/// What a directory entry or a stat call found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    /// Symbolic links, sockets, devices: skipped by walks, unreadable as replicas.
    Other,
}

/// A directory entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub kind: EntryKind,
}

/// Metadata of a path, without following links.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    pub kind: EntryKind,
    pub size: u64,
    pub mtime: SystemTime,
    /// Permission bits masked to the rwx triplet.
    pub mode: u32,
}

/// What an atomic write recorded: the modification time the filesystem kept, plus a warning when a non-fatal step failed.
pub struct Written {
    pub mtime: SystemTime,
    pub warning: Option<String>,
}

/// File operations the engine needs.
pub trait Medium: Send + Sync {
    /// Entries of a directory. A missing directory is an empty listing; any other failure is an error.
    fn list(&self, dir: &Path) -> io::Result<Vec<Entry>>;
    /// Metadata of a path, None when nothing is there.
    fn stat(&self, path: &Path) -> io::Result<Option<Meta>>;
    /// Open a file for reading.
    fn open(&self, path: &Path) -> io::Result<Box<dyn Read + Send>>;
    /// Write content atomically into place, creating parent directories, applying mode and mtime when given.
    fn write(&self, path: &Path, content: &mut dyn Read, mtime: Option<SystemTime>, mode: Option<u32>) -> Result<Written>;
    /// Remove a file; a file already gone counts as removed.
    fn remove(&self, path: &Path) -> io::Result<()>;
    /// Write a small file in one call, for markers.
    fn put(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    /// Whether a path is a directory.
    fn is_dir(&self, path: &Path) -> bool {
        matches!(self.stat(path), Ok(Some(Meta { kind: EntryKind::Dir, .. })))
    }
}

/// The local filesystem.
#[derive(Clone, Copy, Debug, Default)]
pub struct Local;

/// Permission bits masked to the classic rwx triplet.
fn mode_bits(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o777
}

/// Classify a file type.
fn kind_of(ty: fs::FileType) -> EntryKind {
    if ty.is_file() { EntryKind::File } else if ty.is_dir() { EntryKind::Dir } else { EntryKind::Other }
}

impl Medium for Local {
    fn list(&self, dir: &Path) -> io::Result<Vec<Entry>> {
        let read = match fs::read_dir(dir) {
            Ok(r) => r,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut out = Vec::new();
        for entry in read {
            let entry = entry?;
            out.push(Entry { name: entry.file_name().to_string_lossy().into_owned(), kind: kind_of(entry.file_type()?) });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn stat(&self, path: &Path) -> io::Result<Option<Meta>> {
        match fs::symlink_metadata(path) {
            Ok(m) => Ok(Some(Meta { kind: kind_of(m.file_type()), size: m.len(), mtime: m.modified()?, mode: mode_bits(&m) })),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        Ok(Box::new(fs::File::open(path)?))
    }

    fn write(&self, path: &Path, content: &mut dyn Read, mtime: Option<SystemTime>, mode: Option<u32>) -> Result<Written> {
        let parent = path.parent().ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?;
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        let tmp = parent.join(sibling_name(path, ".", TEMP_SUFFIX));
        let mut warning = None;
        let staged: Result<()> = (|| {
            let mut f = fs::File::create(&tmp).with_context(|| format!("creating temp file {}", tmp.display()))?;
            // The mode goes on before any byte lands, so a private donor's content is never world-readable in the temp file.
            if let Some(m) = mode {
                use std::os::unix::fs::PermissionsExt;
                if let Err(e) = f.set_permissions(fs::Permissions::from_mode(m)) {
                    warning = Some(format!("could not set mode {m:o} on {}: {e}", path.display()));
                }
            }
            io::copy(content, &mut f).with_context(|| format!("writing {}", tmp.display()))?;
            if let Some(t) = mtime {
                f.set_modified(t).with_context(|| format!("setting mtime on {}", tmp.display()))?;
            }
            f.sync_all().with_context(|| format!("syncing {}", tmp.display()))?;
            Ok(())
        })();
        if let Err(e) = staged {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        fs::rename(&tmp, path).with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
        // A directory sync makes the rename durable; filesystems refusing it still hold the synced content.
        if let Ok(d) = fs::File::open(parent) {
            let _ = d.sync_all();
        }
        let mtime = fs::symlink_metadata(path).and_then(|m| m.modified()).with_context(|| format!("reading mtime of {}", path.display()))?;
        Ok(Written { mtime, warning })
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        match fs::remove_file(path) {
            Err(e) if e.kind() != ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    fn put(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = fs::File::create(path)?;
        f.write_all(bytes)
    }
}

/// Build a sibling name `<prefix><name><suffix>`, falling back to a hash of the name when the result would exceed NAME_MAX.
pub fn sibling_name(path: &Path, prefix: &str, suffix: &str) -> String {
    let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
    if prefix.len() + name.len() + suffix.len() <= NAME_MAX {
        format!("{prefix}{name}{suffix}")
    } else {
        format!("{prefix}{}{suffix}", &blake3::hash(name.as_bytes()).to_hex()[..16])
    }
}

/// The marker path for a target file.
pub fn marker_path(target: &Path) -> PathBuf {
    target.parent().unwrap_or(Path::new("/")).join(sibling_name(target, "", MARKER_SUFFIX))
}

/// True for the daemon's own temporary and marker files, which walks skip.
pub fn is_own_file(name: &str) -> bool {
    name.ends_with(TEMP_SUFFIX) || name.ends_with(MARKER_SUFFIX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn sibling_names_fall_back_to_a_hash_past_name_max() {
        let short = Path::new("/d/notes.txt");
        assert_eq!(sibling_name(short, ".", TEMP_SUFFIX), ".notes.txt.replica-write");
        assert_eq!(marker_path(short), PathBuf::from("/d/notes.txt.replica.tmp"));
        let long = PathBuf::from("/d").join("x".repeat(250));
        let tmp = sibling_name(&long, ".", TEMP_SUFFIX);
        assert!(tmp.len() <= NAME_MAX && tmp.starts_with('.') && tmp.ends_with(TEMP_SUFFIX), "{tmp}");
        assert!(is_own_file(&tmp) && is_own_file("a.replica.tmp") && !is_own_file("a.txt"));
    }

    #[test]
    fn local_writes_atomically_with_mode_and_mtime() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("sub/secret.txt");
        let pinned = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let w = Local.write(&path, &mut &b"x"[..], Some(pinned), Some(0o600)).unwrap();
        assert!(w.warning.is_none());
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(w.mtime, pinned);
        let w = Local.write(&path, &mut &b"yy"[..], None, None).unwrap();
        assert!(w.warning.is_none());
        assert_eq!(fs::read(&path).unwrap(), b"yy");
        let meta = Local.stat(&path).unwrap().unwrap();
        assert_eq!((meta.kind, meta.size), (EntryKind::File, 2));
        assert!(fs::read_dir(tmp.path().join("sub")).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(TEMP_SUFFIX)));
        // Listing a missing directory is empty; stat of a missing path is None; removing twice is fine.
        assert!(Local.list(&tmp.path().join("absent")).unwrap().is_empty());
        assert_eq!(Local.stat(&tmp.path().join("absent")).unwrap(), None);
        Local.remove(&path).unwrap();
        Local.remove(&path).unwrap();
        let names: Vec<String> = Local.list(tmp.path()).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["sub"]);
    }
}
