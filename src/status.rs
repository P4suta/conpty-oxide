// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

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
/// Obtained from either front end's `Session::wait`, `Session::try_wait`,
/// `SessionOutput::status`, or the lower-level `Child::wait` and
/// `Child::try_wait`. The wrapped value is exactly what `GetExitCodeProcess`
/// reported, read only after the process handle was confirmed signaled — so
/// it can never be the `STILL_ACTIVE` sentinel of a still-running process.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use]
pub struct ExitStatus(u32);

impl ExitStatus {
    /// Wraps a raw exit code obtained from `GetExitCodeProcess`.
    #[cfg(any(feature = "blocking", feature = "tokio", test))]
    pub(super) const fn from_raw(code: u32) -> Self {
        Self(code)
    }

    /// Returns the raw exit code.
    ///
    /// Unlike `std::process::ExitStatus::code` this is not an [`Option`]: a
    /// Windows process always has an exit code, including when it was
    /// terminated by `Child::kill`.
    #[must_use]
    pub const fn code(&self) -> u32 {
        self.0
    }

    /// Returns whether the process exited with code `0`.
    #[must_use]
    pub const fn success(&self) -> bool {
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
#[path = "status_tests.rs"]
mod tests;
