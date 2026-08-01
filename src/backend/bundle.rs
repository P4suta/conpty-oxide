// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Discovery and validation of standalone `conpty.dll` bundles.
//!
//! This module performs every filesystem and version-resource check before the
//! facade maps executable code. The DLL and `OpenConsole.exe` communicate over
//! a private protocol, so an unprovable pair is rejected rather than tried.

use std::env;
use std::ffi::c_void;
use std::fs;
use std::io;
use std::iter;
use std::mem::{size_of, transmute};
use std::path::{Path, PathBuf};
use std::ptr;
use std::slice;

use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use super::exports::{wide_path, ProcAddress};
use crate::error::BackendError;

pub(super) const CONPTY_DLL: &str = "conpty.dll";
pub(super) const OPEN_CONSOLE_EXE: &str = "OpenConsole.exe";
const PRODUCT_VERSION_KEY: &str = "ProductVersion";
pub(super) const UNKNOWN_VERSION: &str = "unknown";

const IMAGE_FILE_MACHINE_I386: u16 = 0x014c;
const IMAGE_FILE_MACHINE_AMD64: u16 = 0x8664;
const IMAGE_FILE_MACHINE_ARM64: u16 = 0xAA64;

type IsWow64Process2Fn = unsafe extern "system" fn(HANDLE, *mut u16, *mut u16) -> BOOL;

/// Filesystem result of validating one candidate bundle directory.
pub(super) struct ValidatedBundle {
    pub(super) dir: PathBuf,
    pub(super) dll: PathBuf,
}

/// Discovers and validates the DLL/console-host pair in `dir`.
pub(super) fn validate(dir: &Path, verify_pair: bool) -> Result<ValidatedBundle, BackendError> {
    let absolute = absolute_dir(dir)
        .map_err(|source| BackendError::dll_not_found(dir.to_path_buf(), source))?;
    let dll = absolute.join(CONPTY_DLL);

    let metadata = match fs::metadata(&dll) {
        Ok(metadata) => metadata,
        Err(source) => {
            return Err(BackendError::dll_not_found(absolute, source));
        },
    };
    if !metadata.is_file() {
        return Err(BackendError::dll_not_found(
            absolute,
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "conpty.dll is not a regular file",
            ),
        ));
    }

    let host = find_console_host(&absolute)
        .ok_or_else(|| BackendError::open_console_missing(dll.clone()))?;
    if verify_pair {
        check_version_pair(&dll, &host)?;
    }

    Ok(ValidatedBundle { dir: absolute, dll })
}

/// Returns the directory containing the running executable.
pub(super) fn exe_dir() -> Option<PathBuf> {
    env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

/// Records a rejected optional bundle before automatic selection falls back.
#[cfg(feature = "tracing")]
pub(super) fn log_rejected(dir: &Path, err: &BackendError) {
    tracing::warn!(
        dir = %dir.display(),
        error = %err,
        "ignoring the bundled conpty.dll; falling back to the system ConPTY"
    );
}

/// Discards a rejected optional bundle when diagnostics are disabled.
#[cfg(not(feature = "tracing"))]
pub(super) const fn log_rejected(_dir: &Path, _err: &BackendError) {}

/// Makes `dir` absolute while rejecting Windows drive-relative paths.
pub(super) fn absolute_dir(dir: &Path) -> io::Result<PathBuf> {
    let dir = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        env::current_dir()?.join(dir)
    };

    if !dir.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a drive-relative path resolves against that drive's current \
             directory, which this loader refuses to depend on",
        ));
    }

    Ok(dir)
}

/// Locates the `OpenConsole.exe` the DLL itself will search for.
pub(super) fn find_console_host(dir: &Path) -> Option<PathBuf> {
    let adjacent = dir.join(OPEN_CONSOLE_EXE);
    if adjacent.is_file() {
        return Some(adjacent);
    }

    let candidate = dir.join(native_arch_subdir()?).join(OPEN_CONSOLE_EXE);
    candidate.is_file().then_some(candidate)
}

