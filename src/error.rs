// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Stable error classifications with opaque diagnostic context.

#[cfg(any(feature = "blocking", feature = "tokio", test))]
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::PathBuf;

/// Convenience alias for results produced by this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// The operation phase in which an [`Error`] occurred.
///
/// The classification is the stable part of the error contract. Diagnostic
/// context remains available through [`Display`](fmt::Display),
/// [`Debug`](fmt::Debug), and the [`source`](std::error::Error::source) chain
/// without making that context part of the crate's `SemVer` surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// Loading or validating the `ConPTY` backend failed.
    Backend,
    /// Creating the pseudoconsole or its pipes failed.
    CreateConsole,
    /// Spawning the root process failed.
    Spawn,
    /// Resizing the pseudoconsole failed.
    Resize,
    /// Clearing the pseudoconsole failed.
    Clear,
    /// The selected backend does not provide a requested capability.
    UnsupportedFeature,
    /// A requested terminal size was invalid.
    InvalidSize,
    /// Waiting for the root process failed.
    Wait,
    /// Terminating the process tree failed.
    Kill,
    /// Reading from or writing to a pseudoconsole pipe failed.
    Io,
}

/// A failure produced by `conpty-oxide`.
///
/// The representation is intentionally private. Use [`Error::kind`] for
/// control flow, [`Error::io_error`] for a directly held OS error, and
/// [`Error::backend_error`] for backend initialization details.
pub struct Error {
    repr: ErrorRepr,
}

