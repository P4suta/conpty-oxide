//! Asynchronous pseudoconsole sessions, driven by Tokio.
//!
//! This is the async front end of the crate and a faithful mirror of the
//! `blocking` module: the same builder, the same session shape, the same
//! lifecycle rules. Only the I/O differs — [`Pty`] implements [`AsyncRead`]
//! and [`AsyncWrite`] instead of [`Read`](std::io::Read) and
//! [`Write`](std::io::Write), and [`Child::wait`] is a future.
//!
//! A session is built in three steps:
//!
//! 1. [`Pty::builder`] creates the pseudoconsole and its two pipes.
//! 2. [`Command::spawn`] launches the root child process attached to it.
//! 3. The caller reads rendered output from the [`Pty`] and writes user input
//!    into it, then awaits [`Child::wait`].
//!
//! # Build inside a runtime
//!
//! [`PtyBuilder::build`] registers the session's pipes with the current
//! runtime's I/O driver, so it must be called from within a Tokio runtime —
//! see its documentation for what happens otherwise.
//!
//! # Drive the output pipe concurrently with the child
//!
//! Microsoft's guidance for pseudoconsole sessions strongly recommends
//! servicing each I/O channel on its own thread — here, by something other
//! than whatever is waiting on the child. The deadlock that rule prevents is
//! real: the console host writes rendered output eagerly; once the pipe
//! buffer fills, the host — and with it the child — stops making progress.
//!
//! Async makes this easier than threads do, but it does not make it optional:
//! a task that awaits [`Child::wait`] without also polling the output will
//! hang as soon as the child produces more than a pipe buffer's worth of text.
//! Use [`Pty::into_split`] to hand the read half to its own task, or
//! `tokio::select!`/`tokio::join!` to drive both from one.
//!
//! # Closing the input pipe ends the session
//!
//! Dropping the write half of a session is **not** the console equivalent of
//! closing a child's stdin. The console host treats end-of-file on its input
//! pipe as "the terminal window was closed" and sends a close event to every
//! attached client, which terminates them: a child killed this way reports
//! exit code `0xC000013A` (`STATUS_CONTROL_C_EXIT`) and any output it had not
//! flushed yet is lost. The same is true of `AsyncWriteExt::shutdown`, which
//! closes the pipe deliberately.
//!
//! Keep the write half — or the whole [`Pty`] — alive until the child has
//! exited. Dropping it earlier is a way to *stop* a session, not a way to
//! signal one.
//!
//! # The end-of-file contract
//!
//! Reading a [`Pty`] (or an [`OwnedReadHalf`]) yields `Ok(0)` exactly once the
//! session is over, and every disconnect-flavoured OS error — most importantly
//! `ERROR_BROKEN_PIPE` — is mapped to that same clean end-of-file rather than
//! surfacing as an error. How the crate *reaches* that point depends on the
//! backend, exactly as it does for the blocking front end:
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
//! use conpty_oxide::{Command, Pty, Size};
//! use tokio::io::AsyncReadExt;
//!
//! # #[tokio::main]
//! # async fn main() -> conpty_oxide::Result<()> {
//! let pty = Pty::builder().size(Size::new(24, 80)).build()?;
//! let mut child = Command::new("cmd.exe")
//!     .args(["/c", "echo", "hello"])
//!     .spawn(&pty)?;
//!
//! // The output pipe must be drained while the child runs. The write half is
//! // unused here but deliberately kept alive: dropping it would end the
//! // session early (see above).
//! let (mut reader, writer, _controller) = pty.into_split();
//! let output = tokio::spawn(async move {
//!     let mut buf = Vec::new();
//!     reader.read_to_end(&mut buf).await.map(|_| buf)
//! });
//!
//! let status = child.wait().await?;
//! let output = output.await.expect("the reader task must not panic")?;
//! drop(writer);
//!
//! print!("{}", String::from_utf8_lossy(&output));
//! assert!(status.success());
//! # Ok(())
//! # }
//! ```

use std::ffi::OsStr;
use std::io;
use std::os::windows::io::{
    AsHandle, AsRawHandle, BorrowedHandle, IntoRawHandle, OwnedHandle, RawHandle,
};
use std::path::Path;
use std::pin::Pin;
use std::ptr;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::NamedPipeServer;
use tokio::task::JoinHandle;
use windows_sys::Win32::System::IO::CancelIoEx;

use crate::backend::{BackendKind, ConPtyBackend};
use crate::core::is_disconnect_error;
use crate::core::job::Job;
use crate::core::pipes::{create_overlapped_pipes, OverlappedPipes};
use crate::core::pseudocon::{ConsoleShared, PseudoConsole};
use crate::core::session::{self, Session, KILL_EXIT_CODE};
use crate::core::wait::ProcessWaiter;
use crate::error::{Error, Result};
use crate::size::Size;
use crate::status::ExitStatus;

