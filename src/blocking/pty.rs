// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Blocking pseudoconsole ownership and synchronous I/O.

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::OwnedHandle;
use std::sync::Arc;

use super::builder::PtyBuilder;
#[cfg(test)]
use crate::backend::BackendKind;
use crate::core::is_disconnect_error;
use crate::core::pseudocon::ConsoleShared;
use crate::core::session::Session as SessionCore;
#[cfg(test)]
use crate::error::Result;
#[cfg(test)]
use crate::size::Size;
use crate::PtyController;

/// A pseudoconsole session: the console plus both ends of its I/O.
///
/// `Pty` implements [`Read`] (rendered console output) and [`Write`] (console
/// input). Because both use `&mut self`, reading and writing concurrently
/// requires splitting the session first — see [`Pty::split`] for a borrowed
/// split and [`Pty::into_split`] for an owned one.
///
/// # Teardown
///
/// Dropping a `Pty` retires the read end first, then the write end, then the
/// pseudoconsole itself. Dropping never waits for legacy
/// `ClosePseudoConsole`: a close that may block is handed to a detached
/// worker, whatever order the halves of a split session are dropped in. If
/// Windows cannot create that worker, teardown leaves the handle for process
/// cleanup instead of risking a wedged destructor.
///
/// Because closing the input pipe is part of that teardown, dropping a `Pty`
/// whose child is still running **terminates the child** — see the module
/// documentation. Keep the session alive until
/// [`Child::wait`](super::Child::wait) returns.
pub(crate) struct Pty {
    pub(super) reader: ConoutReader,
    pub(super) writer: ConinWriter,
    pub(super) inner: Arc<SessionCore>,
}

/// Shows the session's identity — its size and backend — rather than raw
/// handle values and the private lifecycle state, which are noise that varies
/// between runs and would otherwise become de-facto public surface.
/// [`ConPtyBackend`](crate::ConPtyBackend)'s own `Debug` follows the same rule.
impl fmt::Debug for Pty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pty")
            .field("size", &self.inner.size())
            .field("backend_kind", self.inner.backend_kind())
            .finish_non_exhaustive()
    }
}

impl Pty {
    /// Starts building a session.
    #[must_use]
    pub(crate) fn builder() -> PtyBuilder {
        PtyBuilder::default()
    }

    /// Resizes the pseudoconsole.
    ///
    /// The child observes the new dimensions the way a real console resize is
    /// reported: `GetConsoleScreenBufferInfo` returns the new size, and a
    /// program in virtual-terminal mode sees the redraw.
    ///
    /// # Errors
    ///
    /// [`crate::ErrorKind::Resize`] with the backend failure, or
    /// an
    /// [`io::ErrorKind::NotConnected`] error once the session has been torn
    /// down.
    #[cfg(test)]
    pub(crate) fn resize(&self, size: Size) -> Result<()> {
        self.inner.resize(size)
    }

