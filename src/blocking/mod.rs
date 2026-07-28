//! Blocking pseudoconsole sessions.
//!
//! This is the synchronous front end of the crate. A session is built in three
//! steps:
//!
//! 1. [`Pty::builder`] creates the pseudoconsole and its two pipes.
//! 2. [`Command::spawn`] launches the root child process attached to it.
//! 3. The caller reads rendered output from the [`Pty`] and writes user input
//!    into it, then [`Child::wait`]s.
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
//! Use [`Pty::into_split`] to move the read half onto its own thread; the
//! returned [`PtyController`] still resizes the session from anywhere.
//!
//! # Closing the input pipe ends the session
//!
//! Dropping the write half of a session is **not** the console equivalent of
//! closing a child's stdin. The console host treats end-of-file on its input
//! pipe as "the terminal window was closed" and sends a close event to every
//! attached client, which terminates them: a child killed this way reports
//! exit code `0xC000013A` (`STATUS_CONTROL_C_EXIT`) and any output it had not
//! flushed yet is lost.
//!
//! Keep the write half — or the whole [`Pty`] — alive until the child has
//! exited. Dropping it earlier is a way to *stop* a session, not a way to
//! signal one.
//!
//! # The end-of-file contract
//!
//! Reading a [`Pty`] (or an [`OwnedReadHalf`]) returns `Ok(0)` exactly once the
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
//!   the output stream. A watcher thread waits for the root child to exit,
//!   grants a short grace period for the reader to drain the tail, and closes
//!   the pseudoconsole — which breaks the pipe and produces end-of-file. This
//!   is what [`PtyBuilder::eof_on_root_exit`] controls.
//!
//! # Examples
//!
//! ```no_run
//! use std::io::Read;
//! use std::thread;
//!
//! use conpty_oxide::blocking::{Command, Pty};
//! use conpty_oxide::Size;
//!
//! # fn main() -> conpty_oxide::Result<()> {
//! let pty = Pty::builder().size(Size::new(24, 80)).build()?;
//! let mut child = Command::new("cmd.exe")
//!     .args(["/c", "echo", "hello"])
//!     .spawn(&pty)?;
//!
//! // The output pipe must be drained while the child runs. The write half is
//! // unused here but deliberately kept alive: dropping it would end the
//! // session early (see above).
//! let (mut reader, writer, _controller) = pty.into_split();
//! let output = thread::spawn(move || {
//!     let mut buf = Vec::new();
//!     reader.read_to_end(&mut buf).map(|_| buf)
//! });
//!
//! let status = child.wait()?;
//! let output = output.join().expect("the reader thread must not panic")?;
//! drop(writer);
//!
//! print!("{}", String::from_utf8_lossy(&output));
//! assert!(status.success());
//! # Ok(())
//! # }
//! ```

use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, OwnedHandle, RawHandle};
use std::path::Path;
use std::sync::Arc;

use crate::backend::{BackendKind, ConPtyBackend};
use crate::core::is_disconnect_error;
use crate::core::job::Job;
use crate::core::pipes::{create_sync_pipes, SyncPipes};
use crate::core::pseudocon::{ConsoleShared, PseudoConsole};
use crate::core::session::{self, Session, KILL_EXIT_CODE};
use crate::core::wait::ProcessWaiter;
use crate::error::{Error, Result};
use crate::size::Size;
use crate::status::ExitStatus;

/// Builder for a [`Pty`].
///
/// Created by [`Pty::builder`]. Every option has a working default, so
/// `Pty::builder().build()` is a complete 24x80 session on the system backend.
#[derive(Debug, Clone)]
pub struct PtyBuilder {
    size: Size,
    backend: Option<ConPtyBackend>,
    inherit_cursor: bool,
    eof_on_root_exit: bool,
}

/// 24x80, the system backend, no cursor inheritance, end-of-file forced when
/// the root child exits.
impl Default for PtyBuilder {
    fn default() -> Self {
        Self {
            size: Size::default(),
            backend: None,
            inherit_cursor: false,
            eof_on_root_exit: true,
        }
    }
}

impl PtyBuilder {
    /// Sets the initial size of the pseudoconsole.
    ///
    /// Defaults to [`Size::default`] (24 rows by 80 columns). The size can be
    /// changed later with [`Pty::resize`].
    #[must_use]
    pub fn size(mut self, size: Size) -> Self {
        self.size = size;
        self
    }

    /// Uses `backend` instead of the process-wide default.
    ///
    /// Without this, [`build`](Self::build) uses the backend installed with
    /// [`ConPtyBackend::set_global_default`], or loads the system one.
    #[must_use]
    pub fn backend(mut self, backend: ConPtyBackend) -> Self {
        self.backend = Some(backend);
        self
    }

    /// Sets `PSEUDOCONSOLE_INHERIT_CURSOR`, making the new pseudoconsole adopt
    /// the cursor position of the calling process's console. Defaults to
    /// `false`.
    ///
    /// # Warning
    ///
    /// The flag makes the pseudoconsole write a Device Status Report
    /// (`ESC [ 6 n`) to the output pipe immediately after creation and stop
    /// processing input entirely until the reply is written back to the input
    /// pipe. Enable it only if the session's output is already being drained
    /// by another thread *and* the reply is echoed back — otherwise the child
    /// never receives any input. Teardown hangs have also been reported with
    /// the flag set (microsoft/terminal#17688), so leaving it off is
    /// recommended.
    #[must_use]
    pub fn inherit_cursor(mut self, inherit: bool) -> Self {
        self.inherit_cursor = inherit;
        self
    }

    /// Controls whether the crate forces end-of-file when the root child
    /// exits. Defaults to `true`.
    ///
    /// This only has an effect on backends without `ReleasePseudoConsole`
    /// (everything before Windows 11 24H2); a released session reaches
    /// end-of-file naturally and never starts a watcher.
    ///
    /// With `true`, [`Command::spawn`] starts a watcher thread that waits for
    /// the root child to exit, waits about a second more for the reader to
    /// drain the console host's remaining output, and then closes the
    /// pseudoconsole so the reader sees end-of-file. The side effects are
    /// worth spelling out:
    ///
    /// - Output written by *descendants* that outlive the root child (a
    ///   detached background process, for instance) may be cut off, because
    ///   the session ends with the root and not with the last writer.
    /// - The session is torn down even if the caller still holds the
    ///   [`PtyController`], so [`PtyController::resize`] starts failing with
    ///   [`io::ErrorKind::NotConnected`] after the child exits.
    ///
    /// With `false`, no watcher is started and the reader of a legacy session
    /// will **not** observe end-of-file when the child exits. It then only
    /// arrives when the read half is dropped or the whole session is, so the
    /// caller must have another way of knowing the session is finished (for
    /// example [`Child::wait`] on a separate thread). Prefer `false` only when
    /// the child's descendants matter more than a prompt end-of-file.
    #[must_use]
    pub fn eof_on_root_exit(mut self, eof: bool) -> Self {
        self.eof_on_root_exit = eof;
        self
    }

