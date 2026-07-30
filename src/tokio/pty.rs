// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::pin::Pin;
use std::ptr;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use ::tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use ::tokio::net::windows::named_pipe::NamedPipeServer;
use windows_sys::Win32::System::IO::CancelIoEx;

#[cfg(test)]
use crate::backend::BackendKind;
use crate::core::is_disconnect_error;
use crate::core::pseudocon::ConsoleShared;
use crate::core::session::Session as SessionCore;
#[cfg(test)]
use crate::error::Result;
#[cfg(test)]
use crate::size::Size;

use super::builder::PtyBuilder;
use crate::PtyController;

/// An asynchronous pseudoconsole session: the console plus both ends of its
/// I/O.
///
/// `Pty` implements [`AsyncRead`] (rendered console output) and [`AsyncWrite`]
/// (console input). Because both use `Pin<&mut Self>`, reading and writing
/// concurrently requires splitting the session first — see [`Pty::split`] for
/// a borrowed split and [`Pty::into_split`] for an owned one.
///
/// # Teardown
///
/// Dropping a `Pty` retires the read end first, then the write end, then the
/// pseudoconsole itself. Dropping never blocks and never leaks the session,
/// whatever order the halves of a split session are dropped in.
///
/// One async-specific caveat: dropping a session's pipes only *initiates*
/// their OS-level close. An overlapped operation still in flight is
/// cancelled, and the handle actually closes when the runtime's I/O driver
/// retires the cancelled operation. The lifecycle machinery accounts for
/// this — a `ClosePseudoConsole` that cannot be proven prompt runs on a
/// detached thread, never on the dropping one — but it does mean that on a
/// backend without `ReleasePseudoConsole` the console host may observe the
/// session's end only after the I/O driver next runs. Prompt teardown at the
/// OS level therefore additionally wants a live runtime; the drop itself
/// never waits for one.
///
/// Because closing the input pipe is part of that teardown, dropping a `Pty`
/// whose child is still running **terminates the child** — see the module
/// documentation. Keep the session alive until
/// [`crate::tokio::Child::wait`] returns.
pub(crate) struct Pty {
    pub(super) reader: ConoutReader,
    pub(super) writer: ConinWriter,
    pub(super) inner: Arc<SessionCore>,
}

/// Shows the session's identity — its size and backend — rather than raw
/// handle values and the private lifecycle state, which are noise that varies
/// between runs and would otherwise become de-facto public surface.
/// [`crate::ConPtyBackend`]'s own `Debug` follows the same rule.
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
    /// This is a plain synchronous method: the underlying
    /// `ResizePseudoConsole` is a short signal write to the console host that
    /// never blocks, so making it a future would buy nothing.
    ///
    /// # Errors
    ///
    /// [`crate::ErrorKind::Resize`] with the backend failure, or an
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
    /// [`crate::ErrorKind::UnsupportedFeature`] on the system backend. Bundling a
    /// `conpty.dll` (see [`ConPtyBackend::from_dir`]) is what makes it
    /// available; [`Pty::supports_clear`] answers in advance.
    ///
    /// # Errors
    ///
    /// - [`crate::ErrorKind::UnsupportedFeature`] if the backend has no clear
    ///   export.
    /// - [`crate::ErrorKind::Clear`] with the backend failure, or an
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
    /// [`crate::tokio::Command::spawn_in`] and end-of-file arrives naturally
    /// once the console host exits. With `false`, end-of-file has to be forced
    /// by the legacy watcher that [`PtyBuilder::eof_on_root_exit`] controls,
    /// about a second after the root child exits.
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
    /// scope, for instance to `tokio::io::copy` in both directions under a
    /// `tokio::try_join!`. The borrow covers the whole `Pty`, so
    /// [`Pty::resize`] cannot be called while the halves are alive and the
    /// halves cannot be moved into a task that outlives this scope — use
    /// [`Pty::into_split`] when either is needed.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn split(&mut self) -> (ReadHalf<'_>, WriteHalf<'_>) {
        let Self { reader, writer, .. } = self;
        (ReadHalf { reader }, WriteHalf { writer })
    }

    /// Splits the session into independently owned read and write halves.
    ///
    /// This is the shape a real session usually wants: the
    /// [`OwnedReadHalf`] moves into a dedicated reader task (which the
    /// pseudoconsole effectively requires anyway, see the module docs), the
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
impl AsyncRead for Pty {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().reader).poll_read(cx, buf)
    }
}

/// Writes console input. See [`OwnedWriteHalf`] for what flushing and shutting
/// down mean here.
impl AsyncWrite for Pty {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().writer).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Every conin writer is unbuffered, so flush is deliberately a direct
        // no-op. Delegating would have exactly the same behavior and state,
        // but spelling the contract here avoids pretending otherwise.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_shutdown(cx)
    }
}

/// Borrowed read half of an asynchronous [`Pty`], from [`Pty::split`].
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
impl AsyncRead for ReadHalf<'_> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.get_mut().reader).poll_read(cx, buf)
    }
}