    /// Returns the size last accepted by [`Pty::resize`], or the size the
    /// session was built with.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn size(&self) -> Size {
        self.inner.size()
    }

    /// Clears the pseudoconsole's screen and scrollback.
    ///
    /// This is the "clear buffer" operation of a terminal emulator, performed
    /// by the console host itself: everything it has rendered so far is
    /// discarded, and the client keeps running untouched. It is a signal, not
    /// output — nothing is written into the session's input pipe and the child
    /// is not notified.
    ///
    /// The session's reader is unaffected: bytes already delivered stay
    /// delivered. What changes is what the console host will re-render, so a
    /// full-screen application repaints on its next update.
    ///
    /// # Availability
    ///
    /// `ClearPseudoConsole` is not part of the public Windows SDK and
    /// `kernel32.dll` does not export it, so this fails with
    /// [`crate::ErrorKind::UnsupportedFeature`] on the
    /// system backend. Bundling a `conpty.dll` (see
    /// [`ConPtyBackend::from_dir`](crate::ConPtyBackend::from_dir)) is what
    /// makes it available; [`Pty::supports_clear`] answers in advance.
    ///
    /// # Errors
    ///
    /// - [`crate::ErrorKind::UnsupportedFeature`] if the
    ///   backend has no clear export.
    /// - [`crate::ErrorKind::Clear`] with the backend failure, or
    ///   an
    ///   [`io::ErrorKind::NotConnected`] error once the session has been torn
    ///   down.
    ///
    /// [`ConPtyBackend::from_dir`]: crate::ConPtyBackend::from_dir
    #[cfg(test)]
    pub(crate) fn clear(&self) -> Result<()> {
        self.inner.clear()
    }

    /// Returns whether [`Pty::clear`] is available on this session's backend.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn supports_clear(&self) -> bool {
        self.inner.supports_clear()
    }

    /// Returns whether this session's backend exports
    /// `ReleasePseudoConsole`, which decides which of the two lifecycles from
    /// the module documentation the session runs.
    ///
    /// With `true`, the session is released right after
    /// [`Command::spawn_in`](super::Command::spawn_in) and end-of-file arrives
    /// naturally once the console host exits. With `false`, end-of-file has to
    /// be forced by the legacy watcher that [`PtyBuilder::eof_on_root_exit`]
    /// controls, about a second after the root child exits.
    ///
    /// A session built without an explicit backend can only learn its
    /// lifecycle here: which backend the default resolves to depends on the
    /// operating system and on any bundle next to the executable.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn supports_release(&self) -> bool {
        self.inner.supports_release()
    }

    /// Returns which `ConPTY` implementation backs this session.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn backend_kind(&self) -> &BackendKind {
        self.inner.backend_kind()
    }

    /// Returns a cloneable control handle for this pseudoconsole.
    #[must_use]
    pub(crate) fn controller(&self) -> PtyController {
        PtyController::new(Arc::clone(&self.inner))
    }

    /// Borrows the read and write halves separately.
    ///
    /// Useful to hand the two directions to different helpers within one
    /// scope. The borrow covers the whole `Pty`, so [`Pty::resize`] cannot be
    /// called while the halves are alive and the halves cannot be moved to
    /// another thread that outlives this one — use [`Pty::into_split`] when
    /// either is needed.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn split(&mut self) -> (ReadHalf<'_>, WriteHalf<'_>) {
        let Self { reader, writer, .. } = self;
        (ReadHalf { reader }, WriteHalf { writer })
    }

    /// Splits the session into independently owned read and write halves.
    ///
    /// This is the shape a real session usually wants: the
    /// [`OwnedReadHalf`] moves to a dedicated reader thread (which the
    /// pseudoconsole requires anyway, see the module docs), the
    /// [`OwnedWriteHalf`] goes wherever input is produced. Obtain a
    /// [`PtyController`] first with [`Pty::controller`] when control operations
    /// must continue after this call. Both halves retain the session strongly.
    #[must_use]
    pub(crate) fn into_split(self) -> (OwnedReadHalf, OwnedWriteHalf) {
        let Self {
            reader,
            writer,
            inner,
        } = self;
        let read_session = Arc::clone(&inner);
        (
            OwnedReadHalf {
                reader,
                _session: read_session,
            },
            OwnedWriteHalf {
                writer,
                _session: inner,
            },
        )
    }
}

/// Reads rendered console output. See [`OwnedReadHalf`] for the end-of-file
/// contract.
impl Read for Pty {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

/// Writes console input. [`flush`](Write::flush) is a no-op.
impl Write for Pty {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Borrowed read half of a [`Pty`], from [`Pty::split`].
#[cfg(test)]
pub(crate) struct ReadHalf<'a> {
    reader: &'a mut ConoutReader,
}

/// Deliberately opaque: the interesting state lives in the [`Pty`] this
/// borrows from.
#[cfg(test)]
impl fmt::Debug for ReadHalf<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadHalf").finish_non_exhaustive()
    }
}

#[cfg(test)]
impl Read for ReadHalf<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

/// Borrowed write half of a [`Pty`], from [`Pty::split`].
///
/// Writing has the same semantics as [`OwnedWriteHalf`]; dropping this borrow
/// does not close anything, because the pipe stays owned by the [`Pty`].
#[cfg(test)]
pub(crate) struct WriteHalf<'a> {
    writer: &'a mut ConinWriter,
}

/// Deliberately opaque: the interesting state lives in the [`Pty`] this
/// borrows from.
#[cfg(test)]
impl fmt::Debug for WriteHalf<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WriteHalf").finish_non_exhaustive()
    }
}

