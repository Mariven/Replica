//! A file's mtime, size, and hash, and how that compares to the last-sync record.

use std::time::SystemTime;

/// mtime, size, and content hash. We treat two snapshots as the same file if the hashes match. mtime and size just let us skip hashing when nothing looks different.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub mtime: SystemTime,
    pub size: u64,
    pub hash: [u8; 32],
}

/// What we found at a path: the file is there, it's gone, or we couldn't read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Present(Snapshot),
    Missing,
    Unreadable(String),
}

/// The live file compared to the last-sync record: same, edited, new, gone, or unreadable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Unchanged,
    Edited,
    Appeared,
    Disappeared,
    Unreadable,
}

/// Compare the live status to the recorded snapshot, or to having no record at all.
pub fn classify(record: Option<&Snapshot>, status: &Status) -> Change {
    match (record, status) {
        (_, Status::Unreadable(_)) => Change::Unreadable,
        (None, Status::Missing) => Change::Unchanged,
        (None, Status::Present(_)) => Change::Appeared,
        (Some(_), Status::Missing) => Change::Disappeared,
        (Some(r), Status::Present(s)) => {
            if r.hash == s.hash { Change::Unchanged } else { Change::Edited }
        }
    }
}
