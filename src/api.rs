// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Public types shared by the blocking and Tokio front ends.

use std::fmt;
#[cfg(any(feature = "blocking", feature = "tokio"))]
use std::sync::Arc;

use crate::backend::ConPtyBackend;
#[cfg(any(feature = "blocking", feature = "tokio"))]
use crate::core::session::Session as SessionCore;
use crate::error::Result;
use crate::size::Size;
use crate::status::ExitStatus;

/// Safe configuration for a managed pseudoconsole session.
///
/// Managed sessions deliberately expose only the initial terminal size and
/// backend choice. Cursor inheritance, manual EOF policy, and detached
/// spawning are outside the 0.1 API.
#[derive(Debug, Clone, Default)]
pub struct SessionOptions {
    size: Size,
    backend: Option<ConPtyBackend>,
}

impl SessionOptions {
    /// Creates options with an 80x24 terminal and automatic backend selection.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the initial terminal size.
    #[must_use]
    pub const fn size(mut self, size: Size) -> Self {
        self.size = size;
        self
    }

    /// Selects a `ConPTY` backend for this session.
    #[must_use]
    pub fn backend(mut self, backend: ConPtyBackend) -> Self {
        self.backend = Some(backend);
        self
    }

    #[cfg(any(feature = "blocking", feature = "tokio"))]
    #[must_use]
    pub(super) fn into_parts(self) -> (Size, Option<ConPtyBackend>) {
        (self.size, self.backend)
    }
}

/// Virtual-terminal output collected from a managed session.
///
/// `ConPTY` exposes one rendered VT byte stream rather than distinct stdout and
/// stderr channels, so this type intentionally does not pretend otherwise.
///
/// The byte buffer may be large, so collecting output does not also make it
/// implicitly cloneable; a hidden compile-fail doctest pins the missing
/// `Clone`.
#[must_use]
pub struct SessionOutput {
    status: ExitStatus,
    bytes: Vec<u8>,
}

impl SessionOutput {
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    pub(super) const fn new(status: ExitStatus, bytes: Vec<u8>) -> Self {
        Self { status, bytes }
    }

    /// Returns the root process's exit status.
    pub const fn status(&self) -> ExitStatus {
        self.status
    }

    /// Borrows the rendered UTF-8/VT byte stream.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consumes the result and returns the rendered UTF-8/VT byte stream.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl fmt::Debug for SessionOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionOutput")
            .field("status", &self.status)
            .field("bytes", &format_args!("{} bytes", self.bytes.len()))
            .finish()
    }
}

/// Cloneable control handle shared by both public front ends.
///
/// It contains no pipe or runtime-specific state. Clones may be used from any
/// thread, and the pseudoconsole remains alive while the controller or either
/// owned I/O half still exists.
#[derive(Clone)]
pub struct PtyController {
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    pub(super) inner: Arc<SessionCore>,
    #[cfg(not(any(feature = "blocking", feature = "tokio")))]
    uninhabited: std::convert::Infallible,
}

impl PtyController {
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    pub(super) const fn new(inner: Arc<SessionCore>) -> Self {
        Self { inner }
    }
}

#[cfg(any(feature = "blocking", feature = "tokio"))]
impl PtyController {
    /// Resizes the pseudoconsole.
    ///
    /// # Errors
    ///
    /// Returns an error with [`crate::ErrorKind::Resize`] when the session is
    /// closed or the backend rejects the requested size.
    pub fn resize(&self, size: Size) -> Result<()> {
        self.inner.resize(size)
    }

    /// Returns the last successfully applied terminal size.
    #[must_use]
    pub fn size(&self) -> Size {
        self.inner.size()
    }

    /// Clears the pseudoconsole screen and scrollback.
    ///
    /// # Errors
    ///
    /// Returns an error with [`crate::ErrorKind::UnsupportedFeature`] when
    /// clearing is unavailable, or [`crate::ErrorKind::Clear`] when the
    /// operation fails.
    pub fn clear(&self) -> Result<()> {
        self.inner.clear()
    }

    /// Returns whether this backend provides `ClearPseudoConsole`.
    #[must_use]
    pub fn supports_clear(&self) -> bool {
        self.inner.supports_clear()
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) fn supports_release(&self) -> bool {
        self.inner.supports_release()
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) fn reader_finished(&self) -> bool {
        self.inner.reader_finished()
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) fn backend_kind(&self) -> &crate::backend::BackendKind {
        self.inner.backend_kind()
    }
}

#[cfg(any(feature = "blocking", feature = "tokio"))]
impl fmt::Debug for PtyController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PtyController")
            .field("size", &self.inner.size())
            .field("supports_clear", &self.inner.supports_clear())
            .finish_non_exhaustive()
    }
}

/// [`SessionOutput`] never becomes implicitly cloneable:
///
/// ```compile_fail
/// use conpty_oxide::SessionOutput;
///
/// fn require_clone<T: Clone>() {}
/// require_clone::<SessionOutput>();
/// ```
#[cfg(doctest)]
mod api_boundary {}
