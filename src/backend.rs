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
//!    11 24H2 (build 26100) and later, and `ClearPseudoConsole` exists only in
//!    the standalone `conpty.dll`. Whether they are available decides which
//!    shutdown strategy the crate uses and which operations it can offer, and
//!    the presence of the export is the check microsoft/terminal recommends —
//!    *not* an OS build-number comparison, which misfires under compatibility
//!    shims and on backported builds (microsoft/terminal#19112).
//!
//! The same loader serves a bundled `conpty.dll`, which is why symbol lookup
//! goes through [`resolve_export`]: that DLL exports each entry point twice,
//! once under its canonical `Conpty`-prefixed name and once under the bare
//! system name.
//!
//! # Loading a bundled `conpty.dll`
//!
//! [`ConPtyBackend::from_dir`] takes a directory and does four things before
//! any code from it runs:
//!
//! 1. It checks that `conpty.dll` is there at all.
//! 2. It locates the `OpenConsole.exe` the DLL will launch — next to the DLL,
//!    or in the architecture subdirectory the DLL itself searches.
//! 3. It compares the two files' `ProductVersion` resources. The DLL and the
//!    console host speak a private, versioned protocol, and a bad ConPTY
//!    bundle takes the client process down with a FailFast rather than an
//!    error (wezterm#7774), so it is far better to refuse the bundle than to
//!    crash later.
//! 4. It loads the DLL by absolute path with `LoadLibraryExW` and a search
//!    policy that never consults `PATH`, the current directory, or the
//!    registry, so a stray `conpty.dll` cannot be planted into the process.
//!
//! [`ConPtyBackend::auto`] applies that to the executable's own directory and
//! silently falls back to the operating system's ConPTY, which is what an
//! application that merely *may* ship a bundle wants.

use std::env;
use std::ffi::c_void;
use std::fmt;
use std::fs;
use std::io;
use std::iter;
use std::mem;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, BorrowedHandle};
use std::path::{Path, PathBuf};
use std::ptr;
use std::slice;
use std::sync::{Arc, OnceLock};

use windows_sys::core::{BOOL, HRESULT};
use windows_sys::Win32::Foundation::{FreeLibrary, HANDLE, HMODULE};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use windows_sys::Win32::System::Console::COORD;
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleHandleW, GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
    LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

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
/// Off by default in both front ends; `PtyBuilder::inherit_cursor` opts in,
/// and its documentation carries the caveats in full. The flag makes the
/// pseudoconsole emit a Device
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
const CLEAR_PSEUDO_CONSOLE: &str = "ClearPseudoConsole";

/// File name of the standalone pseudoconsole implementation.
const CONPTY_DLL: &str = "conpty.dll";

/// File name of the console host `conpty.dll` launches.
const OPEN_CONSOLE_EXE: &str = "OpenConsole.exe";

/// Name of the version-resource string the bundle check compares.
const PRODUCT_VERSION_KEY: &str = "ProductVersion";

/// Placeholder used in [`BackendError::VersionMismatch`] when a file carries
/// no readable `ProductVersion`.
const UNKNOWN_VERSION: &str = "unknown";

/// `IMAGE_FILE_MACHINE_*` values naming the machine architectures
/// `conpty.dll` knows a console-host subdirectory for.
///
/// Defined locally rather than pulled from `windows-sys`: they are the only
/// things the `Win32_System_SystemInformation` feature would be enabled for.
const IMAGE_FILE_MACHINE_I386: u16 = 0x014c;
const IMAGE_FILE_MACHINE_AMD64: u16 = 0x8664;
const IMAGE_FILE_MACHINE_ARM64: u16 = 0xAA64;

/// Whether `ClearPseudoConsole` may be called on this target at all.
///
/// microsoft/terminal changed the export's signature: it used to be
/// `ConptyClearPseudoConsole(HPCON)` and since "Preserve the cursor row during
/// Clear Buffer" (microsoft/terminal#18976) it is
/// `ConptyClearPseudoConsole(HPCON, BOOL keepCursorRow)`. Both spellings are
/// exported under the same undecorated name, so the arity cannot be probed.
///
/// On x86-64 and AArch64 that is harmless: the extra argument travels in a
/// register that an older one-argument build simply ignores, and the caller
/// owns the stack. On x86 `WINAPI` is `__stdcall`, where the *callee* pops the
/// arguments — calling the wrong arity there corrupts the stack pointer. The
/// capability is therefore reported as absent on x86 rather than gambling on
/// which build is loaded.
const CLEAR_ABI_IS_CALLABLE: bool = !cfg!(target_arch = "x86");

/// `HRESULT WINAPI CreatePseudoConsole(COORD, HANDLE, HANDLE, DWORD, HPCON*)`
type CreatePseudoConsoleFn =
    unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> HRESULT;

/// `HRESULT WINAPI ResizePseudoConsole(HPCON, COORD)`
type ResizePseudoConsoleFn = unsafe extern "system" fn(HPCON, COORD) -> HRESULT;

/// `void WINAPI ClosePseudoConsole(HPCON)`
type ClosePseudoConsoleFn = unsafe extern "system" fn(HPCON);

/// `HRESULT WINAPI ReleasePseudoConsole(HPCON)`
type ReleasePseudoConsoleFn = unsafe extern "system" fn(HPCON) -> HRESULT;

/// `HRESULT WINAPI ConptyClearPseudoConsole(HPCON, BOOL keepCursorRow)`
///
/// See [`CLEAR_ABI_IS_CALLABLE`] for why the two-argument spelling is the one
/// this crate uses.
type ClearPseudoConsoleFn = unsafe extern "system" fn(HPCON, BOOL) -> HRESULT;

/// An untyped export address, as returned by `GetProcAddress`.
type ProcAddress = unsafe extern "system" fn() -> isize;

/// `BOOL WINAPI IsWow64Process2(HANDLE, USHORT*, USHORT*)`
///
/// Resolved dynamically by [`native_machine`]; see there for why it is not
/// linked statically.
type IsWow64Process2Fn = unsafe extern "system" fn(HANDLE, *mut u16, *mut u16) -> BOOL;

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

/// Keeps a dynamically loaded module mapped for as long as its exports may be
/// called.
///
/// A [`ConptyApi`] is nothing but raw code addresses, and nothing in the type
/// system ties them to the module they were resolved from. If the module were
/// unloaded — which for an external DLL is one stray `FreeLibrary` away — every
/// one of those pointers would dangle and calling them would be undefined
/// behaviour. Holding this guard next to the addresses makes the mapping
/// outlive them by construction.
#[derive(Debug)]
struct ModuleGuard {
    module: HMODULE,
}

// SAFETY: `module` is an opaque module base address, not a thread-affine
// resource: `GetProcAddress`, `LoadLibraryExW`, and `FreeLibrary` are all
// documented as thread-safe, and this guard exposes no mutation. The reference
// it owns is released exactly once, in `Drop`, from whichever thread drops the
// last `Arc`.
unsafe impl Send for ModuleGuard {}
// SAFETY: see above; the guard has no interior mutability at all.
unsafe impl Sync for ModuleGuard {}

/// Releases the module reference taken when the backend was loaded.
impl Drop for ModuleGuard {
    fn drop(&mut self) {
        // SAFETY: `module` is the handle `LoadLibraryExW` returned for this
        // guard, it has not been freed before (only `Drop` frees it, and it
        // runs once), and every function pointer resolved from it lives in the
        // same `BackendInner` that owns this guard.
        let _ = unsafe { FreeLibrary(self.module) };
    }
}