/// Builder for an asynchronous [`Pty`].
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
    /// by another task *and* the reply is echoed back — otherwise the child
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
    /// example a task awaiting [`Child::wait`]). Prefer `false` only when the
    /// child's descendants matter more than a prompt end-of-file.
    #[must_use]
    pub fn eof_on_root_exit(mut self, eof: bool) -> Self {
        self.eof_on_root_exit = eof;
        self
    }

    /// Creates the pseudoconsole and its pipes.
    ///
    /// # Tokio runtime
    ///
    /// **This must be called from within a Tokio runtime.** Unlike the
    /// blocking front end, a session's pipe ends are named pipes opened for
    /// overlapped I/O and registered with the runtime's I/O driver, which is
    /// only reachable from inside a runtime context (`#[tokio::main]`,
    /// `#[tokio::test]`, `Runtime::block_on`, `Runtime::enter`, or any task
    /// spawned on one).
    ///
    /// Called with no runtime in scope, this returns
    /// [`Error::CreateConsole`] wrapping an [`io::ErrorKind::Other`] error
    /// rather than creating anything.
    ///
    /// # Panics
    ///
    /// The one misuse that cannot be turned into an error is a runtime whose
    /// I/O driver is disabled
    /// ([`Builder::enable_io`](tokio::runtime::Builder::enable_io) not
    /// called): Tokio panics when the pipes are registered there, exactly as
    /// it does for its own socket types.
    ///
    /// # Errors
    ///
    /// - [`Error::Backend`] if no backend was given and the ConPTY API cannot
    ///   be loaded (Windows older than 10 1809).
    /// - [`Error::CreateConsole`] if there is no runtime, if the pipes cannot
    ///   be created or registered, or if `CreatePseudoConsole` fails.
    pub fn build(self) -> Result<Pty> {
        let backend = match self.backend {
            Some(backend) => backend,
            None => ConPtyBackend::resolve_default()?,
        };

        // Checked before any OS resource exists, so a call from outside a
        // runtime fails cleanly instead of leaking a pseudoconsole and two
        // pipes on its way into Tokio's registration panic.
        if tokio::runtime::Handle::try_current().is_err() {
            return Err(Error::CreateConsole(io::Error::other(
                "an async Pty must be built from within a Tokio runtime: its \
                 pipes are registered with the runtime's I/O driver",
            )));
        }

        let OverlappedPipes {
            conout_server,
            conout_client,
            conin_server,
            conin_client,
        } = create_overlapped_pipes().map_err(Error::CreateConsole)?;

        // Registered before the pseudoconsole exists so that a registration
        // failure has no console to tear down; the client ends are still
        // untouched here and are closed by their `OwnedHandle` destructors.
        let conout = register(conout_server).map_err(Error::CreateConsole)?;
        let conin = register(conin_server).map_err(Error::CreateConsole)?;

        // The two client ends are consumed here: the pseudoconsole closes them
        // as soon as the console host has its own duplicates, which is what
        // makes end-of-file possible at all.
        let console = PseudoConsole::new(
            backend.clone(),
            self.size,
            conin_client,
            conout_client,
            self.inherit_cursor,
        )
        .map_err(Error::CreateConsole)?;

        let shared = Arc::clone(console.shared());
        // An async reader cannot promise that dropping it closes the conout
        // read end at the OS level (see `ConoutReader`'s `Drop`); telling the
        // lifecycle core up front keeps it from ever treating "reader closed"
        // as a promptness proof for `ClosePseudoConsole`.
        shared.set_reader_close_deferred();
        Ok(Pty {
            reader: ConoutReader::new(conout, shared),
            writer: ConinWriter::new(conin),
            inner: Arc::new(Session::new(
                console,
                backend,
                self.size,
                self.eof_on_root_exit,
            )),
        })
    }
}

