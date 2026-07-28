//! Error types for `conpty-oxide`.
//!
//! [`Error`] is the crate-wide error type; each variant corresponds to one
//! phase of pseudoconsole operation (backend loading, console creation,
//! spawning, resizing, waiting, killing, or plain pipe I/O). Backend loading
//! has its own dedicated [`BackendError`] type because locating and validating
//! `conpty.dll` can fail in several distinct, user-actionable ways.

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;

/// Convenience alias for results produced by this crate.
///
/// The error type defaults to [`Error`] but can be overridden, e.g.
/// `Result<T, BackendError>`.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The crate-wide error type.
///
/// Variants are grouped by operation phase rather than by underlying cause,
/// so callers can tell *what* the crate was doing when the failure occurred.
/// Most variants carry the originating [`io::Error`] as their
/// [`source`](std::error::Error::source).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Loading or validating the ConPTY backend failed.
    #[error("failed to initialize the ConPTY backend")]
    Backend(#[from] BackendError),

    /// `CreatePseudoConsole` (or the equivalent backend call) failed.
    #[error("failed to create pseudoconsole")]
    CreateConsole(#[source] io::Error),

    /// Spawning the child process attached to the pseudoconsole failed.
    #[error("failed to spawn `{}`", .program.to_string_lossy())]
    Spawn {
        /// The program that could not be spawned.
        program: OsString,
        /// The underlying OS error.
        source: io::Error,
    },

    /// `ResizePseudoConsole` (or the equivalent backend call) failed.
    #[error("failed to resize pseudoconsole")]
    Resize(#[source] io::Error),

    /// A requested size was outside the valid range.
    ///
    /// Both dimensions must be in `1..=`[`Size::MAX_DIMENSION`], because
    /// ConPTY's `COORD` stores each dimension as a positive `i16`.
    ///
    /// [`Size::MAX_DIMENSION`]: crate::Size::MAX_DIMENSION
    #[error(
        "invalid pseudoconsole size: {rows} rows x {cols} cols \
         (each dimension must be 1..={max})",
        max = crate::Size::MAX_DIMENSION
    )]
    InvalidSize {
        /// The rejected row count.
        rows: u16,
        /// The rejected column count.
        cols: u16,
    },

    /// Waiting for the child process to exit failed.
    #[error("failed to wait for child process")]
    Wait(#[source] io::Error),

    /// Terminating the child process (tree) failed.
    #[error("failed to kill child process")]
    Kill(#[source] io::Error),

    /// An I/O error on the pseudoconsole pipes.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Errors from locating, loading, and validating the ConPTY backend.
///
/// The backend is either a bundled `conpty.dll` (with its companion
/// `OpenConsole.exe`) or the ConPTY API built into the system console host.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BackendError {
    /// A bundled `conpty.dll` was expected in `dir` but could not be loaded.
    #[error("conpty.dll not found in `{}`", .dir.display())]
    DllNotFound {
        /// The directory that was searched.
        dir: PathBuf,
        /// The underlying OS error.
        source: io::Error,
    },

    /// The DLL loaded but does not export a required symbol.
    #[error("`{}` is missing required export `{symbol}`", .dll.display())]
    MissingExport {
        /// Path of the DLL that was loaded.
        dll: PathBuf,
        /// Name of the missing export.
        symbol: &'static str,
    },

    /// `OpenConsole.exe` was not found next to the bundled `conpty.dll`.
    ///
    /// A bundled `conpty.dll` delegates console hosting to an
    /// `OpenConsole.exe` located in the same directory; without it the DLL is
    /// unusable.
    #[error("OpenConsole.exe not found next to `{}`", .dll.display())]
    OpenConsoleMissing {
        /// Path of the DLL that requires the missing `OpenConsole.exe`.
        dll: PathBuf,
    },

    /// The bundled DLL's version does not match what this executable expects.
    #[error(
        "version mismatch for `{}`: dll version {dll_version}, \
         exe expects {exe_version}",
        .dll.display()
    )]
    VersionMismatch {
        /// Path of the mismatched DLL.
        dll: PathBuf,
        /// Version reported by the DLL.
        dll_version: String,
        /// Version this executable was built against.
        exe_version: String,
    },

    /// ConPTY is not available on this Windows installation.
    #[error(
        "ConPTY is not available on this version of Windows; \
         Windows 10 1809 (build 17763) or later is required"
    )]
    Unsupported,
}

