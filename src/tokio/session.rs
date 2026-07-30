// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt;
use std::future::Future;
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
/// [`Session::wait`] drains and discards output concurrently, while
/// [`Session::collect_output`] retains it. Use [`Session::into_parts`] for
/// interactive or externally coordinated I/O.
///
/// The managed child always has kill-on-drop and Job kill-on-close enabled.
/// Dropping an unfinished `Session` therefore terminates the root process and
/// every descendant. The same guarantee applies when a future that owns the
/// session—most notably [`Session::wait`] or [`Session::collect_output`]—is cancelled.
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
    /// Waits for the root process while draining and discarding VT output.
    ///
    /// Output and the root wait are polled concurrently. Once the root status
    /// is saved, remaining descendants are terminated and the teardown tail is
    /// drained to EOF without allocating an output-sized buffer.
    ///
    /// The future owns the session. Cancelling it drops the managed child and
    /// terminates the process tree.
    ///
    /// Terminal input remains open until the root exits. Input shutdown is
    /// session teardown, not an ordinary stdin EOF signal.
    ///
    /// # Errors
    ///
    /// Returns an error if output cannot be drained, the root process status
    /// cannot be obtained, or the remaining process tree cannot be
    /// terminated.
    pub async fn wait(self) -> Result<ExitStatus> {
        Ok(self.complete(false).await?.status())
    }

    /// Waits for the root process while collecting the remaining VT output.
    ///
    /// Collection leaves terminal input open until the root exits, so the
    /// caller must first arrange for the program to finish through its own
    /// protocol. `ConPTY` has no ordinary stdin half-close: closing its input
    /// is terminal teardown and could replace the real exit status.
    ///
    /// Output and the root wait are polled concurrently. Once the root status
    /// is captured, any descendants still in the managed Job are terminated,
    /// terminal input is retired, and the reader drains the teardown tail to
    /// EOF. This gives released and legacy `ConPTY` backends the same finite,
    /// root-bounded completion rule.
    ///
    /// Bytes already read from this `Session` are not included. The future
    /// owns the whole session; cancelling it drops the managed child first and
    /// terminates the process tree.
    ///
    /// Collection is unbounded and may allocate as much memory as the child
    /// writes. Use [`Session::wait`] when output is unnecessary, or
    /// [`Session::into_parts`] to process it as a stream.
    ///
    /// # Errors
    ///
    /// Returns an error if output cannot be drained, the root process status
    /// cannot be obtained, or the remaining process tree cannot be
    /// terminated.
    pub async fn collect_output(self) -> Result<SessionOutput> {
        self.complete(true).await
    }

    async fn complete(mut self, collect: bool) -> Result<SessionOutput> {
        let mut bytes = Vec::new();
        let (status, output_finished) =
            collect_until_root(&mut self.child, &mut self.output, &mut bytes, collect).await?;

        // The root status is cached before this call, so terminating the Job
        // now affects only descendants that outlived it.
        let kill = self.child.kill();
        let Self {
            child,
            mut output,
            mut input,
            controller,
        } = self;
        // Closing the last Job handle is the fallback if explicit termination
        // failed. It must happen before input retirement and tail draining so
        // a released console cannot remain held by a descendant.
        drop(child);
        let input_result = std::future::poll_fn(|cx| Pin::new(&mut input).poll_shutdown(cx)).await;
        drop(input);
        drop(controller);

        let output_result = if output_finished {
            Ok(())
        } else {
            drain_to_end(&mut output, &mut bytes, collect).await
        };
        output_result?;
        input_result?;
        kill?;
        Ok(SessionOutput::new(status, bytes))
    }

    /// Decomposes this session for interactive or externally coordinated I/O.
    #[must_use]
    /// Splitting changes ownership only: root exit still terminates remaining
    /// descendants and advances output to EOF. It does not detach the session.
    ///
    pub fn into_parts(self) -> SessionParts {
        SessionParts {
            child: self.child,
            output: self.output,
            input: self.input,
            controller: self.controller,
        }
    }
}

enum CollectionEvent {
    Root(Result<ExitStatus>),
    Output(io::Result<usize>),
}

async fn collect_until_root(
    child: &mut Child,
    output: &mut OwnedReadHalf,
    bytes: &mut Vec<u8>,
    collect: bool,
) -> Result<(ExitStatus, bool)> {
    // Keep the returned public future small enough to live comfortably in an
    // executor task. This fixed buffer is backpressure plumbing, not a
    // collection limit.
    let mut chunk = [0_u8; 4096];
    let mut output_finished = false;
    let mut wait = std::pin::pin!(child.wait());

    let status = loop {
        let event = std::future::poll_fn(|cx| {
            if let Poll::Ready(status) = wait.as_mut().poll(cx) {
                return Poll::Ready(CollectionEvent::Root(status));
            }
            if output_finished {
                return Poll::Pending;
            }

            let mut read_buf = ReadBuf::new(&mut chunk);
            match Pin::new(&mut *output).poll_read(cx, &mut read_buf) {
                Poll::Ready(Ok(())) => {
                    Poll::Ready(CollectionEvent::Output(Ok(read_buf.filled().len())))
                },
                Poll::Ready(Err(err)) => Poll::Ready(CollectionEvent::Output(Err(err))),
                Poll::Pending => Poll::Pending,
            }
        })
        .await;

        match event {
            CollectionEvent::Root(status) => break status?,
            CollectionEvent::Output(Ok(0)) => output_finished = true,
            CollectionEvent::Output(Ok(read)) if collect => {
                bytes.extend_from_slice(&chunk[..read]);
            },
            CollectionEvent::Output(Ok(_read)) => {},
            CollectionEvent::Output(Err(err)) => return Err(err.into()),
        }
    };

    Ok((status, output_finished))
}

async fn drain_to_end(
    output: &mut OwnedReadHalf,
    bytes: &mut Vec<u8>,
    collect: bool,
) -> io::Result<()> {
    let mut chunk = [0_u8; 4096];
    loop {
        let read = std::future::poll_fn(|cx| {
            let mut read_buf = ReadBuf::new(&mut chunk);
            match Pin::new(&mut *output).poll_read(cx, &mut read_buf) {
                Poll::Ready(Ok(())) => Poll::Ready(Ok(read_buf.filled().len())),
                Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
                Poll::Pending => Poll::Pending,
            }
        })
        .await?;
        if read == 0 {
            return Ok(());
        }
        if collect {
            bytes.extend_from_slice(&chunk[..read]);
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
