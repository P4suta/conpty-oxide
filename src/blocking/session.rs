// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Managed blocking sessions and root-bounded output collection.

use std::fmt;
use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::command::Child;
use super::pty::{OwnedReadHalf, OwnedWriteHalf};
use crate::error::Result;
use crate::size::Size;
use crate::status::ExitStatus;
use crate::{PtyController, SessionOutput};

/// A managed blocking pseudoconsole session.
///
/// Reading and writing delegate to the session's output and input streams.
/// [`Session::wait`] drains and discards output concurrently, while
/// [`Session::collect_output`] retains it. Use [`Session::into_parts`] for
/// interactive or externally coordinated I/O.
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

    /// Waits for the root process while draining and discarding VT output.
    ///
    /// Output is drained on a dedicated thread, so a child that writes more
    /// than the pipe capacity cannot deadlock. Once the root status is saved,
    /// remaining descendants are terminated and the teardown tail is drained
    /// to EOF without allocating an output-sized buffer.
    ///
    /// Terminal input remains open until the root exits. Closing input is
    /// session teardown, not an ordinary stdin EOF signal.
    ///
    /// # Errors
    ///
    /// Returns an error if the reader thread cannot be created, output cannot
    /// be drained, the root process status cannot be obtained, or the
    /// remaining process tree cannot be terminated.
    pub fn wait(self) -> Result<ExitStatus> {
        Ok(self.complete(false)?.status())
    }

    /// Waits for the root process while collecting the remaining VT output.
    ///
    /// Collection leaves terminal input open until the root exits, so the
    /// caller must first arrange for the program to finish through its own
    /// protocol. `ConPTY` has no ordinary stdin half-close: closing its input
    /// is terminal teardown and could replace the real exit status.
    ///
    /// Output is drained on a dedicated thread while the root runs. Once its
    /// status is captured, any descendants still in the managed Job are
    /// terminated, terminal input is retired, and the reader drains the
    /// teardown tail to EOF. This gives released and legacy `ConPTY` backends
    /// the same finite, root-bounded completion rule.
    ///
    /// Bytes already read from this `Session` are not included.
    ///
    /// Collection is unbounded and may allocate as much memory as the child
    /// writes. Use [`Session::wait`] when output is unnecessary, or
    /// [`Session::into_parts`] to process it as a stream.
    ///
    /// # Errors
    ///
    /// Returns an error if the reader thread cannot be created, output cannot
    /// be drained, the root process status cannot be obtained, or the
    /// remaining process tree cannot be terminated.
    pub fn collect_output(self) -> Result<SessionOutput> {
        self.complete(true)
    }

    fn complete(self, collect: bool) -> Result<SessionOutput> {
        let (completed_tx, completed_rx) = mpsc::sync_channel(1);
        let mut output = self.output;
        let reader = thread::Builder::new()
            .name("conpty-oxide-output".into())
            .spawn(move || {
                let result = if collect {
                    let mut bytes = Vec::new();
                    output.read_to_end(&mut bytes).map(|_| bytes)
                } else {
                    io::copy(&mut output, &mut io::sink()).map(|_| Vec::new())
                };
                match completed_tx.send(()) {
                    Ok(()) | Err(_) => {},
                }
                result
            })?;

        BlockingCollector {
            child: Some(self.child),
            input: Some(self.input),
            controller: Some(self.controller),
            reader: Some(reader),
            completed: completed_rx,
        }
        .finish()
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

/// Owns an in-progress blocking collection in teardown-safe field order.
///
/// If collection unwinds, the child and its kill-on-close Job are dropped
/// before terminal input. That preserves the root status whenever possible
/// and prevents the reader worker from being left behind with a live tree.
struct BlockingCollector {
    child: Option<Child>,
    input: Option<OwnedWriteHalf>,
    controller: Option<PtyController>,
    reader: Option<JoinHandle<io::Result<Vec<u8>>>>,
    completed: Receiver<()>,
}

impl BlockingCollector {
    fn finish(mut self) -> Result<SessionOutput> {
        let mut output = None;

        let status = loop {
            let child = self
                .child
                .as_mut()
                .ok_or_else(|| io::Error::other("the collection child was already retired"))?;
            match child.try_wait() {
                Ok(Some(status)) => break Some(Ok(status)),
                Ok(None) => {},
                Err(err) => break Some(Err(err)),
            }

            match self.completed.recv_timeout(Duration::from_millis(10)) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                    output = Some(self.join_reader());
                    break match output.as_ref() {
                        Some(Ok(_bytes)) => Some(
                            self.child
                                .as_mut()
                                .ok_or_else(|| {
                                    io::Error::other("the collection child was already retired")
                                })?
                                .wait(),
                        ),
                        Some(Err(_reader_error)) => None,
                        None => {
                            return Err(io::Error::other(
                                "the output reader completed without a result",
                            )
                            .into());
                        },
                    };
                },
                Err(RecvTimeoutError::Timeout) => {},
            }
        };

        // The root status is cached before this call on the success path, so
        // terminating the Job now affects only descendants that outlived it.
        let kill = self
            .child
            .as_mut()
            .ok_or_else(|| io::Error::other("the collection child was already retired"))?
            .kill();
        // Drop the Job before retiring input or joining the reader. If the
        // explicit termination failed, kill-on-close gets one last chance to
        // remove a descendant that would otherwise keep released ConPTY open.
        drop(self.child.take());
        drop(self.input.take());
        drop(self.controller.take());

        let bytes = output.unwrap_or_else(|| self.join_reader())?;
        let status = status
            .ok_or_else(|| io::Error::other("output collection ended without a root status"))??;
        kill?;
        Ok(SessionOutput::new(status, bytes))
    }

    fn join_reader(&mut self) -> io::Result<Vec<u8>> {
        self.reader
            .take()
            .ok_or_else(|| io::Error::other("the output reader was already joined"))?
            .join()
            .map_err(|_panic_payload| io::Error::other("the output reader thread panicked"))?
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
