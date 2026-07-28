//! Dynamic loading of the ConPTY entry points.
//!
//! The pseudoconsole API (`CreatePseudoConsole` and friends) is not linked
//! statically. It is resolved at run time with `GetProcAddress`, for two
//! reasons:
//!
//! 1. **Graceful degradation.** Linking `CreatePseudoConsole` statically makes
//!    the executable fail to start on Windows versions older than 10 1809
//!    (build 17763) with an unhelpful loader error. Resolving it dynamically
//!    turns that into a plain [`BackendError::Unsupported`].
//! 2. **Capability detection.** `ReleasePseudoConsole` only exists on Windows
//!    11 24H2 (build 26100) and later. Whether it is available decides which
//!    shutdown strategy the crate uses, and the presence of the export is the
//!    check microsoft/terminal recommends — *not* an OS build-number
//!    comparison, which misfires under compatibility shims and on backported
//!    builds (microsoft/terminal#19112).
//!
//! The same loader also serves a bundled `conpty.dll` in a later phase, which
//! is why symbol lookup goes through [`resolve_export`]: that DLL exports each
//! entry point twice, once under its canonical `Conpty`-prefixed name and once
//! under the bare system name.

use std::ffi::c_void;
use std::fmt;
use std::io;
use std::iter;
use std::mem;
use std::os::windows::io::{AsRawHandle, BorrowedHandle};
use std::path::PathBuf;
use std::ptr;
use std::sync::{Arc, OnceLock};

use windows_sys::core::HRESULT;
use windows_sys::Win32::Foundation::{HANDLE, HMODULE};
use windows_sys::Win32::System::Console::COORD;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

use crate::error::BackendError;
use crate::size::Size;

/// Handle to a pseudoconsole session, as returned by `CreatePseudoConsole`.
///
/// `HPCON` is an opaque pointer-sized value; the ConPTY headers declare it as
/// a distinct `HANDLE`-like type, and this alias mirrors that. Being a raw
/// pointer it is neither [`Send`] nor [`Sync`], which is deliberate: any type
/// that *owns* an `HPCON` has to state its own thread-safety argument. That
/// argument is not vacuous — `ClosePseudoConsole` must be called from a thread
/// other than the one reading the conout pipe, so an owner is required to be
/// [`Send`].
// Keep the Windows SDK spelling so the type greps against the C documentation.
#[allow(clippy::upper_case_acronyms)]
pub(crate) type HPCON = *mut c_void;

/// `PSEUDOCONSOLE_INHERIT_CURSOR`, for `CreatePseudoConsole`'s `dwFlags`.
///
/// Off by default everywhere in this crate, and no public API enables it
/// yet. The flag makes the pseudoconsole emit a Device
/// Status Report (`ESC [ 6 n`) on conout immediately after creation and stop
/// processing input entirely until the answer is written back to conin. A
/// caller that is not already pumping both pipes therefore deadlocks, and
/// there are unresolved reports of the flag hanging teardown as well
/// (microsoft/terminal#17688). Inheriting the cursor is not worth that.
pub(crate) const PSEUDOCONSOLE_INHERIT_CURSOR: u32 = 0x1;

/// Prefix under which the standalone `conpty.dll` exports its entry points.
const CONPTY_EXPORT_PREFIX: &str = "Conpty";

const CREATE_PSEUDO_CONSOLE: &str = "CreatePseudoConsole";
const RESIZE_PSEUDO_CONSOLE: &str = "ResizePseudoConsole";
const CLOSE_PSEUDO_CONSOLE: &str = "ClosePseudoConsole";
const RELEASE_PSEUDO_CONSOLE: &str = "ReleasePseudoConsole";

/// `HRESULT WINAPI CreatePseudoConsole(COORD, HANDLE, HANDLE, DWORD, HPCON*)`
type CreatePseudoConsoleFn =
    unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> HRESULT;

/// `HRESULT WINAPI ResizePseudoConsole(HPCON, COORD)`
type ResizePseudoConsoleFn = unsafe extern "system" fn(HPCON, COORD) -> HRESULT;

/// `void WINAPI ClosePseudoConsole(HPCON)`
type ClosePseudoConsoleFn = unsafe extern "system" fn(HPCON);

/// `HRESULT WINAPI ReleasePseudoConsole(HPCON)`
type ReleasePseudoConsoleFn = unsafe extern "system" fn(HPCON) -> HRESULT;