/// Borrowed write half of an asynchronous [`Pty`], from [`Pty::split`].
///
/// Writing has the same semantics as [`OwnedWriteHalf`], including the fact
/// that shutting it down ends the session. Dropping this borrow, on the other
/// hand, closes nothing, because the pipe stays owned by the [`Pty`].
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
impl AsyncWrite for WriteHalf<'_> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.get_mut().writer).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.get_mut().writer).poll_shutdown(cx)
    }
}

/// Owned output half returned by [`Session::into_parts`](super::Session::into_parts).
///
/// # End-of-file
///
/// A read completes with zero bytes when the session is over. Errors that mean
/// "the other end is gone" — `ERROR_BROKEN_PIPE`, `ERROR_HANDLE_EOF`,
/// `ERROR_NO_DATA`, `ERROR_PIPE_NOT_CONNECTED` — are reported as that same
/// end-of-file, not as errors, so `AsyncReadExt::read_to_end` finishes cleanly
/// instead of failing on the last read.
///
/// Reaching end-of-file, and dropping this half, are both reported to the
/// session's lifecycle machinery: they are the events that let
/// `ClosePseudoConsole` run promptly instead of waiting for a reader that will
/// never come back. The reporting happens in [`Drop`] and never blocks: a
/// close runs inline only where it is proven prompt, and one that cannot be
/// proven prompt — possible on a backend without `ReleasePseudoConsole`,
/// because dropping an async pipe closes the OS handle only once the I/O
/// driver retires its in-flight read — is handed to a detached thread
/// instead.
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

impl AsyncRead for OwnedReadHalf {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().reader).poll_read(cx, buf)
    }
}

/// Owned input half returned by [`Session::into_parts`](super::Session::into_parts).
///
/// Bytes written here become console input for the child, exactly as if they
/// had been typed: line-oriented programs expect `\r\n`, not `\n`.
///
/// `AsyncWriteExt::flush` is a no-op: writes go straight to the pipe, and
/// there is no user-space buffer to push.
///
/// # Dropping — or shutting down — this half ends the session
///
/// Closing the input pipe is not a polite "no more input" signal. The console
/// host reads it as the terminal being closed and sends a close event to every
/// attached client, so a child that is still running is terminated with exit
/// code `0xC000013A` (`STATUS_CONTROL_C_EXIT`) and loses any output it had not
/// written yet. Hold on to this half until the child has exited — or close it
/// deliberately, as a way to end a session that ignores everything else.
///
/// `AsyncWriteExt::shutdown` is that deliberate close: it never blocks, it
/// cancels a write still in flight rather than flushing it, and it makes
/// later writes fail with [`io::ErrorKind::BrokenPipe`]; shutting down again
/// is a no-op. Dropping this half closes the pipe the same way. One caveat:
/// with an overlapped write in flight, the OS handle closes only once the
/// runtime's I/O driver has retired the cancelled operation, so the console
/// host observes the close after the driver's next poll — one poll, not an
/// unbounded wait for the host to drain the pipe.
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

impl AsyncWrite for OwnedWriteHalf {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().writer).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_shutdown(cx)
    }
}

/// The server end of the conout named pipe, plus the lifecycle notifications
/// the pseudoconsole state machine needs from a reader.
#[derive(Debug)]
pub(super) struct ConoutReader {
    /// `None` only between the start and the end of [`Drop`].
    pipe: Option<NamedPipeServer>,
    shared: Arc<ConsoleShared>,
    /// Whether end-of-file has already been reported once; the notification
    /// is idempotent, but repeating it on every subsequent poll would take the
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
    pub(super) const fn new(pipe: NamedPipeServer, shared: Arc<ConsoleShared>) -> Self {
        Self {
            pipe: Some(pipe),
            shared,
            saw_eof: false,
        }
    }
}

