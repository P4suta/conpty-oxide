// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;

#[test]
fn success_is_exactly_code_zero() {
    assert!(ExitStatus::from_raw(0).success());
    assert!(!ExitStatus::from_raw(1).success());
    assert!(!ExitStatus::from_raw(u32::MAX).success());
}

#[test]
fn code_returns_the_raw_value() {
    assert_eq!(ExitStatus::from_raw(7).code(), 7);
    // 259 is `STILL_ACTIVE`; as a *reported* exit code it is an ordinary
    // value, because the crate only reads it after the wait completed.
    assert_eq!(ExitStatus::from_raw(259).code(), 259);
}

/// The expectations are exactly what
/// `std::os::windows::process::ExitStatusExt::from_raw` followed by
/// `to_string` produces for each value, so the two types render every
/// status identically.
#[test]
fn display_matches_std_wording() {
    assert_eq!(ExitStatus::from_raw(3).to_string(), "exit code: 3");
    assert_eq!(ExitStatus::from_raw(259).to_string(), "exit code: 259");
    // NTSTATUS-range codes (high bit set) render in hex, as std does:
    // STATUS_CONTROL_C_EXIT, the code this crate documents most.
    assert_eq!(
        ExitStatus::from_raw(0xC000_013A).to_string(),
        "exit code: 0xc000013a"
    );
    assert_eq!(
        ExitStatus::from_raw(u32::MAX).to_string(),
        "exit code: 0xffffffff"
    );
}
