// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;

#[test]
fn disconnect_errors_are_recognized() {
    for code in [
        ERROR_BROKEN_PIPE,
        ERROR_HANDLE_EOF,
        ERROR_NO_DATA,
        ERROR_PIPE_NOT_CONNECTED,
    ] {
        let code = i32::try_from(code).expect("the Win32 error code fits in i32");
        assert!(is_disconnect_error(&io::Error::from_raw_os_error(code)));
    }
    assert!(is_disconnect_error(&io::Error::new(
        io::ErrorKind::BrokenPipe,
        "synthetic"
    )));
    assert!(!is_disconnect_error(&io::Error::new(
        io::ErrorKind::PermissionDenied,
        "synthetic"
    )));
}
