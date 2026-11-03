//! The database's record of a file, the file as a cycle finds it on disk, and the transition between the two.

use std::time::SystemTime;

/// The BLAKE3 hash of a file's content.
pub type Hash = [u8; 32];

/// The modification time, size, and content hash of a file at a single moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub mtime: SystemTime,
    pub size: u64,
    pub hash: Hash,
}

/// The database's record for a key at a replica.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Basis {
    /// The daemon has no record of the key at this replica.
    None,
    /// The file is recorded as deleted here, because the daemon deleted it or found it missing after a sync.
    Tombstone,
    /// The file as it was after the last sync.
    Snapshot(Snapshot),
}

impl Basis {
    /// The recorded snapshot, if there is one.
    pub fn snapshot(&self) -> Option<&Snapshot> {
        match self {
            Basis::Snapshot(s) => Some(s),
            _ => None,
        }
    }

    /// True if the daemon has a snapshot or a tombstone for this replica.
    pub fn is_recorded(&self) -> bool {
        !matches!(self, Basis::None)
    }
}

/// The state of a key's file at a replica, as a cycle finds it on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Observation {
    Present(Snapshot),
    Missing,
    Unreadable(String),
}

impl Observation {
    /// The observed snapshot, if the file is present.
    pub fn snapshot(&self) -> Option<&Snapshot> {
        match self {
            Observation::Present(s) => Some(s),
            _ => None,
        }
    }

    /// The observed content hash, if the file is present.
    pub fn hash(&self) -> Option<Hash> {
        self.snapshot().map(|s| s.hash)
    }
}

/// How the file at a replica changed between its basis and this cycle's observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transition {
    Unchanged,
    Edited,
    /// The file is present but has no record. If another replica of the key has a record, this one conforms to its peers. Otherwise it proposes its own content.
    Appeared,
    /// The file is present where a tombstone is recorded. It counts as an edit, since the file was deleted here and then put back.
    Recreated,
    Disappeared,
    Unreadable,
}

/// The transition from a recorded basis to what the cycle observed.
pub fn classify(basis: &Basis, observation: &Observation) -> Transition {
    match (basis, observation) {
        (_, Observation::Unreadable(_)) => Transition::Unreadable,
        (Basis::None, Observation::Missing) => Transition::Unchanged,
        (Basis::None, Observation::Present(_)) => Transition::Appeared,
        (Basis::Tombstone, Observation::Missing) => Transition::Unchanged,
        (Basis::Tombstone, Observation::Present(_)) => Transition::Recreated,
        (Basis::Snapshot(_), Observation::Missing) => Transition::Disappeared,
        (Basis::Snapshot(r), Observation::Present(s)) => if r.hash == s.hash { Transition::Unchanged } else { Transition::Edited },
    }
}

/// The first four bytes of a hash in hex, for logs and the dashboard.
pub fn short_hash(hash: &Hash) -> String {
    hash[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// The full hash in hex, which is also the name of its file in the blob store.
pub fn hex(hash: &Hash) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parse a 64-character hex string into a hash, or return None if the text is not one.
pub fn from_hex(text: &str) -> Option<Hash> {
    if text.len() != 64 || !text.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(hash: u8) -> Snapshot {
        Snapshot { mtime: SystemTime::UNIX_EPOCH, size: 1, hash: [hash; 32] }
    }

    #[test]
    fn classify_covers_each_basis_and_observation_pair() {
        assert_eq!(classify(&Basis::None, &Observation::Missing), Transition::Unchanged);
        assert_eq!(classify(&Basis::None, &Observation::Present(snap(1))), Transition::Appeared);
        assert_eq!(classify(&Basis::Tombstone, &Observation::Missing), Transition::Unchanged);
        assert_eq!(classify(&Basis::Tombstone, &Observation::Present(snap(1))), Transition::Recreated);
        assert_eq!(classify(&Basis::Snapshot(snap(1)), &Observation::Missing), Transition::Disappeared);
        assert_eq!(classify(&Basis::Snapshot(snap(1)), &Observation::Present(snap(1))), Transition::Unchanged);
        assert_eq!(classify(&Basis::Snapshot(snap(1)), &Observation::Present(snap(2))), Transition::Edited);
        // A file that cannot be read is Unreadable whatever its basis.
        assert_eq!(classify(&Basis::None, &Observation::Unreadable("denied".into())), Transition::Unreadable);
        assert_eq!(classify(&Basis::Snapshot(snap(1)), &Observation::Unreadable("denied".into())), Transition::Unreadable);
    }

    #[test]
    fn hex_round_trips() {
        let h = [0xabu8; 32];
        assert_eq!(from_hex(&hex(&h)), Some(h));
        assert_eq!(short_hash(&h), "abababab");
        assert_eq!(from_hex("zz"), None);
    }
}
