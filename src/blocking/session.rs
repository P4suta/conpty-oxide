// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Managed blocking sessions and complete-output collection.

use std::fmt;
use std::io::{self, Read, Write};

use super::command::Child;
use super::pty::{OwnedReadHalf, OwnedWriteHalf};
use crate::error::Result;
use crate::size::Size;
use crate::status::ExitStatus;
use crate::{PtyController, SessionOutput};

/// A managed blocking pseudoconsole session.
///
/// Reading and writing delegate to the session's output and input streams.
/// There is deliberately no `wait` method: waiting without draining output can
/// deadlock once the pipe fills. Use [`Session::wait_with_output`] for complete
/// collection or [`Session::into_parts`] for interactive operation.
///
/// Dropping an unfinished managed session closes its kill-on-close Job and
/// terminates the root process together with every descendant.
pub struct Session {
    pub(super) child: Child,
    pub(super) output: OwnedReadHalf,
    pub(super) input: OwnedWriteHalf,
    pub(super) controller: PtyController,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("child", &self.child)
            .field("controller", &self.controller)
            .finish_non_exhaustive()
    }
}

impl Session {
    pub(super) const fn new(
        child: Child,
        output: OwnedReadHalf,
        input: OwnedWriteHalf,
        controller: PtyController,
    ) -> Self {
        Self {
            child,
            output,
            input,
            controller,
        }
    }

    /// Returns the root process identifier.
    #[must_use]
    pub const fn id(&self) -> u32 {
        self.child.id()
    }

    /// Returns the exit status when the root process has already exited.
    ///
    /// # Errors
    ///
    /// Returns an error with [`crate::ErrorKind::Wait`] if Windows cannot
    /// query the process.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Terminates the root process and every descendant in its Job.
    ///
    /// # Errors
    ///
    /// Returns an error with [`crate::ErrorKind::Kill`] if Windows cannot
    /// terminate the Job.
    pub fn kill(&mut self) -> Result<()> {
        self.child.kill()
    }

    /// Resizes the pseudoconsole.
    ///
    /// # Errors
    ///
    /// Returns an error with [`crate::ErrorKind::Resize`] if the backend
    /// rejects the size, or an [`std::io::ErrorKind::NotConnected`] source
    /// after teardown.
    pub fn resize(&self, size: Size) -> Result<()> {
        self.controller.resize(size)
    }

    /// Returns the last successfully applied terminal size.
    #[must_use]
    pub fn size(&self) -> Size {
        self.controller.size()
    }

    /// Clears the pseudoconsole screen and scrollback.
    ///
    /// # Errors
    ///
    /// Returns an error with [`crate::ErrorKind::UnsupportedFeature`] when the
    /// backend has no clear operation, [`crate::ErrorKind::Clear`] on backend
    /// failure, or an [`std::io::ErrorKind::NotConnected`] source after
    /// teardown.
    pub fn clear(&self) -> Result<()> {
        self.controller.clear()
    }

    /// Returns whether this backend supports clearing the console.
    #[must_use]
    pub fn supports_clear(&self) -> bool {
        self.controller.supports_clear()
    }

    /// Drains output to EOF, then returns the already-finished child status.
    ///
    /// The input half remains open throughout the read. Closing `ConPTY` input
    /// is terminal teardown, not ordinary stdin EOF, and doing it early can
    /// replace the child's real exit status with `STATUS_CONTROL_C_EXIT`.
    ///
    /// # Errors
    ///
    /// Returns an error if output cannot be drained or the root process
    /// status cannot be obtained.
    pub fn wait_with_output(mut self) -> Result<SessionOutput> {
        let mut bytes = Vec::new();
        self.output.read_to_end(&mut bytes)?;
        let status = self.child.wait()?;
        Ok(SessionOutput::new(status, bytes))
    }

    /// Decomposes this session for interactive or externally coordinated I/O.
    #[must_use]
    pub fn into_parts(self) -> SessionParts {
        SessionParts {
            child: self.child,
            output: self.output,
            input: self.input,
            controller: self.controller,
        }
    }
}

impl Read for Session {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.output.read(buf)
    }
}

impl Write for Session {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.input.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Independently owned parts of a managed blocking session.
#[non_exhaustive]
pub struct SessionParts {
    /// Root process and kill-on-drop Job ownership.
    pub child: Child,
    /// Rendered virtual-terminal output.
    pub output: OwnedReadHalf,
    /// Console input.
    pub input: OwnedWriteHalf,
    /// Cloneable resize/clear/capability control.
    pub controller: PtyController,
}

impl fmt::Debug for SessionParts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionParts")
            .field("child", &self.child)
            .field("controller", &self.controller)
            .finish_non_exhaustive()
    }
}
