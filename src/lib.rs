//! Replica synchronizes related local files.
//! To coordinate paths across a relation, scan filesystem entries at each target.
//! After comparing content hashes against stored history, apply policy rules to schedule copy or delete operations.
//!
//! - [`identifier`]: Resolve relative paths, filtering candidate entries with glob patterns.
//! - [`label`]: Expand path templates using label definitions.
//! - [`status`]: Inspect filesystem trees, recording divergence from stored sync states.
//! - [`policy`]: Evaluate operational rules before scheduling copy or delete actions.
//! - [`store`]: Persist sync states, intent journals, archive indices on disk.
//! - [`archive`]: Preserve target files prior to replacement or removal.
//! - [`engine`]: Drive sync cycles from target discovery through plan execution.
//! - [`config`]: Parse relation specifications from YAML input files.

pub mod archive;
pub mod identifier;
pub mod label;
pub mod status;
pub mod policy;
pub mod store;
pub mod engine;
pub mod config;
