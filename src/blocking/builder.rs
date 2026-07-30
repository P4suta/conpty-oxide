// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Low-level blocking pseudoconsole construction.

use std::sync::Arc;

use super::pty::{ConinWriter, ConoutReader, Pty};
use crate::backend::ConPtyBackend;
use crate::core::options::PtyOptions;
use crate::core::pipes::{create_sync_pipes, SyncPipes};
use crate::core::pseudocon::PseudoConsole;
use crate::core::session::Session as SessionCore;
use crate::error::{Error, Result};
use crate::size::Size;

/// Builder for a [`Pty`].
///
/// Created by [`Pty::builder`]. Every option has a working default, so
/// `Pty::builder().build()` is a complete 24x80 session using automatic
/// backend selection.
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
    /// by another thread *and* the reply is echoed back — otherwise the child
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
    /// With `true`, [`Command::spawn_in`](super::Command::spawn_in) registers a
    /// process wait. After the
    /// root exits, a short-lived worker grants about a second for the reader
    /// to drain the console host's remaining output, then closes the
    /// pseudoconsole so the reader sees end-of-file. The side effects are:
    ///
    /// - Output written by *descendants* that outlive the root child (a
    ///   detached background process, for instance) may be cut off, because
    ///   the session ends with the root and not with the last writer.
    /// - The session is torn down even if the caller still holds the
    ///   [`PtyController`](crate::PtyController), so
    ///   [`PtyController::resize`](crate::PtyController::resize) starts failing
    ///   with [`std::io::ErrorKind::NotConnected`] after the child exits.
    ///
    /// With `false`, no watcher is started and the reader of a legacy session
    /// will **not** observe end-of-file when the child exits. It then only
    /// arrives when the read half is dropped or the whole session is, so the
    /// caller must have another way of knowing the session is finished (for
    /// example [`Child::wait`](super::Child::wait) on a separate thread).
    /// Prefer `false` only when the child's descendants matter more than a
    /// prompt end-of-file.
    #[must_use]
    #[cfg(test)]
    pub(crate) const fn eof_on_root_exit(mut self, eof: bool) -> Self {
        self.options.eof_on_root_exit = eof;
        self
    }

    /// Creates the pseudoconsole and its pipes.
    ///
    /// # Errors
    ///
    /// - [`crate::ErrorKind::Backend`] if no backend was given and
    ///   the `ConPTY` API cannot be loaded (Windows older than 10 1809).
    /// - [`crate::ErrorKind::CreateConsole`] if the pipes or
    ///   `CreatePseudoConsole` fail.
    pub(crate) fn build(self) -> Result<Pty> {
        let backend = match self.options.backend {
            Some(backend) => backend,
            None => ConPtyBackend::resolve_default()?,
        };

        let SyncPipes {
            conout_read,
            conout_write,
            conin_read,
            conin_write,
        } = create_sync_pipes().map_err(Error::create_console)?;

        // The two client ends are consumed here: the pseudoconsole closes them
        // as soon as the console host has its own duplicates, which is what
        // makes end-of-file possible at all.
        let console = PseudoConsole::new(
            backend,
            self.options.size,
            conin_read,
            conout_write,
            self.options.inherit_cursor,
        )
        .map_err(Error::create_console)?;

        let shared = Arc::clone(console.shared());
        let inner = Arc::new(SessionCore::new(console, self.options.eof_on_root_exit));
        Ok(Pty {
            reader: ConoutReader::new(conout_read, shared),
            writer: ConinWriter::new(conin_write, Arc::clone(&inner)),
            inner,
        })
    }
}
