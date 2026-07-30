// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use ::tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::error::Result;
use crate::size::Size;
use crate::status::ExitStatus;
use crate::{PtyController, SessionOutput};

use super::{Child, OwnedReadHalf, OwnedWriteHalf};

/// A managed asynchronous pseudoconsole session.
///
/// There is deliberately no `wait` method: waiting without draining output
/// can deadlock once the pipe fills. Use [`Session::wait_with_output`] for
/// collection or [`Session::into_parts`] for interactive operation.
///
/// The managed child always has kill-on-drop and Job kill-on-close enabled.
/// Dropping an unfinished `Session` therefore terminates the root process and
/// every descendant. The same guarantee applies when a future that owns the
/// session—most notably [`Session::wait_with_output`]—is cancelled.
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

    /// Drains output to EOF, then awaits the already-finished root process.
    ///
    /// The future owns the whole session. Cancelling it drops the managed
    /// child and its kill-on-close Job, terminating the process tree.
    ///
    /// # Errors
    ///
    /// Returns an error if output cannot be drained or the root process
    /// status cannot be obtained.
    pub async fn wait_with_output(mut self) -> Result<SessionOutput> {
        let mut bytes = Vec::new();
        // Keep the returned future small enough to live comfortably in an
        // executor task without forcing callers to box it. Output is drained
        // incrementally either way; this buffer is backpressure plumbing, not
        // a collection limit.
        let mut chunk = [0_u8; 4096];
        loop {
            let read = {
                let mut read_buf = ReadBuf::new(&mut chunk);
                std::future::poll_fn(|cx| Pin::new(&mut self.output).poll_read(cx, &mut read_buf))
                    .await?;
                read_buf.filled().len()
            };
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
        }
        let status = self.child.wait().await?;
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

impl AsyncRead for Session {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().output).poll_read(cx, buf)
    }
}

impl AsyncWrite for Session {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().input).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().input).poll_shutdown(cx)
    }
}

/// Independently owned parts of a managed asynchronous session.
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
