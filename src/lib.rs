//! Replica keeps related local files in sync.
//!
//! A *relation* is a set of files that should have the same content.
//! Each cycle we look at those files, compare them to a stored record, and copy or delete according to the relation's policy.
//!
//! - [`identifier`]: relative paths and glob filters
//! - [`label`]: labels, refs, and path templates
//! - [`status`]: snapshots and how they differ from the last sync
//! - [`policy`]: which copies and deletes to run
//! - [`store`]: the last-sync record, the intent journal, and the archive index
//! - [`archive`]: content we keep when overwriting or deleting
//! - [`engine`]: a full cycle, from discovery through applying a plan
//! - [`config`]: load relations from YAML

pub mod archive;
pub mod identifier;
pub mod label;
pub mod status;
pub mod policy;
pub mod store;
pub mod engine;
pub mod config;
