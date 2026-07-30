// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `ConPTY` export resolution and dynamic-module lifetime pinning.
//!
//! Function pointers and their `LoadLibraryExW` reference live together in the
//! parent backend object. Keeping those responsibilities here makes the unsafe
//! condition local: no resolved address may outlive its [`ModuleGuard`].

use std::io;
use std::iter;
use std::mem;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

use windows_sys::core::{BOOL, HRESULT};
#[cfg(any(feature = "blocking", feature = "tokio", test))]
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
#[cfg(any(feature = "blocking", feature = "tokio", test))]
use windows_sys::Win32::System::Console::COORD;
use windows_sys::Win32::System::Console::HPCON;
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
};

/// Prefix under which standalone `conpty.dll` exports its entry points.
const CONPTY_EXPORT_PREFIX: &str = "Conpty";

pub(super) const CREATE_PSEUDO_CONSOLE: &str = "CreatePseudoConsole";
const RESIZE_PSEUDO_CONSOLE: &str = "ResizePseudoConsole";
const CLOSE_PSEUDO_CONSOLE: &str = "ClosePseudoConsole";
const RELEASE_PSEUDO_CONSOLE: &str = "ReleasePseudoConsole";
const CLEAR_PSEUDO_CONSOLE: &str = "ClearPseudoConsole";

/// Loader search policy for standalone `conpty.dll` dependencies.
///
/// These flags are disjoint bit values. Keep the combination in one place so
/// tests can assert that neither security boundary is accidentally dropped.
pub(super) const fn restricted_search_flags() -> u32 {
    LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32
}

/// Whether the changing `ClearPseudoConsole` ABI is callable on this target.
///
/// On x86, `WINAPI` is `__stdcall` and an arity mismatch corrupts the stack.
/// x86-64 and `AArch64` pass the extra argument in a register an older export
/// ignores, so the current two-argument spelling works with both revisions.
const CLEAR_ABI_IS_CALLABLE: bool = !cfg!(target_arch = "x86");

#[cfg(any(feature = "blocking", feature = "tokio", test))]
type CreatePseudoConsoleFn =
    unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> HRESULT;
#[cfg(any(feature = "blocking", feature = "tokio", test))]
type ResizePseudoConsoleFn = unsafe extern "system" fn(HPCON, COORD) -> HRESULT;
#[cfg(any(feature = "blocking", feature = "tokio", test))]
type ClosePseudoConsoleFn = unsafe extern "system" fn(HPCON);
type ReleasePseudoConsoleFn = unsafe extern "system" fn(HPCON) -> HRESULT;
type ClearPseudoConsoleFn = unsafe extern "system" fn(HPCON, BOOL) -> HRESULT;

/// Untyped export address returned by `GetProcAddress`.
pub(super) type ProcAddress = unsafe extern "system" fn() -> isize;

/// Resolves a `ConPTY` entry point from an already-loaded module.
///
/// Standalone builds prefer the stable `Conpty`-prefixed alias; system
/// `kernel32.dll` falls through to the bare SDK spelling.
///
/// # Safety
///
/// `module` must be a valid mapped module handle for the duration of the call.
pub(super) unsafe fn resolve_export(module: HMODULE, name: &str) -> Option<ProcAddress> {
    fn c_name(prefix: &str, name: &str) -> Vec<u8> {
        prefix
            .bytes()
            .chain(name.bytes())
            .chain(iter::once(0))
            .collect()
    }

    let prefixed = c_name(CONPTY_EXPORT_PREFIX, name);
    // SAFETY: `module` is live by contract and `prefixed` is NUL-terminated.
    let address = unsafe { GetProcAddress(module, prefixed.as_ptr()) };
    if address.is_some() {
        return address;
    }

    let bare = c_name("", name);
    // SAFETY: as above, with a second NUL-terminated symbol spelling.
    unsafe { GetProcAddress(module, bare.as_ptr()) }
}

/// Keeps a dynamically loaded module mapped while its exports may be called.
#[derive(Debug)]
pub(super) struct ModuleGuard {
    pub(super) module: HMODULE,
}

// SAFETY: a module handle is an opaque, non-thread-affine base address.
// Loader reference counting and export lookup are process-wide thread-safe
// operations, and this guard exposes no mutation.
unsafe impl Send for ModuleGuard {}
// SAFETY: see the Send argument; shared access observes only the opaque handle.
unsafe impl Sync for ModuleGuard {}

impl Drop for ModuleGuard {
    fn drop(&mut self) {
        // SAFETY: this is the `LoadLibraryExW` reference owned uniquely by the
        // guard and it is released exactly once.
        let _ = unsafe { FreeLibrary(self.module) };
    }
}

/// Resolved `ConPTY` entry points of one pinned module.
///
/// The required exports are non-optional, so a constructed backend is always
/// usable. `release` and `clear` describe optional capabilities.
#[derive(Debug)]
pub(super) struct ConptyApi {
    #[cfg(any(feature = "blocking", feature = "tokio", test))]
    pub(super) create: CreatePseudoConsoleFn,
    #[cfg(any(feature = "blocking", feature = "tokio", test))]
    pub(super) resize: ResizePseudoConsoleFn,
    #[cfg(any(feature = "blocking", feature = "tokio", test))]
    pub(super) close: ClosePseudoConsoleFn,
    pub(super) release: Option<ReleasePseudoConsoleFn>,
    pub(super) clear: Option<ClearPseudoConsoleFn>,
}

