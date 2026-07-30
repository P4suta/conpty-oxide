// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Spawning the child process attached to a pseudoconsole.
//!
//! This module is one function, [`spawn`], plus the RAII scaffolding that
//! makes it leak-free. It turns a [`Command`] into a running process that is
//! (a) connected to a pseudoconsole and (b) a member of a job object, both
//! established atomically at creation through the extended startup
//! information's attribute list.
//!
//! # The attribute list
//!
//! `CreateProcessW` learns about the pseudoconsole and the job from a
//! `PROC_THREAD_ATTRIBUTE_LIST` reached via `STARTUPINFOEXW`. Building one has
//! a two-call protocol that reads like a bug: the first
//! `InitializeProcThreadAttributeList` is passed a NULL list and is *expected
//! to fail* with `ERROR_INSUFFICIENT_BUFFER`, having written the required byte
//! count; the caller allocates that many bytes and calls again to initialize
//! them. The two attributes then set are:
//!
//! - `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`, whose value is the `HPCON`
//!   *itself* — the handle is passed by value, not by pointer, matching the
//!   official `ConPTY` sample.
//! - `PROC_THREAD_ATTRIBUTE_JOB_LIST`, whose value is a pointer to an array of
//!   job handles. The array must stay alive and unmoved until the attribute
//!   list is destroyed, which is why [`spawn`] keeps the handle in a local
//!   declared before the list.
//!
//! # Why the child gets no inherited handles
//!
//! `bInheritHandles` is `FALSE`, unconditionally. `ConPTY` does not need
//! inheritance — the console host received its own duplicates of the pipe ends
//! when the pseudoconsole was created — and any handle that did leak into the
//! child would keep the conout pipe open past the child's death, destroying
//! the end-of-file contract this crate is built around.
//!
//! For the same reason the standard handles are set to `INVALID_HANDLE_VALUE`
//! with `STARTF_USESTDHANDLES` (the approach wezterm takes). Leaving
//! `dwFlags` clear would make the child inherit *our* process's standard
//! handles, so a parent whose stdout is redirected to a file would silently
//! hand that file to a child that is supposed to talk only to the
//! pseudoconsole. Console applications attached to a `ConPTY` open their
//! `CONIN$`/`CONOUT$` through the console connection rather than through these
//! fields, so blanking them costs nothing.

use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::ptr;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use std::sync::Arc;

use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList,
    UpdateProcThreadAttribute, CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_JOB_LIST,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
};

use crate::backend::HPCON;
use crate::command::{to_wide_nul, Command};
use crate::core::job::Job;

/// Number of attributes [`spawn`] puts in the process attribute list.
const ATTRIBUTE_COUNT: u32 = 2;

/// A freshly created child process.
///
/// Only the process handle is kept: the primary thread handle that
/// `CreateProcessW` also returns is closed immediately, since this crate never
/// resumes or inspects the thread.
#[derive(Debug)]
pub(super) struct SpawnedChild {
    /// Handle to the child process, used for waiting and for querying the
    /// exit code.
    pub(super) process: OwnedHandle,
    /// The child's process identifier.
    pub(super) pid: u32,
}