#[cfg(test)]
impl Write for WriteHalf<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Owned output half returned by [`Session::into_parts`](super::Session::into_parts).
///
/// # End-of-file
///
/// [`read`](Read::read) returns `Ok(0)` when the session is over. Errors that
/// mean "the other end is gone" — `ERROR_BROKEN_PIPE`, `ERROR_HANDLE_EOF`,
/// `ERROR_NO_DATA`, `ERROR_PIPE_NOT_CONNECTED` — are reported as that same
/// end-of-file, not as errors, so `read_to_end` finishes cleanly instead of
/// failing on the last read.
///
/// Reaching end-of-file, and dropping this half, are both reported to the
/// session's lifecycle machinery: they are the events that let
/// `ClosePseudoConsole` run promptly instead of waiting for a reader that will
/// never come back.
///
/// The bytes are a UTF-8 virtual-terminal stream. They arrive in whatever
/// chunks the console host produced, so a multi-byte character can straddle
/// two reads — decode across reads (or buffer) rather than per read.
pub struct OwnedReadHalf {
    reader: ConoutReader,
    /// Keeps the console alive even when every controller is dropped first.
    _session: Arc<SessionCore>,
}

/// Deliberately opaque: what this half owns — a pipe handle and lifecycle
/// bookkeeping — is exactly what `Debug` must not turn into public surface.
impl fmt::Debug for OwnedReadHalf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedReadHalf").finish_non_exhaustive()
    }
}

impl Read for OwnedReadHalf {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

/// Owned input half returned by [`Session::into_parts`](super::Session::into_parts).
///
/// Bytes written here become console input for the child, exactly as if they
/// had been typed: line-oriented programs expect `\r\n`, not `\n`.
///
/// [`flush`](Write::flush) is a no-op: writes go straight to the pipe, and
/// there is no user-space buffer to push.
///
/// # Dropping this half ends the session
///
/// Closing this half is not a polite "no more input" signal. It closes conin
/// and requests pseudoconsole close, which sends a close event to every
/// attached client. A child that is still running is therefore terminated
/// with exit code `0xC000013A` (`STATUS_CONTROL_C_EXIT`) and loses any output
/// it had not written yet. Hold on to this half until the child has exited —
/// or drop it deliberately to end a session that ignores everything else.
pub struct OwnedWriteHalf {
    writer: ConinWriter,
    /// Keeps the console alive even when every controller is dropped first.
    _session: Arc<SessionCore>,
}

/// Deliberately opaque: nothing but the conin pipe handle lives here.
impl fmt::Debug for OwnedWriteHalf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedWriteHalf").finish_non_exhaustive()
    }
}

impl Write for OwnedWriteHalf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The read end of the conout pipe, plus the lifecycle notifications the
/// pseudoconsole state machine needs from a reader.
#[derive(Debug)]
pub(super) struct ConoutReader {
    /// `None` only between the start and the end of [`Drop`].
    file: Option<File>,
    shared: Arc<ConsoleShared>,
    /// Whether end-of-file has already been reported once; the notification
    /// is idempotent, but repeating it on every subsequent read would take the
    /// state lock for nothing.
    saw_eof: bool,
}

/// Runs one reader EOF notification and suppresses repeated observations.
fn notify_eof_once(saw_eof: &mut bool, notify: impl FnOnce()) {
    if !*saw_eof {
        *saw_eof = true;
        notify();
    }
}

/// Maps only disconnect-class pipe failures to the EOF result.
fn conout_error_as_eof(err: io::Error) -> io::Result<()> {
    if is_disconnect_error(&err) {
        Ok(())
    } else {
        Err(err)
    }
}

impl ConoutReader {
    pub(super) fn new(handle: OwnedHandle, shared: Arc<ConsoleShared>) -> Self {
        Self {
            file: Some(File::from(handle)),
            shared,
            saw_eof: false,
        }
    }

    /// Reports end-of-file to the lifecycle state machine, once.
    ///
    /// This may run `ClosePseudoConsole` inline on the calling (reader)
    /// thread. That is the one case where closing from the reader is correct:
    /// end-of-file proves the console host is already gone, so the close has
    /// nothing left to wait for.
    fn on_eof(&mut self) {
        let shared = &self.shared;
        notify_eof_once(&mut self.saw_eof, || shared.notify_reader_eof());
    }
}