/// Converts an [`Error`] into an [`io::Error`].
///
/// This is required by `Read`/`Write`/`AsyncRead` trait implementations,
/// which must surface failures as `io::Error`. [`Error::Io`] unwraps to the
/// inner error, preserving its [`io::ErrorKind`]; every other
/// variant is wrapped whole via [`io::Error::other`] so the full source chain
/// stays available through
/// [`get_ref`](io::Error::get_ref)/[`into_inner`](io::Error::into_inner).
impl From<Error> for io::Error {
    fn from(err: Error) -> Self {
        match err {
            Error::Io(inner) => inner,
            other => io::Error::other(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    fn io_err(kind: io::ErrorKind) -> io::Error {
        io::Error::new(kind, "underlying os error")
    }

    #[test]
    fn display_messages() {
        let cases: [(Error, &str); 6] = [
            (
                Error::CreateConsole(io_err(io::ErrorKind::Other)),
                "failed to create pseudoconsole",
            ),
            (
                Error::Spawn {
                    program: OsString::from("cmd.exe"),
                    source: io_err(io::ErrorKind::NotFound),
                },
                "failed to spawn `cmd.exe`",
            ),
            (
                Error::Resize(io_err(io::ErrorKind::Other)),
                "failed to resize pseudoconsole",
            ),
            (
                Error::InvalidSize { rows: 0, cols: 80 },
                "invalid pseudoconsole size: 0 rows x 80 cols \
                 (each dimension must be 1..=32767)",
            ),
            (
                Error::Wait(io_err(io::ErrorKind::Other)),
                "failed to wait for child process",
            ),
            (
                Error::Kill(io_err(io::ErrorKind::Other)),
                "failed to kill child process",
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(err.to_string(), expected);
        }
    }

    #[test]
    fn backend_display_messages() {
        let cases: [(BackendError, &str); 5] = [
            (
                BackendError::DllNotFound {
                    dir: PathBuf::from("C:\\app"),
                    source: io_err(io::ErrorKind::NotFound),
                },
                "conpty.dll not found in `C:\\app`",
            ),
            (
                BackendError::MissingExport {
                    dll: PathBuf::from("C:\\app\\conpty.dll"),
                    symbol: "CreatePseudoConsole",
                },
                "`C:\\app\\conpty.dll` is missing required export `CreatePseudoConsole`",
            ),
            (
                BackendError::OpenConsoleMissing {
                    dll: PathBuf::from("C:\\app\\conpty.dll"),
                },
                "OpenConsole.exe not found next to `C:\\app\\conpty.dll`",
            ),
            (
                BackendError::VersionMismatch {
                    dll: PathBuf::from("C:\\app\\conpty.dll"),
                    dll_version: "1.19".to_string(),
                    exe_version: "1.22".to_string(),
                },
                "version mismatch for `C:\\app\\conpty.dll`: dll version 1.19, exe expects 1.22",
            ),
            (
                BackendError::Unsupported,
                "ConPTY is not available on this version of Windows; \
                 Windows 10 1809 (build 17763) or later is required",
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(err.to_string(), expected);
        }
    }

    #[test]
    fn phase_variants_expose_io_source() {
        let err = Error::CreateConsole(io_err(io::ErrorKind::PermissionDenied));
        let source = err.source().expect("CreateConsole must have a source");
        let io_source = source
            .downcast_ref::<io::Error>()
            .expect("source must be an io::Error");
        assert_eq!(io_source.kind(), io::ErrorKind::PermissionDenied);

        let err = Error::Spawn {
            program: OsString::from("cmd.exe"),
            source: io_err(io::ErrorKind::NotFound),
        };
        assert!(err.source().is_some());
    }

    #[test]
    fn backend_source_chain_is_two_levels_deep() {
        let err = Error::from(BackendError::DllNotFound {
            dir: PathBuf::from("C:\\app"),
            source: io_err(io::ErrorKind::NotFound),
        });
        let backend = err.source().expect("Backend must have a source");
        assert!(backend.downcast_ref::<BackendError>().is_some());
        let io_source = backend
            .source()
            .expect("DllNotFound must have a source")
            .downcast_ref::<io::Error>()
            .expect("source must be an io::Error");
        assert_eq!(io_source.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn into_io_error_preserves_kind_for_io_variant() {
        let err = Error::Io(io_err(io::ErrorKind::BrokenPipe));
        let converted = io::Error::from(err);
        assert_eq!(converted.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn into_io_error_wraps_other_variants() {
        let err = Error::InvalidSize { rows: 0, cols: 0 };
        let converted = io::Error::from(err);
        assert_eq!(converted.kind(), io::ErrorKind::Other);
        assert!(converted
            .get_ref()
            .expect("wrapped error must be present")
            .downcast_ref::<Error>()
            .is_some());
    }
}