/// Names the architecture subdirectory searched on the native machine.
pub(super) fn native_arch_subdir() -> Option<&'static str> {
    machine_arch_subdir(native_machine())
}

/// Maps an `IMAGE_FILE_MACHINE_*` value to the directory spelling used by
/// the standalone ConPTY package.
pub(super) const fn machine_arch_subdir(machine: u16) -> Option<&'static str> {
    match machine {
        IMAGE_FILE_MACHINE_AMD64 => Some("x64"),
        IMAGE_FILE_MACHINE_ARM64 => Some("arm64"),
        IMAGE_FILE_MACHINE_I386 => Some("x86"),
        _ => None,
    }
}

/// Reports the native `IMAGE_FILE_MACHINE_*` value.
fn native_machine() -> u16 {
    #[cfg(target_arch = "x86_64")]
    const COMPILED_FOR: u16 = IMAGE_FILE_MACHINE_AMD64;
    #[cfg(target_arch = "aarch64")]
    const COMPILED_FOR: u16 = IMAGE_FILE_MACHINE_ARM64;
    #[cfg(target_arch = "x86")]
    const COMPILED_FOR: u16 = IMAGE_FILE_MACHINE_I386;
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "x86")))]
    const COMPILED_FOR: u16 = 0;

    let module_name: Vec<u16> = "kernel32.dll".encode_utf16().chain(iter::once(0)).collect();
    // SAFETY: `module_name` is NUL-terminated and outlives the call.
    let module = unsafe { GetModuleHandleW(module_name.as_ptr()) };
    if module.is_null() {
        return COMPILED_FOR;
    }

    // SAFETY: kernel32 is live process-wide and the name is NUL-terminated.
    let Some(address) = (unsafe { GetProcAddress(module, b"IsWow64Process2\0".as_ptr()) }) else {
        return COMPILED_FOR;
    };
    // SAFETY: this kernel32 export has the signature represented by the alias.
    let is_wow64_process2 = unsafe { transmute::<ProcAddress, IsWow64Process2Fn>(address) };

    let mut process_machine = 0;
    let mut native = 0;
    // SAFETY: this returns the documented current-process pseudo-handle.
    let current_process = unsafe { GetCurrentProcess() };
    // SAFETY: the current-process pseudo-handle is valid, both outputs live,
    // and the function address was resolved from process-wide `kernel32.dll`.
    let ok = unsafe { is_wow64_process2(current_process, &mut process_machine, &mut native) };
    selected_native_machine(ok, native, COMPILED_FOR)
}

/// Selects the runtime-reported native machine after `IsWow64Process2`.
pub(super) const fn selected_native_machine(ok: BOOL, native: u16, compiled_for: u16) -> u16 {
    if ok == 0 {
        compiled_for
    } else {
        native
    }
}

/// Fails unless both files carry the same numeric `ProductVersion`.
fn check_version_pair(dll: &Path, host: &Path) -> Result<(), BackendError> {
    let dll_version = read_product_version(dll);
    let exe_version = read_product_version(host);
    if versions_are_compatible(dll_version.as_deref(), exe_version.as_deref()) {
        return Ok(());
    }

    Err(BackendError::version_mismatch(
        dll.to_path_buf(),
        dll_version.unwrap_or_else(|| UNKNOWN_VERSION.to_owned()),
        exe_version.unwrap_or_else(|| UNKNOWN_VERSION.to_owned()),
    ))
}

/// Decides whether a DLL/host version-resource pair may be used together.
pub(super) fn versions_are_compatible(dll: Option<&str>, host: Option<&str>) -> bool {
    match (dll.and_then(parse_version), host.and_then(parse_version)) {
        (Some(dll), Some(host)) => dll == host,
        _ => false,
    }
}