/// The resolved ConPTY entry points of one module.
///
/// `release` and `clear` are [`None`] where the module does not export them;
/// the other three are mandatory.
///
/// This type is deliberately neither [`Copy`] nor [`Clone`]. The addresses are
/// only valid while the module they came from stays mapped, and that mapping
/// is pinned by the [`ModuleGuard`] stored alongside them in [`BackendInner`].
/// Making the table trivially copyable would let the pointers escape that pin
/// with nothing to notice.
#[derive(Debug)]
pub(crate) struct ConptyApi {
    create: CreatePseudoConsoleFn,
    resize: ResizePseudoConsoleFn,
    close: ClosePseudoConsoleFn,
    release: Option<ReleasePseudoConsoleFn>,
    clear: Option<ClearPseudoConsoleFn>,
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
            let clear = if CLEAR_ABI_IS_CALLABLE {
                resolve_export(module, CLEAR_PSEUDO_CONSOLE)
                    .map(|f| mem::transmute::<ProcAddress, ClearPseudoConsoleFn>(f))
            } else {
                None
            };

            Ok(Self {
                create,
                resize,
                close,
                release,
                clear,
            })
        }
    }

    /// Returns a copy of this table with `release` removed.
    ///
    /// Copying the addresses out is sound only because the caller stores the
    /// result next to a clone of the same [`ModuleGuard`]; see
    /// [`ConPtyBackend::without_release`].
    fn without_release(&self) -> Self {
        Self {
            create: self.create,
            resize: self.resize,
            close: self.close,
            release: None,
            clear: self.clear,
        }
    }
}

/// Which ConPTY implementation a [`ConPtyBackend`] is bound to.
///
/// Marked `#[non_exhaustive]`, like the crate's error enums: new kinds may be
/// added in later releases, so matches on it need a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
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

    /// The resolved entry points, or [`None`] for the inert backend
    /// [`ConPtyBackend::auto`] returns when no ConPTY implementation could be
    /// loaded at all.
    api: Option<ConptyApi>,

    /// Pins the module `api` was resolved from.
    ///
    /// [`None`] for the system backend: `kernel32.dll` is mapped into every
    /// Win32 process for its entire lifetime, so there is nothing to pin and no
    /// reference to release.
    module: Option<Arc<ModuleGuard>>,
}

/// A loaded ConPTY implementation.
///
/// Cloning is cheap: clones share one [`Arc`], so resolving the entry points
/// happens once per backend rather than once per pseudoconsole.
///
/// # Thread safety
///
/// `ConPtyBackend` is `Send + Sync`, and that is sound:
///
/// - `BackendInner` holds a [`BackendKind`] — a unit variant or a
///   [`PathBuf`] — a table of bare `extern "system"` function pointers, and a
///   module guard. Function pointers are `Send + Sync`: they are immutable code
///   addresses, not resources. The guard states its own argument.
/// - The backend owns no `HPCON`, no OS handle, and no interior mutability,
///   so a shared `&ConPtyBackend` exposes nothing mutable.
/// - The module the addresses point into stays loaded for the lifetime of the
///   backend: the system backend targets `kernel32.dll`, which is mapped into
///   every Win32 process and never unloaded, and an external backend owns a
///   `LoadLibraryExW` reference that is released only when the last clone is
///   dropped.
/// - `Send` is not merely convenient but required. `ClosePseudoConsole` must
///   not be called from the thread reading the conout pipe, so the shutdown
///   path necessarily runs on a different thread from the reader and both need
///   the backend.
pub struct ConPtyBackend {
    inner: Arc<BackendInner>,
}

/// Process-wide default installed by [`ConPtyBackend::set_global_default`].
static GLOBAL_DEFAULT: OnceLock<ConPtyBackend> = OnceLock::new();

