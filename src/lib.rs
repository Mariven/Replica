//! Replica keeps files in sync between folders on one machine.
//!
//! - [`pattern`]: path patterns with variables, matching and substitution.
//! - [`site`]: sites, keys, replicas, and the walk that finds the keys under a replica.
//! - [`observe`]: snapshots, bases, observations, and the transition from a basis to an observation.
//! - [`flow`]: flow statements, the closure over their edges, and the resolver rules.
//! - [`decide`]: the rules that decide what each replica of a key should hold.
//! - [`medium`]: file operations behind a trait, with the local filesystem implementation.
//! - [`blobs`]: the content-addressed blob store.
//! - [`store`]: the SQLite state database.
//! - [`apply`]: the executor, which turns an assignment into file writes and database records.
//! - [`event`]: events and hooks.
//! - [`action`]: shell commands for actions and transforms.
//! - [`engine`]: cycles, discovery, reports, and gates.
//! - [`config`]: loading and validating the configuration.
//! - [`watch`]: filesystem notifications for the main loop.
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