impl Read for ConoutReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // An empty buffer must not be mistaken for end-of-file.
        if buf.is_empty() {
            return Ok(0);
        }
        let Some(file) = self.file.as_mut() else {
            return Ok(0);
        };

        match file.read(buf) {
            Ok(0) => {
                self.on_eof();
                Ok(0)
            },
            Ok(read) => Ok(read),
            Err(err) => match conout_error_as_eof(err) {
                Ok(()) => {
                    self.on_eof();
                    Ok(0)
                },
                Err(err) => Err(err),
            },
        }
    }
}

/// Closes the read end, then tells the lifecycle state machine about it.
///
/// The order matters: with the handle already closed, a `ClosePseudoConsole`
/// triggered by the notification cannot block waiting for this reader — the
/// console host's writes fail instead. This is the documented "close the
/// output pipe first" shutdown.
impl Drop for ConoutReader {
    fn drop(&mut self) {
        drop(self.file.take());
        self.shared.notify_reader_closed();
    }
}

/// The write end of the conin pipe and its session-close notification.
///
/// Dropping closes the pipe first, then asks the lifecycle core to close a
/// spawned pseudoconsole. The explicit close request is required on legacy
/// Windows, where conin end-of-file alone does not reliably retire clients.
#[derive(Debug)]
pub(super) struct ConinWriter {
    file: Option<File>,
    session: Arc<SessionCore>,
}

impl ConinWriter {
    pub(super) fn new(handle: OwnedHandle, session: Arc<SessionCore>) -> Self {
        Self {
            file: Some(File::from(handle)),
            session,
        }
    }
}

impl Write for ConinWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "input is closed"))?
            .write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        // Writes reach the pipe synchronously and this type adds no buffering,
        // so there is nothing to flush. Flushing the underlying handle would
        // call `FlushFileBuffers`, which on a pipe blocks until the *reader*
        // has consumed everything — a deadlock, not a flush.
        Ok(())
    }
}

impl Drop for ConinWriter {
    fn drop(&mut self) {
        // Close conin before requesting HPCON close. The detached legacy
        // closer may start immediately, and must observe the terminal input
        // as already retired.
        drop(self.file.take());
        self.session.request_close_after_input();
    }
}

#[cfg(test)]
mod behavior_tests {
    use std::cell::Cell;
    use std::io;
    use std::sync::Arc;

    use super::{conout_error_as_eof, notify_eof_once, Pty};
    use crate::backend::ConPtyBackend;
    use crate::blocking::Command;

    #[test]
    fn writer_drop_requests_close_while_the_controller_keeps_the_session_alive() {
        let backend = ConPtyBackend::system()
            .expect("ConPTY must be available")
            .without_release();
        let pty = Pty::builder()
            .backend(backend)
            .eof_on_root_exit(false)
            .build()
            .expect("building a forced-legacy pty must succeed");
        let controller = pty.controller();
        let shared = Arc::clone(&pty.reader.shared);
        let child = Command::new("cmd.exe")
            .args(["/c", "pause"])
            .kill_on_drop(true)
            .spawn_in(&pty)
            .expect("spawning must succeed");
        let (reader, writer) = pty.into_split();

        drop(reader);
        assert!(
            !shared.is_closed(),
            "reader retirement alone must not request pseudoconsole close"
        );
        drop(writer);
        assert!(
            shared.is_closed(),
            "writer drop must claim pseudoconsole close while the controller keeps it alive"
        );

        drop(child);
        drop(controller);
    }

    #[test]
    fn eof_notification_runs_exactly_once() {
        let mut saw_eof = false;
        let notifications = Cell::new(0);
        notify_eof_once(&mut saw_eof, || notifications.set(notifications.get() + 1));
        notify_eof_once(&mut saw_eof, || notifications.set(notifications.get() + 1));
        assert!(saw_eof);
        assert_eq!(notifications.get(), 1);
    }

    #[test]
    fn only_disconnect_errors_become_eof() {
        assert!(conout_error_as_eof(io::Error::new(io::ErrorKind::BrokenPipe, "closed")).is_ok());
        let err = conout_error_as_eof(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
            .expect_err("an unrelated I/O failure must not become EOF");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }
}
