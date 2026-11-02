//! Replica keeps related local files in agreed states.
//!
//! - [`pattern`]: path patterns with variables; matching and substitution.
//! - [`site`]: sites, keys, replicas, discovery of keys under a replica.
//! - [`observe`]: snapshots, observations, bases, transitions.
//! - [`flow`]: flow statements, kinds, closure, resolver rules.
//! - [`decide`]: the named rules deciding what each replica of a key should hold.
//! - [`medium`]: the file operation trait and the local filesystem.
//! - [`blobs`]: the content-addressed blob store.
//! - [`store`]: the SQLite state database.

pub mod pattern;
pub mod site;
pub mod observe;
pub mod flow;
pub mod decide;
pub mod medium;
pub mod blobs;
pub mod store;
