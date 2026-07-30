// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Blocking pseudoconsole sessions.
//!
//! This is the synchronous front end of the crate. For complete command
//! output, [`Command::output`] is the safe short path: it creates a managed
//! [`Session`], drains output without hidden tasks, and returns the status and
//! VT bytes. [`Command::spawn`] returns the same managed session for
//! interactive I/O.
//!
//! # Service the output pipe from another thread
//!
//! Microsoft's guidance for pseudoconsole sessions strongly recommends
//! servicing each I/O channel on its own thread, and the deadlock that rule
//! prevents is real: the console host writes rendered output eagerly; once
//! the pipe buffer fills, the host — and with it the child — stops making
//! progress. A program that spawns a child
//! and then calls [`Child::wait`] without draining the output will hang as
//! soon as the child produces more than a pipe buffer's worth of text.
//!
//! Use [`Session::into_parts`] to move [`OwnedReadHalf`] onto its own thread
//! while retaining input, child, and resize/clear control.
//!
//! # Closing the input pipe ends the session
//!
//! Dropping the write half of a session is **not** the console equivalent of
//! closing a child's stdin. The crate closes conin and requests pseudoconsole
//! close; the latter sends a close event to every attached client, which
//! terminates them. A child killed this way reports exit code `0xC000013A`
//! (`STATUS_CONTROL_C_EXIT`) and any output it had not flushed yet is lost.
//!
//! Keep the write half — or the whole [`Session`] — alive until the child has
//! exited. Dropping it earlier is a way to *stop* a session, not a way to
//! signal one.
//!
//! # The end-of-file contract
//!
//! Reading a [`Session`] (or an [`OwnedReadHalf`]) returns `Ok(0)` exactly once
//! the
//! session is over, and every disconnect-flavoured OS error — most importantly
//! `ERROR_BROKEN_PIPE` — is mapped to that same clean end-of-file rather than
//! surfacing as an error. How the crate *reaches* that point depends on the
//! backend:
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
//! use conpty_oxide::blocking::PtyController;
//! ```
//!
//! ```no_run
//! use conpty_oxide::blocking::Command;
//!
//! # fn main() -> conpty_oxide::Result<()> {
//! let output = Command::new("cmd.exe")
//!     .args(["/c", "echo", "hello"])
//!     .output()?;
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
use std::io::{self, Read, Write};
#[cfg(test)]
use std::os::windows::io::{AsHandle, AsRawHandle};

#[cfg(test)]
use crate::backend::BackendKind;
#[cfg(test)]
use crate::{ConPtyBackend, ExitStatus, Size};

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
