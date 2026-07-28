//! Internal building blocks shared by the blocking and async front ends.
//!
//! Nothing in this module tree is part of the public API. It holds the
//! Windows-specific plumbing that both front ends need: the synchronous
//! anonymous pipes that carry the pseudoconsole's I/O streams, the
//! pseudoconsole lifecycle state machine built on top of them, the job object
//! that owns the child's process tree, the `CreateProcessW` call that attaches
//! a child to both, and child-process exit detection.
//!
//! # Why synchronous pipes
//!
//! `CreatePseudoConsole` documents `hInput` and `hOutput` as "restricted to
//! synchronous I/O", i.e. handles that do not require an `OVERLAPPED`
//! structure. Anonymous pipes from `CreatePipe` are always synchronous, so
//! they satisfy that requirement by construction. The `tokio` front end
//! therefore cannot hand tokio's overlapped named-pipe handles to ConPTY; it
//! services these synchronous handles from blocking worker threads instead.

pub(crate) mod job;
pub(crate) mod pipes;
pub(crate) mod proc;
pub(crate) mod pseudocon;
pub(crate) mod wait;

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
pub(crate) fn is_disconnect_error(err: &io::Error) -> bool {
    const DISCONNECTED: [u32; 4] = [
        ERROR_BROKEN_PIPE,
        ERROR_HANDLE_EOF,
        ERROR_NO_DATA,
        ERROR_PIPE_NOT_CONNECTED,
    ];

    let raw_matches = err
        .raw_os_error()
        .is_some_and(|code| DISCONNECTED.iter().any(|&known| known as i32 == code));
    raw_matches
        || matches!(
            err.kind(),
            io::ErrorKind::BrokenPipe | io::ErrorKind::UnexpectedEof
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnect_errors_are_recognized() {
        assert!(is_disconnect_error(&io::Error::from_raw_os_error(
            ERROR_BROKEN_PIPE as i32
        )));
        assert!(is_disconnect_error(&io::Error::from_raw_os_error(
            ERROR_HANDLE_EOF as i32
        )));
        assert!(is_disconnect_error(&io::Error::from_raw_os_error(
            ERROR_NO_DATA as i32
        )));
        assert!(is_disconnect_error(&io::Error::from_raw_os_error(
            ERROR_PIPE_NOT_CONNECTED as i32
        )));
        assert!(is_disconnect_error(&io::Error::new(
            io::ErrorKind::BrokenPipe,
            "synthetic"
        )));
        assert!(!is_disconnect_error(&io::Error::new(
            io::ErrorKind::PermissionDenied,
            "synthetic"
        )));
    }
}
