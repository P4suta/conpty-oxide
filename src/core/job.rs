// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Session Job policy built on windows-spawn's sole Job implementation.

use std::io;

pub(super) use windows_spawn::Job;

/// Exit code used when terminating a managed process tree.
pub(crate) const KILL_EXIT_CODE: u32 = 1;

/// Creates the shared session Job and applies its drop policy.
pub(super) fn create(kill_on_close: bool) -> io::Result<Job> {
    let job = Job::create()?;
    job.set_kill_on_close(kill_on_close)?;
    Ok(job)
}

#[cfg(test)]
#[path = "job_tests.rs"]
mod tests;
