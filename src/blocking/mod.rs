// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Blocking pseudoconsole sessions.
//!
//! [`Command::spawn`] creates a managed [`Session`]. Choose one of three paths:
//!
//! - [`Session::wait`] when output is unnecessary;
//! - [`Session::collect_output`] to retain the remaining raw VT bytes;
//! - [`Session::into_parts`] for interactive or externally coordinated I/O.
//!
//! All three remain managed and root-bounded. `into_parts` separates ownership;
//! it does not detach the child or its descendants.
//!
//! # Service output concurrently
//!
//! [Microsoft's ConPTY guidance](https://learn.microsoft.com/en-us/windows/console/creating-a-pseudoconsole-session)
//! recommends servicing conin and conout on separate threads. Once conout's pipe
//! buffer fills, the console host and its client can stop making progress. A
//! caller that blocks in [`Child::wait`] without another thread draining output
//! can therefore deadlock.
//!
//! [`Session::wait`] and [`Session::collect_output`] perform the root wait and
//! output drain together. With [`Session::into_parts`], move [`OwnedReadHalf`]
//! to its own reader thread while retaining input, child, and resize/clear
//! control elsewhere.
//!
//! # Input shutdown ends the session
//!
//! Dropping [`OwnedWriteHalf`] is not the console equivalent of closing a
//! child's stdin. It closes conin and requests pseudoconsole teardown, which
//! sends a close event to attached clients. Keep input alive until the program
//! exits through its own protocol.
//!
//! # Output and root completion
//!
//! Conout is one raw UTF-8/VT byte stream; stdout and stderr are not separate,
//! and this crate does not parse or decode it. Decode across reads because a
//! UTF-8 code point or VT sequence may span chunks.
//!
//! Root exit bounds the managed session. A registered wait saves the root's
//! real status and terminates remaining Job members. Released backends then
//! reach EOF naturally. Legacy backends grant a drain grace and request close
//! from a dedicated worker so the same output stream reaches EOF without
//! running a potentially blocking close on the reader thread.
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
//! let status = Command::new("cmd.exe")
//!     .args(["/c", "exit", "0"])
//!     .spawn()?
//!     .wait()?;
//! assert!(status.success());
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
