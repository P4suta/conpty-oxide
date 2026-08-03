// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;

use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsHandle, AsRawHandle};
use std::ptr;

use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE_FLAG_INHERIT};
use windows_sys::Win32::System::JobObjects::{
    JobObjectExtendedLimitInformation, QueryInformationJobObject,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

/// Reads back the job's extended limit information.
fn limit_flags(job: &Job) -> JOB_OBJECT_LIMIT {
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    let mut returned: u32 = 0;
    // SAFETY: `job` holds a live job handle, and `limits` / `returned` are
    // valid out-parameters of the sizes the information class expects.
    let ok = unsafe {
        QueryInformationJobObject(
            job.as_handle().as_raw_handle(),
            JobObjectExtendedLimitInformation,
            ptr::addr_of_mut!(limits).cast(),
            u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                .expect("the fixed information structure fits in u32"),
            &mut returned,
        )
    };
    assert_ne!(
        ok,
        0,
        "QueryInformationJobObject failed: {}",
        io::Error::last_os_error()
    );
    limits.BasicLimitInformation.LimitFlags
}

#[test]
fn create_without_kill_on_close_sets_no_limits() {
    let job = create(false).expect("creating a job must succeed");
    assert_eq!(limit_flags(&job), 0);
}

#[test]
fn create_with_kill_on_close_sets_the_limit() {
    let job = create(true).expect("creating a job must succeed");
    assert_eq!(
        limit_flags(&job) & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
    );
}

#[test]
fn the_handle_is_not_inheritable() {
    let job = create(true).expect("creating a job must succeed");
    let mut flags: u32 = 0;
    // SAFETY: the handle is live and `flags` is a valid out-parameter.
    let ok = unsafe { GetHandleInformation(job.as_handle().as_raw_handle(), &mut flags) };
    assert_ne!(
        ok,
        0,
        "GetHandleInformation failed: {}",
        io::Error::last_os_error()
    );
    assert_eq!(flags & HANDLE_FLAG_INHERIT, 0);
}

#[test]
fn terminate_succeeds_on_an_empty_job() {
    let job = create(false).expect("creating a job must succeed");
    job.terminate(1)
        .expect("terminating an empty job must succeed");
    // Idempotent: a job with no members can be terminated repeatedly.
    job.terminate(1)
        .expect("terminating an empty job twice must succeed");
}
