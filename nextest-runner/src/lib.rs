// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

#![warn(missing_docs)]

//! Core functionality for [cargo nextest](https://crates.io/crates/cargo-nextest). For a
//! higher-level overview, see that documentation.
//!
//! Here's the basic flow of operations in nextest.
//!
//! ## Building the test list
//!
//! 1. `cargo test --no-run` is invoked to build test binaries. (This is handled by cargo-nextest;
//!    nextest just processes the messages produced by the command.)
//! 2. The messages generated by Cargo are processed into a list of [`list::RustTestArtifact`]
//!    instances.
//! 3. Separately, a [`test_filter::TestFilter`] is created based on text filters, along with the
//!    run-ignored and partitioning filters if provided.
//! 4. The list of test binaries and test filter are combined. Each binary is run with `--list` to
//!    grab the list of tests, the given filters are applied to it, and everything is put together
//!    to create a [`list::TestList`].
//!
//! If `cargo nextest list-tests` is called, this [`list::TestList`] is printed out. If `cargo
//! nextest run` is called, nextest proceeds to run the tests.
//!
//! ## Running the tests
//!
//! 1. A new [`runner::TestRunner`] is created with the test list and appropriate configuration.
//! 2. The runner sets up two thread pools:
//!     * The *run pool*: each thread in this pool executes a test (+ 1 thread for overall
//!       management).
//!     * The *wait pool*: each thread in this pool monitors the status of a test being run by the
//!       run pool.
//! 3. The test runner is executed with a callback to send [`reporter::TestEvent`] instances to the
//!    test reporter.
//! 4. The test runner iterates over the test list to get individual [`list::TestInstance`]
//!    information. Test instances are sent to the thread pool to be executed.
//! 5. If a test fails and fail-fast is true, or if a signal is encountered, the run is cancelled;
//!    currently executing tests are allowed to complete, but no new tests are scheduled.
//! 6. The test reporter sees events and prints them to stderr (and aggregates them if necessary
//!    based on configs).

pub mod config;
pub mod errors;
mod helpers;
pub mod list;
pub mod partition;
pub mod reporter;
pub mod reuse_build;
pub mod runner;
pub mod signal;
mod stopwatch;
pub mod target_runner;
pub mod test_filter;
