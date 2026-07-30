// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Synchronous anonymous pipes used by the blocking frontend.
//!
//! Anonymous pipe handles from `CreatePipe` never take an `OVERLAPPED`,
//! matching `CreatePseudoConsole`'s synchronous-handle requirement. Every
//! handle is deliberately non-inheritable: `ConPTY` duplicates the ends it
//! needs, and an inherited copy would keep the session from reaching EOF.

use std::io;
use std::mem::size_of;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::ptr;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Pipes::CreatePipe;

/// The four pipe ends of a blocking pseudoconsole session.
///
/// The two ends handed to `CreatePseudoConsole` must be closed by us as soon
/// as the console host has duplicated them. The frontend retains
/// `conout_read` and `conin_write`.
#[derive(Debug)]
pub(crate) struct SyncPipes {
    /// Read end of conout. The frontend reads pseudoconsole output from this.
    pub(crate) conout_read: OwnedHandle,
    /// Write end of conout, passed to `CreatePseudoConsole`.
    pub(crate) conout_write: OwnedHandle,
    /// Read end of conin, passed to `CreatePseudoConsole`.
    pub(crate) conin_read: OwnedHandle,
    /// Write end of conin. The frontend writes console input to this.
    pub(crate) conin_write: OwnedHandle,
}

/// Creates the conout and conin anonymous pipes for one blocking session.
///
/// Both pipes use the system default buffer size and are non-inheritable.
pub(crate) fn create_sync_pipes() -> io::Result<SyncPipes> {
    let (conout_read, conout_write) = create_pipe()?;
    let (conin_read, conin_write) = create_pipe()?;

    Ok(SyncPipes {
        conout_read,
        conout_write,
        conin_read,
        conin_write,
    })
}

/// Creates one non-inheritable anonymous pipe, returning `(read, write)`.
fn create_pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let attributes = non_inheritable_attributes();

    let mut read: HANDLE = ptr::null_mut();
    let mut write: HANDLE = ptr::null_mut();

    // SAFETY: `read` and `write` are live, correctly typed out-parameters,
    // and `attributes` outlives the call. `nSize = 0` requests the system
    // default buffer size.
    let created = unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) };
    if created == 0 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: successful `CreatePipe` returned this valid handle and transfers
    // its ownership to the wrapper.
    let read = unsafe { OwnedHandle::from_raw_handle(read) };
    // SAFETY: as above for the distinct write handle.
    let write = unsafe { OwnedHandle::from_raw_handle(write) };
    Ok((read, write))
}

/// Security attributes shared by every pipe end: no inheritance.
fn non_inheritable_attributes() -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(u32::MAX),
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 0,
    }
}

#[cfg(test)]
#[path = "anonymous_tests.rs"]
mod tests;
