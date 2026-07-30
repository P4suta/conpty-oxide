// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Job objects: owning the child's process tree so it can be killed whole.
//!
//! A shell spawned under a pseudoconsole is rarely a single process — it
//! launches children of its own, and those children launch more. Terminating
//! only the process this crate spawned would leave that tree running, still
//! attached to the pseudoconsole and still holding the console host alive.
//! Windows' answer is a job object: every process created with
//! `PROC_THREAD_ATTRIBUTE_JOB_LIST` joins the job at creation, its descendants
//! inherit the membership, and `TerminateJobObject` ends all of them at once.
//!
//! Two properties of this design are worth stating explicitly:
//!
//! - **The console host is not in the job.** `conhost.exe` / `OpenConsole.exe`
//!   is a child of *our* process, created by `CreatePseudoConsole`, not by the
//!   `CreateProcessW` call that carries the job attribute. Killing the job
//!   therefore kills the session's processes but leaves the pseudoconsole
//!   itself to be torn down by the lifecycle state machine in
//!   [`crate::core::pseudocon`], exactly as an ordinary child exit would.
//! - **No `CREATE_SUSPENDED` dance is needed.** Assigning a job after
//!   `CreateProcessW` returns leaves a window in which the child can spawn
//!   grandchildren outside the job, which is why older code creates the
//!   process suspended and resumes it after `AssignProcessToJobObject`.
//!   `PROC_THREAD_ATTRIBUTE_JOB_LIST` closes that window by assigning the
//!   process before its first instruction runs; it requires Windows 8, and
//!   `ConPTY` already requires Windows 10 1809.

use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr;

/// Exit code used when terminating a managed process tree.
///
/// Matches `std::process::Child::kill`, which passes `1` to
/// `TerminateProcess`.
pub(crate) const KILL_EXIT_CODE: u32 = 1;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

/// An unnamed job object that owns one session's process tree.
///
/// The handle is closed when the `Job` is dropped. Whether that also kills the
/// tree depends on the `kill_on_close` flag passed to [`Job::create`]: the
/// kernel destroys a job — terminating its members — only once the last handle
/// to it is closed *and* `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is set.
#[derive(Debug)]
pub(super) struct Job(OwnedHandle);

impl Job {
    /// Creates an unnamed, private job object.
    ///
    /// With `kill_on_close` set, the job carries
    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so dropping the last handle to it
    /// terminates every process still in the tree. That is the kill-on-drop
    /// semantic, and it holds even if the owning process dies abruptly: the
    /// kernel closes the handle on process teardown and the job goes with it,
    /// which no user-space `Drop` could guarantee.
    ///
    /// Without the flag, closing the handle just detaches: the tree keeps
    /// running and can still be terminated explicitly via [`Job::terminate`].
    ///
    /// # Errors
    ///
    /// Returns the OS error from `CreateJobObjectW` or
    /// `SetInformationJobObject`.
    pub(super) fn create(kill_on_close: bool) -> io::Result<Self> {
        // SAFETY: a NULL `lpJobAttributes` requests the default security
        // descriptor and a non-inheritable handle — the latter matters,
        // because the crate spawns with `bInheritHandles = FALSE` and relies
        // on nothing leaking into children. A NULL `lpName` creates an
        // unnamed job that only this handle refers to.
        let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `CreateJobObjectW` reported success, so `handle` is a valid,
        // open handle exclusively owned by this process. Transferring it to an
        // `OwnedHandle` here — before any further fallible step — is what
        // makes the error paths below leak-free.
        let job = Self(unsafe { OwnedHandle::from_raw_handle(handle) });

        // The limit block is written unconditionally: with no flags set it is
        // a no-op that restates the job's initial state, and keeping a single
        // code path means the `kill_on_close = false` case is exercised by the
        // same call the `true` case uses.
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        if kill_on_close {
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        }

        // SAFETY: `job` owns a live job handle; `limits` is a fully
        // initialized `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` that outlives the
        // call, and its size is the one the information class expects.
        let ok = unsafe {
            SetInformationJobObject(
                job.raw_handle(),
                JobObjectExtendedLimitInformation,
                ptr::addr_of!(limits).cast(),
                u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                    .unwrap_or(u32::MAX),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(job)
    }

    /// Returns the raw job handle, for `PROC_THREAD_ATTRIBUTE_JOB_LIST`.
    ///
    /// The value is valid for as long as this `Job` is alive. The attribute
    /// takes a *pointer* to a handle, so callers must copy this into a
    /// variable that outlives the attribute list rather than passing a
    /// temporary.
    pub(super) fn raw_handle(&self) -> HANDLE {
        self.0.as_raw_handle()
    }

    /// Terminates every process in the job with `exit_code`.
    ///
    /// This is the "kill tree" operation: descendants are members too, so one
    /// call ends the whole tree. Terminating an empty job (or one whose
    /// processes have already exited) succeeds and does nothing, which makes
    /// this safe to call unconditionally during teardown.
    ///
    /// Termination is asynchronous — the call returns once the kernel has
    /// scheduled it. Callers that need to observe the result must wait on the
    /// process handle afterwards.
    ///
    /// # Errors
    ///
    /// Returns the OS error from `TerminateJobObject`.
    pub(super) fn terminate(&self, exit_code: u32) -> io::Result<()> {
        // SAFETY: `self.0` is a live job handle owned by `self`, opened with
        // `JOB_OBJECT_TERMINATE` access (creation grants full access).
        let ok = unsafe { TerminateJobObject(self.raw_handle(), exit_code) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "job_tests.rs"]
mod tests;