#[derive(Debug, thiserror::Error)]
enum ErrorRepr {
    #[error("failed to initialize the ConPTY backend")]
    Backend(#[source] BackendError),
    #[cfg(any(feature = "blocking", feature = "tokio", test))]
    #[error("failed to create pseudoconsole")]
    CreateConsole(#[source] io::Error),
    #[cfg(any(feature = "blocking", feature = "tokio", test))]
    #[error("failed to spawn `{}`", .program.to_string_lossy())]
    Spawn {
        program: OsString,
        source: io::Error,
    },
    #[cfg(any(feature = "blocking", feature = "tokio", test))]
    #[error("failed to resize pseudoconsole")]
    Resize(#[source] io::Error),
    #[cfg(any(feature = "blocking", feature = "tokio", test))]
    #[error("failed to clear pseudoconsole")]
    Clear(#[source] io::Error),
    #[cfg(any(feature = "blocking", feature = "tokio", test))]
    #[error("the ConPTY backend does not support {feature}")]
    UnsupportedFeature { feature: &'static str },
    #[error(
        "invalid pseudoconsole size: {rows} rows x {cols} cols \
         (each dimension must be 1..={max})",
        max = crate::Size::MAX_DIMENSION
    )]
    InvalidSize { rows: u16, cols: u16 },
    #[cfg(any(feature = "blocking", feature = "tokio", test))]
    #[error("failed to wait for child process")]
    Wait(#[source] io::Error),
    #[cfg(any(feature = "blocking", feature = "tokio", test))]
    #[error("failed to kill child process")]
    Kill(#[source] io::Error),
    #[error("{0}")]
    Io(
        #[from]
        #[source]
        io::Error,
    ),
}

impl Error {
    /// Returns the stable classification of this failure.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        match self.repr {
            ErrorRepr::Backend(_) => ErrorKind::Backend,
            #[cfg(any(feature = "blocking", feature = "tokio", test))]
            ErrorRepr::CreateConsole(_) => ErrorKind::CreateConsole,
            #[cfg(any(feature = "blocking", feature = "tokio", test))]
            ErrorRepr::Spawn { .. } => ErrorKind::Spawn,
            #[cfg(any(feature = "blocking", feature = "tokio", test))]
            ErrorRepr::Resize(_) => ErrorKind::Resize,
            #[cfg(any(feature = "blocking", feature = "tokio", test))]
            ErrorRepr::Clear(_) => ErrorKind::Clear,
            #[cfg(any(feature = "blocking", feature = "tokio", test))]
            ErrorRepr::UnsupportedFeature { .. } => ErrorKind::UnsupportedFeature,
            ErrorRepr::InvalidSize { .. } => ErrorKind::InvalidSize,
            #[cfg(any(feature = "blocking", feature = "tokio", test))]
            ErrorRepr::Wait(_) => ErrorKind::Wait,
            #[cfg(any(feature = "blocking", feature = "tokio", test))]
            ErrorRepr::Kill(_) => ErrorKind::Kill,
            ErrorRepr::Io(_) => ErrorKind::Io,
        }
    }

    /// Returns the directly held I/O error, when this failure has one.
    ///
    /// This does not walk the source chain. In particular, a backend failure
    /// returns `None`; call [`Error::backend_error`] and then
    /// [`BackendError::io_error`] for that case.
    #[must_use]
    pub const fn io_error(&self) -> Option<&io::Error> {
        match &self.repr {
            #[cfg(any(feature = "blocking", feature = "tokio", test))]
            ErrorRepr::CreateConsole(source)
            | ErrorRepr::Resize(source)
            | ErrorRepr::Clear(source)
            | ErrorRepr::Wait(source)
            | ErrorRepr::Kill(source)
            | ErrorRepr::Spawn { source, .. } => Some(source),
            ErrorRepr::Io(source) => Some(source),
            #[cfg(any(feature = "blocking", feature = "tokio", test))]
            ErrorRepr::UnsupportedFeature { .. } => None,
            ErrorRepr::Backend(_) | ErrorRepr::InvalidSize { .. } => None,
        }
    }

    /// Returns the backend failure when initialization or validation failed.
    #[must_use]
    pub const fn backend_error(&self) -> Option<&BackendError> {
        match &self.repr {
            ErrorRepr::Backend(source) => Some(source),
            #[cfg(any(feature = "blocking", feature = "tokio", test))]
            ErrorRepr::CreateConsole(_)
            | ErrorRepr::Spawn { .. }
            | ErrorRepr::Resize(_)
            | ErrorRepr::Clear(_)
            | ErrorRepr::UnsupportedFeature { .. }
            | ErrorRepr::Wait(_)
            | ErrorRepr::Kill(_) => None,
            ErrorRepr::InvalidSize { .. } | ErrorRepr::Io(_) => None,
        }
    }

    #[cfg(any(feature = "blocking", feature = "tokio", test))]
    pub(crate) const fn create_console(source: io::Error) -> Self {
        Self {
            repr: ErrorRepr::CreateConsole(source),
        }
    }

    #[cfg(any(feature = "blocking", feature = "tokio", test))]
    pub(crate) const fn spawn(program: OsString, source: io::Error) -> Self {
        Self {
            repr: ErrorRepr::Spawn { program, source },
        }
    }

    #[cfg(any(feature = "blocking", feature = "tokio", test))]
    pub(crate) const fn resize(source: io::Error) -> Self {
        Self {
            repr: ErrorRepr::Resize(source),
        }
    }

    #[cfg(any(feature = "blocking", feature = "tokio", test))]
    pub(crate) const fn clear(source: io::Error) -> Self {
        Self {
            repr: ErrorRepr::Clear(source),
        }
    }

    #[cfg(any(feature = "blocking", feature = "tokio", test))]
    pub(crate) const fn unsupported_feature(feature: &'static str) -> Self {
        Self {
            repr: ErrorRepr::UnsupportedFeature { feature },
        }
    }

    pub(crate) const fn invalid_size(rows: u16, cols: u16) -> Self {
        Self {
            repr: ErrorRepr::InvalidSize { rows, cols },
        }
    }

    #[cfg(any(feature = "blocking", feature = "tokio", test))]
    pub(crate) const fn wait(source: io::Error) -> Self {
        Self {
            repr: ErrorRepr::Wait(source),
        }
    }

    #[cfg(any(feature = "blocking", feature = "tokio", test))]
    pub(crate) const fn kill(source: io::Error) -> Self {
        Self {
            repr: ErrorRepr::Kill(source),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.repr.fmt(f)
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Error")
            .field("kind", &self.kind())
            .field("context", &self.repr)
            .finish()
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.repr.source()
    }
}

impl From<BackendError> for Error {
    fn from(source: BackendError) -> Self {
        Self {
            repr: ErrorRepr::Backend(source),
        }
    }
}

impl From<io::Error> for Error {
    fn from(source: io::Error) -> Self {
        Self {
            repr: ErrorRepr::Io(source),
        }
    }
}

/// The failure class reported by a [`BackendError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BackendErrorKind {
    /// A requested `conpty.dll` could not be found or loaded.
    DllNotFound,
    /// A loaded DLL did not export a required function.
    MissingExport,
    /// A bundled DLL had no accompanying `OpenConsole.exe`.
    OpenConsoleMissing,
    /// The DLL and console host were not a validated matching pair.
    VersionMismatch,
    /// The system does not provide the required `ConPTY` API.
    Unsupported,
}

/// A failure while locating, loading, or validating a `ConPTY` backend.
///
/// The representation is private so paths, symbol names, and version strings
/// can evolve without becoming `SemVer` commitments.
pub struct BackendError {
    repr: BackendErrorRepr,
}

#[derive(Debug, thiserror::Error)]
enum BackendErrorRepr {
    #[error("conpty.dll not found in `{}`", .dir.display())]
    DllNotFound { dir: PathBuf, source: io::Error },
    #[error("`{}` is missing required export `{symbol}`", .dll.display())]
    MissingExport { dll: PathBuf, symbol: &'static str },
    #[error("OpenConsole.exe not found next to `{}`", .dll.display())]
    OpenConsoleMissing { dll: PathBuf },
    #[error(
        "version mismatch: `{}` reports {dll_version} \
         but its OpenConsole.exe reports {exe_version}",
        .dll.display()
    )]
    VersionMismatch {
        dll: PathBuf,
        dll_version: String,
        exe_version: String,
    },
    #[error(
        "ConPTY is not available on this version of Windows; \
         Windows 10 1809 (build 17763) or later is required"
    )]
    Unsupported,
}

