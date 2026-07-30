// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Internal building blocks shared by the blocking and async front ends.
//!
//! Nothing in this module tree is part of the public API. It holds the
//! Windows-specific plumbing that both front ends need: the pipes that carry
//! the pseudoconsole's I/O streams, the pseudoconsole lifecycle state machine
//! built on top of them, the job object that owns the child's process tree,
//! the `CreateProcessW` call that attaches a child to both, child-process exit
//! detection, and the session state and spawn order the two front ends share
//! verbatim.
//!
//! # Why `ConPTY` only ever gets synchronous handles
//!
//! `CreatePseudoConsole` documents `hInput` and `hOutput` as "restricted to
//! synchronous I/O", i.e. handles that do not require an `OVERLAPPED`
//! structure. The blocking front end satisfies that with anonymous pipes from
//! `CreatePipe`, which are synchronous by construction. The `tokio` front end
//! cannot service synchronous handles itself — tokio's named-pipe types
//! require overlapped handles for I/O-completion-port registration — so it
//! uses named-pipe pairs instead: the overlapped server ends stay with us,
//! and the synchronous client ends go to `ConPTY`. Either way `ConPTY` sees only
//! synchronous handles, which both old and new console hosts accept.

#[cfg(any(feature = "blocking", feature = "tokio"))]
pub(super) mod child;
mod job;
#[cfg(any(feature = "blocking", feature = "tokio"))]
pub(super) mod options;
pub(super) mod pipes;
mod proc;
pub(super) mod pseudocon;
#[cfg(any(feature = "blocking", feature = "tokio"))]
pub(super) mod session;
pub(super) mod wait;

use std::io;

use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_HANDLE_EOF, ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED,
};

/// Returns whether `err` means "the other side of this console connection is
/// gone" — a torn-down pipe or a console host that has already exited.
///
/// Two callers share this one judgement so a finished session looks the same
/// from every direction on every Windows version: the blocking reader maps
/// these errors to end-of-file, and [`pseudocon::ConsoleShared::resize`] maps
/// them to [`io::ErrorKind::NotConnected`].
///
/// The Rust standard library already maps `ERROR_BROKEN_PIPE` and
/// `ERROR_HANDLE_EOF` from reads to `Ok(0)`, so for the reader this catches
/// the remaining teardown codes; listing all four keeps the contract
/// independent of that implementation detail.
pub(super) fn is_disconnect_error(err: &io::Error) -> bool {
    const DISCONNECTED: [u32; 4] = [
        ERROR_BROKEN_PIPE,
        ERROR_HANDLE_EOF,
        ERROR_NO_DATA,
        ERROR_PIPE_NOT_CONNECTED,
    ];

    let raw_matches = err
        .raw_os_error()
        .and_then(|code| u32::try_from(code).ok())
        .is_some_and(|code| DISCONNECTED.contains(&code));
    raw_matches
        || matches!(
            err.kind(),
            io::ErrorKind::BrokenPipe | io::ErrorKind::UnexpectedEof
        )
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
