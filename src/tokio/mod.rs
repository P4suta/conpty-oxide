// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Asynchronous pseudoconsole sessions, driven by Tokio.
//!
//! This is the async front end of the crate and a faithful mirror of the
//! `blocking` module. [`Command::spawn`] creates a managed [`Session`] for
//! interactive I/O. After arranging for the root program to exit,
//! [`Session::collect_output`] drains output concurrently and returns its
//! status and remaining VT bytes.
//!
//! [`Session::into_parts`] provides independent owned I/O, child, and control
//! handles. [`Child::wait`] uses
//! `RegisterWaitForSingleObject`, so no Tokio blocking-pool thread remains
//! parked while the process is alive.
//!
//! # Build inside a runtime
//!
//! [`Command::spawn`] registers the session's pipes with the current runtime's
//! I/O driver, so it must be called from within a Tokio runtime.
//!
//! # Drive the output pipe concurrently with the child
//!
//! Microsoft's guidance for pseudoconsole sessions strongly recommends
//! servicing each I/O channel on its own thread — here, by something other
//! than whatever is waiting on the child. The deadlock that rule prevents is
//! real: the console host writes rendered output eagerly; once the pipe
//! buffer fills, the host — and with it the child — stops making progress.
//!
//! Async makes this easier than threads do, but it does not make it optional:
//! a task that awaits [`Child::wait`] without also polling the output will
//! hang as soon as the child produces more than a pipe buffer's worth of text.
//! Use [`Session::into_parts`] to hand the read half to its own task, or
//! `tokio::select!`/`tokio::join!` to drive both from one.
//!
//! # Closing the input pipe ends the session
//!
//! Dropping the write half of a session is **not** the console equivalent of
//! closing a child's stdin. The crate closes conin and requests pseudoconsole
//! close; the latter sends a close event to every attached client, which
//! terminates them. A child killed this way reports exit code `0xC000013A`
//! (`STATUS_CONTROL_C_EXIT`) and any output it had not flushed yet is lost.
//! The same is true of `AsyncWriteExt::shutdown`.
//!
//! Keep the write half — or the whole [`Session`] — alive until the child has
//! exited. Dropping it earlier is a way to *stop* a session, not a way to
//! signal one.
//!
//! # The end-of-file contract
//!
//! Reading a [`Session`] (or an [`OwnedReadHalf`]) yields `Ok(0)` exactly once
//! the
//! session is over, and every disconnect-flavoured OS error — most importantly
//! `ERROR_BROKEN_PIPE` — is mapped to that same clean end-of-file rather than
//! surfacing as an error. How the crate *reaches* that point depends on the
//! backend, exactly as it does for the blocking front end:
//!
//! - Where `ReleasePseudoConsole` exists (Windows 11 24H2 and later, or a
//!   bundled `conpty.dll`), the pseudoconsole is released right after the
//!   child is spawned. The console host then exits on its own once every
//!   client has disconnected, and end-of-file arrives naturally.
//! - Otherwise the console host outlives the child and nothing would ever end
//!   the output stream. A Windows registered wait observes the root child;
//!   after exit, a short-lived worker grants a drain grace and closes the
//!   pseudoconsole.
//!
//! # Examples
//!
//! Shared control types have one canonical path at the crate root:
//!
//! ```compile_fail
//! use conpty_oxide::tokio::PtyController;
//! ```
//!
//! ```no_run
//! use conpty_oxide::tokio::Command;
//!
//! # #[tokio::main]
//! # async fn main() -> conpty_oxide::Result<()> {
//! let output = Command::new("cmd.exe")
//!     .args(["/c", "echo", "hello"])
//!     .spawn()?
//!     .collect_output()
//!     .await?;
//! print!("{}", String::from_utf8_lossy(output.as_bytes()));
//! assert!(output.status().success());
//! # Ok(())
//! # }
//! ```

mod builder;
mod command;
mod pty;
mod session;

pub use command::{Child, Command};
pub use pty::{OwnedReadHalf, OwnedWriteHalf};
pub use session::{Session, SessionParts};

#[cfg(test)]
use crate::backend::{BackendKind, ConPtyBackend};
#[cfg(test)]
#[cfg(test)]
use crate::size::Size;
#[cfg(test)]
use crate::status::ExitStatus;
#[cfg(test)]
use std::os::windows::io::{AsHandle, AsRawHandle};
#[cfg(test)]
use std::{io, sync::Arc};

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