impl BackendError {
    /// Returns the stable classification of this backend failure.
    #[must_use]
    pub const fn kind(&self) -> BackendErrorKind {
        match self.repr {
            BackendErrorRepr::DllNotFound { .. } => BackendErrorKind::DllNotFound,
            BackendErrorRepr::MissingExport { .. } => BackendErrorKind::MissingExport,
            BackendErrorRepr::OpenConsoleMissing { .. } => BackendErrorKind::OpenConsoleMissing,
            BackendErrorRepr::VersionMismatch { .. } => BackendErrorKind::VersionMismatch,
            BackendErrorRepr::Unsupported => BackendErrorKind::Unsupported,
        }
    }

    /// Returns the directly held I/O error, when this failure has one.
    #[must_use]
    pub const fn io_error(&self) -> Option<&io::Error> {
        match &self.repr {
            BackendErrorRepr::DllNotFound { source, .. } => Some(source),
            BackendErrorRepr::MissingExport { .. }
            | BackendErrorRepr::OpenConsoleMissing { .. }
            | BackendErrorRepr::VersionMismatch { .. }
            | BackendErrorRepr::Unsupported => None,
        }
    }

    pub(crate) const fn dll_not_found(dir: PathBuf, source: io::Error) -> Self {
        Self {
            repr: BackendErrorRepr::DllNotFound { dir, source },
        }
    }

    pub(crate) const fn missing_export(dll: PathBuf, symbol: &'static str) -> Self {
        Self {
            repr: BackendErrorRepr::MissingExport { dll, symbol },
        }
    }

    pub(crate) const fn open_console_missing(dll: PathBuf) -> Self {
        Self {
            repr: BackendErrorRepr::OpenConsoleMissing { dll },
        }
    }

    pub(crate) const fn version_mismatch(
        dll: PathBuf,
        dll_version: String,
        exe_version: String,
    ) -> Self {
        Self {
            repr: BackendErrorRepr::VersionMismatch {
                dll,
                dll_version,
                exe_version,
            },
        }
    }

    pub(crate) const fn unsupported() -> Self {
        Self {
            repr: BackendErrorRepr::Unsupported,
        }
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.repr.fmt(f)
    }
}

impl fmt::Debug for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BackendError")
            .field("kind", &self.kind())
            .field("context", &self.repr)
            .finish()
    }
}

impl std::error::Error for BackendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.repr.source()
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