    /// Creates the pseudoconsole and its pipes.
    ///
    /// # Errors
    ///
    /// - [`Error::Backend`] if no backend was given and the ConPTY API cannot
    ///   be loaded (Windows older than 10 1809).
    /// - [`Error::CreateConsole`] if the pipes or `CreatePseudoConsole` fail.
    pub fn build(self) -> Result<Pty> {
        let backend = match self.backend {
            Some(backend) => backend,
            None => ConPtyBackend::resolve_default()?,
        };

        let SyncPipes {
            conout_read,
            conout_write,
            conin_read,
            conin_write,
        } = create_sync_pipes().map_err(Error::CreateConsole)?;

        // The two client ends are consumed here: the pseudoconsole closes them
        // as soon as the console host has its own duplicates, which is what
        // makes end-of-file possible at all.
        let console = PseudoConsole::new(
            backend.clone(),
            self.size,
            conin_read,
            conout_write,
            self.inherit_cursor,
        )
        .map_err(Error::CreateConsole)?;

        let shared = Arc::clone(console.shared());
        Ok(Pty {
            reader: ConoutReader::new(conout_read, shared),
            writer: ConinWriter::new(conin_write),
            inner: Arc::new(Session::new(
                console,
                backend,
                self.size,
                self.eof_on_root_exit,
            )),
        })
    }
}

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
/// pseudoconsole itself, which is the order that keeps `ClosePseudoConsole`
/// from blocking: with the read end gone the console host's writes fail
/// instead of waiting for a reader. Dropping never blocks and never leaks the
/// session, whatever order the halves of a split session are dropped in.
///
/// Because closing the input pipe is part of that teardown, dropping a `Pty`
/// whose child is still running **terminates the child** — see the module
/// documentation. Keep the session alive until [`Child::wait`] returns.
pub struct Pty {
    reader: ConoutReader,
    writer: ConinWriter,
    inner: Arc<Session>,
}

/// Shows the session's identity — its size and backend — rather than raw
/// handle values and the private lifecycle state, which are noise that varies
/// between runs and would otherwise become de-facto public surface.
/// [`ConPtyBackend`]'s own `Debug` follows the same rule.
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
    pub fn builder() -> PtyBuilder {
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
    /// [`Error::Resize`] wrapping the backend failure, or an
    /// [`io::ErrorKind::NotConnected`] error once the session has been torn
    /// down.
    pub fn resize(&self, size: Size) -> Result<()> {
        self.inner.resize(size)
    }

    /// Returns the size last accepted by [`Pty::resize`], or the size the
    /// session was built with.
    #[must_use]
    pub fn size(&self) -> Size {
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
    /// [`Error::UnsupportedFeature`] on the system backend. Bundling a
    /// `conpty.dll` (see [`ConPtyBackend::from_dir`]) is what makes it
    /// available; [`Pty::supports_clear`] answers in advance.
    ///
    /// # Errors
    ///
    /// - [`Error::UnsupportedFeature`] if the backend has no clear export.
    /// - [`Error::Clear`] wrapping the backend failure, or an
    ///   [`io::ErrorKind::NotConnected`] error once the session has been torn
    ///   down.
    ///
    /// [`ConPtyBackend::from_dir`]: crate::ConPtyBackend::from_dir
    pub fn clear(&self) -> Result<()> {
        self.inner.clear()
    }

    /// Returns whether [`Pty::clear`] is available on this session's backend.
    #[must_use]
    pub fn supports_clear(&self) -> bool {
        self.inner.supports_clear()
    }

    /// Returns whether this session's backend exports
    /// `ReleasePseudoConsole`, which decides which of the two lifecycles from
    /// the module documentation the session runs.
    ///
    /// With `true`, the session is released right after [`Command::spawn`]
    /// and end-of-file arrives naturally once the console host exits. With
    /// `false`, end-of-file has to be forced by the legacy watcher that
    /// [`PtyBuilder::eof_on_root_exit`] controls, about a second after the
    /// root child exits.
    ///
    /// A session built without an explicit backend can only learn its
    /// lifecycle here: which backend the default resolves to depends on the
    /// operating system and on any bundle next to the executable.
    #[must_use]
    pub fn supports_release(&self) -> bool {
        self.inner.supports_release()
    }

    /// Returns which ConPTY implementation backs this session.
    #[must_use]
    pub fn backend_kind(&self) -> &BackendKind {
        self.inner.backend_kind()
    }

    /// Borrows the read and write halves separately.
    ///
    /// Useful to hand the two directions to different helpers within one
    /// scope. The borrow covers the whole `Pty`, so [`Pty::resize`] cannot be
    /// called while the halves are alive and the halves cannot be moved to
    /// another thread that outlives this one — use [`Pty::into_split`] when
    /// either is needed.
    #[must_use]
    pub fn split(&mut self) -> (ReadHalf<'_>, WriteHalf<'_>) {
        let Self { reader, writer, .. } = self;
        (ReadHalf { reader }, WriteHalf { writer })
    }

    /// Splits the session into three independently owned parts.
    ///
    /// This is the shape a real session usually wants: the
    /// [`OwnedReadHalf`] moves to a dedicated reader thread (which the
    /// pseudoconsole requires anyway, see the module docs), the
    /// [`OwnedWriteHalf`] goes wherever input is produced, and the
    /// [`PtyController`] stays behind to resize. All three are [`Send`] and
    /// [`Sync`], and each may be dropped in any order.
    #[must_use]
    pub fn into_split(self) -> (OwnedReadHalf, OwnedWriteHalf, PtyController) {
        let Self {
            reader,
            writer,
            inner,
        } = self;
        (
            OwnedReadHalf { reader },
            OwnedWriteHalf { writer },
            PtyController { inner },
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
        self.writer.flush()
    }
}

/// Borrowed read half of a [`Pty`], from [`Pty::split`].
pub struct ReadHalf<'a> {
    reader: &'a mut ConoutReader,
}

/// Deliberately opaque: the interesting state lives in the [`Pty`] this
/// borrows from.
impl fmt::Debug for ReadHalf<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadHalf").finish_non_exhaustive()
    }
}