/// Parses the numeric prefix of a `ProductVersion` into four components.
///
/// A field carrying a non-numeric label contributes its leading digits and
/// then ends the version, so `1.24.1234-hotfix` and `1.24.9999-beta` stay
/// distinguishable instead of both truncating to `1.24`.
pub(super) fn parse_version(text: &str) -> Option<[u64; 4]> {
    let text = text.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    let mut parts = [0; 4];
    let mut seen = 0;
    for field in text.split('.') {
        if seen == parts.len() {
            break;
        }
        let field = field.trim();
        if let Ok(value) = field.parse::<u64>() {
            parts[seen] = value;
            seen += 1;
        } else {
            let digits = &field[..field.bytes().take_while(u8::is_ascii_digit).count()];
            if let Ok(value) = digits.parse::<u64>() {
                parts[seen] = value;
                seen += 1;
            }
            break;
        }
    }
    (seen > 0).then_some(parts)
}

/// Reads a file's `ProductVersion` string resource.
pub(super) fn read_product_version(path: &Path) -> Option<String> {
    let path = wide_path(path).ok()?;

    let mut ignored_handle = 0;
    // SAFETY: `path` is NUL-terminated and the out-parameter is live.
    let size = unsafe { GetFileVersionInfoSizeW(path.as_ptr(), &mut ignored_handle) };
    if size == 0 {
        return None;
    }

    // A u32 allocation preserves the alignment required by version records.
    let mut block: Vec<u32> = vec![0; (size as usize).div_ceil(size_of::<u32>())];
    // SAFETY: `block` has exactly the byte capacity reported for this resource.
    let read = unsafe { GetFileVersionInfoW(path.as_ptr(), 0, size, block.as_mut_ptr().cast()) };
    if read == 0 {
        return None;
    }

    // SAFETY: `block` was filled with a version resource by the API above.
    let (value, len) = unsafe { query_version_value(&block, "\\VarFileInfo\\Translation") }?;
    let count = translation_count(len);
    // SAFETY: the returned pointer and byte count point into live, aligned
    // version-resource storage.
    let translations = unsafe { slice::from_raw_parts(value.cast::<[u16; 2]>(), count) };

    for &[language, code_page] in translations {
        let sub_block =
            format!("\\StringFileInfo\\{language:04x}{code_page:04x}\\{PRODUCT_VERSION_KEY}");
        // SAFETY: `block` remains the live resource backing the returned value.
        let Some((value, len)) = (unsafe { query_version_value(&block, &sub_block) }) else {
            continue;
        };
        // SAFETY: string lengths from VerQueryValueW count UTF-16 code units.
        let text = unsafe { slice::from_raw_parts(value.cast::<u16>(), len as usize) };
        let text = String::from_utf16_lossy(text);
        let text = trim_resource_string(&text);
        if !text.is_empty() {
            return Some(text.to_owned());
        }
    }
    None
}

/// Converts the translation table's byte length to its element count.
pub(super) const fn translation_count(byte_len: u32) -> usize {
    byte_len as usize / size_of::<[u16; 2]>()
}

/// Removes padding used by version-resource strings.
pub(super) fn trim_resource_string(text: &str) -> &str {
    text.trim_matches(|c: char| c == '\0' || c.is_whitespace())
}

/// Queries a sub-block from a live version-resource allocation.
///
/// # Safety
///
/// `block` must contain data filled by `GetFileVersionInfoW`; the returned
/// pointer remains valid only while `block` is live.
unsafe fn query_version_value(block: &[u32], sub_block: &str) -> Option<(*const c_void, u32)> {
    let sub_block: Vec<u16> = sub_block.encode_utf16().chain(iter::once(0)).collect();
    let mut value: *mut c_void = ptr::null_mut();
    let mut len = 0;

    // SAFETY: guaranteed by this function's contract; all out-parameters and
    // the NUL-terminated query string are live for the call.
    let found = unsafe {
        VerQueryValueW(
            block.as_ptr().cast(),
            sub_block.as_ptr(),
            &mut value,
            &mut len,
        )
    };
    if found == 0 || value.is_null() || len == 0 {
        return None;
    }
    Some((value.cast_const(), len))
}