/// An untyped export address, as returned by `GetProcAddress`.
type ProcAddress = unsafe extern "system" fn() -> isize;

/// Resolves a ConPTY entry point from an already-loaded module.
///
/// The `Conpty`-prefixed alias is tried first, then the bare name.
/// microsoft/terminal's standalone `conpty.dll` exports both spellings, but
/// only the prefixed one is guaranteed across its releases; `kernel32.dll`
/// exports only the bare one. Trying the prefix first therefore prefers the
/// stable name when a bundled DLL is in play and falls through to the system
/// API otherwise.
///
/// # Safety
///
/// `module` must be a valid handle to a module that stays loaded for as long
/// as the returned address is used. The caller must transmute the result to
/// the signature that actually belongs to `name`.
unsafe fn resolve_export(module: HMODULE, name: &str) -> Option<ProcAddress> {
    debug_assert!(
        name.is_ascii() && !name.contains('\0'),
        "export names must be NUL-free ASCII"
    );

    /// Builds the NUL-terminated ANSI name `GetProcAddress` expects.
    fn c_name(prefix: &str, name: &str) -> Vec<u8> {
        let mut buf = Vec::with_capacity(prefix.len() + name.len() + 1);
        buf.extend_from_slice(prefix.as_bytes());
        buf.extend_from_slice(name.as_bytes());
        buf.push(0);
        buf
    }

    for prefix in [CONPTY_EXPORT_PREFIX, ""] {
        let symbol = c_name(prefix, name);
        // SAFETY: `module` is a live module handle per this function's
        // contract, and `symbol` is a NUL-terminated byte string that
        // outlives the call.
        if let Some(address) = unsafe { GetProcAddress(module, symbol.as_ptr()) } {
            return Some(address);
        }
    }

    None
}

/// The resolved ConPTY entry points of one module.
///
/// `release` is [`None`] on Windows versions that predate
/// `ReleasePseudoConsole`; the other three are mandatory.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ConptyApi {
    create: CreatePseudoConsoleFn,
    resize: ResizePseudoConsoleFn,
    close: ClosePseudoConsoleFn,
    release: Option<ReleasePseudoConsoleFn>,
}

impl ConptyApi {
    /// Resolves every ConPTY entry point from `module`.
    ///
    /// # Errors
    ///
    /// Returns the name of the first *required* export that could not be
    /// resolved, so the caller can report it as
    /// [`BackendError::MissingExport`] (bundled DLL) or
    /// [`BackendError::Unsupported`] (system API).
    ///
    /// # Safety
    ///
    /// `module` must be a valid handle to a module that stays loaded for as
    /// long as the returned `ConptyApi` is used, and its exports must have the
    /// signatures documented for the ConPTY API.
    unsafe fn from_module(module: HMODULE) -> Result<Self, &'static str> {
        // SAFETY: delegated to this function's contract. Each address is
        // transmuted to the signature Microsoft documents for that export,
        // which the caller warrants matches the module's actual exports.
        unsafe {
            let create = resolve_export(module, CREATE_PSEUDO_CONSOLE)
                .ok_or(CREATE_PSEUDO_CONSOLE)
                .map(|f| mem::transmute::<ProcAddress, CreatePseudoConsoleFn>(f))?;
            let resize = resolve_export(module, RESIZE_PSEUDO_CONSOLE)
                .ok_or(RESIZE_PSEUDO_CONSOLE)
                .map(|f| mem::transmute::<ProcAddress, ResizePseudoConsoleFn>(f))?;
            let close = resolve_export(module, CLOSE_PSEUDO_CONSOLE)
                .ok_or(CLOSE_PSEUDO_CONSOLE)
                .map(|f| mem::transmute::<ProcAddress, ClosePseudoConsoleFn>(f))?;
            let release = resolve_export(module, RELEASE_PSEUDO_CONSOLE)
                .map(|f| mem::transmute::<ProcAddress, ReleasePseudoConsoleFn>(f));

            Ok(Self {
                create,
                resize,
                close,
                release,
            })
        }
    }
}

/// Which ConPTY implementation a [`ConPtyBackend`] is bound to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BackendKind {
    /// The ConPTY API built into the operating system (`kernel32.dll`).
    System,

    /// A standalone `conpty.dll` loaded from the given path.
    ///
    /// A bundled DLL ships a newer console host than the operating system
    /// provides, which is how an application can get `ReleasePseudoConsole`
    /// semantics on a Windows version that does not have them natively.
    External {
        /// Path of the loaded `conpty.dll`.
        dll: PathBuf,
    },
}