impl Read for ReadHalf<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

/// Borrowed write half of a [`Pty`], from [`Pty::split`].
///
/// Writing has the same semantics as [`OwnedWriteHalf`]; dropping this borrow
/// does not close anything, because the pipe stays owned by the [`Pty`].
pub struct WriteHalf<'a> {
    writer: &'a mut ConinWriter,
}

/// Deliberately opaque: the interesting state lives in the [`Pty`] this
/// borrows from.
impl fmt::Debug for WriteHalf<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WriteHalf").finish_non_exhaustive()
    }
}

impl Write for WriteHalf<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// Owned read half of a [`Pty`], from [`Pty::into_split`].
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

/// Owned write half of a [`Pty`], from [`Pty::into_split`].
///
/// Bytes written here become console input for the child, exactly as if they
/// had been typed: line-oriented programs expect `\r\n`, not `\n`.
///
/// [`flush`](Write::flush) is a no-op: writes go straight to the pipe, and
/// there is no user-space buffer to push.
///
/// # Dropping this half ends the session
///
/// Closing the input pipe is not a polite "no more input" signal. The console
/// host reads it as the terminal being closed and sends a close event to every
/// attached client, so a child that is still running is terminated with exit
/// code `0xC000013A` (`STATUS_CONTROL_C_EXIT`) and loses any output it had not
/// written yet. Hold on to this half until the child has exited — or drop it
/// deliberately, as a way to end a session that ignores everything else.
pub struct OwnedWriteHalf {
    writer: ConinWriter,
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
        self.writer.flush()
    }
}

/// Control handle for a split session, from [`Pty::into_split`].
///
/// Holds the pseudoconsole itself, so the session stays usable while the read
/// and write halves live on other threads. Dropping it does not end the
/// session: the read half keeps the lifecycle state alive and the
/// pseudoconsole is closed once every part is gone.
pub struct PtyController {
    inner: Arc<Session>,
}

/// Shows the session's identity, exactly as [`Pty`]'s `Debug` does.
impl fmt::Debug for PtyController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PtyController")
            .field("size", &self.inner.size())
            .field("backend_kind", self.inner.backend_kind())
            .finish_non_exhaustive()
    }
}

impl PtyController {
    /// Resizes the pseudoconsole. See [`Pty::resize`].
    ///
    /// # Errors
    ///
    /// [`Error::Resize`] wrapping the backend failure, or an
    /// [`io::ErrorKind::NotConnected`] error once the session has been torn
    /// down.
    pub fn resize(&self, size: Size) -> Result<()> {
        self.inner.resize(size)
    }

    /// Returns the size last accepted by [`PtyController::resize`], or the
    /// size the session was built with.
    #[must_use]
    pub fn size(&self) -> Size {
        self.inner.size()
    }

    /// Clears the pseudoconsole's screen and scrollback. See [`Pty::clear`].
    ///
    /// # Errors
    ///
    /// - [`Error::UnsupportedFeature`] if the backend has no clear export.
    /// - [`Error::Clear`] wrapping the backend failure, or an
    ///   [`io::ErrorKind::NotConnected`] error once the session has been torn
    ///   down.
    pub fn clear(&self) -> Result<()> {
        self.inner.clear()
    }

    /// Returns whether [`PtyController::clear`] is available on this session's
    /// backend.
    #[must_use]
    pub fn supports_clear(&self) -> bool {
        self.inner.supports_clear()
    }

    /// Returns whether this session's backend exports
    /// `ReleasePseudoConsole`. See [`Pty::supports_release`].
    #[must_use]
    pub fn supports_release(&self) -> bool {
        self.inner.supports_release()
    }

    /// Returns which ConPTY implementation backs this session.
    #[must_use]
    pub fn backend_kind(&self) -> &BackendKind {
        self.inner.backend_kind()
    }
}

/// The read end of the conout pipe, plus the lifecycle notifications the
/// pseudoconsole state machine needs from a reader.
#[derive(Debug)]
struct ConoutReader {
    /// `None` only between the start and the end of [`Drop`].
    file: Option<File>,
    shared: Arc<ConsoleShared>,
    /// Whether end-of-file has already been reported once; the notification
    /// is idempotent, but repeating it on every subsequent read would take the
    /// state lock for nothing.
    saw_eof: bool,
}

impl ConoutReader {
    fn new(handle: OwnedHandle, shared: Arc<ConsoleShared>) -> Self {
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
        if !self.saw_eof {
            self.saw_eof = true;
            self.shared.notify_reader_eof();
        }
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
            }
            Ok(read) => Ok(read),
            Err(err) if is_disconnect_error(&err) => {
                self.on_eof();
                Ok(0)
            }
            Err(err) => Err(err),
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

/// The write end of the conin pipe.
///
/// Nothing but the handle: dropping it closes the pipe, which the console
/// host reads as the terminal going away and tears the session down.
#[derive(Debug)]
struct ConinWriter {
    file: File,
}

impl ConinWriter {
    fn new(handle: OwnedHandle) -> Self {
        Self {
            file: File::from(handle),
        }
    }
}

impl Write for ConinWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        // Writes reach the pipe synchronously and this type adds no buffering,
        // so there is nothing to flush. Flushing the underlying handle would
        // call `FlushFileBuffers`, which on a pipe blocks until the *reader*
        // has consumed everything — a deadlock, not a flush.
        Ok(())
    }
}

/// A command to run inside a pseudoconsole.
///
/// Mirrors [`std::process::Command`]: the builder methods take `&mut self` and
/// return `&mut Self`, so a whole invocation can be written as one expression.
/// The differences from the standard library are the ones a pseudoconsole
/// forces:
///
/// - There is no stdio configuration. The child's console *is* the
///   pseudoconsole; its standard handles are deliberately set to
///   `INVALID_HANDLE_VALUE` so a redirected parent cannot leak its own stdio
///   into the child.
/// - No handles are inherited (`bInheritHandles` is `FALSE`), because a leaked
///   copy of the output pipe would keep the session from ever reaching
///   end-of-file.
/// - The child and every descendant it creates join a job object, which is
///   what makes [`Child::kill`] terminate the whole tree.
#[derive(Debug, Clone)]
pub struct Command {
    inner: crate::command::Command,
}