/// Cached result of [`ConPtyBackend::auto`], used when no global default was
/// installed.
///
/// Caching matters for more than speed: it keeps a bundled `conpty.dll` loaded
/// once per process instead of once per session.
static AUTO_DEFAULT: OnceLock<ConPtyBackend> = OnceLock::new();

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
                api: Some(api),
                // kernel32.dll needs no pin; see `BackendInner::module`.
                module: None,
            }),
        })
    }

    /// Loads a bundled `conpty.dll` from `dir`, validating the bundle first.
    ///
    /// A bundle is `conpty.dll` plus the `OpenConsole.exe` it launches, as
    /// shipped by the `Microsoft.Windows.Console.ConPTY` NuGet package. Both
    /// must come from the same package: the DLL and the console host share a
    /// private protocol with no compatibility promise across releases, and a
    /// bad ConPTY bundle crashes the client process rather than degrading —
    /// wezterm#7774 is PowerShell dying with a `0x8013_1623` FailFast until
    /// the bundle was replaced. This constructor therefore refuses a pair it
    /// cannot prove consistent — use [`Self::from_dir_unchecked`] to override
    /// that.
    ///
    /// Note the check's limit: it proves the pair *matches*, not that it is
    /// current. wezterm#7774's actual configuration was a matched but outdated
    /// pair, which this validation accepts; keeping the bundled version up to
    /// date remains the application's responsibility.
    ///
    /// The console host is looked for exactly where `conpty.dll` itself will
    /// look: next to the DLL first, then in the single subdirectory named
    /// after the machine's *native* architecture (`x64`, `arm64`, or `x86`).
    /// A host anywhere else — a cross-architecture subdirectory, say — does
    /// not count, because the DLL never searches there and would silently run
    /// every session against the operating system's inbox `conhost.exe`
    /// instead of the file this constructor validated. Placing
    /// `OpenConsole.exe` next to the DLL, as `scripts/fetch-conpty.ps1` does,
    /// is the recommended layout.
    ///
    /// A relative `dir` is resolved against the current working directory once,
    /// here. The DLL is then loaded by absolute path with a search policy that
    /// excludes `PATH`, the current directory, the application directory, and
    /// the registry, so nothing but `dir` and `System32` can satisfy the load.
    /// A *drive-relative* `dir` (`C:dir`) is rejected as
    /// [`BackendError::DllNotFound`]: it names a path relative to that drive's
    /// own current directory, which cannot be resolved once and pinned.
    ///
    /// # Errors
    ///
    /// - [`BackendError::DllNotFound`] if `dir/conpty.dll` is missing or
    ///   cannot be loaded (the source carries the OS error, e.g.
    ///   `ERROR_BAD_EXE_FORMAT` for a file that is not a DLL at all).
    /// - [`BackendError::OpenConsoleMissing`] if no `OpenConsole.exe`
    ///   accompanies the DLL.
    /// - [`BackendError::VersionMismatch`] if the two files report different
    ///   `ProductVersion` resources, or if either version cannot be read.
    /// - [`BackendError::MissingExport`] if the DLL lacks
    ///   `CreatePseudoConsole`, `ResizePseudoConsole`, or
    ///   `ClosePseudoConsole`.
    pub fn from_dir(dir: impl AsRef<Path>) -> Result<Self, BackendError> {
        Self::load_from_dir(dir.as_ref(), true)
    }

    /// Loads a bundled `conpty.dll` from `dir` **without** checking that it
    /// matches its `OpenConsole.exe`.
    ///
    /// Every other check [`Self::from_dir`] performs still runs; only the
    /// version comparison is skipped.
    ///
    /// # Why this is dangerous
    ///
    /// `conpty.dll` and `OpenConsole.exe` communicate over a private, versioned
    /// protocol and are shipped as a pair for that reason. Running a DLL
    /// against a console host from a different release is not a graceful
    /// degradation: the failure mode of a bad ConPTY bundle is a hard crash of
    /// the *client* process — in wezterm#7774, PowerShell dies with a
    /// `0x8013_1623` FailFast — at an arbitrary later point, far from this
    /// call.
    ///
    /// Use this only when the version resources are unreadable for a reason you
    /// control, for example a locally rebuilt `OpenConsole.exe` that carries no
    /// version stamp, and you can guarantee the pair by other means. Prefer
    /// [`Self::from_dir`] everywhere else.
    ///
    /// # Errors
    ///
    /// The same as [`Self::from_dir`], minus
    /// [`BackendError::VersionMismatch`].
    pub fn from_dir_unchecked(dir: impl AsRef<Path>) -> Result<Self, BackendError> {
        Self::load_from_dir(dir.as_ref(), false)
    }

    /// Shared implementation of [`Self::from_dir`] and
    /// [`Self::from_dir_unchecked`].
    fn load_from_dir(dir: &Path, verify_pair: bool) -> Result<Self, BackendError> {
        let dir = absolute_dir(dir).map_err(|source| BackendError::DllNotFound {
            dir: dir.to_path_buf(),
            source,
        })?;
        let dll = dir.join(CONPTY_DLL);

        // Step 1: is there a DLL to load? `metadata` also rejects a directory
        // named `conpty.dll`, which would otherwise reach `LoadLibraryExW`.
        let metadata = fs::metadata(&dll).map_err(|source| BackendError::DllNotFound {
            dir: dir.clone(),
            source,
        })?;
        if !metadata.is_file() {
            return Err(BackendError::DllNotFound {
                dir: dir.clone(),
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "conpty.dll is not a regular file",
                ),
            });
        }

        // Step 2: the DLL is useless without the console host it spawns, and
        // finding that out now beats finding it out from a session that never
        // produces output.
        let host = find_console_host(&dir)
            .ok_or_else(|| BackendError::OpenConsoleMissing { dll: dll.clone() })?;

        // Step 3: prove the pair is consistent before any of its code runs.
        if verify_pair {
            check_version_pair(&dll, &host)?;
        }

        // Step 4: map the DLL. From here on the module guard owns the
        // reference, so an early return below unloads it again.
        let module = load_module(&dll).map_err(|source| BackendError::DllNotFound {
            dir: dir.clone(),
            source,
        })?;

        // Step 5: resolve the exports out of the pinned module.
        // SAFETY: `module` is live and owned by the guard, which outlives the
        // `ConptyApi` because both end up in the same `BackendInner`. The
        // exports are microsoft/terminal's, whose signatures the type aliases
        // above mirror.
        let api = unsafe { ConptyApi::from_module(module.module) }.map_err(|symbol| {
            BackendError::MissingExport {
                dll: dll.clone(),
                symbol,
            }
        })?;

        Ok(Self {
            inner: Arc::new(BackendInner {
                kind: BackendKind::External { dll },
                api: Some(api),
                module: Some(Arc::new(module)),
            }),
        })
    }

    /// Returns the best backend available to this process, never failing.
    ///
    /// The search order is:
    ///
    /// 1. A bundle next to the current executable. If `conpty.dll` sits in the
    ///    executable's directory it is loaded with [`Self::from_dir`], with all
    ///    of its validation.
    /// 2. The operating system's ConPTY ([`Self::system`]).
    ///
    /// A bundle that fails to load is not an error: the process still has the
    /// system implementation, and falling back to it is what an application
    /// that merely *may* ship a bundle wants. The rejection is recorded with
    /// `tracing::warn!` when the `tracing` feature is enabled, so a bundle that
    /// is silently ignored — a version-mismatched pair, say — is still
    /// diagnosable.
    ///
    /// If even the system implementation is missing (Windows older than 10
    /// 1809), the returned backend is *inert*: it carries no entry points, and
    /// building a session on it fails with [`BackendError::Unsupported`]
    /// instead of this call failing. That keeps the error at the point where a
    /// caller is already handling one.
    #[must_use]
    pub fn auto() -> Self {
        if let Some(dir) = exe_dir() {
            // Only attempt the load when a bundle is actually present:
            // otherwise every ordinary program would log a warning about a
            // `conpty.dll` it never intended to ship.
            if dir.join(CONPTY_DLL).is_file() {
                match Self::from_dir(&dir) {
                    Ok(backend) => return backend,
                    Err(err) => log_bundle_rejected(&dir, &err),
                }
            }
        }

        Self::system().unwrap_or_else(|_| Self::inert())
    }

    /// Builds the entry-point-less backend [`Self::auto`] falls back to.
    fn inert() -> Self {
        Self {
            inner: Arc::new(BackendInner {
                // The backend it *tried* to be; there is no "nothing" variant,
                // and inventing one would put an unusable state into a public
                // enum every caller has to match on.
                kind: BackendKind::System,
                api: None,
                module: None,
            }),
        }
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
        self.inner
            .api
            .as_ref()
            .is_some_and(|api| api.release.is_some())
    }

    /// Returns whether this backend can clear the pseudoconsole's buffer.
    ///
    /// `ClearPseudoConsole` is not part of the public Windows SDK and
    /// `kernel32.dll` does not export it, so this is `false` on the system
    /// backend and `true` only for a bundled `conpty.dll` that exports
    /// `ConptyClearPseudoConsole`.
    ///
    /// It is also `false` on 32-bit x86 regardless of the DLL: the export
    /// changed arity between releases (microsoft/terminal#18976) and `__stdcall`
    /// makes an arity mismatch corrupt the stack, so the call is not offered
    /// where it cannot be made safely.
    #[must_use]
    pub fn supports_clear(&self) -> bool {
        self.inner
            .api
            .as_ref()
            .is_some_and(|api| api.clear.is_some())
    }

    /// Returns a clone of this backend with the `ReleasePseudoConsole` export
    /// removed.
    ///
    /// Sessions on the returned backend behave exactly as on a Windows version
    /// that predates the export (everything before Windows 11 24H2):
    /// [`Self::supports_release`] answers `false`, releasing after spawn is
    /// impossible, and end-of-file has to be forced by the legacy watcher.
    ///
    /// This works on every backend, including an external one: the stripped
    /// clone shares the original's module pin, so the addresses it copies stay
    /// valid for as long as it does.
    ///
    /// This is a test hook, hidden because it is not part of the supported API
    /// surface. It exists so this crate's own suite — and any downstream
    /// harness — can exercise the legacy shutdown path deterministically on
    /// machines whose operating system does export `ReleasePseudoConsole`,
    /// where every ordinary session runs in released mode and the legacy code
    /// paths would otherwise be unreachable through the public API.
    #[doc(hidden)]
    #[must_use]
    pub fn without_release(&self) -> Self {
        Self {
            inner: Arc::new(BackendInner {
                kind: self.inner.kind.clone(),
                api: self.inner.api.as_ref().map(ConptyApi::without_release),
                // Share the pin rather than re-loading: the copied addresses
                // point into the very module the original keeps mapped.
                module: self.inner.module.clone(),
            }),
        }
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
    /// one was installed, otherwise the cached result of [`Self::auto`].
    ///
    /// # Errors
    ///
    /// Returns [`BackendError::Unsupported`] when the backend is the inert one
    /// [`Self::auto`] falls back to, i.e. when this Windows installation has no
    /// ConPTY at all.
    pub(crate) fn resolve_default() -> Result<Self, BackendError> {
        let backend = match GLOBAL_DEFAULT.get() {
            Some(backend) => backend,
            None => AUTO_DEFAULT.get_or_init(Self::auto),
        };
        if backend.inner.api.is_none() {
            return Err(BackendError::Unsupported);
        }
        Ok(backend.clone())
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
    /// Returns the failing `HRESULT` mapped to an [`io::Error`], or
    /// [`io::ErrorKind::Unsupported`] on the inert backend from
    /// [`Self::auto`].
    pub(crate) fn create(
        &self,
        size: Size,
        input_read: BorrowedHandle<'_>,
        output_write: BorrowedHandle<'_>,
        flags: u32,
    ) -> io::Result<HPCON> {
        let api = self.inner.api.as_ref().ok_or_else(unavailable)?;
        let (rows, cols) = size.to_i16_pair();
        let size = COORD { X: cols, Y: rows };
        let mut hpc: HPCON = ptr::null_mut();

        // SAFETY: `api.create` was resolved from a module this backend keeps
        // mapped. Both handles are borrowed for the duration of the call, and
        // `hpc` is a valid out-parameter.
        let hr = unsafe {
            (api.create)(
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
        // Unreachable when the contract holds: an inert backend never produced
        // an `HPCON` in the first place.
        let api = self.inner.api.as_ref().ok_or_else(unavailable)?;
        let (rows, cols) = size.to_i16_pair();
        let size = COORD { X: cols, Y: rows };

        // SAFETY: `hpc` is live per this function's contract, and the
        // function pointer was resolved from a module this backend keeps
        // mapped.
        let hr = unsafe { (api.resize)(hpc, size) };
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
        // Unreachable when the contract holds; see `Self::resize`.
        let Some(api) = self.inner.api.as_ref() else {
            return;
        };

        // SAFETY: `hpc` is live and unclosed per this function's contract.
        unsafe { (api.close)(hpc) }
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
        let release = self.inner.api.as_ref()?.release?;

        // SAFETY: `hpc` is live per this function's contract, and the
        // function pointer was resolved from a module this backend keeps
        // mapped.
        let hr = unsafe { release(hpc) };
        Some(hresult_ok(hr))
    }

    /// Calls `ClearPseudoConsole`, or returns [`None`] if the backend does not
    /// export it (see [`Self::supports_clear`]).
    ///
    /// Clearing discards the console host's scrollback and its visible screen,
    /// as the "clear buffer" action of a terminal emulator does. It is a signal
    /// to the host, not a write into conout, so it stays valid after a release
    /// and needs no cooperation from the reader.
    ///
    /// `keepCursorRow` is passed as `FALSE`, which is deliberate: it is the
    /// behaviour every version of the export has — the parameter was added in
    /// microsoft/terminal#18976 to *opt out* of clearing the cursor's row — so
    /// this operation means the same thing whichever `conpty.dll` is loaded.
    ///
    /// # Errors
    ///
    /// Returns `Some(Err(..))` with the failing `HRESULT` mapped to an
    /// [`io::Error`]; `E_INVALIDARG` for a null handle, otherwise the failure
    /// of the write to the signal pipe.
    ///
    /// # Safety
    ///
    /// `hpc` must be a live handle from [`Self::create`] on *this* backend
    /// that has not yet been passed to [`Self::close`].
    #[must_use]
    pub(crate) unsafe fn clear(&self, hpc: HPCON) -> Option<io::Result<()>> {
        let clear = self.inner.api.as_ref()?.clear?;

        // SAFETY: `hpc` is live per this function's contract, and the
        // function pointer was resolved from a module this backend keeps
        // mapped. The two-argument call shape is sound on every target that
        // resolves the export at all; see `CLEAR_ABI_IS_CALLABLE`.
        let hr = unsafe { clear(hpc, 0) };
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
            .field("supports_clear", &self.supports_clear())
            .finish()
    }
}

/// The error reported when a backend has no entry points at all.
fn unavailable() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "this ConPTY backend has no pseudoconsole entry points",
    )
}

/// Returns the directory the running executable lives in.
fn exe_dir() -> Option<PathBuf> {
    env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

/// Reports a bundle that [`ConPtyBackend::auto`] decided not to use.
///
/// Not an error for the caller — the system implementation takes over — but
/// worth recording, because a bundle shipped on purpose and then ignored (a
/// mismatched `OpenConsole.exe`, say) would otherwise look like it worked.
fn log_bundle_rejected(dir: &Path, err: &BackendError) {
    #[cfg(feature = "tracing")]
    tracing::warn!(
        dir = %dir.display(),
        error = %err,
        "ignoring the bundled conpty.dll; falling back to the system ConPTY"
    );
    #[cfg(not(feature = "tracing"))]
    {
        let _ = (dir, err);
    }
}

/// Makes `dir` absolute without touching the filesystem beyond reading the
/// current directory.
///
/// `LoadLibraryExW` is only given absolute paths, so that neither `PATH` nor
/// the current directory can decide which `conpty.dll` is loaded. Resolving a
/// relative `dir` here — once, explicitly — keeps that guarantee while still
/// accepting the relative paths callers naturally write.
///
/// One Windows path form survives the join without becoming absolute: a
/// drive-relative path (`C:dir`), which [`Path::join`] replaces the base with
/// wholesale because it carries a drive prefix. Handing it on would make
/// `LoadLibraryExW` resolve it against that drive's *own* current directory —
/// mutable process state, and a current-directory-dependent load even under
/// the strictest search flags — so it is rejected instead of guessed at.
fn absolute_dir(dir: &Path) -> io::Result<PathBuf> {
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

/// Locates the `OpenConsole.exe` a `conpty.dll` in `dir` would launch.
///
/// Mirrors `winconpty`'s own `_ConsoleHostPath` exactly: next to the DLL
/// first, then the *single* subdirectory named after the machine's native
/// architecture. The DLL searches no other subdirectory, so a host anywhere
/// else must not validate the bundle — the DLL would never launch it, every
/// session would silently run against the operating system's inbox
/// `conhost.exe`, and the version-pair check would have proven a file that
/// never executes. The inbox `conhost.exe` fallback itself is deliberately
/// *not* accepted either — the whole point of a bundle is to run its own
/// console host, and silently using the inbox one would hand back a backend
/// with the very behaviour the caller was trying to replace.
fn find_console_host(dir: &Path) -> Option<PathBuf> {
    let adjacent = dir.join(OPEN_CONSOLE_EXE);
    if adjacent.is_file() {
        return Some(adjacent);
    }

    let candidate = dir.join(native_arch_subdir()?).join(OPEN_CONSOLE_EXE);
    candidate.is_file().then_some(candidate)
}

/// Names the architecture subdirectory `conpty.dll` searches for its console
/// host on this machine, or [`None`] when the native machine is not one the
/// DLL has a name for.
///
/// `winconpty`'s `_ConsoleHostPath` selects the subdirectory from the *native*
/// machine reported by `IsWow64Process2`, not from the process's own
/// architecture. An emulated process — an x64 build on ARM64 Windows, say —
/// has to come to the same answer the DLL will, or it would validate a host in
/// `x64/` while the DLL searches `arm64/`.
fn native_arch_subdir() -> Option<&'static str> {
    match native_machine() {
        IMAGE_FILE_MACHINE_AMD64 => Some("x64"),
        IMAGE_FILE_MACHINE_ARM64 => Some("arm64"),
        IMAGE_FILE_MACHINE_I386 => Some("x86"),
        _ => None,
    }
}

/// Reports the machine's native architecture as an `IMAGE_FILE_MACHINE_*`
/// value.
///
/// `IsWow64Process2` is resolved dynamically because it only exists since
/// Windows 10 1709, and importing it statically would make the executable fail
/// to load on older versions — the unhelpful loader error this module
/// dynamically resolves everything to avoid. Where the export is missing, the
/// compile-time architecture is the answer: the emulation pairings in which
/// the two differ (x64 or ARM32 guests on ARM64 hosts) are all newer than the
/// export.
fn native_machine() -> u16 {
    #[cfg(target_arch = "x86_64")]
    const COMPILED_FOR: u16 = IMAGE_FILE_MACHINE_AMD64;
    #[cfg(target_arch = "aarch64")]
    const COMPILED_FOR: u16 = IMAGE_FILE_MACHINE_ARM64;
    #[cfg(target_arch = "x86")]
    const COMPILED_FOR: u16 = IMAGE_FILE_MACHINE_I386;
    // `IMAGE_FILE_MACHINE_UNKNOWN`: no subdirectory gets searched.
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "x86")))]
    const COMPILED_FOR: u16 = 0;

    let module_name: Vec<u16> = "kernel32.dll".encode_utf16().chain(iter::once(0)).collect();
    // SAFETY: `module_name` is a NUL-terminated UTF-16 string that outlives
    // the call.
    let module = unsafe { GetModuleHandleW(module_name.as_ptr()) };
    if module.is_null() {
        return COMPILED_FOR;
    }

    // SAFETY: `module` is kernel32.dll, which stays loaded for the lifetime of
    // the process, and the symbol name is a NUL-terminated byte string.
    let Some(address) = (unsafe { GetProcAddress(module, b"IsWow64Process2\0".as_ptr()) }) else {
        return COMPILED_FOR;
    };
    // SAFETY: kernel32's `IsWow64Process2` has the documented signature the
    // alias mirrors, and the module stays loaded for the address's lifetime.
    let is_wow64_process2 = unsafe { mem::transmute::<ProcAddress, IsWow64Process2Fn>(address) };

    let mut process_machine = 0_u16;
    let mut native = 0_u16;
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle that is always
    // valid, and both out-parameters point at live `u16`s.
    let ok = unsafe { is_wow64_process2(GetCurrentProcess(), &mut process_machine, &mut native) };
    if ok == 0 {
        return COMPILED_FOR;
    }

    native
}