/// The shared, immutable state behind a [`ConPtyBackend`].
#[derive(Debug)]
struct BackendInner {
    kind: BackendKind,
    api: ConptyApi,
}

/// A loaded ConPTY implementation.
///
/// Cloning is cheap: clones share one [`Arc`], so resolving the entry points
/// happens once per backend rather than once per pseudoconsole.
///
/// # Thread safety
///
/// `ConPtyBackend` is `Send + Sync` (auto-derived), and that is sound:
///
/// - [`BackendInner`] holds only a [`BackendKind`] — a unit variant or a
///   [`PathBuf`] — and a [`ConptyApi`] of bare `extern "system"` function
///   pointers. Function pointers are `Send + Sync`: they are immutable code
///   addresses, not resources.
/// - The backend owns no `HPCON`, no OS handle, and no interior mutability,
///   so a shared `&ConPtyBackend` exposes nothing mutable.
/// - The module the addresses point into stays loaded for the lifetime of the
///   backend: the system backend targets `kernel32.dll`, which is mapped into
///   every Win32 process and never unloaded.
/// - `Send` is not merely convenient but required. `ClosePseudoConsole` must
///   not be called from the thread reading the conout pipe, so the shutdown
///   path necessarily runs on a different thread from the reader and both need
///   the backend.
pub struct ConPtyBackend {
    inner: Arc<BackendInner>,
}

/// Process-wide default installed by [`ConPtyBackend::set_global_default`].
static GLOBAL_DEFAULT: OnceLock<ConPtyBackend> = OnceLock::new();

impl ConPtyBackend {
    /// Loads the ConPTY API built into the operating system.
    ///
    /// Resolves the entry points from the already-mapped `kernel32.dll`; no
    /// library is loaded and no reference count is taken, because
    /// `kernel32.dll` is present in every Win32 process for its entire
    /// lifetime.
    ///
    /// # Errors
    ///
    /// Returns [`BackendError::Unsupported`] when `CreatePseudoConsole`,
    /// `ResizePseudoConsole`, or `ClosePseudoConsole` is missing, i.e. on
    /// Windows versions older than 10 1809 (build 17763).
    pub fn system() -> Result<Self, BackendError> {
        // `windows-sys` has no `w!` macro, so the module name is widened at
        // run time. This happens once per backend, not per pseudoconsole.
        let module_name: Vec<u16> = "kernel32.dll".encode_utf16().chain(iter::once(0)).collect();

        // SAFETY: `module_name` is a NUL-terminated UTF-16 string that
        // outlives the call.
        let module = unsafe { GetModuleHandleW(module_name.as_ptr()) };
        if module.is_null() {
            // Unreachable in practice: kernel32.dll is mapped into every
            // Win32 process. Report it as "no ConPTY here" rather than
            // panicking on a hostile or exotic environment.
            return Err(BackendError::Unsupported);
        }

        // SAFETY: `module` is a live handle to kernel32.dll, which stays
        // loaded for the lifetime of the process, and its ConPTY exports have
        // the signatures documented on Microsoft Learn.
        let api =
            unsafe { ConptyApi::from_module(module) }.map_err(|_| BackendError::Unsupported)?;

        Ok(Self {
            inner: Arc::new(BackendInner {
                kind: BackendKind::System,
                api,
            }),
        })
    }

    /// Returns which ConPTY implementation this backend is bound to.
    #[must_use]
    pub fn kind(&self) -> &BackendKind {
        &self.inner.kind
    }

    /// Returns whether this backend exports `ReleasePseudoConsole`.
    ///
    /// When `true`, the crate can relinquish the `HPCON` right after spawning
    /// and let the pseudoconsole exit on its own once every client has
    /// disconnected; conout then reaches end-of-file naturally. When `false`,
    /// end-of-file has to be forced by closing the pseudoconsole after the
    /// child exits, because the console host outlives the child.
    ///
    /// The value depends on the operating system (`ReleasePseudoConsole`
    /// requires Windows 11 24H2 / Server 2025, build 26100) or on the version
    /// of a bundled `conpty.dll`.
    #[must_use]
    pub fn supports_release(&self) -> bool {
        self.inner.api.release.is_some()
    }