impl Command {
    /// Creates a builder for launching `program`.
    ///
    /// The program is not resolved here; a missing executable surfaces as
    /// [`Error::Spawn`] with an [`io::ErrorKind::NotFound`] source.
    #[must_use]
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            inner: crate::command::Command::new(program),
        }
    }

    /// Appends one argument, quoted and escaped as the MSVC C runtime expects.
    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.inner.arg(arg);
        self
    }

    /// Appends several arguments; equivalent to calling [`Command::arg`] for
    /// each one.
    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.inner.args(args);
        self
    }

    /// Appends literal text to the command line, bypassing all quoting.
    ///
    /// Same semantics as `std::os::windows::process::CommandExt::raw_arg`:
    /// intended for callees such as `cmd.exe /c` that parse the raw command
    /// line themselves.
    pub fn raw_arg(&mut self, text: impl AsRef<OsStr>) -> &mut Self {
        self.inner.raw_arg(text);
        self
    }

    /// Sets an environment variable for the child (case-insensitively, as
    /// Windows does).
    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.inner.env(key, value);
        self
    }

    /// Sets several environment variables; equivalent to calling
    /// [`Command::env`] for each pair.
    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.inner.envs(vars);
        self
    }

    /// Removes an environment variable from the child's environment.
    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.inner.env_remove(key);
        self
    }

    /// Clears the child's environment, including modifications recorded so
    /// far. Variables set afterwards still apply.
    pub fn env_clear(&mut self) -> &mut Self {
        self.inner.env_clear();
        self
    }

    /// Sets the child's working directory.
    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.inner.current_dir(dir);
        self
    }

    /// Sets extra `dwCreationFlags` for `CreateProcessW`.
    ///
    /// They are OR'ed with the flags the crate always passes
    /// (`EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT`). Same
    /// semantics as `std::os::windows::process::CommandExt::creation_flags`.
    pub fn creation_flags(&mut self, flags: u32) -> &mut Self {
        self.inner.creation_flags(flags);
        self
    }

    /// Terminates the child's whole process tree when its [`Child`] is
    /// dropped. Defaults to `false`.
    ///
    /// This also sets `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` on the session's
    /// job object, so the tree is terminated by the kernel even if this
    /// process dies without running any destructor.
    pub fn kill_on_drop(&mut self, kill: bool) -> &mut Self {
        self.inner.kill_on_drop(kill);
        self
    }

    /// Spawns the command as the root child of `pty`.
    ///
    /// The session's shutdown strategy is armed as part of this call, in the
    /// order ConPTY requires: the child is created, the pseudoconsole is
    /// released if the backend supports it, and only if it does not — and only
    /// when [`PtyBuilder::eof_on_root_exit`] is set — is the legacy watcher
    /// thread started.
    ///
    /// A pseudoconsole hosts exactly one root child; spawning into a `Pty`
    /// that already has one fails with an [`io::ErrorKind::AlreadyExists`]
    /// source. (Descendants are unrestricted — the child may create as many as
    /// it likes, and they all join the same job object.)
    ///
    /// # Errors
    ///
    /// [`Error::Spawn`] carrying the program name and the underlying failure:
    /// [`io::ErrorKind::NotFound`] for a missing executable,
    /// [`io::ErrorKind::InvalidInput`] for a command line or environment block
    /// that cannot be built, [`io::ErrorKind::AlreadyExists`] for a re-used
    /// `Pty`, or the raw OS error from `CreateProcessW`.
    pub fn spawn(&mut self, pty: &Pty) -> Result<Child> {
        let root = session::spawn_root(&pty.inner, &self.inner)?;
        Ok(Child {
            waiter: root.waiter,
            job: root.job,
            pid: root.pid,
            kill_on_drop: root.kill_on_drop,
            status: None,
        })
    }
}

/// A running (or finished) root child of a pseudoconsole session.
///
/// The handle owns the session's job object as well as the process handle, so
/// [`Child::kill`] terminates the whole process tree rather than just the
/// process this crate created.
///
/// Dropping a `Child` does not wait for the process. Unless
/// [`Command::kill_on_drop`] was set, the tree keeps running; what ends the
/// *session* is the pseudoconsole's own teardown (see the module docs).
pub struct Child {
    waiter: ProcessWaiter,
    job: Job,
    pid: u32,
    kill_on_drop: bool,
    /// Cached once the process is known to have exited, so repeated
    /// `wait`/`try_wait` calls stay cheap and consistent.
    status: Option<ExitStatus>,
}

/// Shows the child's identity — pid, drop policy, and any cached exit status
/// — rather than the raw process and job handles, whose values are noise that
/// varies between runs.
impl fmt::Debug for Child {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Child")
            .field("pid", &self.pid)
            .field("kill_on_drop", &self.kill_on_drop)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl Child {
    /// Returns the child's process identifier.
    ///
    /// The identifier stays valid as long as this `Child` is alive; once the
    /// process handle is closed, Windows may reuse the number.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Waits for the child to exit and returns its status.
    ///
    /// Repeated calls return the cached status instead of waiting again.
    ///
    /// # Deadlock
    ///
    /// The child must be able to make progress while this blocks, which means
    /// something else has to drain the session's output — see the module docs.
    ///
    /// # Errors
    ///
    /// [`Error::Wait`] wrapping the OS error from `WaitForSingleObject` or
    /// `GetExitCodeProcess`.
    pub fn wait(&mut self) -> Result<ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        let status = ExitStatus::from_raw(self.waiter.wait().map_err(Error::Wait)?);
        self.status = Some(status);
        Ok(status)
    }

    /// Returns the exit status if the child has already exited, without
    /// blocking.
    ///
    /// # Errors
    ///
    /// [`Error::Wait`] wrapping the OS error from `WaitForSingleObject` or
    /// `GetExitCodeProcess`.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        match self.waiter.try_wait().map_err(Error::Wait)? {
            Some(code) => {
                let status = ExitStatus::from_raw(code);
                self.status = Some(status);
                Ok(Some(status))
            }
            None => Ok(None),
        }
    }

    /// Terminates the child and every descendant it created.
    ///
    /// This terminates the session's job object, so processes the child
    /// spawned are killed too. Termination is asynchronous: call
    /// [`Child::wait`] afterwards to observe the resulting status, which is
    /// exit code `1`.
    ///
    /// Killing an already-finished tree succeeds and does nothing.
    ///
    /// # Errors
    ///
    /// [`Error::Kill`] wrapping the OS error from `TerminateJobObject`.
    pub fn kill(&mut self) -> Result<()> {
        self.job.terminate(KILL_EXIT_CODE).map_err(Error::Kill)
    }
}