/// Fails unless `dll` and `host` report the same `ProductVersion`.
fn check_version_pair(dll: &Path, host: &Path) -> Result<(), BackendError> {
    let dll_version = read_product_version(dll);
    let exe_version = read_product_version(host);

    if versions_are_compatible(dll_version.as_deref(), exe_version.as_deref()) {
        return Ok(());
    }

    Err(BackendError::VersionMismatch {
        dll: dll.to_path_buf(),
        dll_version: dll_version.unwrap_or_else(|| UNKNOWN_VERSION.to_owned()),
        exe_version: exe_version.unwrap_or_else(|| UNKNOWN_VERSION.to_owned()),
    })
}

/// Decides whether a `conpty.dll` / `OpenConsole.exe` pair may be used
/// together.
///
/// A version that cannot be read is *not* treated as "probably fine": the pair
/// is unprovable, and an unprovable pair is exactly the situation
/// [`ConPtyBackend::from_dir_unchecked`] exists to opt into deliberately.
fn versions_are_compatible(dll: Option<&str>, host: Option<&str>) -> bool {
    match (dll.and_then(parse_version), host.and_then(parse_version)) {
        (Some(dll), Some(host)) => dll == host,
        _ => false,
    }
}

/// Parses a `ProductVersion` string into its numeric components.
///
/// The string is up to four dot-separated decimal numbers (`1.24.1234.0`),
/// sometimes with fewer components and sometimes with a trailing label
/// (`1.24.1234.0-preview`). The components are parsed as `u64` — *not* the
/// 16 bits the binary `VS_FIXEDFILEINFO` fields are limited to — because the
/// string is free-form and microsoft/terminal actually uses it that way: its
/// ConPTY releases stamp a nine-digit date-serial build component
/// (`1.24.260710001`), and a 16-bit parse would overflow there, silently stop,
/// and reduce the pair check to major.minor — accepting exactly the mismatched
/// bundles it exists to reject. Parsing stops at the first component that is
/// not a plain number and pads the rest with zeroes, so `1.22` and `1.22.0.0`
/// compare equal — which is what a human reading those two strings would
/// expect.
///
/// Returns [`None`] when not even the first component is a number, i.e. when
/// the string is not a version at all.
fn parse_version(text: &str) -> Option<[u64; 4]> {
    let text = text.trim_matches(|c: char| c == '\0' || c.is_whitespace());

    let mut parts = [0u64; 4];
    let mut seen = 0;
    for field in text.split('.') {
        if seen == parts.len() {
            break;
        }
        let Ok(value) = field.trim().parse::<u64>() else {
            break;
        };
        parts[seen] = value;
        seen += 1;
    }

    (seen > 0).then_some(parts)
}

