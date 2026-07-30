// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Process and Job ownership shared by both public front ends.

use std::fmt;
use std::os::windows::io::{AsHandle, BorrowedHandle};

use crate::core::job::Job;
use crate::core::session::{RootChild, KILL_EXIT_CODE};
use crate::core::wait::ProcessWaiter;
use crate::error::{Error, Result};
use crate::status::ExitStatus;

/// Runtime-independent ownership and state for a root child process.
///
/// Blocking and Tokio children differ only in how they wait. The process
/// handle, Job, status cache, kill behavior, and Drop contract live here so
/// those semantics cannot drift between front ends.
pub(crate) struct ChildCore {
    waiter: ProcessWaiter,
    job: Job,
    pid: u32,
    kill_on_drop: bool,
    status: Option<ExitStatus>,
}

impl ChildCore {
    pub(crate) fn from_root(root: RootChild) -> Self {
        Self {
            waiter: root.waiter,
            job: root.job,
            pid: root.pid,
            kill_on_drop: root.kill_on_drop,
            status: None,
        }
    }

    pub(crate) const fn id(&self) -> u32 {
        self.pid
    }

    #[cfg(feature = "tokio")]
    pub(crate) const fn status(&self) -> Option<ExitStatus> {
        self.status
    }

    #[cfg(feature = "blocking")]
    pub(crate) fn wait_blocking(&mut self) -> Result<ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        let status = ExitStatus::from_raw(self.waiter.wait().map_err(Error::wait)?);
        self.status = Some(status);
        Ok(status)
    }

    pub(crate) fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        let Some(code) = self.waiter.try_wait().map_err(Error::wait)? else {
            return Ok(None);
        };
        let status = ExitStatus::from_raw(code);
        self.status = Some(status);
        Ok(Some(status))
    }

    #[cfg(feature = "tokio")]
    pub(crate) fn cache_exit_code(&mut self, code: u32) -> ExitStatus {
        let status = ExitStatus::from_raw(code);
        self.status = Some(status);
        status
    }

    pub(crate) fn kill(&self) -> Result<()> {
        self.job.terminate(KILL_EXIT_CODE).map_err(Error::kill)
    }
}

impl AsHandle for ChildCore {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.waiter.as_handle()
    }
}

impl fmt::Debug for ChildCore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Child")
            .field("pid", &self.pid)
            .field("kill_on_drop", &self.kill_on_drop)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl Drop for ChildCore {
    fn drop(&mut self) {
        if self.kill_on_drop {
            if let Err(err) = self.job.terminate(KILL_EXIT_CODE) {
                log_drop_kill_failure(&err);
            }
        }
    }
}

#[cfg(feature = "tracing")]
fn log_drop_kill_failure(err: &std::io::Error) {
    let logged = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tracing::error!(error = %err, "failed to terminate the child job during drop");
    }));
    if let Err(payload) = logged {
        // A tracing subscriber is arbitrary user code. Drop must not propagate
        // its panic, especially while another panic may already be unwinding.
        // Retain the payload because a user-defined payload destructor may
        // itself panic.
        std::mem::forget(payload);
    }
}

#[cfg(not(feature = "tracing"))]
const fn log_drop_kill_failure(_err: &std::io::Error) {}

#[cfg(all(test, feature = "tracing"))]
mod tests {
    use super::log_drop_kill_failure;

    #[test]
    fn drop_kill_failure_is_logged() {
        let events = crate::tracing_test_support::count_events(|| {
            log_drop_kill_failure(&std::io::Error::other("injected kill failure"));
        });
        assert_eq!(events, 1);
    }
}
