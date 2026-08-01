// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Asynchronous pseudoconsole sessions, driven by Tokio.
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
//! # Build inside a runtime
//!
//! [`Command::spawn`] registers the session pipes with the current runtime's I/O
//! driver, so it must be called from within a Tokio runtime. [`Child::wait`]
//! uses `RegisterWaitForSingleObject`; it does not park a Tokio blocking thread.
//!
//! # Drive output concurrently
//!
//! [Microsoft's ConPTY guidance](https://learn.microsoft.com/en-us/windows/console/creating-a-pseudoconsole-session)
//! recommends servicing conin and conout concurrently. Once conout's pipe buffer
//! fills, the console host and its client can stop making progress. Awaiting
//! [`Child::wait`] without also polling output can therefore deadlock.
//!
//! [`Session::wait`] and [`Session::collect_output`] poll the root wait and
//! output together. With [`Session::into_parts`], hand [`OwnedReadHalf`] to a
//! reader task or use `tokio::select!`/`tokio::join!`.
//!
//! # Input shutdown ends the session
//!
//! Dropping or shutting down [`OwnedWriteHalf`] is not the console equivalent
//! of closing a child's stdin. It closes conin and requests pseudoconsole
//! teardown, which sends a close event to attached clients. Keep input alive
//! until the program exits through its own protocol.
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
//! from a dedicated worker, giving both backends the same completion meaning.
//! Cancelling a future that owns a managed session terminates its process tree.
//!
//! # Examples
//!
//! ```no_run
//! use conpty_oxide::tokio::Command;
//!
//! # #[tokio::main]
//! # async fn main() -> conpty_oxide::Result<()> {
//! let status = Command::new("cmd.exe")
//!     .args(["/c", "exit", "0"])
//!     .spawn()?
//!     .wait()
//!     .await?;
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
use crate::backend::{BackendKind, ConPtyBackend};
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

/// Shared control types keep one canonical path at the crate root:
///
/// ```compile_fail
/// use conpty_oxide::tokio::PtyController;
/// ```
#[cfg(doctest)]
mod api_boundary {}
