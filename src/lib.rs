// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Correctness-first Windows `ConPTY` (pseudoconsole) library.
//!
//! `conpty-oxide` wraps the Windows pseudoconsole (`ConPTY`) API with a focus on
//! getting the hard parts right:
//!
//! - A well-defined EOF contract for the console output pipe.
//! - No hangs around `ClosePseudoConsole`.
//! - Reliable process-tree termination ("kill tree") via Job objects.
//! - A blocking API (default `blocking` feature) and an async API behind the
//!   `tokio` feature.
//! - Dynamic loading of `conpty.dll`, falling back to the system console API.
//!
//! This crate targets Windows exclusively and does not compile on other
//! platforms.
//!
//! Low-level lifecycle types are intentionally not part of the 0.1 contract:
//!
//! ```compile_fail
//! use conpty_oxide::blocking::Pty;
//! ```
//!
//! ```compile_fail
//! use conpty_oxide::tokio::PtyBuilder;
//! ```
//!
//! Backend identity and unchecked bundle loading are private implementation
//! details:
//!
//! ```compile_fail
//! use conpty_oxide::BackendKind;
//! ```
//!
//! ```compile_fail
//! let backend = conpty_oxide::ConPtyBackend::from_dir_unchecked(".");
//! ```
//!
//! Errors are opaque and the result alias always uses this crate's error:
//!
//! ```compile_fail
//! fn inspect(error: conpty_oxide::Error) {
//!     match error {
//!         conpty_oxide::Error::Io(_) => {}
//!     }
//! }
//! ```
//!
//! ```compile_fail
//! type ForeignResult = conpty_oxide::Result<(), std::io::Error>;
//! ```
//!
//! # Where to start
//!
// The two paragraphs below are feature-gated so their intra-doc links always
// point at something that exists: neither front end is guaranteed to be
// compiled in, and a link into a module that was configured out is a rustdoc
// error rather than a dead link.
#![cfg_attr(
    feature = "blocking",
    doc = "[`blocking`] holds the synchronous API. Start a managed session with",
    doc = "[`blocking::Command::spawn`], then choose [`blocking::Session::wait`]",
    doc = "when output is unnecessary, [`blocking::Session::collect_output`] to",
    doc = "retain raw VT, or [`blocking::Session::into_parts`] for independently",
    doc = "owned I/O, child, and control handles.",
    doc = ""
)]
#![cfg_attr(
    feature = "tokio",
    doc = "The [`tokio`] module mirrors all three paths with `AsyncRead`/`AsyncWrite`",
    doc = "streams and registered process waits. Frontend types never change meaning",
    doc = "based on the selected feature: choose `blocking` or `tokio` explicitly.",
    doc = ""
)]
#![cfg_attr(
    not(feature = "tokio"),
    doc = "The `tokio` feature — not enabled in this build of the documentation —",
    doc = "adds a symmetric `conpty_oxide::tokio` frontend.",
    doc = ""
)]
//! # Choosing a `ConPTY` implementation
//! # Managed sessions
//!
//! A managed session is bounded by its root process. Once the root's real exit
//! status is saved, descendants remaining in the session Job are terminated
//! and the output tail proceeds to EOF. Splitting with `into_parts` changes
//! ownership only; it does not detach the process tree.
//!
//! Input drop or shutdown ends the terminal session rather than delivering an
//! ordinary stdin EOF. Output is one raw UTF-8/VT byte stream with no separate
//! stdout and stderr channels. `collect_output` retains an unbounded amount of
//! output; use `wait` to discard it safely or owned parts to stream it.
//!
//!
//! Automatic selection needs no setup: it prefers a validated standalone
//! `conpty.dll` bundle next to the executable, then falls back to the operating
//! system's `ConPTY`. An application can also select a bundle explicitly to get
//! the newer console host's behaviour on older Windows versions:
//!
//! - [`ConPtyBackend::auto`] uses a valid bundle found next to the executable,
//!   falls back to the system implementation when that bundle is rejected,
//!   and returns an error if neither is usable. This is also what the default
//!   backend selection does.
//! - [`ConPtyBackend::from_dir`] loads a bundle from a directory you name,
//!   validating that its `conpty.dll` and `OpenConsole.exe` are a matching
//!   pair before either runs.
//! - With either frontend enabled, `SessionOptions::backend` selects a backend
//!   for a managed session.
//!
//! Cursor inheritance, manual EOF policy, detached sessions, and pre-staged
//! spawning are intentionally outside the 0.1 API. They can be added later as
//! typed advanced operations when concrete use cases justify them.

// `cargo test --doc` normally inspects only Rust source, not README.md. Under
// the all-frontend configuration used by CI, append the README while rustdoc
// is collecting tests so its blocking, Tokio, and low-level snippets are the
// exact text compiled. It is omitted from ordinary API documentation and from
// single/no-frontend doctest legs, where one of those snippets is intentionally
// unavailable.
#![cfg_attr(
    all(doctest, feature = "blocking", feature = "tokio"),
    doc = include_str!("../README.md")
)]
// docs.rs passes `--cfg docsrs` (see `[package.metadata.docs.rs]`), which
// turns on the nightly-only `doc_cfg` feature: every feature-gated item then
// carries an "Available on crate feature … only" badge. Stable builds never
// see the cfg, so this is inert everywhere else. (The badges used to need a
// separate `doc_auto_cfg` feature; that was merged into `doc_cfg` and removed
// in 1.92 — rust-lang/rust#138907 — so naming it here breaks the docs.rs
// build.)
#![cfg_attr(docsrs, feature(doc_cfg))]
// Every public item carries documentation, and this keeps it that way under
// every driver — `cargo rustc`, rustdoc, rust-analyzer — including invocations
// where Cargo does not forward the workspace lint table.
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(windows))]
compile_error!(
    "conpty-oxide only supports Windows targets; \
     build it with a `*-pc-windows-*` target."
);

#[cfg(any(feature = "blocking", feature = "tokio"))]
mod api;
mod backend;
#[cfg(any(feature = "blocking", feature = "tokio", test))]
mod command;
#[cfg(any(feature = "blocking", feature = "tokio", test))]
mod core;
mod error;
mod size;
mod status;

#[cfg(all(test, feature = "tracing"))]
mod tracing_test_support;

#[cfg(feature = "blocking")]
pub mod blocking;

#[cfg(feature = "tokio")]
pub mod tokio;

#[cfg(any(feature = "blocking", feature = "tokio"))]
pub use api::{PtyController, SessionOptions, SessionOutput};
pub use backend::ConPtyBackend;
pub use error::{BackendError, BackendErrorKind, Error, ErrorKind, Result};
pub use size::Size;
pub use status::ExitStatus;
