use std::time::SystemTime;

// What one ref's file looked like at a moment. The hash is the authoritative equality; mtime/size are the cheap pre-filter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub mtime: SystemTime,
    pub size: u64,
    pub hash: [u8; 32],
}

// The current state of an identifier at one ref. Absence is a first-class status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Present(Snapshot),
    Missing,
    Unreadable(String),
}

// Per-ref change, derived by comparing the status against what is on record from the last resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Unchanged,
    Edited,
    Appeared,
    Disappeared,
    Unreadable,
}

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
