//! Child process exit status.
//!
//! Windows exit codes are plain `DWORD`s: there are no signals and no
//! "terminated by" case, so [`ExitStatus`] is a thin, always-valid wrapper
//! around a `u32` rather than the platform-abstracting enum a cross-platform
//! API would need. The type lives outside the front-end modules because the
//! blocking and async APIs both hand it back from `wait`.

use core::fmt;

/// The exit status of a child process that has terminated.
///
/// Obtained from either front end's `Child::wait` or `Child::try_wait`. The
/// wrapped value is exactly what `GetExitCodeProcess` reported, read only
/// after the process handle was confirmed signaled — so it can never be the
/// `STILL_ACTIVE` sentinel of a still-running process.
///
/// # Examples
///
/// ```
/// use conpty_oxide::ExitStatus;
///
/// # fn check(status: ExitStatus) {
/// if status.success() {
///     println!("clean exit");
/// } else {
///     println!("failed with {}", status.code());
/// }
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExitStatus(u32);

impl ExitStatus {
    /// Wraps a raw exit code obtained from `GetExitCodeProcess`.
    pub(crate) fn from_raw(code: u32) -> Self {
        Self(code)
    }

    /// Returns the raw exit code.
    ///
    /// Unlike `std::process::ExitStatus::code` this is not an [`Option`]: a
    /// Windows process always has an exit code, including when it was
    /// terminated by `Child::kill`.
    #[must_use]
    pub fn code(&self) -> u32 {
        self.0
    }

    /// Returns whether the process exited with code `0`.
    #[must_use]
    pub fn success(&self) -> bool {
        self.0 == 0
    }
}

/// Formats as `exit code: <code>`, matching `std::process::ExitStatus` on
/// Windows: decimal for ordinary codes, hexadecimal when the high bit is set.
///
/// The hexadecimal case matters more here than it would elsewhere, because
/// the code this crate documents most — `STATUS_CONTROL_C_EXIT`, reported by
/// a child whose terminal went away — is in that range: it renders as
/// `exit code: 0xc000013a`, the spelling `NTSTATUS` values are written in
/// everywhere, rather than the unrecognizable `3221225786`.
impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 & 0x8000_0000 != 0 {
            write!(f, "exit code: {:#x}", self.0)
        } else {
            write!(f, "exit code: {}", self.0)
        }
    }
}

#[cfg(test)]
mod tests {
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
}