/// Reads the `ProductVersion` string from a file's version resource.
///
/// Returns [`None`] when the file has no version resource, or none that spells
/// out a `ProductVersion`.
fn read_product_version(path: &Path) -> Option<String> {
    let path = wide_path(path).ok()?;

    let mut ignored_handle: u32 = 0;
    // SAFETY: `path` is a NUL-terminated UTF-16 string that outlives the call
    // and `ignored_handle` is a valid out-parameter (the API always writes
    // zero there; the parameter exists only for source compatibility).
    let size = unsafe { GetFileVersionInfoSizeW(path.as_ptr(), &mut ignored_handle) };
    if size == 0 {
        return None;
    }

    // The block is allocated as `u32`s rather than bytes on purpose: the
    // version resource is a tree of 32-bit-aligned records, and `VerQueryValueW`
    // hands out interior pointers to them. A `Vec<u8>` would only guarantee
    // byte alignment.
    let mut block: Vec<u32> = vec![0; (size as usize).div_ceil(mem::size_of::<u32>())];
    // SAFETY: `block` has room for `size` bytes, which is exactly what
    // `GetFileVersionInfoSizeW` reported for this file.
    let read = unsafe { GetFileVersionInfoW(path.as_ptr(), 0, size, block.as_mut_ptr().cast()) };
    if read == 0 {
        return None;
    }

    // `\VarFileInfo\Translation` is an array of (language, code page) pairs;
    // for this sub-block the reported length counts bytes.
    // SAFETY: `block` holds a version resource `GetFileVersionInfoW` just
    // filled in.
    let (value, len) = unsafe { query_version_value(&block, "\\VarFileInfo\\Translation") }?;
    let count = len as usize / mem::size_of::<[u16; 2]>();
    // SAFETY: the API reported `len` valid bytes at `value`, which points into
    // the still-live `block`, and `[u16; 2]` needs 2-byte alignment, which the
    // 32-bit-aligned record satisfies.
    //
    // The pairs are copied out right away rather than iterated in place: the
    // loop below queries `block` again, and nothing should have to reason about
    // a borrow of it overlapping a raw pointer into it.
    let translations = unsafe { slice::from_raw_parts(value.cast::<[u16; 2]>(), count) }.to_vec();

    for [language, code_page] in translations {
        let sub_block =
            format!("\\StringFileInfo\\{language:04x}{code_page:04x}\\{PRODUCT_VERSION_KEY}");
        // SAFETY: as above.
        let Some((value, len)) = (unsafe { query_version_value(&block, &sub_block) }) else {
            continue;
        };
        // For a string value the reported length counts characters, the
        // terminating NUL included.
        // SAFETY: the API reported `len` valid UTF-16 code units at `value`,
        // which points into the still-live `block`.
        let text = unsafe { slice::from_raw_parts(value.cast::<u16>(), len as usize) };
        let text = String::from_utf16_lossy(text);
        let text = text.trim_matches(|c: char| c == '\0' || c.is_whitespace());
        if !text.is_empty() {
            return Some(text.to_owned());
        }
    }

    None
}

