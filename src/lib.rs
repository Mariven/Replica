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
//! - [`apply`]: the executor turning an assignment into writes and records.
//! - [`event`]: events and hooks.
//! - [`action`]: shell commands for actions and transforms.
//! - [`engine`]: cycles, discovery, reports, gates.
//! - [`config`]: loading and validating the configuration.
//! - [`watch`]: filesystem change notification feeding the main loop.
//! - [`dashboard`]: the loopback HTTP dashboard.
//! - [`cli`]: subcommands and the ones that run without a daemon.

pub mod pattern;
pub mod site;
pub mod observe;
pub mod flow;
pub mod decide;
pub mod medium;
pub mod blobs;
pub mod store;
pub mod apply;
pub mod event;
pub mod action;
pub mod engine;
pub mod config;
pub mod watch;
pub mod dashboard;
pub mod cli;