/// Hands one overlapped pipe end to the current runtime's I/O driver.
///
/// # Errors
///
/// Returns the registration failure. The handle is closed either way — on
/// success by the returned [`NamedPipeServer`], on failure by the `mio` pipe
/// that `from_raw_handle` wraps it in before registration is attempted — so no
/// path leaks it and none closes it twice.
fn register(handle: OwnedHandle) -> io::Result<NamedPipeServer> {
    let raw = handle.into_raw_handle();
    // SAFETY: `create_overlapped_pipes` opened this server end with
    // `FILE_FLAG_OVERLAPPED`, which is what `from_raw_handle` requires in
    // order to associate it with the runtime's I/O completion port, and
    // `into_raw_handle` above relinquished this crate's ownership, so the
    // resulting `NamedPipeServer` really is the handle's sole owner. The
    // caller has already established that a runtime context exists.
    unsafe { NamedPipeServer::from_raw_handle(raw) }
}

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
/// documentation. Keep the session alive until [`Child::wait`] returns.
#[derive(Debug)]
pub struct Pty {
    reader: ConoutReader,
    writer: ConinWriter,
    inner: Arc<Session>,
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
    /// This is a plain synchronous method: the underlying
    /// `ResizePseudoConsole` is a short signal write to the console host that
    /// never blocks, so making it a future would buy nothing.
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
    /// scope, for instance to `tokio::io::copy` in both directions under a
    /// `tokio::try_join!`. The borrow covers the whole `Pty`, so
    /// [`Pty::resize`] cannot be called while the halves are alive and the
    /// halves cannot be moved into a task that outlives this scope — use
    /// [`Pty::into_split`] when either is needed.
    pub fn split(&mut self) -> (ReadHalf<'_>, WriteHalf<'_>) {
        let Self { reader, writer, .. } = self;
        (ReadHalf { reader }, WriteHalf { writer })
    }

    /// Splits the session into three independently owned parts.
    ///
    /// This is the shape a real session usually wants: the
    /// [`OwnedReadHalf`] moves into a dedicated reader task (which the
    /// pseudoconsole effectively requires anyway, see the module docs), the
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

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_shutdown(cx)
    }
}

/// Borrowed read half of an asynchronous [`Pty`], from [`Pty::split`].
#[derive(Debug)]
pub struct ReadHalf<'a> {
    reader: &'a mut ConoutReader,
}

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
#[derive(Debug)]
pub struct WriteHalf<'a> {
    writer: &'a mut ConinWriter,
}

impl AsyncWrite for WriteHalf<'_> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.get_mut().writer).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.get_mut().writer).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.get_mut().writer).poll_shutdown(cx)
    }
}

/// Owned read half of an asynchronous [`Pty`], from [`Pty::into_split`].
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
#[derive(Debug)]
pub struct OwnedReadHalf {
    reader: ConoutReader,
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

/// Owned write half of an asynchronous [`Pty`], from [`Pty::into_split`].
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
#[derive(Debug)]
pub struct OwnedWriteHalf {
    writer: ConinWriter,
}

impl AsyncWrite for OwnedWriteHalf {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().writer).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_shutdown(cx)
    }
}

/// Control handle for a split asynchronous session, from [`Pty::into_split`].
///
/// Holds the pseudoconsole itself, so the session stays usable while the read
/// and write halves live in other tasks. Dropping it does not end the session:
/// the read half keeps the lifecycle state alive and the pseudoconsole is
/// closed once every part is gone.
///
/// Every method is synchronous — each one is a short signal to the console
/// host — so a controller is equally usable from async and from blocking code.
#[derive(Debug)]
pub struct PtyController {
    inner: Arc<Session>,
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

/// The server end of the conout named pipe, plus the lifecycle notifications
/// the pseudoconsole state machine needs from a reader.
#[derive(Debug)]
struct ConoutReader {
    /// `None` only between the start and the end of [`Drop`].
    pipe: Option<NamedPipeServer>,
    shared: Arc<ConsoleShared>,
    /// Whether end-of-file has already been reported once; the notification
    /// is idempotent, but repeating it on every subsequent poll would take the
    /// state lock for nothing.
    saw_eof: bool,
}

impl ConoutReader {
    fn new(pipe: NamedPipeServer, shared: Arc<ConsoleShared>) -> Self {
        Self {
            pipe: Some(pipe),
            shared,
            saw_eof: false,
        }
    }