/// Borrows the child's process handle, e.g. to duplicate it or to wait on it
/// together with other objects.
impl AsHandle for Child {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.waiter.as_handle()
    }
}

/// Returns the child's process handle. The handle stays owned by this `Child`
/// and must not be closed.
impl AsRawHandle for Child {
    fn as_raw_handle(&self) -> RawHandle {
        self.waiter.as_handle().as_raw_handle()
    }
}

/// Terminates the process tree when [`Command::kill_on_drop`] was set.
///
/// The kill is unconditional rather than skipped for an already-reaped child:
/// descendants can outlive the root, and terminating an empty job object is a
/// documented no-op. Failures are ignored — a destructor has nowhere to report
/// them, and the job object's `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` limit makes
/// the kernel finish the job when the handle closes moments later.
impl Drop for Child {
    fn drop(&mut self) {
        if self.kill_on_drop {
            let _ = self.job.terminate(KILL_EXIT_CODE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::core::session::CLEAR_FEATURE;

    use std::panic;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    /// Generous per-test budget: spawning `cmd.exe` under a fresh
    /// pseudoconsole plus a legacy teardown grace period is comfortably under
    /// this, and a hang is the failure mode being guarded against.
    const TEST_TIMEOUT: Duration = Duration::from_secs(30);

    /// `STATUS_CONTROL_C_EXIT`: the code a client reports when its terminal
    /// goes away, i.e. the crate's documented consequence of closing conin on
    /// a live session.
    const STATUS_CONTROL_C_EXIT: u32 = 0xC000_013A;

    /// Runs `f` on a helper thread and fails the test if it has not finished
    /// within [`TEST_TIMEOUT`].
    ///
    /// Every interesting failure in this module is a deadlock — an undrained
    /// output pipe, a `ClosePseudoConsole` that never returns, a `wait` for a
    /// child that can no longer run. Without a watchdog those would stall the
    /// whole test binary instead of failing one test. A panic inside `f` is
    /// re-raised here so assertion failures keep their original message.
    fn complete_within(name: &str, f: impl FnOnce() + Send + 'static) {
        let (done_tx, done_rx) = mpsc::channel();
        let handle = thread::Builder::new()
            .name(format!("watchdog-subject-{name}"))
            .spawn(move || {
                f();
                let _ = done_tx.send(());
            })
            .expect("spawning the test subject thread must succeed");

        match done_rx.recv_timeout(TEST_TIMEOUT) {
            Ok(()) => {}
            // The sender was dropped without sending: `f` panicked, and the
            // join below re-raises it with its original message.
            Err(mpsc::RecvTimeoutError::Disconnected) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("`{name}` hung for more than {TEST_TIMEOUT:?}")
            }
        }
        if let Err(payload) = handle.join() {
            panic::resume_unwind(payload);
        }
    }

    fn pty() -> Pty {
        Pty::builder().build().expect("building a pty must succeed")
    }

    /// A session forced onto the legacy shutdown path, whatever the OS.
    ///
    /// On machines whose ConPTY exports `ReleasePseudoConsole` (Windows 11
    /// 24H2 and later), every ordinary session in this module runs in released
    /// mode and `Command::spawn` never arms the legacy watcher. Stripping the
    /// export from the backend makes the spawn path take the watcher route for
    /// real, so its regressions fail here instead of only on pre-24H2 CI.
    fn legacy_pty() -> Pty {
        let backend = ConPtyBackend::system()
            .expect("ConPTY must be available")
            .without_release();
        assert!(!backend.supports_release());
        Pty::builder()
            .backend(backend)
            .build()
            .expect("building a forced-legacy pty must succeed")
    }

    /// A session under test: a running child, a thread draining its output,
    /// and the two halves that must stay alive while it runs.
    ///
    /// Keeping the write half open for the child's whole life is not
    /// housekeeping — closing the input pipe makes the console host terminate
    /// its clients, which would both corrupt the exit status and hide a broken
    /// end-of-file contract behind a trivially broken pipe.
    struct Running {
        child: Child,
        reader: thread::JoinHandle<Vec<u8>>,
        writer: OwnedWriteHalf,
        controller: PtyController,
    }

    impl Running {
        /// Spawns `command` in a fresh 24x80 session.
        fn start(command: &mut Command) -> Self {
            Self::start_in(pty(), command)
        }

        /// Spawns `command` in `pty`.
        fn start_in(pty: Pty, command: &mut Command) -> Self {
            let child = command.spawn(&pty).expect("spawning must succeed");
            Self::attach(pty, child)
        }

        /// Starts draining the output of an already-spawned child, which
        /// ConPTY requires to happen while the child runs.
        fn attach(pty: Pty, child: Child) -> Self {
            let (mut read_half, writer, controller) = pty.into_split();
            let reader = thread::Builder::new()
                .name("test-conout-reader".into())
                .spawn(move || {
                    let mut sink = Vec::new();
                    read_half
                        .read_to_end(&mut sink)
                        .expect("reading to end-of-file must succeed");
                    sink
                })
                .expect("spawning the reader thread must succeed");
            Self {
                child,
                reader,
                writer,
                controller,
            }
        }

        /// Waits for the child, then for end-of-file, and returns the rendered
        /// output together with the exit status.
        fn finish(self) -> (String, ExitStatus) {
            let Self {
                mut child,
                reader,
                writer,
                controller,
            } = self;
            let status = child.wait().expect("waiting must succeed");
            // Joining is the real assertion: it returns only once the session
            // reached end-of-file, and since the write half is still open,
            // that end-of-file can only have come from the crate's own
            // shutdown path (a natural release, or the legacy watcher).
            let output = reader.join().expect("the reader thread must not panic");
            drop(writer);
            drop(controller);
            (String::from_utf8_lossy(&output).into_owned(), status)
        }
    }

    /// Runs `cmd.exe` with `args` to completion in a fresh session.
    fn run_cmd(args: &[&str]) -> (String, ExitStatus) {
        Running::start(Command::new("cmd.exe").args(args)).finish()
    }

    #[test]
    fn owned_parts_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Pty>();
        assert_send_sync::<OwnedReadHalf>();
        assert_send_sync::<OwnedWriteHalf>();
        assert_send_sync::<PtyController>();
        assert_send_sync::<Child>();
        assert_send_sync::<Command>();
        assert_send_sync::<PtyBuilder>();
    }

    /// `Debug` output ends up in logs and bug reports, so it must show a
    /// session's identity — and must not leak raw handle values or the
    /// private lifecycle state machine, which would make internals part of
    /// the observable surface.
    #[test]
    fn debug_shows_identity_not_internals() {
        complete_within("debug_shows_identity_not_internals", || {
            let mut pty = pty();
            let rendered = format!("{pty:?}");
            assert!(rendered.starts_with("Pty"), "{rendered}");
            assert!(rendered.contains("size"), "{rendered}");
            assert!(rendered.contains("System"), "{rendered}");
            for leak in ["hpcon", "File", "handle", "state", "released"] {
                assert!(!rendered.contains(leak), "`{leak}` leaked: {rendered}");
            }

            let (read_half, write_half) = pty.split();
            assert_eq!(format!("{read_half:?}"), "ReadHalf { .. }");
            assert_eq!(format!("{write_half:?}"), "WriteHalf { .. }");

            let mut running = Running::start(Command::new("cmd.exe").args(["/c", "exit", "0"]));
            let rendered = format!("{:?}", running.child);
            assert!(rendered.contains("pid"), "{rendered}");
            assert!(!rendered.contains("handle"), "{rendered}");
            running.child.wait().expect("waiting must succeed");
            let rendered = format!("{:?}", running.child);
            assert!(
                rendered.contains("status"),
                "a reaped child must show its cached status: {rendered}"
            );

            assert_eq!(format!("{:?}", running.writer), "OwnedWriteHalf { .. }");
            let rendered = format!("{:?}", running.controller);
            assert!(rendered.starts_with("PtyController"), "{rendered}");
            assert!(rendered.contains("backend_kind"), "{rendered}");
            running.finish();
        });
    }

    #[test]
    fn builder_defaults_to_24_by_80_on_the_system_backend() {
        let pty = pty();
        assert_eq!(pty.size(), Size::new(24, 80));
        assert_eq!(pty.backend_kind(), &BackendKind::System);
    }

    #[test]
    fn builder_honours_an_explicit_size_and_backend() {
        let backend = ConPtyBackend::system().expect("ConPTY must be available");
        let pty = Pty::builder()
            .size(Size::new(50, 132))
            .backend(backend)
            .inherit_cursor(false)
            .eof_on_root_exit(true)
            .build()
            .expect("building must succeed");
        assert_eq!(pty.size(), Size::new(50, 132));
    }

    /// `eof_on_root_exit`'s documented behaviour depends on whether the
    /// backend has `ReleasePseudoConsole`, so a caller must be able to ask a
    /// built session which lifecycle it runs — and the answer has to match
    /// the backend's own, session by session rather than machine-wide.
    #[test]
    fn supports_release_matches_the_backend() {
        let backend = ConPtyBackend::system().expect("ConPTY must be available");
        let expected = backend.supports_release();
        let pty = Pty::builder()
            .backend(backend)
            .build()
            .expect("building must succeed");
        assert_eq!(pty.supports_release(), expected);
        let (_reader, _writer, controller) = pty.into_split();
        assert_eq!(controller.supports_release(), expected);

        // The query reflects the session's own backend, not the machine.
        let legacy = legacy_pty();
        assert!(!legacy.supports_release());
        let (_reader, _writer, controller) = legacy.into_split();
        assert!(!controller.supports_release());
    }

    #[test]
    fn resize_updates_the_reported_size() {
        let pty = pty();
        pty.resize(Size::new(40, 120)).expect("resize must succeed");
        assert_eq!(pty.size(), Size::new(40, 120));

        let (_reader, _writer, controller) = pty.into_split();
        assert_eq!(controller.size(), Size::new(40, 120));
        controller
            .resize(Size::new(24, 80))
            .expect("resize must succeed");
        assert_eq!(controller.size(), Size::new(24, 80));
        assert_eq!(controller.backend_kind(), &BackendKind::System);
    }

    /// Runs a short child in `pty` to completion, then checks the documented
    /// resize contract for a finished session.
    ///
    /// Both lifecycle modes must report the same thing: on a released backend
    /// the console host is gone but the `HPCON` is still open, so the error is
    /// the normalized disconnect from the resize FFI; on a legacy backend the
    /// watcher has closed the pseudoconsole, so it comes from the close-state
    /// check. Either way the caller must see `NotConnected`.
    fn assert_resize_after_session_end_is_not_connected(pty: Pty) {
        let Running {
            mut child,
            reader,
            writer,
            controller,
        } = Running::start_in(pty, Command::new("cmd.exe").args(["/c", "exit", "0"]));
        child.wait().expect("waiting must succeed");
        // End-of-file proves the session is over (and, on a legacy backend,
        // that the watcher has already closed the pseudoconsole).
        reader.join().expect("the reader thread must not panic");

        let err = controller
            .resize(Size::new(30, 100))
            .expect_err("resizing a finished session must fail");
        match err {
            Error::Resize(source) => {
                assert_eq!(
                    source.kind(),
                    io::ErrorKind::NotConnected,
                    "a finished session must report NotConnected on every \
                     backend, got: {source:?}"
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
        drop(writer);
    }

    /// The system backend exports no `ClearPseudoConsole`, so on an ordinary
    /// machine this exercises the typed refusal. On a session backed by a
    /// bundled `conpty.dll` the same test proves the call goes through — the
    /// assertion is that the capability query and the operation agree.
    #[test]
    fn clear_agrees_with_the_reported_capability() {
        complete_within("clear_agrees_with_the_reported_capability", || {
            let pty = pty();
            let supported = pty.supports_clear();
            let (_reader, _writer, controller) = pty.into_split();
            assert_eq!(controller.supports_clear(), supported);

            match controller.clear() {
                Ok(()) => assert!(supported, "clear succeeded without a clear export"),
                Err(Error::UnsupportedFeature { feature }) => {
                    assert!(!supported, "clear refused although the export is present");
                    assert_eq!(feature, CLEAR_FEATURE);
                }
                Err(other) => panic!("unexpected error: {other:?}"),
            }
        });
    }

    #[test]
    fn clear_on_a_pty_matches_clear_on_its_controller() {
        complete_within("clear_on_a_pty_matches_clear_on_its_controller", || {
            let pty = pty();
            let from_pty = pty.clear();
            let supported = pty.supports_clear();
            let (_reader, _writer, controller) = pty.into_split();
            let from_controller = controller.clear();
            assert_eq!(from_pty.is_ok(), supported);
            assert_eq!(from_controller.is_ok(), supported);
        });
    }

    #[test]
    fn resize_after_the_session_ends_reports_not_connected() {
        complete_within(
            "resize_after_the_session_ends_reports_not_connected",
            || {
                assert_resize_after_session_end_is_not_connected(pty());
            },
        );
    }

    #[test]
    fn forced_legacy_resize_after_the_session_ends_reports_not_connected() {
        complete_within(
            "forced_legacy_resize_after_the_session_ends_reports_not_connected",
            || {
                assert_resize_after_session_end_is_not_connected(legacy_pty());
            },
        );
    }

    #[test]
    fn a_forced_legacy_session_reaches_end_of_file() {
        complete_within("a_forced_legacy_session_reaches_end_of_file", || {
            const MARKER: &str = "conpty-oxide-forced-legacy-marker";
            let (output, status) = Running::start_in(
                legacy_pty(),
                Command::new("cmd.exe").args(["/c", "echo", MARKER]),
            )
            .finish();
            // `finish` joining the reader is the real assertion: the session
            // was never released, so only the legacy watcher's close can
            // produce the end-of-file the reader thread waits for. A
            // regression in arming the watcher (handle duplication, grace
            // handling, the release/legacy decision) hangs here and is killed
            // by the watchdog instead of passing silently on a 24H2 machine.
            assert!(
                output.contains(MARKER),
                "marker missing from the rendered output: {output:?}"
            );
            assert!(status.success(), "unexpected status: {status}");
        });
    }

    #[test]
    fn echoed_output_reaches_the_reader_and_the_session_ends() {
        complete_within("echoed_output_reaches_the_reader", || {
            const MARKER: &str = "conpty-oxide-blocking-marker";
            let (output, status) = run_cmd(&["/c", "echo", MARKER]);
            assert!(
                output.contains(MARKER),
                "marker missing from the rendered output: {output:?}"
            );
            assert!(status.success(), "unexpected status: {status}");
            assert_eq!(status.code(), 0);
        });
    }

    #[test]
    fn exit_code_is_reported_verbatim() {
        complete_within("exit_code_is_reported_verbatim", || {
            let (_output, status) = run_cmd(&["/c", "exit", "7"]);
            assert_eq!(status.code(), 7);
            assert!(!status.success());
        });
    }

    #[test]
    fn the_environment_reaches_the_child() {
        complete_within("the_environment_reaches_the_child", || {
            const MARKER: &str = "conpty-oxide-blocking-env-9182";
            let (output, _status) = Running::start(
                Command::new("cmd.exe")
                    .args(["/c", "echo", "%CONPTY_OXIDE_BLOCKING_MARKER%"])
                    .env("CONPTY_OXIDE_BLOCKING_MARKER", MARKER),
            )
            .finish();
            // An unexpanded `%CONPTY_OXIDE_BLOCKING_MARKER%` here would mean
            // the environment block never reached the child.
            assert!(
                output.contains(MARKER),
                "marker missing from the rendered output: {output:?}"
            );
        });
    }

    #[test]
    fn the_working_directory_reaches_the_child() {
        complete_within("the_working_directory_reaches_the_child", || {
            let dir = std::env::temp_dir();
            let (output, status) =
                Running::start(Command::new("cmd.exe").args(["/c", "cd"]).current_dir(&dir))
                    .finish();
            assert!(status.success());
            // `cd` without an argument prints the working directory. Comparing
            // the last component avoids depending on 8.3 short paths; without
            // `current_dir` the child would inherit the test runner's
            // directory, whose name is different.
            let leaf = dir
                .file_name()
                .expect("the temp directory must have a name")
                .to_string_lossy()
                .into_owned();
            assert!(
                output.contains(&leaf),
                "working directory missing from the rendered output: {output:?}"
            );
        });
    }

    #[test]
    fn written_input_reaches_the_child() {
        complete_within("written_input_reaches_the_child", || {
            // An interactive `cmd.exe` only exits when it reads the `exit`
            // command from its console input, so the child terminating with
            // that exact code proves the bytes travelled through conin.
            let mut running = Running::start(&mut Command::new("cmd.exe"));
            running
                .writer
                .write_all(b"exit 3\r\n")
                .expect("writing console input must succeed");
            running
                .writer
                .flush()
                .expect("flush must be a no-op that succeeds");

            let (_output, status) = running.finish();
            assert_eq!(status.code(), 3);
        });
    }

    #[test]
    fn kill_terminates_the_tree_and_reports_a_status() {
        complete_within("kill_terminates_the_tree", || {
            // `pause` blocks on console input that this test never sends.
            let mut running = Running::start(Command::new("cmd.exe").args(["/c", "pause"]));

            assert_ne!(running.child.id(), 0, "a spawned child must have a pid");
            assert!(
                running
                    .child
                    .try_wait()
                    .expect("polling must succeed")
                    .is_none(),
                "a blocked child must not report a status yet"
            );
            assert!(!running.child.as_raw_handle().is_null());

            running.child.kill().expect("kill must succeed");
            let status = running.child.wait().expect("waiting must succeed");
            assert_eq!(status.code(), KILL_EXIT_CODE);
            // A second kill of a dead tree is a documented no-op.
            running
                .child
                .kill()
                .expect("killing a dead tree must succeed");

            let (_output, again) = running.finish();
            assert_eq!(again, status, "the status must be cached, not re-read");
        });
    }

    #[test]
    fn wait_is_repeatable_and_matches_try_wait() {
        complete_within("wait_is_repeatable", || {
            let mut running = Running::start(Command::new("cmd.exe").args(["/c", "exit", "5"]));
            let first = running.child.wait().expect("waiting must succeed");
            assert_eq!(
                running.child.wait().expect("waiting again must succeed"),
                first
            );
            assert_eq!(
                running.child.try_wait().expect("polling must succeed"),
                Some(first)
            );
            assert_eq!(first.code(), 5);
            running.finish();
        });
    }

    #[test]
    fn kill_on_drop_terminates_the_tree() {
        complete_within("kill_on_drop_terminates_the_tree", || {
            let running = Running::start(
                Command::new("cmd.exe")
                    .args(["/c", "pause"])
                    .kill_on_drop(true),
            );

            // An independent handle, so the process can still be observed
            // after the `Child` — and with it the job object — is gone.
            let watched = ProcessWaiter::new(
                running
                    .child
                    .as_handle()
                    .try_clone_to_owned()
                    .expect("duplicating the process handle must succeed"),
            );
            assert!(watched.try_wait().expect("polling must succeed").is_none());

            let Running {
                child,
                reader,
                writer,
                controller,
            } = running;
            drop(child);
            assert_eq!(
                watched.wait().expect("waiting must succeed"),
                KILL_EXIT_CODE,
                "dropping a kill-on-drop child must terminate the tree"
            );
            reader.join().expect("the reader thread must not panic");
            drop(writer);
            drop(controller);
        });
    }

    #[test]
    fn a_second_spawn_into_the_same_pty_is_rejected() {
        complete_within("a_second_spawn_is_rejected", || {
            let pty = pty();
            let child = Command::new("cmd.exe")
                .args(["/c", "exit", "0"])
                .spawn(&pty)
                .expect("the first spawn must succeed");

            let err = Command::new("cmd.exe")
                .args(["/c", "exit", "0"])
                .spawn(&pty)
                .expect_err("a second spawn must be rejected");
            match err {
                Error::Spawn { source, .. } => {
                    assert_eq!(source.kind(), io::ErrorKind::AlreadyExists);
                }
                other => panic!("unexpected error: {other:?}"),
            }

            let (_output, status) = Running::attach(pty, child).finish();
            assert!(status.success());
        });
    }

    #[test]
    fn a_failed_spawn_leaves_the_session_reusable() {
        complete_within("a_failed_spawn_leaves_the_session_reusable", || {
            let pty = pty();
            let err = Command::new("conpty-oxide-no-such-program.exe")
                .spawn(&pty)
                .expect_err("spawning a missing program must fail");
            match err {
                Error::Spawn { program, source } => {
                    assert_eq!(program, "conpty-oxide-no-such-program.exe");
                    assert_eq!(source.kind(), io::ErrorKind::NotFound);
                }
                other => panic!("unexpected error: {other:?}"),
            }

            // The failed attempt attached nothing, so the session is still
            // good for a real child.
            let (_output, status) =
                Running::start_in(pty, Command::new("cmd.exe").args(["/c", "exit", "0"])).finish();
            assert!(status.success());
        });
    }

    #[test]
    fn an_unbuildable_command_line_is_rejected() {
        let pty = pty();
        let err = Command::new("cmd.exe")
            .arg("embedded\0nul")
            .spawn(&pty)
            .expect_err("an unbuildable command line must fail");
        match err {
            Error::Spawn { source, .. } => {
                assert_eq!(source.kind(), io::ErrorKind::InvalidInput);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn reading_an_empty_buffer_is_not_end_of_file() {
        complete_within("reading_an_empty_buffer_is_not_end_of_file", || {
            let mut pty = pty();
            let (mut reader, _writer) = pty.split();
            assert_eq!(
                reader
                    .read(&mut [])
                    .expect("a zero-length read must succeed"),
                0
            );
            // A zero-length read must not have reported end-of-file, so the
            // session is still open and still resizable.
            pty.resize(Size::new(30, 100))
                .expect("the session must still be open");
        });
    }

    /// The input-side contract the docs state in four places: dropping the
    /// owned write half of a live session closes conin, the console host
    /// reads that as the terminal being closed, and the child is terminated
    /// with `STATUS_CONTROL_C_EXIT` — dropping this half is a way to *stop* a
    /// session, not to signal end of input.
    #[test]
    fn dropping_the_write_half_terminates_the_child() {
        complete_within("dropping_the_write_half_terminates_the_child", || {
            const MARKER: &str = "conpty-oxide-conin-drop-marker";

            let pty = pty();
            let mut child = Command::new("cmd.exe")
                .spawn(&pty)
                .expect("spawning must succeed");
            let (mut reader, mut writer, _controller) = pty.into_split();

            // Closing conin only ends a session that has a client to send
            // the close event to, so first prove the child is attached and
            // reading console input: an interactive `cmd.exe` cannot echo
            // this line before it has done both.
            writer
                .write_all(format!("echo {MARKER}\r\n").as_bytes())
                .expect("writing console input must succeed");
            let mut seen = String::new();
            let mut buf = [0u8; 4096];
            while !seen.contains(MARKER) {
                let read = reader.read(&mut buf).expect("reading must succeed");
                assert_ne!(read, 0, "the session ended before the child started");
                seen.push_str(&String::from_utf8_lossy(&buf[..read]));
            }

            drop(writer);

            // The console host reads the closed input pipe as the terminal
            // going away: it terminates its clients with the documented code,
            // and the session reaches end-of-file with no help from the
            // child.
            let mut sink = Vec::new();
            reader
                .read_to_end(&mut sink)
                .expect("reading to end-of-file must succeed");
            let status = child.wait().expect("waiting must succeed");
            assert_eq!(
                status.code(),
                STATUS_CONTROL_C_EXIT,
                "a child whose terminal went away must report \
                 STATUS_CONTROL_C_EXIT, got: {status}"
            );
        });
    }

    #[test]
    fn a_session_without_the_eof_watcher_still_tears_down() {
        complete_within("a_session_without_the_eof_watcher_still_tears_down", || {
            let pty = Pty::builder()
                .eof_on_root_exit(false)
                .build()
                .expect("building must succeed");
            let mut child = Command::new("cmd.exe")
                .args(["/c", "exit", "0"])
                .spawn(&pty)
                .expect("spawning must succeed");
            assert!(child.wait().expect("waiting must succeed").success());

            // Without a watcher a legacy session never reaches end-of-file on
            // its own, so the reader is retired by dropping the session
            // instead. Dropping must not hang, on any backend.
            drop(pty);
        });
    }

    #[test]
    fn the_controller_keeps_an_idle_session_alive() {
        complete_within("the_controller_keeps_an_idle_session_alive", || {
            let (reader, writer, controller) = pty().into_split();
            // Retiring both pipe ends does not end the session: nothing has
            // asked for a close, and the controller still owns the console.
            drop(reader);
            drop(writer);
            controller
                .resize(Size::new(30, 100))
                .expect("a session with a live controller must still resize");
            assert_eq!(controller.size(), Size::new(30, 100));
        });
    }

    #[test]
    fn dropping_the_parts_in_any_order_completes() {
        complete_within("dropping_the_parts_in_any_order_completes", || {
            // Controller first, then the write half, then the reader: the
            // pseudoconsole outlives its controller and is closed by the last
            // part standing.
            let (reader, writer, controller) = pty().into_split();
            drop(controller);
            drop(writer);
            drop(reader);

            // And the reverse order.
            let (reader, writer, controller) = pty().into_split();
            drop(reader);
            drop(writer);
            drop(controller);
        });
    }
}
