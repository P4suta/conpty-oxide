//! Correctness-first Windows ConPTY (pseudoconsole) library.
//!
//! `conpty-oxide` wraps the Windows pseudoconsole (ConPTY) API with a focus on
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
//! # Where to start
//!
//! [`blocking`] holds the synchronous API: build a [`blocking::Pty`], spawn a
//! [`blocking::Command`] into it, and read the session's output from a
//! dedicated thread. Its module documentation covers the two rules a
//! pseudoconsole imposes on every caller — service the output pipe from
//! another thread, and let the library decide when to close the console.

// Every internal layer of this crate — the command builder, the pipes, the
// pseudoconsole lifecycle, the spawn path — exists to serve a front end. With
// none compiled in (`--no-default-features` leaves only the public type
// definitions reachable) those layers legitimately have no consumer, so the
// dead-code lint is silenced in exactly that configuration. It stays active in
// every configuration that has a front end, including the default one, where
// an unused item really is a mistake.
#![cfg_attr(not(feature = "blocking"), allow(dead_code))]

#[cfg(not(windows))]
compile_error!(
    "conpty-oxide only supports Windows targets; \
     build it with a `*-pc-windows-*` target."
);

mod backend;
mod command;
mod core;
mod error;
mod size;
mod status;

#[cfg(feature = "blocking")]
pub mod blocking;

pub use backend::{BackendKind, ConPtyBackend};
pub use error::{BackendError, Error, Result};
pub use size::Size;
pub use status::ExitStatus;