/// Spawns `cmd` attached to the pseudoconsole `hpcon` and assigned to `job`.
///
/// The caller is expected to have created the pseudoconsole (which already
/// closed its copies of the two client pipe ends) and the job object, and must
/// afterwards drive the pseudoconsole lifecycle: call
/// [`release_after_spawn`](crate::core::pseudocon::ConsoleShared::release_after_spawn)
/// and, when it reports that the backend cannot release, start a legacy
/// watcher with [`spawn_legacy_watcher`](crate::core::wait::spawn_legacy_watcher).
///
/// Creation flags are `EXTENDED_STARTUPINFO_PRESENT |
/// CREATE_UNICODE_ENVIRONMENT`. The Unicode flag is set even when the child
/// inherits our environment, matching the standard
/// library: it describes the format of `lpEnvironment` and is harmless when
/// that pointer is NULL.
///
/// # Errors
///
/// Returns [`io::ErrorKind::InvalidInput`] if the command line or environment
/// block cannot be built (embedded NUL, malformed variable name), otherwise
/// the OS error from the failing Win32 call. Every error path unwinds the
/// attribute list and any handle acquired so far; nothing is leaked.
pub(super) fn spawn(cmd: &Command, hpcon: HPCON, job: &Job) -> io::Result<SpawnedChild> {
    // `CreateProcessW` may modify the command-line buffer in place, so it must
    // be a mutable, NUL-terminated copy we own.
    let mut command_line = cmd.build_command_line()?;
    let environment = cmd.build_environment_block()?;
    let working_dir = cmd
        .get_current_dir()
        .map(|dir| to_wide_nul(dir.as_os_str()))
        .transpose()?;

    // Declared before the attribute list so it is still alive when the list is
    // destroyed: `PROC_THREAD_ATTRIBUTE_JOB_LIST` stores this variable's
    // address, and the documentation requires the pointed-to array to outlive
    // the list.
    let job_handle: HANDLE = job.raw_handle();

    let mut attributes = AttributeList::new(ATTRIBUTE_COUNT)?;
    // SAFETY: `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` takes the `HPCON` by
    // value, so the "pointer" is the handle itself and there is no pointee
    // whose lifetime could end early. `hpcon` is live for the whole call per
    // this function's contract.
    unsafe {
        attributes.set(
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            hpcon as *const c_void,
            size_of::<HPCON>(),
        )?;
    }
    // SAFETY: `job_handle` is a live job handle that outlives `attributes`
    // (see its declaration above), and one handle is exactly the one-element
    // array this attribute expects.
    unsafe {
        attributes.set(
            PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
            ptr::addr_of!(job_handle).cast(),
            size_of::<HANDLE>(),
        )?;
    }

    let startup_info = startup_info(&mut attributes);

    let flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT;
    let mut process_info = PROCESS_INFORMATION::default();

    // SAFETY: every pointer passed here is either NULL or points to a live
    // buffer owned by this frame for the duration of the call:
    // `command_line` is a mutable NUL-terminated UTF-16 buffer, `environment`
    // a double-NUL-terminated block matching `CREATE_UNICODE_ENVIRONMENT`,
    // `working_dir` a NUL-terminated path, and `startup_info` an initialized
    // `STARTUPINFOEXW` whose `cb` and attribute list match `flags`.
    let created = unsafe {
        CreateProcessW(
            ptr::null(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0, // bInheritHandles = FALSE; see the module docs.
            flags,
            environment
                .as_ref()
                .map_or(ptr::null(), |block| block.as_ptr().cast()),
            working_dir.as_ref().map_or(ptr::null(), Vec::as_ptr),
            ptr::addr_of!(startup_info).cast::<STARTUPINFOW>(),
            &mut process_info,
        )
    };
    if created == 0 {
        // `attributes` is destroyed by its `Drop` on the way out.
        return Err(io::Error::last_os_error());
    }

    // SAFETY: `CreateProcessW` succeeded, so both handles are open and owned
    // by this process. Wrapping each in an `OwnedHandle` immediately makes it
    // closed exactly once even if anything below panics.
    let process = unsafe { OwnedHandle::from_raw_handle(process_info.hProcess) };
    // SAFETY: as above, for the primary thread handle.
    let thread = unsafe { OwnedHandle::from_raw_handle(process_info.hThread) };
    // The thread was never suspended and this crate has no use for it; close
    // the handle now so the thread object is freed the moment it exits.
    drop(thread);

    Ok(SpawnedChild {
        process,
        pid: process_info.dwProcessId,
    })
}

/// Builds the extended startup record with deliberately invalid std handles.
fn startup_info(attributes: &mut AttributeList) -> STARTUPINFOEXW {
    STARTUPINFOEXW {
        StartupInfo: STARTUPINFOW {
            // `cb` describes the *extended* structure, which is how
            // `EXTENDED_STARTUPINFO_PRESENT` tells the kernel an attribute
            // list follows the classic fields.
            cb: u32::try_from(size_of::<STARTUPINFOEXW>()).unwrap_or(u32::MAX),
            dwFlags: STARTF_USESTDHANDLES,
            hStdInput: INVALID_HANDLE_VALUE,
            hStdOutput: INVALID_HANDLE_VALUE,
            hStdError: INVALID_HANDLE_VALUE,
            ..Default::default()
        },
        lpAttributeList: attributes.as_ptr(),
    }
}

/// An initialized `PROC_THREAD_ATTRIBUTE_LIST` and the storage behind it.
///
/// `DeleteProcThreadAttributeList` runs in [`Drop`], so every early return in
/// [`spawn`] — including a failed `UpdateProcThreadAttribute` or
/// `CreateProcessW` — unwinds the list correctly.
struct AttributeList {
    /// Backing storage for the opaque list.
    ///
    /// The element type is `usize` rather than `u8` on purpose: the list holds
    /// pointers internally, and a `Vec<u8>` only guarantees byte alignment.
    /// The allocation is never resized after `InitializeProcThreadAttributeList`
    /// has run, so the list's own internal references stay valid.
    buffer: Vec<usize>,
    #[cfg(test)]
    drop_observer: Option<Arc<AtomicBool>>,
}

impl AttributeList {
    /// Allocates and initializes a list with room for `attributes` entries.
    ///
    /// # Errors
    ///
    /// Returns the OS error from either `InitializeProcThreadAttributeList`
    /// call. A *successful* size probe is also an error: it would mean the
    /// required size was never reported and the buffer size is unknown.
    fn new(attributes: u32) -> io::Result<Self> {
        let mut size: usize = 0;
        // SAFETY: the documented size probe. A NULL list is explicitly allowed
        // here; the call writes the required byte count to `size` and fails.
        let probed =
            unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), attributes, 0, &mut size) };
        if probed != 0 {
            return Err(io::Error::other(
                "InitializeProcThreadAttributeList unexpectedly succeeded while probing \
                 for the attribute list size",
            ));
        }
        let probe_error = io::Error::last_os_error();
        if probe_error.raw_os_error() != i32::try_from(ERROR_INSUFFICIENT_BUFFER).ok() {
            return Err(probe_error);
        }

        // Round up to whole `usize` words, and never allocate zero words: an
        // empty `Vec` has a dangling pointer, which must not reach the API.
        let words = size.div_ceil(size_of::<usize>()).max(1);
        let mut buffer = vec![0usize; words];

        // SAFETY: `buffer` is at least `size` bytes long and pointer-aligned,
        // and `size` is the value the probe reported for this many attributes.
        let ok = unsafe {
            InitializeProcThreadAttributeList(buffer.as_mut_ptr().cast(), attributes, 0, &mut size)
        };
        if ok == 0 {
            // Nothing was initialized, so `DeleteProcThreadAttributeList` must
            // not run; returning before constructing `Self` guarantees that.
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            buffer,
            #[cfg(test)]
            drop_observer: None,
        })
    }

    /// Returns the pointer to hand to `STARTUPINFOEXW::lpAttributeList`.
    fn as_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buffer.as_mut_ptr().cast()
    }

    /// Adds or replaces one attribute.
    ///
    /// # Errors
    ///
    /// Returns the OS error from `UpdateProcThreadAttribute`.
    ///
    /// # Safety
    ///
    /// `value` must be what the attribute identified by `attribute` expects,
    /// and `size` its size in bytes. Attributes that take a pointer require
    /// the pointee to stay valid and unmoved until this list is dropped,
    /// because `UpdateProcThreadAttribute` stores the pointer rather than
    /// copying the data.
    unsafe fn set(
        &mut self,
        attribute: usize,
        value: *const c_void,
        size: usize,
    ) -> io::Result<()> {
        // SAFETY: `self` is an initialized attribute list with room for the
        // configured number of attributes; `value` and `size` are valid per
        // this function's contract. The two optional out-parameters are
        // documented as reserved and must be NULL.
        let ok = unsafe {
            UpdateProcThreadAttribute(
                self.as_ptr(),
                0,
                attribute,
                value,
                size,
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        #[cfg(test)]
        if let Some(observer) = &self.drop_observer {
            observer.store(true, Ordering::SeqCst);
        }
        // SAFETY: `Self` is only ever constructed after
        // `InitializeProcThreadAttributeList` succeeded, and `Drop` runs once,
        // so this deletes an initialized list exactly once. The call only
        // releases the list's internal references; the `Vec` frees the memory
        // immediately afterwards.
        unsafe { DeleteProcThreadAttributeList(self.as_ptr()) };
    }
}

#[cfg(test)]
#[path = "proc_tests.rs"]
mod tests;