    /// Reports end-of-file to the lifecycle state machine, once.
    ///
    /// This may run `ClosePseudoConsole` inline on the polling task's thread.
    /// That is the one case where closing from the reader is correct:
    /// end-of-file proves the console host is already gone, so the close has
    /// nothing left to wait for and cannot stall the runtime worker.
    fn on_eof(&mut self) {
        if !self.saw_eof {
            self.saw_eof = true;
            self.shared.notify_reader_eof();
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
                    this.on_eof();
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(err)) if is_disconnect_error(&err) => {
                this.on_eof();
                Poll::Ready(Ok(()))
            }
            other => other,
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
struct ConinWriter {
    /// `None` once the pipe has been closed by a shutdown.
    pipe: Option<NamedPipeServer>,
}

impl ConinWriter {
    fn new(pipe: NamedPipeServer) -> Self {
        Self { pipe: Some(pipe) }
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

/// A command to run inside an asynchronous pseudoconsole session.
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
    /// Like `tokio::process::Command::spawn`, this is a synchronous method
    /// even though the resulting [`Child`] is awaited: `CreateProcessW` does
    /// not block, so there is nothing to yield for. It does not have to run
    /// inside a runtime either — only [`Child::wait`] does.
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
            waiter: Arc::new(root.waiter),
            exit: None,
            job: root.job,
            pid: root.pid,
            kill_on_drop: root.kill_on_drop,
            status: None,
        })
    }
}

/// A running (or finished) root child of an asynchronous pseudoconsole
/// session.
///
/// The handle owns the session's job object as well as the process handle, so
/// [`Child::kill`] terminates the whole process tree rather than just the
/// process this crate created.
///
/// Dropping a `Child` does not wait for the process. Unless
/// [`Command::kill_on_drop`] was set, the tree keeps running; what ends the
/// *session* is the pseudoconsole's own teardown (see the module docs).
#[derive(Debug)]
pub struct Child {
    /// Shared with the blocking task in [`Child::exit`], which is why the
    /// process handle outlives a `Child` that is dropped mid-wait.
    waiter: Arc<ProcessWaiter>,
    /// The in-flight exit wait, created on the first [`Child::wait`] and kept
    /// across cancellations so at most one blocking task per child ever
    /// exists.
    exit: Option<JoinHandle<io::Result<u32>>>,
    job: Job,
    pid: u32,
    kill_on_drop: bool,
    /// Cached once the process is known to have exited, so repeated
    /// `wait`/`try_wait` calls stay cheap and consistent.
    status: Option<ExitStatus>,
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
    /// Repeated calls return the cached status instead of waiting again. Must
    /// be awaited inside a Tokio runtime.
    ///
    /// # Deadlock
    ///
    /// The child must be able to make progress while this is pending, which
    /// means something else has to drain the session's output — see the module
    /// docs.
    ///
    /// # Cancel safety
    ///
    /// This method is cancel-safe. Dropping the returned future loses no
    /// progress and no exit status: the wait runs on a blocking task that is
    /// created once and stored in the `Child`, so a later `wait` resumes the
    /// same wait rather than starting a second one, and a process handle stays
    /// signaled forever anyway.
    ///
    /// What a cancelled wait does *not* do is stop that blocking task — a
    /// thread parked in `WaitForSingleObject` cannot be interrupted, so it
    /// stays parked until the child exits. Since dropping a Tokio runtime
    /// waits for its blocking tasks to finish, a runtime that shuts down while
    /// a watched child is still running will block until the child exits. Call
    /// [`Child::kill`] first if that is not what you want.
    ///
    /// # Panics
    ///
    /// Panics when first awaited outside a Tokio runtime: the wait is pushed
    /// onto the runtime's blocking pool, and `tokio::task::spawn_blocking`
    /// panics with no runtime in scope.
    ///
    /// # Errors
    ///
    /// [`Error::Wait`] wrapping the OS error from `WaitForSingleObject` or
    /// `GetExitCodeProcess`, or the failure of the blocking task itself.
    pub async fn wait(&mut self) -> Result<ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }

        let waiter = Arc::clone(&self.waiter);
        let exit = self
            .exit
            .get_or_insert_with(move || spawn_exit_wait(waiter));
        let joined = exit.await;
        // A `JoinHandle` must not be polled after it has completed, and this
        // one has: whatever the outcome, it is spent and a retry needs a fresh
        // task.
        self.exit = None;

        let code = match joined {
            Ok(result) => result.map_err(Error::Wait)?,
            Err(err) => return Err(Error::Wait(io::Error::other(err))),
        };
        let status = ExitStatus::from_raw(code);
        self.status = Some(status);
        Ok(status)
    }

    /// Returns the exit status if the child has already exited, without
    /// waiting.
    ///
    /// A plain synchronous method: the underlying poll is a zero-timeout wait
    /// on the process handle, which never blocks.
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
    /// spawned are killed too. Termination is asynchronous as far as Windows
    /// is concerned: await [`Child::wait`] afterwards to observe the resulting
    /// status, which is exit code `1`.
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

