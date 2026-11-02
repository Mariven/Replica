//! Replica keeps related local files in agreed states.
//!
//! - [`pattern`]: path patterns with variables, matched against paths and filled back in from bindings.
//! - [`site`]: sites, the replicas a site expands to, keys, and the walk collecting the keys under a replica.
//! - [`observe`]: snapshots, the recorded basis and the current observation of a file, and the transition between them.
//! - [`flow`]: flow statements and their kinds, the closure over them, and the resolver rules.
//! - [`decide`]: the rules deciding which content each replica of a key should hold.
//! - [`medium`]: the file operation trait and the local filesystem.
//! - [`blobs`]: the content-addressed blob store.
//! - [`store`]: the SQLite state database.
//! - [`apply`]: the executor, which turns an assignment into file writes and database records.
//! - [`event`]: events and hooks.
//! - [`action`]: shell commands for actions and transforms.
//! - [`engine`]: the cycle, with discovery, reports, and gates.
//! - [`config`]: loading and validating the configuration.
//! - [`watch`]: filesystem change notification feeding the main loop.
//! - [`dashboard`]: the loopback HTTP dashboard.
//! - [`cli`]: the subcommands, some of which run without a daemon.

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