impl ConptyApi {
    /// Resolves every `ConPTY` entry point from `module`.
    ///
    /// # Safety
    ///
    /// `module` must remain mapped for as long as the returned table is used,
    /// and its exports must have the documented `ConPTY` signatures.
    pub(super) unsafe fn from_module(module: HMODULE) -> Result<Self, &'static str> {
        #[cfg(any(feature = "blocking", feature = "tokio", test))]
        let create = {
            // SAFETY: `module` is live by this function's contract.
            let create = unsafe { resolve_export(module, CREATE_PSEUDO_CONSOLE) }
                .ok_or(CREATE_PSEUDO_CONSOLE)?;
            // SAFETY: the resolved export has the documented SDK signature.
            unsafe { mem::transmute::<ProcAddress, CreatePseudoConsoleFn>(create) }
        };
        #[cfg(not(any(feature = "blocking", feature = "tokio", test)))]
        // SAFETY: `module` is live by this function's contract.
        if unsafe { resolve_export(module, CREATE_PSEUDO_CONSOLE) }.is_none() {
            return Err(CREATE_PSEUDO_CONSOLE);
        }

        #[cfg(any(feature = "blocking", feature = "tokio", test))]
        let resize = {
            // SAFETY: `module` is live by this function's contract.
            let resize = unsafe { resolve_export(module, RESIZE_PSEUDO_CONSOLE) }
                .ok_or(RESIZE_PSEUDO_CONSOLE)?;
            // SAFETY: the resolved export has the documented SDK signature.
            unsafe { mem::transmute::<ProcAddress, ResizePseudoConsoleFn>(resize) }
        };
        #[cfg(not(any(feature = "blocking", feature = "tokio", test)))]
        // SAFETY: `module` is live by this function's contract.
        if unsafe { resolve_export(module, RESIZE_PSEUDO_CONSOLE) }.is_none() {
            return Err(RESIZE_PSEUDO_CONSOLE);
        }

        #[cfg(any(feature = "blocking", feature = "tokio", test))]
        let close = {
            // SAFETY: `module` is live by this function's contract.
            let close = unsafe { resolve_export(module, CLOSE_PSEUDO_CONSOLE) }
                .ok_or(CLOSE_PSEUDO_CONSOLE)?;
            // SAFETY: the resolved export has the documented SDK signature.
            unsafe { mem::transmute::<ProcAddress, ClosePseudoConsoleFn>(close) }
        };
        #[cfg(not(any(feature = "blocking", feature = "tokio", test)))]
        // SAFETY: `module` is live by this function's contract.
        if unsafe { resolve_export(module, CLOSE_PSEUDO_CONSOLE) }.is_none() {
            return Err(CLOSE_PSEUDO_CONSOLE);
        }

        // SAFETY: `module` is live by this function's contract.
        let release = unsafe { resolve_export(module, RELEASE_PSEUDO_CONSOLE) };
        let release = release.map(|release| {
            // SAFETY: the resolved export has the documented SDK signature.
            unsafe { mem::transmute::<ProcAddress, ReleasePseudoConsoleFn>(release) }
        });

        let clear = if CLEAR_ABI_IS_CALLABLE {
            // SAFETY: `module` is live by this function's contract.
            let clear = unsafe { resolve_export(module, CLEAR_PSEUDO_CONSOLE) };
            clear.map(|clear| {
                // SAFETY: the resolved export has the ABI documented above
                // `CLEAR_ABI_IS_CALLABLE`.
                unsafe { mem::transmute::<ProcAddress, ClearPseudoConsoleFn>(clear) }
            })
        } else {
            None
        };

        Ok(Self {
            #[cfg(any(feature = "blocking", feature = "tokio", test))]
            create,
            #[cfg(any(feature = "blocking", feature = "tokio", test))]
            resize,
            #[cfg(any(feature = "blocking", feature = "tokio", test))]
            close,
            release,
            clear,
        })
    }

    #[cfg(test)]
    pub(super) fn without_release(&self) -> Self {
        Self {
            create: self.create,
            resize: self.resize,
            close: self.close,
            release: None,
            clear: self.clear,
        }
    }
}

/// Loads an external DLL by absolute path under a restricted search policy.
pub(super) fn load_module(dll: &Path) -> io::Result<ModuleGuard> {
    let path = wide_path(dll)?;

    // SAFETY: `path` is a NUL-terminated absolute UTF-16 path. The search flags
    // restrict dependencies to the DLL's directory and System32.
    let module =
        unsafe { LoadLibraryExW(path.as_ptr(), ptr::null_mut(), restricted_search_flags()) };
    if module.is_null() {
        return Err(io::Error::last_os_error());
    }

    Ok(ModuleGuard { module })
}

/// Converts a path to NUL-terminated UTF-16, rejecting interior NULs.
pub(super) fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains an interior NUL",
        ));
    }
    wide.push(0);
    Ok(wide)
}
