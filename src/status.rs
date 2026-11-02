//! Filesystem state tracking across synchronization cycles. Capture file snapshots and evaluate live disk status against previous runs to detect modifications.

use std::time::SystemTime;

/// Captured file state holding modification timestamp, byte count, and content hash. Verify timestamps and byte counts before hashing file contents to confirm identical data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub mtime: SystemTime,
    pub size: u64,
    pub hash: [u8; 32],
}

/// Observed disk state for a target path. Record a snapshot when present, mark the path missing when absent, or capture the error message when a read fails.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Present(Snapshot),
    Missing,
    Unreadable(String),
}

/// Path transition between synchronization passes. Classify an entry as unchanged, edited, appeared, disappeared, or unreadable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Unchanged,
    Edited,
    Appeared,
    Disappeared,
    Unreadable,
}

/// Compare current filesystem status with a prior snapshot to determine how a path changed. Check read errors first, check presence or absence next, and compare hashes when both records exist.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(hash: u8) -> Snapshot {
        Snapshot { mtime: SystemTime::UNIX_EPOCH, size: 1, hash: [hash; 32] }
    }

    #[test]
    fn classify_covers_every_record_and_status_pair() {
        assert_eq!(classify(None, &Status::Missing), Change::Unchanged);
        assert_eq!(classify(None, &Status::Present(snap(1))), Change::Appeared);
        assert_eq!(classify(Some(&snap(1)), &Status::Missing), Change::Disappeared);
        assert_eq!(classify(Some(&snap(1)), &Status::Present(snap(1))), Change::Unchanged);
        assert_eq!(classify(Some(&snap(1)), &Status::Present(snap(2))), Change::Edited);
        // A read failure takes precedence over any record.
        assert_eq!(classify(None, &Status::Unreadable("denied".into())), Change::Unreadable);
        assert_eq!(classify(Some(&snap(1)), &Status::Unreadable("denied".into())), Change::Unreadable);
    }
}