    /// Installs this backend as the process-wide default.
    ///
    /// Pseudoconsoles created without an explicit backend use it. The first
    /// call wins and later calls are ignored, so install the default once
    /// during start-up, before creating any pseudoconsole.
    pub fn set_global_default(self) {
        // `OnceLock::set` fails only when a default is already installed, and
        // "first call wins" is the documented behaviour.
        let _ = GLOBAL_DEFAULT.set(self);
    }

    /// Returns the backend to use when the caller did not name one.
    ///
    /// This is the process-wide default from [`Self::set_global_default`] if
    /// one was installed, otherwise a freshly loaded [`Self::system`].
    ///
    /// # Errors
    ///
    /// Propagates [`Self::system`]'s error when no default is installed.
    pub(crate) fn resolve_default() -> Result<Self, BackendError> {
        match GLOBAL_DEFAULT.get() {
            Some(backend) => Ok(backend.clone()),
            None => Self::system(),
        }
    }

    /// Calls `CreatePseudoConsole`.
    ///
    /// `input_read` is the read end of the conin pipe and `output_write` the
    /// write end of the conout pipe; both must be synchronous handles, which
    /// anonymous pipes always are. ConPTY duplicates them, so the caller
    /// should close its own copies as soon as the child has been spawned —
    /// until then the extra references keep conout from ever reaching
    /// end-of-file.
    ///
    /// The returned `HPCON` is *not* owned by any RAII type here; the caller
    /// must eventually pass it to [`Self::close`].
    ///
    /// # Errors
    ///
    /// Returns the failing `HRESULT` mapped to an [`io::Error`].
    pub(crate) fn create(
        &self,
        size: Size,
        input_read: BorrowedHandle<'_>,
        output_write: BorrowedHandle<'_>,
        flags: u32,
    ) -> io::Result<HPCON> {
        let (rows, cols) = size.to_i16_pair();
        let size = COORD { X: cols, Y: rows };
        let mut hpc: HPCON = ptr::null_mut();

        // SAFETY: `self.inner.api.create` was resolved from a loaded module
        // that outlives this backend. Both handles are borrowed for the
        // duration of the call, and `hpc` is a valid out-parameter.
        let hr = unsafe {
            (self.inner.api.create)(
                size,
                input_read.as_raw_handle(),
                output_write.as_raw_handle(),
                flags,
                &mut hpc,
            )
        };
        hresult_ok(hr)?;

        Ok(hpc)
    }

    /// Calls `ResizePseudoConsole`.
    ///
    /// # Errors
    ///
    /// Returns the failing `HRESULT` mapped to an [`io::Error`].
    ///
    /// # Safety
    ///
    /// `hpc` must be a live handle from [`Self::create`] on *this* backend
    /// that has not yet been passed to [`Self::close`].
    pub(crate) unsafe fn resize(&self, hpc: HPCON, size: Size) -> io::Result<()> {
        let (rows, cols) = size.to_i16_pair();
        let size = COORD { X: cols, Y: rows };

        // SAFETY: `hpc` is live per this function's contract, and the
        // function pointer was resolved from a module that outlives the
        // backend.
        let hr = unsafe { (self.inner.api.resize)(hpc, size) };
        hresult_ok(hr)
    }

    /// Calls `ClosePseudoConsole`, releasing the session's resources.
    ///
    /// This returns no status because `ClosePseudoConsole` returns `void`.
    ///
    /// # Safety
    ///
    /// `hpc` must be a live handle from [`Self::create`] on *this* backend
    /// that has not been closed before; the handle is invalid afterwards.
    ///
    /// Beyond memory safety, two liveness rules from the ConPTY documentation
    /// apply, and violating them hangs the process rather than corrupting it:
    ///
    /// - Before Windows 11 24H2 (build 26100), this call waits until every
    ///   client has disconnected. The caller must therefore have closed its
    ///   conout read end first, or keep another thread draining it.
    /// - It must never be called from the thread that reads conout, because
    ///   that thread is exactly the one that would have to make progress for
    ///   the call to return.
    pub(crate) unsafe fn close(&self, hpc: HPCON) {
        // SAFETY: `hpc` is live and unclosed per this function's contract.
        unsafe { (self.inner.api.close)(hpc) }
    }

