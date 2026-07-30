// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;

fn io_err(kind: io::ErrorKind) -> io::Error {
    io::Error::new(kind, "underlying os error")
}

#[test]
fn display_messages_preserve_diagnostic_context() {
    let cases: [(Error, ErrorKind, &str); 8] = [
        (
            Error::create_console(io_err(io::ErrorKind::Other)),
            ErrorKind::CreateConsole,
            "failed to create pseudoconsole",
        ),
        (
            Error::spawn(OsString::from("cmd.exe"), io_err(io::ErrorKind::NotFound)),
            ErrorKind::Spawn,
            "failed to spawn `cmd.exe`",
        ),
        (
            Error::resize(io_err(io::ErrorKind::Other)),
            ErrorKind::Resize,
            "failed to resize pseudoconsole",
        ),
        (
            Error::clear(io_err(io::ErrorKind::Other)),
            ErrorKind::Clear,
            "failed to clear pseudoconsole",
        ),
        (
            Error::unsupported_feature("ClearPseudoConsole"),
            ErrorKind::UnsupportedFeature,
            "the ConPTY backend does not support ClearPseudoConsole",
        ),
        (
            Error::invalid_size(0, 80),
            ErrorKind::InvalidSize,
            "invalid pseudoconsole size: 0 rows x 80 cols \
             (each dimension must be 1..=32767)",
        ),
        (
            Error::wait(io_err(io::ErrorKind::Other)),
            ErrorKind::Wait,
            "failed to wait for child process",
        ),
        (
            Error::kill(io_err(io::ErrorKind::Other)),
            ErrorKind::Kill,
            "failed to kill child process",
        ),
    ];
    for (err, kind, expected) in cases {
        assert_eq!(err.kind(), kind);
        assert_eq!(err.to_string(), expected);
    }
}

#[test]
fn backend_display_messages_preserve_diagnostic_context() {
    let cases: [(BackendError, BackendErrorKind, &str); 5] = [
        (
            BackendError::dll_not_found(PathBuf::from("C:\\app"), io_err(io::ErrorKind::NotFound)),
            BackendErrorKind::DllNotFound,
            "conpty.dll not found in `C:\\app`",
        ),
        (
            BackendError::missing_export(
                PathBuf::from("C:\\app\\conpty.dll"),
                "CreatePseudoConsole",
            ),
            BackendErrorKind::MissingExport,
            "`C:\\app\\conpty.dll` is missing required export `CreatePseudoConsole`",
        ),
        (
            BackendError::open_console_missing(PathBuf::from("C:\\app\\conpty.dll")),
            BackendErrorKind::OpenConsoleMissing,
            "OpenConsole.exe not found next to `C:\\app\\conpty.dll`",
        ),
        (
            BackendError::version_mismatch(
                PathBuf::from("C:\\app\\conpty.dll"),
                "1.19".to_owned(),
                "1.22".to_owned(),
            ),
            BackendErrorKind::VersionMismatch,
            "version mismatch: `C:\\app\\conpty.dll` reports 1.19 \
             but its OpenConsole.exe reports 1.22",
        ),
        (
            BackendError::unsupported(),
            BackendErrorKind::Unsupported,
            "ConPTY is not available on this version of Windows; \
             Windows 10 1809 (build 17763) or later is required",
        ),
    ];
    for (err, kind, expected) in cases {
        assert_eq!(err.kind(), kind);
        assert_eq!(err.to_string(), expected);
    }
}

#[test]
fn direct_io_accessor_and_source_agree() {
    let err = Error::create_console(io_err(io::ErrorKind::PermissionDenied));
    assert_eq!(
        err.io_error().map(io::Error::kind),
        Some(io::ErrorKind::PermissionDenied)
    );

    let source = error::Error::source(&err).expect("the source must be retained");
    let io_source = source
        .downcast_ref::<io::Error>()
        .expect("the source must be an io::Error");
    assert_eq!(io_source.kind(), io::ErrorKind::PermissionDenied);
}

#[test]
fn every_operation_io_error_exposes_the_direct_source_only() {
    let cases = [
        Error::create_console(io_err(io::ErrorKind::Other)),
        Error::spawn(
            OsString::from("missing.exe"),
            io_err(io::ErrorKind::NotFound),
        ),
        Error::resize(io_err(io::ErrorKind::InvalidInput)),
        Error::clear(io_err(io::ErrorKind::PermissionDenied)),
        Error::wait(io_err(io::ErrorKind::TimedOut)),
        Error::kill(io_err(io::ErrorKind::Interrupted)),
        Error::from(io_err(io::ErrorKind::BrokenPipe)),
    ];

    for err in cases {
        assert!(err.io_error().is_some());
        assert!(err.backend_error().is_none());
        assert!(
            error::Error::source(&err)
                .and_then(|source| source.downcast_ref::<io::Error>())
                .is_some(),
            "{:?} must retain its direct I/O source",
            err.kind()
        );
        let rendered = format!("{err:?}");
        assert!(rendered.contains("kind"), "{rendered}");
        assert!(rendered.contains("context"), "{rendered}");
    }
}

#[test]
fn context_only_errors_have_no_typed_source() {
    for err in [
        Error::unsupported_feature("ClearPseudoConsole"),
        Error::invalid_size(0, 0),
    ] {
        assert!(err.io_error().is_none());
        assert!(err.backend_error().is_none());
        assert!(error::Error::source(&err).is_none());
    }
}

#[test]
fn backend_source_chain_and_accessors_are_preserved() {
    let err = Error::from(BackendError::dll_not_found(
        PathBuf::from("C:\\app"),
        io_err(io::ErrorKind::NotFound),
    ));
    assert_eq!(err.kind(), ErrorKind::Backend);
    assert!(err.io_error().is_none());

    let backend = err
        .backend_error()
        .expect("backend failures retain their typed source");
    assert_eq!(backend.kind(), BackendErrorKind::DllNotFound);
    assert_eq!(
        backend.io_error().map(io::Error::kind),
        Some(io::ErrorKind::NotFound)
    );

    let source = error::Error::source(&err).expect("the source must be retained");
    assert!(source.downcast_ref::<BackendError>().is_some());
    let backend_source =
        error::Error::source(backend).expect("the backend OS source must be retained");
    assert!(backend_source.downcast_ref::<io::Error>().is_some());
    let rendered = format!("{backend:?}");
    assert!(rendered.contains("kind"), "{rendered}");
    assert!(rendered.contains("context"), "{rendered}");
}

#[test]
fn backend_errors_without_io_context_report_no_source() {
    let cases = [
        BackendError::missing_export(PathBuf::from("C:\\app\\conpty.dll"), "CreatePseudoConsole"),
        BackendError::open_console_missing(PathBuf::from("C:\\app\\conpty.dll")),
        BackendError::version_mismatch(
            PathBuf::from("C:\\app\\conpty.dll"),
            "1.19".to_owned(),
            "1.22".to_owned(),
        ),
        BackendError::unsupported(),
    ];

    for err in cases {
        assert!(err.io_error().is_none());
        assert!(error::Error::source(&err).is_none());
        let rendered = format!("{err:?}");
        assert!(rendered.contains("kind"), "{rendered}");
        assert!(rendered.contains("context"), "{rendered}");
    }
}

#[test]
fn from_io_error_preserves_kind_and_source() {
    let err = Error::from(io_err(io::ErrorKind::BrokenPipe));
    assert_eq!(err.kind(), ErrorKind::Io);
    assert_eq!(
        err.io_error().map(io::Error::kind),
        Some(io::ErrorKind::BrokenPipe)
    );
}
