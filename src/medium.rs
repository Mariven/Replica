//! File operations for the engine. The engine lists, reads, writes, and removes files through the `Medium` trait instead of calling the filesystem directly. `Local`, for the local filesystem, is the only implementation. The trait is there so that a remote one can be added without changing the engine.
//!
//! A write goes to a temporary file in the target's directory, is synced, and is then renamed over the target. After a crash the target holds either the old file or the complete new one.
//! Before a file is created or deleted, a marker file is placed beside it with a note for anyone who finds it after a crash. Directory walks skip markers.

use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};

/// Suffix of the temporary file a write goes to before the rename.
pub const TEMP_SUFFIX: &str = ".replica-write";
/// Suffix of the marker placed beside a file during a create or delete.
pub const MARKER_SUFFIX: &str = ".replica.tmp";
/// The maximum file name length on most filesystems, in bytes.
const NAME_MAX: usize = 255;

/// The file type, as reported by a directory entry or a stat call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    /// Symbolic links, sockets, devices, and anything else that is neither a file nor a directory. Walks skip these entries, but a replica path of this kind counts as unreadable.
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
    /// The rwx permission bits for owner, group, and others.
    pub mode: u32,
}

/// The result of an atomic write: the file's new modification time, and a warning if a step failed without stopping the write.
pub struct Written {
    pub mtime: SystemTime,
    pub warning: Option<String>,
}

/// The file operations the engine uses.
pub trait Medium: Send + Sync {
    /// Entries of a directory. A missing directory lists as empty, but any other failure is returned as an error.
    fn list(&self, dir: &Path) -> io::Result<Vec<Entry>>;
    /// Metadata of a path, or None if nothing is there.
    fn stat(&self, path: &Path) -> io::Result<Option<Meta>>;
    /// Open a file for reading.
    fn open(&self, path: &Path) -> io::Result<Box<dyn Read + Send>>;
    /// Write content to a path atomically, creating parent directories as needed. If a mode or mtime is given, the new file gets it.
    fn write(&self, path: &Path, content: &mut dyn Read, mtime: Option<SystemTime>, mode: Option<u32>) -> Result<Written>;
    /// Remove a file. If the file is already gone, the call succeeds.
    fn remove(&self, path: &Path) -> io::Result<()>;
    /// Write a small file in one call, for markers.
    fn put(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    /// True if a path is a directory.
    fn is_dir(&self, path: &Path) -> bool {
        matches!(self.stat(path), Ok(Some(Meta { kind: EntryKind::Dir, .. })))
    }
}

/// The local filesystem.
#[derive(Clone, Copy, Debug, Default)]
pub struct Local;

/// The rwx permission bits of a file, for owner, group, and others.
fn mode_bits(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o777
}

/// Map a standard library file type to an `EntryKind`.
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
            // The mode is set before any content is written, so a copy of a private file is protected from the first byte.
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
        // The parent directory is synced so the rename survives a crash. Some filesystems refuse to sync a directory. That error is ignored, because the file content is already synced.
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

/// The name `<prefix><name><suffix>` for a file beside `path`. If that is longer than NAME_MAX, a 16-character hash of the name takes the place of the name.
pub fn sibling_name(path: &Path, prefix: &str, suffix: &str) -> String {
    let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
    if prefix.len() + name.len() + suffix.len() <= NAME_MAX {
        format!("{prefix}{name}{suffix}")
    } else {
        format!("{prefix}{}{suffix}", &blake3::hash(name.as_bytes()).to_hex()[..16])
    }
}

/// Path of the marker beside a target file.
pub fn marker_path(target: &Path) -> PathBuf {
    target.parent().unwrap_or(Path::new("/")).join(sibling_name(target, "", MARKER_SUFFIX))
}

/// True for the daemon's own temporary and marker files, so that walks can skip them.
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
        // A missing directory lists as empty, a missing path stats as None, and removing a file twice succeeds.
        assert!(Local.list(&tmp.path().join("absent")).unwrap().is_empty());
        assert_eq!(Local.stat(&tmp.path().join("absent")).unwrap(), None);
        Local.remove(&path).unwrap();
        Local.remove(&path).unwrap();
        let names: Vec<String> = Local.list(tmp.path()).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["sub"]);
    }
}