/// Runs `VerQueryValueW` for `sub_block`, returning the value pointer and the
/// length the API reported for it.
///
/// The unit of that length depends on the sub-block, which is why it is passed
/// through raw rather than being turned into a slice here.
///
/// # Safety
///
/// `block` must hold a version resource filled in by `GetFileVersionInfoW`,
/// and the returned pointer is only valid while `block` is.
unsafe fn query_version_value(block: &[u32], sub_block: &str) -> Option<(*const c_void, u32)> {
    let sub_block: Vec<u16> = sub_block.encode_utf16().chain(iter::once(0)).collect();
    let mut value: *mut c_void = ptr::null_mut();
    let mut len: u32 = 0;

    // SAFETY: `block` holds a version resource per this function's contract,
    // `sub_block` is a NUL-terminated UTF-16 string that outlives the call, and
    // both out-parameters are valid.
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

/// Maps `dll` into the process by absolute path.
///
/// The search flags are the security-relevant part. `LOAD_LIBRARY_SEARCH_-`
/// `DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32` resolves the DLL's own
/// dependencies from exactly two places: the directory it was loaded from, and
/// `System32`. The older `LOAD_WITH_ALTERED_SEARCH_PATH` is *not* used even
/// though it is the flag usually named for this job, because it still consults
/// the application directory, the current directory, and `PATH` — three places
/// an attacker can plant a DLL in.
fn load_module(dll: &Path) -> io::Result<ModuleGuard> {
    let path = wide_path(dll)?;

    // SAFETY: `path` is a NUL-terminated UTF-16 absolute path that outlives
    // the call, and a null `hFile` is what the API requires (the parameter is
    // reserved).
    let module = unsafe {
        LoadLibraryExW(
            path.as_ptr(),
            ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    if module.is_null() {
        return Err(io::Error::last_os_error());
    }

    Ok(ModuleGuard { module })
}

/// Widens a path for the Win32 `W` APIs.
///
/// # Errors
///
/// Rejects an interior NUL, which would otherwise silently truncate the path
/// and send the call somewhere the caller never named.
fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
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
    use std::sync::atomic::{AtomicU32, Ordering};

    use crate::core::pipes::create_sync_pipes;

    /// A scratch directory that is removed when the guard is dropped.
    ///
    /// The crate has no `tempfile` dependency and does not want one for this:
    /// the loader tests only need a handful of empty files in a directory
    /// nobody else touches.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "conpty-oxide-{tag}-{}-{unique}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("creating the scratch directory must succeed");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        /// Creates an empty file at `relative`, including its parents.
        fn touch(&self, relative: &str) -> PathBuf {
            let file = self.path.join(relative);
            if let Some(parent) = file.parent() {
                fs::create_dir_all(parent).expect("creating the parent directory must succeed");
            }
            fs::write(&file, b"").expect("creating the placeholder file must succeed");
            file
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

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

    /// `ClearPseudoConsole` is not part of the public Windows SDK and no
    /// released `kernel32.dll` exports it, but the query must answer rather
    /// than assume — a future OS could add it, and the crate would then simply
    /// gain the capability.
    #[test]
    fn supports_clear_answers_without_panicking() {
        let backend = ConPtyBackend::system().expect("ConPTY must be available");
        let _: bool = backend.supports_clear();
    }

    #[test]
    fn without_release_drops_only_the_release_export() {
        let backend = ConPtyBackend::system().expect("ConPTY must be available");
        let legacy = backend.without_release();
        assert!(
            !legacy.supports_release(),
            "the forced-legacy clone must not report a release export"
        );
        assert_eq!(legacy.kind(), backend.kind());
        // Every other capability survives the strip.
        assert_eq!(legacy.supports_clear(), backend.supports_clear());
        // The original backend is untouched by the stripped clone.
        assert_eq!(
            backend.supports_release(),
            ConPtyBackend::system()
                .expect("ConPTY must be available")
                .supports_release()
        );
    }

    /// CI pins `CONPTY_OXIDE_EXPECT_RELEASE` per matrix leg (`0` on the
    /// Server 2022 leg, `1` on the 24H2+ leg) precisely so the release/legacy
    /// coverage split cannot rot silently: if the runner images or the matrix
    /// change, this test fails instead of one lifecycle mode simply never
    /// running anywhere. Without the variable (a developer machine), the test
    /// is a no-op.
    #[test]
    fn release_support_matches_the_ci_expectation() {
        let Ok(expected) = std::env::var("CONPTY_OXIDE_EXPECT_RELEASE") else {
            return;
        };
        let expected = match expected.as_str() {
            "0" => false,
            "1" => true,
            other => panic!(r#"CONPTY_OXIDE_EXPECT_RELEASE must be "0" or "1", got {other:?}"#),
        };
        let backend = ConPtyBackend::system().expect("ConPTY must be available");
        assert_eq!(
            backend.supports_release(),
            expected,
            "this machine's ReleasePseudoConsole support does not match what \
             the CI matrix leg expects, so tests no longer cover the \
             lifecycle mode this leg is supposed to cover"
        );
    }

    #[test]
    fn clones_share_one_resolved_api() {
        let backend = ConPtyBackend::system().expect("ConPTY must be available");
        let clone = backend.clone();
        assert_eq!(clone.kind(), backend.kind());
        assert_eq!(clone.supports_release(), backend.supports_release());
    }

    #[test]
    fn debug_shows_kind_and_capabilities() {
        let backend = ConPtyBackend::system().expect("ConPTY must be available");
        let rendered = format!("{backend:?}");
        assert!(rendered.contains("ConPtyBackend"), "{rendered}");
        assert!(rendered.contains("System"), "{rendered}");
        assert!(rendered.contains("supports_release"), "{rendered}");
        assert!(rendered.contains("supports_clear"), "{rendered}");
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

    /// The test binary ships no bundled `conpty.dll`, so `auto` must land on
    /// the system implementation instead of failing or returning something
    /// inert.
    #[test]
    fn auto_falls_back_to_the_system_backend() {
        if exe_dir().is_some_and(|dir| dir.join(CONPTY_DLL).is_file()) {
            // Somebody dropped a bundle next to the test binary; the fallback
            // is then not the path under test.
            return;
        }
        let backend = ConPtyBackend::auto();
        assert_eq!(backend.kind(), &BackendKind::System);
        // Not inert: the system entry points resolved.
        assert!(ConPtyBackend::resolve_default().is_ok());
    }

    #[test]
    fn from_dir_reports_a_missing_directory() {
        let temp = TempDir::new("missing-dir");
        let missing = temp.path().join("no-such-directory");

        let err = ConPtyBackend::from_dir(&missing).expect_err("a missing directory must fail");
        match err {
            BackendError::DllNotFound { dir, source } => {
                assert_eq!(dir, missing);
                assert_eq!(source.kind(), io::ErrorKind::NotFound);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn from_dir_reports_an_empty_directory() {
        let temp = TempDir::new("empty-dir");

        let err = ConPtyBackend::from_dir(temp.path()).expect_err("an empty directory must fail");
        match err {
            BackendError::DllNotFound { dir, source } => {
                assert_eq!(dir, temp.path());
                assert_eq!(source.kind(), io::ErrorKind::NotFound);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    /// A `conpty.dll` without its console host must be rejected *before* it is
    /// mapped: the placeholder here is an empty file, so reaching
    /// `LoadLibraryExW` would produce a different error.
    #[test]
    fn from_dir_reports_a_missing_console_host() {
        let temp = TempDir::new("no-host");
        let dll = temp.touch(CONPTY_DLL);

        let err = ConPtyBackend::from_dir(temp.path())
            .expect_err("a bundle without OpenConsole.exe must fail");
        match err {
            BackendError::OpenConsoleMissing { dll: reported } => assert_eq!(reported, dll),
            other => panic!("unexpected error: {other:?}"),
        }

        // The unchecked variant skips only the version comparison, so it must
        // reject the same bundle for the same reason.
        let err = ConPtyBackend::from_dir_unchecked(temp.path())
            .expect_err("a bundle without OpenConsole.exe must fail");
        assert!(
            matches!(err, BackendError::OpenConsoleMissing { .. }),
            "unexpected error: {err:?}"
        );
    }

    /// The console host may live in the native-architecture subdirectory the
    /// DLL itself searches. Getting past `OpenConsoleMissing` to the version
    /// check is what proves the subdirectory was consulted.
    #[test]
    fn from_dir_finds_the_console_host_in_the_native_architecture_subdirectory() {
        let Some(arch) = native_arch_subdir() else {
            return;
        };
        let temp = TempDir::new("arch-subdir");
        temp.touch(CONPTY_DLL);
        temp.touch(&format!("{arch}/{OPEN_CONSOLE_EXE}"));

        let err = ConPtyBackend::from_dir(temp.path())
            .expect_err("placeholder files carry no version resource");
        match err {
            BackendError::VersionMismatch {
                dll_version,
                exe_version,
                ..
            } => {
                assert_eq!(dll_version, UNKNOWN_VERSION);
                assert_eq!(exe_version, UNKNOWN_VERSION);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    /// A host that sits only in a subdirectory the DLL never searches must not
    /// validate the bundle. `winconpty`'s `_ConsoleHostPath` consults the one
    /// native-architecture subdirectory and then silently falls back to the
    /// inbox `conhost.exe`, so accepting such a layout would prove a file that
    /// is never launched while every session runs against an unvalidated host.
    #[test]
    fn a_console_host_in_a_non_native_subdirectory_is_not_accepted() {
        let temp = TempDir::new("wrong-arch");
        temp.touch(CONPTY_DLL);
        let native = native_arch_subdir();
        for arch in ["x64", "arm64", "x86"] {
            if Some(arch) != native {
                temp.touch(&format!("{arch}/{OPEN_CONSOLE_EXE}"));
            }
        }

        assert_eq!(find_console_host(temp.path()), None);

        let err = ConPtyBackend::from_dir(temp.path())
            .expect_err("a host the DLL would never launch must not validate the bundle");
        assert!(
            matches!(err, BackendError::OpenConsoleMissing { .. }),
            "unexpected error: {err:?}"
        );
    }

    /// Every machine this suite runs on is one of the three architectures the
    /// DLL knows a subdirectory for, so the runtime probe must name one — a
    /// [`None`] here would mean `IsWow64Process2` failed *and* the
    /// compile-time fallback vanished.
    #[test]
    fn the_native_arch_subdirectory_is_known_on_this_machine() {
        let subdir = native_arch_subdir().expect("test machines have a known architecture");
        assert!(["x64", "arm64", "x86"].contains(&subdir), "{subdir}");
    }

    #[test]
    fn console_host_search_prefers_the_dll_directory() {
        let temp = TempDir::new("host-order");
        let adjacent = temp.touch(OPEN_CONSOLE_EXE);
        if let Some(arch) = native_arch_subdir() {
            temp.touch(&format!("{arch}/{OPEN_CONSOLE_EXE}"));
        }

        assert_eq!(find_console_host(temp.path()), Some(adjacent));
    }

    #[test]
    fn console_host_search_reports_nothing_for_an_empty_directory() {
        let temp = TempDir::new("host-none");
        assert_eq!(find_console_host(temp.path()), None);
    }

    /// Unversioned placeholders fail the pair check but pass the unchecked
    /// variant's, which then fails at the load itself — proving the version
    /// comparison is the only step `from_dir_unchecked` drops.
    #[test]
    fn from_dir_unchecked_skips_only_the_version_check() {
        let temp = TempDir::new("unchecked");
        temp.touch(CONPTY_DLL);
        temp.touch(OPEN_CONSOLE_EXE);

        let err = ConPtyBackend::from_dir(temp.path())
            .expect_err("placeholder files carry no version resource");
        assert!(
            matches!(err, BackendError::VersionMismatch { .. }),
            "unexpected error: {err:?}"
        );

        let err = ConPtyBackend::from_dir_unchecked(temp.path())
            .expect_err("an empty file is not a loadable DLL");
        match err {
            BackendError::DllNotFound { dir, source } => {
                assert_eq!(dir, temp.path());
                // Not `NotFound`: the file exists, the loader rejected it.
                assert_ne!(source.kind(), io::ErrorKind::NotFound);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn from_dir_rejects_a_directory_named_like_the_dll() {
        let temp = TempDir::new("dll-is-a-dir");
        fs::create_dir_all(temp.path().join(CONPTY_DLL))
            .expect("creating the decoy directory must succeed");

        let err =
            ConPtyBackend::from_dir(temp.path()).expect_err("a directory is not a loadable DLL");
        match err {
            BackendError::DllNotFound { source, .. } => {
                assert_eq!(source.kind(), io::ErrorKind::InvalidInput);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn a_relative_directory_is_resolved_against_the_working_directory() {
        let relative = Path::new("conpty-oxide-no-such-relative-directory");
        let err = ConPtyBackend::from_dir(relative).expect_err("a missing directory must fail");
        match err {
            BackendError::DllNotFound { dir, .. } => {
                assert!(
                    dir.is_absolute(),
                    "the reported path must be absolute: {dir:?}"
                );
                assert!(dir.ends_with(relative), "unexpected path: {dir:?}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    /// `C:dir` is relative to the C: drive's *own* current directory, which
    /// `Path::join` cannot resolve — it replaces the base outright when the
    /// argument carries a drive prefix — and which `LoadLibraryExW` would
    /// resolve against mutable per-drive state even under the strict search
    /// flags. The only current-directory-independent answer is to refuse.
    #[test]
    fn a_drive_relative_directory_is_rejected() {
        let drive_relative = Path::new("C:conpty-oxide-drive-relative");

        let err = absolute_dir(drive_relative)
            .expect_err("a drive-relative path must not survive absolutization");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

        let err = ConPtyBackend::from_dir(drive_relative)
            .expect_err("a drive-relative directory must be rejected");
        match err {
            BackendError::DllNotFound { dir, source } => {
                assert_eq!(dir, drive_relative);
                assert_eq!(source.kind(), io::ErrorKind::InvalidInput);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    /// The other prefix-less form, a rooted path (`\dir`), *does* absolutize:
    /// the join keeps the working directory's drive, which is the resolution
    /// the OS itself would perform — made once, here, and then pinned.
    #[test]
    fn a_rooted_driveless_directory_takes_the_working_directory_drive() {
        let resolved = absolute_dir(Path::new("\\conpty-oxide-rooted"))
            .expect("a rooted path must absolutize");
        assert!(resolved.is_absolute(), "unexpected path: {resolved:?}");
        assert!(
            resolved.ends_with("conpty-oxide-rooted"),
            "unexpected path: {resolved:?}"
        );
    }

    #[test]
    fn parse_version_reads_the_numeric_prefix() {
        let cases: [(&str, Option<[u64; 4]>); 11] = [
            ("1.24.1234.0", Some([1, 24, 1234, 0])),
            ("1.22.10352.0", Some([1, 22, 10352, 0])),
            // The format microsoft/terminal's ConPTY packages really use: the
            // build component is a nine-digit date serial, far beyond `u16`.
            // A parse that stopped there would compare major.minor only.
            ("1.24.260710001", Some([1, 24, 260_710_001, 0])),
            // Missing components are zeroes, so `1.22` == `1.22.0.0`.
            ("1.22", Some([1, 22, 0, 0])),
            ("3", Some([3, 0, 0, 0])),
            // Whitespace and the resource's trailing NUL are not part of the
            // version.
            ("  1.24.1234.0\0", Some([1, 24, 1234, 0])),
            // A trailing label does not invalidate the numbers before it.
            ("1.24.1234.0-preview", Some([1, 24, 1234, 0])),
            // A fifth component is beyond what the resource format stores.
            ("1.2.3.4.5", Some([1, 2, 3, 4])),
            ("", None),
            ("not a version", None),
            ("v1.24", None),
        ];
        for (text, expected) in cases {
            assert_eq!(parse_version(text), expected, "input: {text:?}");
        }
    }

    #[test]
    fn version_pair_compatibility() {
        let cases: [(Option<&str>, Option<&str>, bool); 10] = [
            (Some("1.24.1234.0"), Some("1.24.1234.0"), true),
            (Some("1.24.260710001"), Some("1.24.260710001"), true),
            // Equal after padding: the same release, spelled two ways.
            (Some("1.22"), Some("1.22.0.0"), true),
            (Some("1.24.1234.0"), Some("1.24.1234.1"), false),
            (Some("1.22.10352.0"), Some("1.24.1234.0"), false),
            // Two real releases of the same minor line, in the nine-digit
            // spelling — the pair measured for pitfall 4 in
            // docs/conpty-pitfalls.md. A 16-bit component parse could not
            // tell them apart; this one must.
            (Some("1.24.260710001"), Some("1.24.260303001"), false),
            // An unreadable version is never assumed to match.
            (None, Some("1.24.1234.0"), false),
            (Some("1.24.1234.0"), None, false),
            (None, None, false),
            (Some("junk"), Some("junk"), false),
        ];
        for (dll, host, expected) in cases {
            assert_eq!(
                versions_are_compatible(dll, host),
                expected,
                "inputs: {dll:?} / {host:?}"
            );
        }
    }

    /// Every Windows system binary carries a version resource, so reading one
    /// must work end to end — the placeholder-file tests above only cover the
    /// "no resource" answer.
    #[test]
    fn product_version_of_a_system_binary_is_readable() {
        // Resolved at run time, not with `env!`: the variable is a property of
        // the machine running the test, not of the one that built it.
        let Ok(system_root) = env::var("SystemRoot") else {
            return;
        };
        let kernel32 = Path::new(&system_root)
            .join("System32")
            .join("kernel32.dll");
        if !kernel32.is_file() {
            return;
        }
        let version = read_product_version(&kernel32).expect("kernel32.dll must carry a version");
        assert!(
            parse_version(&version).is_some(),
            "unparsable version: {version:?}"
        );
    }

    #[test]
    fn reading_a_version_from_a_nonexistent_file_reports_nothing() {
        let temp = TempDir::new("no-version");
        assert_eq!(read_product_version(&temp.path().join("nothing.dll")), None);
        // A file that exists but has no resource answers the same way.
        assert_eq!(read_product_version(&temp.touch("empty.dll")), None);
    }

    #[test]
    fn wide_path_rejects_an_interior_nul() {
        let err = wide_path(Path::new("C:\\a\0b")).expect_err("an interior NUL must be rejected");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

        let wide = wide_path(Path::new("C:\\a")).expect("a plain path must widen");
        // `C`, `:`, `\`, `a`, and the terminator this function appends.
        assert_eq!(wide.len(), 5);
        assert_eq!(wide.last(), Some(&0));
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

        // SAFETY: as above. The system backend exports no `ClearPseudoConsole`,
        // so this is expected to answer `None` rather than clear anything; the
        // assertion keeps the capability query and the call in agreement.
        let cleared = unsafe { backend.clear(hpc) };
        assert_eq!(cleared.is_some(), backend.supports_clear());

        drop(pipes);

        // SAFETY: `hpc` is live, was created by this backend, and has not been
        // closed. The conout read end is already closed and this thread is not
        // reading it, so neither liveness rule is violated.
        unsafe { backend.close(hpc) };
    }

    /// The inert backend must be inert rather than unsound: every entry point
    /// refuses instead of dereferencing a pointer it does not have.
    #[test]
    fn an_inert_backend_refuses_every_operation() {
        let backend = ConPtyBackend::inert();
        assert_eq!(backend.kind(), &BackendKind::System);
        assert!(!backend.supports_release());
        assert!(!backend.supports_clear());

        let pipes = create_sync_pipes().expect("creating pipes must succeed");
        let err = backend
            .create(
                Size::default(),
                pipes.conin_read.as_handle(),
                pipes.conout_write.as_handle(),
                0,
            )
            .expect_err("an inert backend must not create a pseudoconsole");
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);

        // No `HPCON` can exist for this backend, so the remaining calls are
        // unreachable in practice; a null handle is still enough to prove they
        // return instead of jumping through a missing pointer.
        // SAFETY: the backend has no entry points, so nothing is called.
        let err = unsafe { backend.resize(ptr::null_mut(), Size::default()) }
            .expect_err("an inert backend must not resize");
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        // SAFETY: as above.
        assert!(unsafe { backend.release(ptr::null_mut()) }.is_none());
        // SAFETY: as above.
        assert!(unsafe { backend.clear(ptr::null_mut()) }.is_none());
        // SAFETY: as above.
        unsafe { backend.close(ptr::null_mut()) };

        // And a stripped clone of it stays inert rather than panicking.
        assert!(!backend.without_release().supports_release());
    }
}