impl AsyncRead for ConoutReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // An empty buffer must not be mistaken for end-of-file.
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let this = self.get_mut();
        let Some(pipe) = this.pipe.as_mut() else {
            return Poll::Ready(Ok(()));
        };

        let before = buf.filled().len();
        match Pin::new(pipe).poll_read(cx, buf) {
            // Zero bytes into a non-empty buffer is end-of-file. Whether the
            // console host's exit surfaces this way or as a disconnect error
            // below depends on how far the broken pipe got through Tokio's
            // and mio's layers, so both are handled identically.
            Poll::Ready(Ok(())) => {
                if buf.filled().len() == before {
                    let shared = &this.shared;
                    notify_eof_once(&mut this.saw_eof, || shared.notify_reader_eof());
                }
                Poll::Ready(Ok(()))
            },
            Poll::Ready(Err(err)) => match conout_error_as_eof(err) {
                Ok(()) => {
                    let shared = &this.shared;
                    notify_eof_once(&mut this.saw_eof, || shared.notify_reader_eof());
                    Poll::Ready(Ok(()))
                },
                Err(err) => Poll::Ready(Err(err)),
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Retires the read end, then tells the lifecycle state machine about it.
///
/// Dropping the Tokio pipe only *initiates* the OS-level close: with a
/// mio-scheduled overlapped read still in flight — the state of every session
/// between registration and the I/O driver's next poll after conout activity
/// — the drop cancels the read, and the `CloseHandle` runs when the driver
/// retires the cancelled operation, not here. The notification therefore must
/// not be taken as proof that the conout read end is gone, and the lifecycle
/// machinery does not take it as one: the session is marked at build time
/// (`set_reader_close_deferred`), so a `ClosePseudoConsole` that would need
/// that proof to be prompt is routed to a detached thread instead of running
/// on this destructor's thread. Both steps below are synchronous and
/// non-blocking, which is what lets them happen in a destructor at all.
impl Drop for ConoutReader {
    fn drop(&mut self) {
        drop(self.pipe.take());
        self.shared.notify_reader_closed();
    }
}

/// The server end of the conin named pipe.
///
/// Nothing but the handle: dropping it — or shutting it down — closes the
/// pipe, which the console host reads as the terminal going away. Both go
/// through [`Self::close_pipe`], which cancels an in-flight write first.
#[derive(Debug)]
pub(super) struct ConinWriter {
    /// `None` once the pipe has been closed by a shutdown.
    pipe: Option<NamedPipeServer>,
    /// Per-instance proof that Drop took the cancellation path.
    #[cfg(test)]
    close_observer: Option<Arc<AtomicBool>>,
}

impl ConinWriter {
    pub(super) const fn new(pipe: NamedPipeServer) -> Self {
        Self {
            pipe: Some(pipe),
            #[cfg(test)]
            close_observer: None,
        }
    }

    /// Borrows the pipe, or reports that a shutdown has already closed it.
    fn pipe(&mut self) -> io::Result<&mut NamedPipeServer> {
        self.pipe.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the pseudoconsole input pipe has been shut down",
            )
        })
    }

    /// Cancels any in-flight overlapped write, then closes the pipe.
    ///
    /// mio deliberately lets a pending write run to completion before the
    /// handle is closed, which is right for data pipes and wrong for conin: a
    /// conin write goes pending exactly when the console host has stopped
    /// draining input, and the close *is* the "terminal is gone" signal the
    /// caller is trying to send — flushing would defer that signal until the
    /// wedged host reads again, potentially forever. `CancelIoEx` bounds the
    /// deferral to one poll of the runtime's I/O driver instead: the write
    /// completes as cancelled, the driver retires it, and the handle closes.
    /// Idempotent, synchronous, and never blocking.
    fn close_pipe(&mut self) {
        let Some(pipe) = self.pipe.take() else {
            return;
        };
        #[cfg(test)]
        if let Some(observer) = &self.close_observer {
            observer.store(true, Ordering::SeqCst);
        }
        // SAFETY: `pipe` still owns the handle, so it is live for the call; a
        // null OVERLAPPED requests cancellation of every operation this
        // process issued on it, which is the intent (conin carries no reads —
        // mio's eager registration read fails synchronously on an outbound
        // pipe). Failure needs no handling: `ERROR_NOT_FOUND` just means
        // nothing was pending, and the drop below closes the handle — or
        // schedules the close — either way.
        unsafe { CancelIoEx(pipe.as_raw_handle(), ptr::null()) };
        drop(pipe);
    }
}

/// Dropping the write half is documented to end the session, so the close
/// must not linger behind an in-flight write either; see
/// [`ConinWriter::close_pipe`].
impl Drop for ConinWriter {
    fn drop(&mut self) {
        self.close_pipe();
    }
}

impl AsyncWrite for ConinWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let pipe = match self.get_mut().pipe() {
            Ok(pipe) => pipe,
            Err(err) => return Poll::Ready(Err(err)),
        };
        Pin::new(pipe).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Writes reach the pipe without passing through any buffer of ours, so
        // there is nothing to flush. Flushing the underlying handle would call
        // `FlushFileBuffers`, which on a pipe blocks until the *reader* has
        // consumed everything — a deadlock, not a flush.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Closing the handle is the shutdown: a named pipe has no half-close,
        // and this direction's end-of-file is precisely what tells the console
        // host that the terminal is gone. Idempotent, and never blocking; a
        // write still in flight is cancelled rather than flushed (see
        // `close_pipe`).
        self.get_mut().close_pipe();
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod behavior_tests {
    use std::cell::Cell;
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use super::{conout_error_as_eof, notify_eof_once, Pty};

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

    #[tokio::test]
    async fn dropping_a_pty_runs_the_conin_cancellation_path() {
        let mut pty = Pty::builder().build().expect("building must succeed");
        let closed = Arc::new(AtomicBool::new(false));
        pty.writer.close_observer = Some(Arc::clone(&closed));

        drop(pty);

        assert!(
            closed.load(Ordering::SeqCst),
            "ConinWriter::drop must cancel pending I/O before closing the pipe"
        );
    }
}