    /// Calls `ReleasePseudoConsole`, or returns [`None`] if the backend does
    /// not export it (see [`Self::supports_release`]).
    ///
    /// Releasing hands ownership of the session to the pseudoconsole itself:
    /// once every client has disconnected, the console host exits on its own
    /// and conout fails with `ERROR_BROKEN_PIPE`, which the reader maps to
    /// end-of-file. That breaks the ownership cycle in which the application
    /// waits for the session to end while the session waits for the
    /// application to close it.
    ///
    /// Releasing does **not** free the `HPCON`: [`Self::close`] must still be
    /// called afterwards to reclaim it.
    ///
    /// # Errors
    ///
    /// Returns `Some(Err(..))` with the failing `HRESULT` mapped to an
    /// [`io::Error`]. Microsoft documents `E_INVALIDARG` as the only expected
    /// failure.
    ///
    /// # Safety
    ///
    /// `hpc` must be a live handle from [`Self::create`] on *this* backend
    /// that has not yet been passed to [`Self::close`].
    #[must_use]
    pub(crate) unsafe fn release(&self, hpc: HPCON) -> Option<io::Result<()>> {
        let release = self.inner.api.release?;

        // SAFETY: `hpc` is live per this function's contract, and the
        // function pointer was resolved from a module that outlives the
        // backend.
        let hr = unsafe { release(hpc) };
        Some(hresult_ok(hr))
    }
}

impl Clone for ConPtyBackend {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// Prints the backend's identity rather than raw function addresses, which
/// are noise and vary between runs.
impl fmt::Debug for ConPtyBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConPtyBackend")
            .field("kind", &self.inner.kind)
            .field("supports_release", &self.supports_release())
            .finish()
    }
}

/// Turns an `HRESULT` into a [`io::Result`], failing on `FAILED(hr)`.
fn hresult_ok(hr: HRESULT) -> io::Result<()> {
    if hr >= 0 {
        Ok(())
    } else {
        Err(hresult_to_io_error(hr))
    }
}

