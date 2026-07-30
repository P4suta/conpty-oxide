// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::io;
use std::os::windows::io::{IntoRawHandle, OwnedHandle};
use std::sync::Arc;

use ::tokio::net::windows::named_pipe::NamedPipeServer;

use crate::backend::ConPtyBackend;
use crate::core::options::PtyOptions;
use crate::core::pipes::{create_overlapped_pipes, OverlappedPipes};
use crate::core::pseudocon::PseudoConsole;
use crate::core::session::Session as SessionCore;
use crate::error::{Error, Result};
use crate::size::Size;

use super::pty::Pty;
use super::pty::{ConinWriter, ConoutReader};

/// Builder for an asynchronous [`Pty`].
///
/// Created by [`Pty::builder`]. Every option has a working default, so
/// `Pty::builder().build()` is a complete 24x80 session using automatic backend detection.
#[derive(Debug, Clone, Default)]
pub(crate) struct PtyBuilder {
    options: PtyOptions,
}

impl PtyBuilder {
    /// Sets the initial size of the pseudoconsole.
    ///
    /// Defaults to [`Size::default`] (24 rows by 80 columns). The size can be
    /// changed later with [`Pty::resize`].
    #[must_use]
    pub(crate) const fn size(mut self, size: Size) -> Self {
        self.options.size = size;
        self
    }

    /// Uses `backend` instead of automatic backend detection.
    ///
    /// Without this, [`build`](Self::build) uses the cached result of
    /// [`ConPtyBackend::auto`].
    #[must_use]
    pub(crate) fn backend(mut self, backend: ConPtyBackend) -> Self {
        self.options.backend = Some(backend);
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
    #[cfg(test)]
    pub(crate) const fn inherit_cursor(mut self, inherit: bool) -> Self {
        self.options.inherit_cursor = inherit;
        self
    }

    /// Controls whether the crate forces end-of-file when the root child
    /// exits. Defaults to `true`.
    ///
    /// This only has an effect on backends without `ReleasePseudoConsole`
    /// (everything before Windows 11 24H2); a released session reaches
    /// end-of-file naturally and never starts a watcher.
    ///
    /// With `true`, [`crate::tokio::Command::spawn_in`] registers a process
    /// wait. After the root exits, a short-lived worker grants about a second
    /// for the reader to drain the console host's remaining output, then
    /// closes the pseudoconsole so the reader sees end-of-file. The side
    /// effects are:
    ///
    /// - Output written by *descendants* that outlive the root child (a
    ///   detached background process, for instance) may be cut off, because
    ///   the session ends with the root and not with the last writer.
    /// - The session is torn down even if the caller still holds the
    ///   [`crate::PtyController`], so [`crate::PtyController::resize`] starts
    ///   failing with [`io::ErrorKind::NotConnected`] after the child exits.
    ///
    /// With `false`, no watcher is started and the reader of a legacy session
    /// will **not** observe end-of-file when the child exits. It then only
    /// arrives when the read half is dropped or the whole session is, so the
    /// caller must have another way of knowing the session is finished (for
    /// example a task awaiting [`crate::tokio::Child::wait`]). Prefer `false`
    /// only when the child's descendants matter more than a prompt
    /// end-of-file.
    #[must_use]
    #[cfg(test)]
    pub(crate) const fn eof_on_root_exit(mut self, eof: bool) -> Self {
        self.options.eof_on_root_exit = eof;
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
    /// Called with no runtime in scope, or from a runtime whose I/O driver is
    /// disabled, this returns
    /// [`crate::ErrorKind::CreateConsole`] with an [`io::ErrorKind::Other`] source
    /// rather than creating anything.
    ///
    /// # Errors
    ///
    /// - [`crate::ErrorKind::Backend`] if no backend was given and the `ConPTY` API cannot
    ///   be loaded (Windows older than 10 1809).
    /// - [`crate::ErrorKind::CreateConsole`] if there is no I/O-enabled runtime, if the
    ///   pipes cannot be created or registered, or if `CreatePseudoConsole`
    ///   fails.
    pub(crate) fn build(self) -> Result<Pty> {
        let backend = match self.options.backend {
            Some(backend) => backend,
            None => ConPtyBackend::resolve_default()?,
        };

        // Checked before any OS resource exists, so a call from outside a
        // runtime fails cleanly instead of leaking a pseudoconsole and two
        // pipes on its way into Tokio's registration panic.
        if ::tokio::runtime::Handle::try_current().is_err() {
            return Err(Error::create_console(io::Error::other(
                "an async Pty must be built from within a Tokio runtime: its \
                 pipes are registered with the runtime's I/O driver",
            )));
        }

        let OverlappedPipes {
            conout_server,
            conout_client,
            conin_server,
            conin_client,
        } = create_overlapped_pipes().map_err(Error::create_console)?;

        // Registered before the pseudoconsole exists so that a registration
        // failure has no console to tear down; the client ends are still
        // untouched here and are closed by their `OwnedHandle` destructors.
        let conout = register(conout_server).map_err(Error::create_console)?;
        let conin = register(conin_server).map_err(Error::create_console)?;

        // The two client ends are consumed here: the pseudoconsole closes them
        // as soon as the console host has its own duplicates, which is what
        // makes end-of-file possible at all.
        let console = PseudoConsole::new(
            backend,
            self.options.size,
            conin_client,
            conout_client,
            self.options.inherit_cursor,
        )
        .map_err(Error::create_console)?;

        let shared = Arc::clone(console.shared());
        // An async reader cannot promise that dropping it closes the conout
        // read end at the OS level (see `ConoutReader`'s `Drop`); telling the
        // lifecycle core up front keeps it from ever treating "reader closed"
        // as a promptness proof for `ClosePseudoConsole`.
        shared.set_reader_close_deferred();
        Ok(Pty {
            reader: ConoutReader::new(conout, shared),
            writer: ConinWriter::new(conin),
            inner: Arc::new(SessionCore::new(console, self.options.eof_on_root_exit)),
        })
    }
}

/// Hands one overlapped pipe end to the current runtime's I/O driver.
///
/// # Errors
///
/// Returns the registration failure. Tokio currently panics internally when
/// a runtime exists but its I/O driver is disabled, despite this registration
/// API returning [`io::Result`]; that known configuration failure is caught at
/// this ownership boundary and converted to an error.
///
/// The handle is closed on every path: by the returned
/// [`NamedPipeServer`] on success, by the `mio` pipe on an ordinary error or
/// while unwinding, and by [`OwnedHandle`] if ownership has not yet been
/// transferred. No path leaks it or closes it twice.
fn register(handle: OwnedHandle) -> io::Result<NamedPipeServer> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let raw = handle.into_raw_handle();
        // SAFETY: `create_overlapped_pipes` opened this server end with
        // `FILE_FLAG_OVERLAPPED`, which is what `from_raw_handle` requires in
        // order to associate it with the runtime's I/O completion port, and
        // `into_raw_handle` above relinquished this crate's ownership, so the
        // resulting `NamedPipeServer` really is the handle's sole owner. The
        // caller has already established that a runtime context exists.
        unsafe { NamedPipeServer::from_raw_handle(raw) }
    }))
    .unwrap_or_else(|_| {
        Err(io::Error::other(
            "the Tokio runtime's I/O driver is disabled; enable it before \
             building an async Pty",
        ))
    })
}