/// Starts the blocking task that waits for one child to exit.
///
/// Exit detection is handle-based (`WaitForSingleObject`, then
/// `GetExitCodeProcess` — never the other way around, see
/// [`ProcessWaiter`]), and Windows offers no way to await a process handle
/// through an I/O completion port, so the wait is pushed onto Tokio's blocking
/// pool. The `Arc` is what makes that safe against a `Child` dropped mid-wait:
/// the task keeps the process handle alive for as long as it is parked on it.
///
/// This one function is the whole coupling between [`Child::wait`] and the
/// waiting mechanism. Swapping it for a thread-free `RegisterWaitForSingleObject`
/// callback that completes a channel would change nothing else, as long as the
/// replacement still resolves to the exit code once the process has exited.
fn spawn_exit_wait(waiter: Arc<ProcessWaiter>) -> JoinHandle<io::Result<u32>> {
    tokio::task::spawn_blocking(move || waiter.wait())
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
///
/// An unfinished exit wait is detached rather than aborted: a blocking task
/// that has already started cannot be cancelled, and it holds its own
/// reference to the process handle, so letting it run out is both safe and the
/// only option.
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

    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use crate::core::session::CLEAR_FEATURE;

    /// Generous per-test budget: spawning `cmd.exe` under a fresh
    /// pseudoconsole plus a legacy teardown grace period is comfortably under
    /// this, and a hang is the failure mode being guarded against.
    const TEST_TIMEOUT: Duration = Duration::from_secs(30);

    /// Awaits `f`, failing the test if it has not finished within
    /// [`TEST_TIMEOUT`].
    ///
    /// Every interesting failure in this module is a stall — an undrained
    /// output pipe, a `ClosePseudoConsole` that never returns, a wait for a
    /// child that can no longer run. Without this the whole test binary would
    /// hang instead of one test failing.
    async fn complete_within<F: std::future::Future>(name: &str, f: F) -> F::Output {
        match tokio::time::timeout(TEST_TIMEOUT, f).await {
            Ok(output) => output,
            Err(_) => panic!("`{name}` hung for more than {TEST_TIMEOUT:?}"),
        }
    }

    /// Kills the test process if the guarded test has not finished within
    /// [`TEST_TIMEOUT`]; disarmed by its own [`Drop`].
    ///
    /// [`complete_within`] cannot report a destructor that blocks: on the
    /// current-thread runtime `#[tokio::test]` uses, a wedged `Drop` occupies
    /// the only thread and takes the runtime's timer down with it. The
    /// teardown-heavy tests below — the ones whose interesting statements are
    /// bare `drop`s outside any timeout scope — arm this process-killing
    /// guard instead (the integration harness's watchdog pattern), so a
    /// future teardown regression fails the `--lib` run rather than hanging
    /// it forever.
    struct ProcessWatchdog {
        finished: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Drop for ProcessWatchdog {
        fn drop(&mut self) {
            self.finished
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn process_watchdog(name: &'static str) -> ProcessWatchdog {
        use std::sync::atomic::{AtomicBool, Ordering};

        let finished = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&finished);
        std::thread::Builder::new()
            .name(format!("watchdog-{name}"))
            .spawn(move || {
                let deadline = std::time::Instant::now() + TEST_TIMEOUT;
                while !flag.load(Ordering::SeqCst) {
                    if std::time::Instant::now() >= deadline {
                        // 101 is what the harness reports for a failing run,
                        // so a killed test reads as a failure, not a crash.
                        eprintln!(
                            "conpty-oxide: `{name}` did not finish within \
                             {TEST_TIMEOUT:?}; assuming a wedged destructor \
                             and killing the test process"
                        );
                        std::process::exit(101);
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            })
            .expect("spawning the watchdog thread must succeed");
        ProcessWatchdog { finished }
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

    /// A session under test: a running child, a task draining its output, and
    /// the two halves that must stay alive while it runs.
    ///
    /// Keeping the write half open for the child's whole life is not
    /// housekeeping — closing the input pipe makes the console host terminate
    /// its clients, which would both corrupt the exit status and hide a broken
    /// end-of-file contract behind a trivially broken pipe.
    struct Running {
        child: Child,
        reader: JoinHandle<Vec<u8>>,
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
            let reader = tokio::spawn(async move {
                let mut sink = Vec::new();
                read_half
                    .read_to_end(&mut sink)
                    .await
                    .expect("reading to end-of-file must succeed");
                sink
            });
            Self {
                child,
                reader,
                writer,
                controller,
            }
        }

        /// Waits for the child, then for end-of-file, and returns the rendered
        /// output together with the exit status.
        async fn finish(self) -> (String, ExitStatus) {
            let Self {
                mut child,
                reader,
                writer,
                controller,
            } = self;
            let status = child.wait().await.expect("waiting must succeed");
            // Joining is the real assertion: it returns only once the session
            // reached end-of-file, and since the write half is still open,
            // that end-of-file can only have come from the crate's own
            // shutdown path (a natural release, or the legacy watcher).
            let output = reader.await.expect("the reader task must not panic");
            drop(writer);
            drop(controller);
            (String::from_utf8_lossy(&output).into_owned(), status)
        }
    }

    /// Runs `cmd.exe` with `args` to completion in a fresh session.
    async fn run_cmd(args: &[&str]) -> (String, ExitStatus) {
        Running::start(Command::new("cmd.exe").args(args))
            .finish()
            .await
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

    /// The one misuse the front end is required to turn into an error rather
    /// than a panic, because it is the easy mistake to make: building a
    /// session from ordinary synchronous code.
    #[test]
    fn building_outside_a_runtime_is_an_error() {
        let err = Pty::builder()
            .build()
            .expect_err("building without a runtime must fail");
        match err {
            Error::CreateConsole(source) => {
                assert!(
                    source.to_string().contains("Tokio runtime"),
                    "the error must name the cause, got: {source}"
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn builder_defaults_to_24_by_80_on_the_system_backend() {
        let pty = pty();
        assert_eq!(pty.size(), Size::new(24, 80));
        assert_eq!(pty.backend_kind(), &BackendKind::System);
    }

    #[tokio::test]
    async fn builder_honours_an_explicit_size_and_backend() {
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
    #[tokio::test]
    async fn supports_release_matches_the_backend() {
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

    #[tokio::test]
    async fn resize_updates_the_reported_size() {
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
    async fn assert_resize_after_session_end_is_not_connected(pty: Pty) {
        let Running {
            mut child,
            reader,
            writer,
            controller,
        } = Running::start_in(pty, Command::new("cmd.exe").args(["/c", "exit", "0"]));
        child.wait().await.expect("waiting must succeed");
        // End-of-file proves the session is over (and, on a legacy backend,
        // that the watcher has already closed the pseudoconsole).
        reader.await.expect("the reader task must not panic");

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

    #[tokio::test]
    async fn resize_after_the_session_ends_reports_not_connected() {
        complete_within(
            "resize_after_the_session_ends_reports_not_connected",
            assert_resize_after_session_end_is_not_connected(pty()),
        )
        .await;
    }

    #[tokio::test]
    async fn forced_legacy_resize_after_the_session_ends_reports_not_connected() {
        complete_within(
            "forced_legacy_resize_after_the_session_ends_reports_not_connected",
            assert_resize_after_session_end_is_not_connected(legacy_pty()),
        )
        .await;
    }

    /// The system backend exports no `ClearPseudoConsole`, so on an ordinary
    /// machine this exercises the typed refusal. On a session backed by a
    /// bundled `conpty.dll` the same test proves the call goes through — the
    /// assertion is that the capability query and the operation agree.
    #[tokio::test]
    async fn clear_agrees_with_the_reported_capability() {
        let pty = pty();
        let supported = pty.supports_clear();
        let from_pty = pty.clear();
        let (_reader, _writer, controller) = pty.into_split();
        assert_eq!(controller.supports_clear(), supported);

        assert_eq!(from_pty.is_ok(), supported);
        match controller.clear() {
            Ok(()) => assert!(supported, "clear succeeded without a clear export"),
            Err(Error::UnsupportedFeature { feature }) => {
                assert!(!supported, "clear refused although the export is present");
                assert_eq!(feature, CLEAR_FEATURE);
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn echoed_output_reaches_the_reader_and_the_session_ends() {
        const MARKER: &str = "conpty-oxide-async-marker";
        let (output, status) = complete_within(
            "echoed_output_reaches_the_reader",
            run_cmd(&["/c", "echo", MARKER]),
        )
        .await;
        assert!(
            output.contains(MARKER),
            "marker missing from the rendered output: {output:?}"
        );
        assert!(status.success(), "unexpected status: {status}");
        assert_eq!(status.code(), 0);
    }

    #[tokio::test]
    async fn a_forced_legacy_session_reaches_end_of_file() {
        const MARKER: &str = "conpty-oxide-async-forced-legacy-marker";
        // `finish` awaiting the reader is the real assertion: the session was
        // never released, so only the legacy watcher's close can produce the
        // end-of-file the reader task waits for.
        let (output, status) = complete_within(
            "a_forced_legacy_session_reaches_end_of_file",
            Running::start_in(
                legacy_pty(),
                Command::new("cmd.exe").args(["/c", "echo", MARKER]),
            )
            .finish(),
        )
        .await;
        assert!(
            output.contains(MARKER),
            "marker missing from the rendered output: {output:?}"
        );
        assert!(status.success(), "unexpected status: {status}");
    }

    #[tokio::test]
    async fn exit_code_is_reported_verbatim() {
        let (_output, status) = complete_within(
            "exit_code_is_reported_verbatim",
            run_cmd(&["/c", "exit", "7"]),
        )
        .await;
        assert_eq!(status.code(), 7);
        assert!(!status.success());
    }

    #[tokio::test]
    async fn the_environment_reaches_the_child() {
        const MARKER: &str = "conpty-oxide-async-env-9182";
        let (output, _status) = complete_within(
            "the_environment_reaches_the_child",
            Running::start(
                Command::new("cmd.exe")
                    .args(["/c", "echo", "%CONPTY_OXIDE_ASYNC_MARKER%"])
                    .env("CONPTY_OXIDE_ASYNC_MARKER", MARKER),
            )
            .finish(),
        )
        .await;
        // An unexpanded `%CONPTY_OXIDE_ASYNC_MARKER%` here would mean the
        // environment block never reached the child.
        assert!(
            output.contains(MARKER),
            "marker missing from the rendered output: {output:?}"
        );
    }

    #[tokio::test]
    async fn written_input_reaches_the_child() {
        // An interactive `cmd.exe` only exits when it reads the `exit` command
        // from its console input, so the child terminating with that exact
        // code proves the bytes travelled through conin.
        let mut running = Running::start(&mut Command::new("cmd.exe"));
        running
            .writer
            .write_all(b"exit 3\r\n")
            .await
            .expect("writing console input must succeed");
        running
            .writer
            .flush()
            .await
            .expect("flush must be a no-op that succeeds");

        let (_output, status) =
            complete_within("written_input_reaches_the_child", running.finish()).await;
        assert_eq!(status.code(), 3);
    }

    #[tokio::test]
    async fn kill_terminates_the_tree_and_reports_a_status() {
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
        let status = complete_within("kill_terminates_the_tree", running.child.wait())
            .await
            .expect("waiting must succeed");
        assert_eq!(status.code(), KILL_EXIT_CODE);
        // A second kill of a dead tree is a documented no-op.
        running
            .child
            .kill()
            .expect("killing a dead tree must succeed");

        let (_output, again) = complete_within("kill_teardown", running.finish()).await;
        assert_eq!(again, status, "the status must be cached, not re-read");
    }

    #[tokio::test]
    async fn wait_is_repeatable_and_matches_try_wait() {
        let mut running = Running::start(Command::new("cmd.exe").args(["/c", "exit", "5"]));
        let first = complete_within("wait_is_repeatable", running.child.wait())
            .await
            .expect("waiting must succeed");
        assert_eq!(
            running
                .child
                .wait()
                .await
                .expect("waiting again must succeed"),
            first
        );
        assert_eq!(
            running.child.try_wait().expect("polling must succeed"),
            Some(first)
        );
        assert_eq!(first.code(), 5);
        complete_within("wait_is_repeatable_teardown", running.finish()).await;
    }

    /// A dropped `wait` future must lose nothing: the next `wait` has to
    /// return the real exit status, not an error and not a hang.
    #[tokio::test]
    async fn a_cancelled_wait_can_be_retried() {
        let mut running = Running::start(Command::new("cmd.exe").args(["/c", "pause"]));

        // The child is blocked on input, so this wait cannot possibly finish
        // and is guaranteed to be cancelled mid-flight.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), running.child.wait())
                .await
                .is_err(),
            "a blocked child must not report a status yet"
        );

        running.child.kill().expect("kill must succeed");
        let status = complete_within("a_cancelled_wait_can_be_retried", running.child.wait())
            .await
            .expect("a retried wait must succeed");
        assert_eq!(status.code(), KILL_EXIT_CODE);
        complete_within("a_cancelled_wait_teardown", running.finish()).await;
    }

    #[tokio::test]
    async fn kill_on_drop_terminates_the_tree() {
        let running = Running::start(
            Command::new("cmd.exe")
                .args(["/c", "pause"])
                .kill_on_drop(true),
        );

        // An independent handle, so the process can still be observed after
        // the `Child` — and with it the job object — is gone.
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
        complete_within("kill_on_drop_terminates_the_tree", reader)
            .await
            .expect("the reader task must not panic");
        drop(writer);
        drop(controller);
    }

    #[tokio::test]
    async fn a_second_spawn_into_the_same_pty_is_rejected() {
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

        let (_output, status) = complete_within(
            "a_second_spawn_is_rejected",
            Running::attach(pty, child).finish(),
        )
        .await;
        assert!(status.success());
    }

    #[tokio::test]
    async fn a_failed_spawn_leaves_the_session_reusable() {
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

        // The failed attempt attached nothing, so the session is still good
        // for a real child.
        let (_output, status) = complete_within(
            "a_failed_spawn_leaves_the_session_reusable",
            Running::start_in(pty, Command::new("cmd.exe").args(["/c", "exit", "0"])).finish(),
        )
        .await;
        assert!(status.success());
    }

    #[tokio::test]
    async fn reading_an_empty_buffer_is_not_end_of_file() {
        let mut pty = pty();
        let (mut reader, _writer) = pty.split();
        assert_eq!(
            reader
                .read(&mut [])
                .await
                .expect("a zero-length read must succeed"),
            0
        );
        // A zero-length read must not have reported end-of-file, so the
        // session is still open and still resizable.
        pty.resize(Size::new(30, 100))
            .expect("the session must still be open");
    }

    /// Shutting the write half down is the documented way to end a session
    /// from the input side: it must close the input pipe without blocking,
    /// refuse further writes, and bring the whole session down with it.
    #[tokio::test]
    async fn shutting_down_the_write_half_ends_the_session() {
        complete_within("shutting_down_the_write_half_ends_the_session", async {
            const MARKER: &str = "conpty-oxide-async-ready-marker";

            let pty = pty();
            let mut child = Command::new("cmd.exe")
                .spawn(&pty)
                .expect("spawning must succeed");
            let (mut reader, mut writer, controller) = pty.into_split();

            // Closing conin only ends a session that has a client to send the
            // close event to, so the test first proves the child is attached
            // and reading console input: `cmd.exe` cannot echo this line back
            // before it has done both.
            writer
                .write_all(format!("echo {MARKER}\r\n").as_bytes())
                .await
                .expect("writing console input must succeed");
            let mut seen = String::new();
            let mut buf = [0u8; 4096];
            while !seen.contains(MARKER) {
                let read = reader.read(&mut buf).await.expect("reading must succeed");
                assert_ne!(read, 0, "the session ended before the child started");
                seen.push_str(&String::from_utf8_lossy(&buf[..read]));
            }

            writer.shutdown().await.expect("shutdown must succeed");
            let err = writer
                .write_all(b"exit\r\n")
                .await
                .expect_err("writing after a shutdown must fail");
            assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
            // Shutting down twice is a no-op, not a second close.
            writer
                .shutdown()
                .await
                .expect("a repeated shutdown must succeed");

            // The console host reads the closed input pipe as the terminal
            // going away and terminates its clients, so the session reaches
            // end-of-file with no help from the child.
            let mut sink = Vec::new();
            reader
                .read_to_end(&mut sink)
                .await
                .expect("reading to end-of-file must succeed");
            let status = child.wait().await.expect("waiting must succeed");
            assert!(
                !status.success(),
                "a session ended from the input side must not report success, \
                 got: {status}"
            );
            drop(controller);
        })
        .await;
    }

    #[tokio::test]
    async fn a_session_without_the_eof_watcher_still_tears_down() {
        let _watchdog = process_watchdog("a_session_without_the_eof_watcher_still_tears_down");
        let pty = Pty::builder()
            .eof_on_root_exit(false)
            .build()
            .expect("building must succeed");
        let mut child = Command::new("cmd.exe")
            .args(["/c", "exit", "0"])
            .spawn(&pty)
            .expect("spawning must succeed");
        assert!(complete_within("without_the_eof_watcher", child.wait())
            .await
            .expect("waiting must succeed")
            .success());

        // Without a watcher a legacy session never reaches end-of-file on its
        // own, so the reader is retired by dropping the session instead.
        // Dropping must not hang, on any backend.
        drop(pty);
    }

    #[tokio::test]
    async fn the_controller_keeps_an_idle_session_alive() {
        let _watchdog = process_watchdog("the_controller_keeps_an_idle_session_alive");
        let (reader, writer, controller) = pty().into_split();
        // Retiring both pipe ends does not end the session: nothing has asked
        // for a close, and the controller still owns the console.
        drop(reader);
        drop(writer);
        controller
            .resize(Size::new(30, 100))
            .expect("a session with a live controller must still resize");
        assert_eq!(controller.size(), Size::new(30, 100));
    }

    #[tokio::test]
    async fn dropping_the_parts_in_any_order_completes() {
        let _watchdog = process_watchdog("dropping_the_parts_in_any_order_completes");
        // Controller first, then the write half, then the reader: the
        // pseudoconsole outlives its controller and is closed by the last part
        // standing.
        let (reader, writer, controller) = pty().into_split();
        drop(controller);
        drop(writer);
        drop(reader);

        // And the reverse order.
        let (reader, writer, controller) = pty().into_split();
        drop(reader);
        drop(writer);
        drop(controller);
    }
}