/// Converts a failed `HRESULT` into an [`io::Error`].
///
/// `HRESULT_FROM_WIN32` wraps a plain Win32 error code as `0x8007_xxxx`.
/// Unwrapping that back to the bare code is what lets [`io::Error`] classify
/// it (`ERROR_ACCESS_DENIED` becomes [`io::ErrorKind::PermissionDenied`]
/// rather than an unrecognised code) and format a readable message. Any other
/// facility is passed through unchanged, which at least keeps the exact
/// `HRESULT` visible in the error's `Display`.
fn hresult_to_io_error(hr: HRESULT) -> io::Error {
    /// Mask selecting the severity and facility bits of an `HRESULT`.
    const FACILITY_MASK: u32 = 0xFFFF_0000;
    /// Severity `FAILED` plus `FACILITY_WIN32`, i.e. an `HRESULT_FROM_WIN32`.
    const FAILED_FACILITY_WIN32: u32 = 0x8007_0000;

    let bits = hr as u32;
    if bits & FACILITY_MASK == FAILED_FACILITY_WIN32 {
        io::Error::from_raw_os_error((bits & 0xFFFF) as i32)
    } else {
        io::Error::from_raw_os_error(hr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::windows::io::AsHandle;

    use crate::core::pipes::create_sync_pipes;

    #[test]
    fn system_backend_loads_on_this_machine() {
        let backend = ConPtyBackend::system().expect("ConPTY must be available on a test machine");
        assert_eq!(backend.kind(), &BackendKind::System);
    }

    #[test]
    fn supports_release_answers_without_panicking() {
        let backend = ConPtyBackend::system().expect("ConPTY must be available");
        // The value is OS dependent (Windows 11 24H2 / build 26100 and later
        // export `ReleasePseudoConsole`), so only the query is asserted.
        let _: bool = backend.supports_release();
    }

    #[test]
    fn clones_share_one_resolved_api() {
        let backend = ConPtyBackend::system().expect("ConPTY must be available");
        let clone = backend.clone();
        assert_eq!(clone.kind(), backend.kind());
        assert_eq!(clone.supports_release(), backend.supports_release());
    }

    #[test]
    fn debug_shows_kind_and_release_support() {
        let backend = ConPtyBackend::system().expect("ConPTY must be available");
        let rendered = format!("{backend:?}");
        assert!(rendered.contains("ConPtyBackend"), "{rendered}");
        assert!(rendered.contains("System"), "{rendered}");
        assert!(rendered.contains("supports_release"), "{rendered}");
    }

    #[test]
    fn resolve_default_yields_a_usable_backend() {
        // Whether or not another test has installed a global default, the
        // only backend obtainable in this process is the system one.
        let backend = ConPtyBackend::resolve_default().expect("ConPTY must be available");
        assert_eq!(backend.kind(), &BackendKind::System);
    }

    #[test]
    fn set_global_default_keeps_the_first_backend() {
        ConPtyBackend::system()
            .expect("ConPTY must be available")
            .set_global_default();
        // A second install is ignored rather than panicking.
        ConPtyBackend::system()
            .expect("ConPTY must be available")
            .set_global_default();
        assert_eq!(
            ConPtyBackend::resolve_default()
                .expect("a default is installed")
                .kind(),
            &BackendKind::System
        );
    }

    #[test]
    fn bare_export_names_resolve_from_kernel32() {
        let name: Vec<u16> = "kernel32.dll".encode_utf16().chain(iter::once(0)).collect();
        // SAFETY: NUL-terminated UTF-16 string that outlives the call.
        let module = unsafe { GetModuleHandleW(name.as_ptr()) };
        assert!(!module.is_null());

        // kernel32 exports the bare names only, so this also proves the
        // `Conpty`-prefixed attempt falls through instead of failing.
        // SAFETY: `module` is kernel32.dll, which is always loaded.
        assert!(unsafe { resolve_export(module, CREATE_PSEUDO_CONSOLE) }.is_some());
        // SAFETY: as above.
        assert!(unsafe { resolve_export(module, "ThisExportDoesNotExist") }.is_none());
    }

    #[test]
    fn hresult_ok_accepts_success_codes() {
        assert!(hresult_ok(0).is_ok()); // S_OK
        assert!(hresult_ok(1).is_ok()); // S_FALSE is a success code
        assert!(hresult_ok(i32::MAX).is_ok());
    }

    #[test]
    fn hresult_win32_facility_unwraps_to_the_os_error() {
        // HRESULT_FROM_WIN32(ERROR_ACCESS_DENIED)
        let err = hresult_to_io_error(0x8007_0005_u32 as i32);
        assert_eq!(err.raw_os_error(), Some(5));
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);

        // E_INVALIDARG is HRESULT_FROM_WIN32(ERROR_INVALID_PARAMETER)
        let err = hresult_to_io_error(0x8007_0057_u32 as i32);
        assert_eq!(err.raw_os_error(), Some(87));
    }

    #[test]
    fn hresult_other_facilities_pass_through() {
        // E_NOTIMPL lives in FACILITY_NULL, so there is nothing to unwrap and
        // the raw code must survive verbatim.
        let hr = 0x8000_4001_u32 as i32;
        assert_eq!(hresult_to_io_error(hr).raw_os_error(), Some(hr));

        // A success code must never be handed to the converter, but if it is,
        // it still round-trips rather than being misread as FACILITY_WIN32.
        assert_eq!(
            hresult_to_io_error(0x0001_0000).raw_os_error(),
            Some(0x1_0000)
        );
    }

    /// Exercises the full wrapper against the real ConPTY API.
    ///
    /// The teardown order is the one the documentation requires. Every pipe
    /// end is closed *before* `ClosePseudoConsole`, including the conout read
    /// end: on Windows versions before 24H2 that call waits until all clients
    /// have disconnected, so leaving conout open (and unread, as here — no
    /// child is ever spawned) would hang the test. Reading conout to
    /// end-of-file instead would be the other documented option, but it would
    /// hang for the same reason, since only `ClosePseudoConsole` can end the
    /// clientless session.
    #[test]
    fn create_resize_close_round_trip() {
        let backend = ConPtyBackend::system().expect("ConPTY must be available");
        let pipes = create_sync_pipes().expect("creating pipes must succeed");

        let hpc = backend
            .create(
                Size::new(24, 80),
                pipes.conin_read.as_handle(),
                pipes.conout_write.as_handle(),
                0,
            )
            .expect("CreatePseudoConsole must succeed");
        assert!(!hpc.is_null());

        // SAFETY: `hpc` is live and was created by this backend.
        unsafe { backend.resize(hpc, Size::new(50, 132)) }
            .expect("ResizePseudoConsole must succeed");

        drop(pipes);

        // SAFETY: `hpc` is live, was created by this backend, and has not been
        // closed. The conout read end is already closed and this thread is not
        // reading it, so neither liveness rule is violated.
        unsafe { backend.close(hpc) };
    }
}
