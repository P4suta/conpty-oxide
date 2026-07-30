// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Dynamic loading of the `ConPTY` entry points.
//!
//! The pseudoconsole API (`CreatePseudoConsole` and friends) is not linked
//! statically. It is resolved at run time with `GetProcAddress`, for two
//! reasons:
//!
//! 1. **Graceful degradation.** Linking `CreatePseudoConsole` statically makes
//!    the executable fail to start on Windows versions older than 10 1809
//!    (build 17763) with an unhelpful loader error. Resolving it dynamically
//!    turns that into [`crate::BackendErrorKind::Unsupported`].
//! 2. **Capability detection.** `ReleasePseudoConsole` only exists on Windows
//!    11 24H2 (build 26100) and later, and `ClearPseudoConsole` exists only in
//!    the standalone `conpty.dll`. Whether they are available decides which
//!    shutdown strategy the crate uses and which operations it can offer, and
//!    the presence of the export is the check microsoft/terminal recommends —
//!    *not* an OS build-number comparison, which misfires under compatibility
//!    shims and on backported builds (microsoft/terminal#19112).
//!
//! The same loader serves a bundled `conpty.dll`, which is why symbol lookup
//! goes through [`exports::resolve_export`]: that DLL exports each entry point twice,
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
//!    console host speak a private, versioned protocol, and a bad `ConPTY`
//!    bundle takes the client process down with a `FailFast` rather than an
//!    error (wezterm#7774), so it is far better to refuse the bundle than to
//!    crash later.
//! 4. It loads the DLL by absolute path with `LoadLibraryExW` and a search
//!    policy that never consults `PATH`, the current directory, or the
//!    registry, so a stray `conpty.dll` cannot be planted into the process.
//!
//! [`ConPtyBackend::auto`] applies that to the executable's own directory and
//! falls back to the operating system's `ConPTY`, which is what an
//! application that merely *may* ship a bundle wants, and returns
//! [`crate::BackendErrorKind::Unsupported`] when neither implementation is
//! usable.

mod bundle;
mod exports;

use std::fmt;
#[cfg(any(feature = "blocking", feature = "tokio", test))]
use std::io;
use std::iter;
#[cfg(any(feature = "blocking", feature = "tokio", test))]
use std::os::windows::io::{AsRawHandle, BorrowedHandle};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(any(feature = "blocking", feature = "tokio", test))]
use windows_sys::core::HRESULT;
#[cfg(any(feature = "blocking", feature = "tokio", test))]
use windows_sys::Win32::System::Console::COORD;
/// Official Windows SDK pseudoconsole handle and cursor-inheritance flag.
#[cfg(any(feature = "blocking", feature = "tokio", test))]
pub(super) use windows_sys::Win32::System::Console::{HPCON, PSEUDOCONSOLE_INHERIT_CURSOR};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;

#[cfg(test)]
use bundle::{
    absolute_dir, find_console_host, machine_arch_subdir, native_arch_subdir, parse_version,
    read_product_version, selected_native_machine, translation_count, trim_resource_string,
    versions_are_compatible, OPEN_CONSOLE_EXE, UNKNOWN_VERSION,
};
use bundle::{exe_dir, log_rejected, validate, CONPTY_DLL};
use exports::{load_module, ConptyApi, ModuleGuard};
#[cfg(test)]
use exports::{resolve_export, restricted_search_flags, wide_path, CREATE_PSEUDO_CONSOLE};

use crate::error::BackendError;
#[cfg(any(feature = "blocking", feature = "tokio", test))]
use crate::size::Size;

/// Which `ConPTY` implementation a [`ConPtyBackend`] is bound to.
///
/// Marked `#[non_exhaustive]`, like the crate's error enums: new kinds may be
/// added in later releases, so matches on it need a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub(crate) enum BackendKind {
    /// The `ConPTY` API built into the operating system (`kernel32.dll`).
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

    /// The resolved entry points.
    ///
    /// A public backend is always usable: construction fails before a
    /// `BackendInner` exists when the required `ConPTY` exports are absent.
    api: ConptyApi,

    /// Pins the module `api` was resolved from.
    ///
    /// [`None`] for the system backend: `kernel32.dll` is mapped into every
    /// Win32 process for its entire lifetime, so there is nothing to pin and no
    /// reference to release.
    module_pin: Option<Arc<ModuleGuard>>,
}

/// A loaded `ConPTY` implementation.
///
/// Cloning is cheap: clones share one [`Arc`], so resolving the entry points
/// happens once per backend rather than once per pseudoconsole.
///
/// # Thread safety
///
/// `ConPtyBackend` is `Send + Sync`, and that is sound:
///
/// - `BackendInner` holds a private backend-kind value — a unit variant or a
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

/// A fallible initializer that caches only its first successful result.
///
/// [`OnceLock::get_or_init`] cannot represent retryable failure. Pairing the
/// value cell with a short initialization mutex keeps the detector outside the
/// permanent state while ensuring concurrent first callers do not load the
/// same DLL more than once. A poisoned mutex is still usable here: neither a
/// detector error nor a panic can partially initialize `value`.
#[derive(Debug)]
struct SuccessfulCache<T> {
    value: OnceLock<T>,
    initialization: Mutex<()>,
}

impl<T> SuccessfulCache<T> {
    const fn new() -> Self {
        Self {
            value: OnceLock::new(),
            initialization: Mutex::new(()),
        }
    }
}

impl<T: Clone> SuccessfulCache<T> {
    fn get_or_try_init<E>(&self, detect: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
        if let Some(value) = self.value.get() {
            return Ok(value.clone());
        }

        let _initialization = self
            .initialization
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(value) = self.value.get() {
            return Ok(value.clone());
        }

        let detected = detect()?;
        if self.value.set(detected.clone()).is_ok() {
            return Ok(detected);
        }
        Ok(self.value.get().map_or(detected, Clone::clone))
    }
}

/// Cached successful result of [`ConPtyBackend::auto`].
///
/// Caching matters for more than speed: it keeps a bundled `conpty.dll` loaded
/// once per process instead of once per session.
static AUTO_DEFAULT: SuccessfulCache<ConPtyBackend> = SuccessfulCache::new();

impl ConPtyBackend {
    /// Loads the `ConPTY` API built into the operating system.
    ///
    /// Resolves the entry points from the already-mapped `kernel32.dll`; no
    /// library is loaded and no reference count is taken, because
    /// `kernel32.dll` is present in every Win32 process for its entire
    /// lifetime.
    ///
    /// # Errors
    ///
    /// Returns [`crate::BackendErrorKind::Unsupported`] when
    /// `CreatePseudoConsole`,
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
            return Err(BackendError::unsupported());
        }

        // SAFETY: `module` is a live handle to kernel32.dll, which stays
        // loaded for the lifetime of the process, and its ConPTY exports have
        // the signatures documented on Microsoft Learn.
        let api = match unsafe { ConptyApi::from_module(module) } {
            Ok(api) => api,
            Err(symbol) => {
                log_missing_system_export(symbol);
                return Err(BackendError::unsupported());
            },
        };

        Ok(Self {
            inner: Arc::new(BackendInner {
                kind: BackendKind::System,
                api,
                // kernel32.dll needs no pin; see `BackendInner::module_pin`.
                module_pin: None,
            }),
        })
    }

    /// Loads a bundled `conpty.dll` from `dir`, validating the bundle first.
    ///
    /// A bundle is `conpty.dll` plus the `OpenConsole.exe` it launches, as
    /// shipped by the `Microsoft.Windows.Console.ConPTY` NuGet package. Both
    /// must come from the same package: the DLL and the console host share a
    /// private protocol with no compatibility promise across releases, and a
    /// bad `ConPTY` bundle crashes the client process rather than degrading —
    /// wezterm#7774 is PowerShell dying with a `0x8013_1623` `FailFast` until
    /// the bundle was replaced. This constructor therefore refuses a pair it
    /// cannot prove consistent; public callers cannot bypass this validation.
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
    /// [`crate::BackendErrorKind::DllNotFound`]: it names a path relative to that drive's
    /// own current directory, which cannot be resolved once and pinned.
    ///
    /// # Errors
    ///
    /// - [`crate::BackendErrorKind::DllNotFound`] if `dir/conpty.dll` is missing or
    ///   cannot be loaded (the source carries the OS error, e.g.
    ///   `ERROR_BAD_EXE_FORMAT` for a file that is not a DLL at all).
    /// - [`crate::BackendErrorKind::OpenConsoleMissing`] if no `OpenConsole.exe`
    ///   accompanies the DLL.
    /// - [`crate::BackendErrorKind::VersionMismatch`] if the two files report different
    ///   `ProductVersion` resources, or if either version cannot be read.
    /// - [`crate::BackendErrorKind::MissingExport`] if the DLL lacks
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
    /// degradation: the failure mode of a bad `ConPTY` bundle is a hard crash of
    /// the *client* process — in wezterm#7774, PowerShell dies with a
    /// `0x8013_1623` `FailFast` — at an arbitrary later point, far from this
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
    /// [`crate::BackendErrorKind::VersionMismatch`].
    #[cfg(test)]
    pub(crate) fn from_dir_unchecked(dir: impl AsRef<Path>) -> Result<Self, BackendError> {
        Self::load_from_dir(dir.as_ref(), false)
    }

    /// Shared implementation of [`Self::from_dir`] and
    /// [`Self::from_dir_unchecked`].
    fn load_from_dir(dir: &Path, verify_pair: bool) -> Result<Self, BackendError> {
        // Discovery and validation complete before executable code is mapped.
        let bundle = validate(dir, verify_pair)?;
        let dir = bundle.dir;
        let dll = bundle.dll;

        let module =
            load_module(&dll).map_err(|source| BackendError::dll_not_found(dir.clone(), source))?;

        // SAFETY: the module stays pinned in the same `BackendInner` as the
        // resolved table, and standalone ConPTY exports use the SDK signatures.
        let api = unsafe { ConptyApi::from_module(module.module) }
            .map_err(|symbol| BackendError::missing_export(dll.clone(), symbol))?;

        Ok(Self {
            inner: Arc::new(BackendInner {
                kind: BackendKind::External { dll },
                api,
                module_pin: Some(Arc::new(module)),
            }),
        })
    }

    /// Returns the best backend available to this process.
    ///
    /// The search order is:
    ///
    /// 1. A bundle next to the current executable. If `conpty.dll` sits in the
    ///    executable's directory it is loaded with [`Self::from_dir`], with all
    ///    of its validation.
    /// 2. The operating system's `ConPTY` ([`Self::system`]).
    ///
    /// A bundle that fails to load is not an error: the process still has the
    /// system implementation, and falling back to it is what an application
    /// that merely *may* ship a bundle wants. The rejection is recorded with
    /// `tracing::warn!` when the `tracing` feature is enabled, so a bundle that
    /// is silently ignored — a version-mismatched pair, say — is still
    /// diagnosable.
    ///
    /// # Errors
    ///
    /// Returns [`crate::BackendErrorKind::Unsupported`] when neither a valid bundle nor
    /// the system `ConPTY` implementation is available.
    pub fn auto() -> Result<Self, BackendError> {
        AUTO_DEFAULT.get_or_try_init(Self::detect_auto)
    }

    /// Performs one uncached automatic-detection attempt.
    fn detect_auto() -> Result<Self, BackendError> {
        if let Some(dir) = exe_dir() {
            // Only attempt the load when a bundle is actually present:
            // otherwise every ordinary program would log a warning about a
            // `conpty.dll` it never intended to ship.
            if dir.join(CONPTY_DLL).is_file() {
                match Self::from_dir(&dir) {
                    Ok(backend) => return Ok(backend),
                    Err(err) => log_rejected(&dir, &err),
                }
            }
        }

        Self::system()
    }

    /// Returns which `ConPTY` implementation this backend is bound to.
    #[must_use]
    pub(crate) fn kind(&self) -> &BackendKind {
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
    pub(crate) fn supports_release(&self) -> bool {
        self.inner.api.release.is_some()
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
        self.inner.api.clear.is_some()
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
    /// This crate-private test hook lets the unit suite exercise the legacy
    /// shutdown path deterministically on machines whose operating system
    /// exports `ReleasePseudoConsole`, where ordinary sessions otherwise run
    /// only in released mode.
    #[must_use]
    #[cfg(test)]
    pub(super) fn without_release(&self) -> Self {
        Self {
            inner: Arc::new(BackendInner {
                kind: self.inner.kind.clone(),
                api: self.inner.api.without_release(),
                // Share the pin rather than re-loading: the copied addresses
                // point into the very module the original keeps mapped.
                module_pin: self.inner.module_pin.clone(),
            }),
        }
    }

    /// Replaces only the close export so lifecycle tests can observe a
    /// detached FFI call without passing a fabricated handle to Windows.
    #[cfg(test)]
    pub(super) fn with_test_close(&self, close: unsafe extern "system" fn(HPCON)) -> Self {
        Self {
            inner: Arc::new(BackendInner {
                kind: self.inner.kind.clone(),
                api: self.inner.api.with_close(close),
                module_pin: self.inner.module_pin.clone(),
            }),
        }
    }

    /// Returns the backend to use when the caller did not name one.
    ///
    /// Only successful automatic detection is cached; failures remain
    /// retryable.
    #[cfg(any(feature = "blocking", feature = "tokio", test))]
    pub(super) fn resolve_default() -> Result<Self, BackendError> {
        Self::auto()
    }

    /// Calls `CreatePseudoConsole`.
    ///
    /// `input_read` is the read end of the conin pipe and `output_write` the
    /// write end of the conout pipe; both must be synchronous handles, which
    /// anonymous pipes always are. `ConPTY` duplicates them, so the caller
    /// should close its own copies as soon as the child has been spawned —
    /// until then the extra references keep conout from ever reaching
    /// end-of-file.
    ///
    /// The returned `HPCON` is *not* owned by any RAII type here; the caller
    /// must eventually pass it to [`Self::close`].
    ///
    /// # Errors
    ///
    /// Returns the failing `HRESULT` mapped to an [`io::Error`]. Construction
    /// has already proved that the backend provides this required export.
    #[cfg(any(feature = "blocking", feature = "tokio", test))]
    pub(super) fn create(
        &self,
        size: Size,
        input_read: BorrowedHandle<'_>,
        output_write: BorrowedHandle<'_>,
        flags: u32,
    ) -> io::Result<HPCON> {
        let api = &self.inner.api;
        let (rows, cols) = size.to_i16_pair();
        let size = COORD { X: cols, Y: rows };
        let mut hpc: HPCON = 0;

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
    #[cfg(any(feature = "blocking", feature = "tokio", test))]
    pub(super) unsafe fn resize(&self, hpc: HPCON, size: Size) -> io::Result<()> {
        let api = &self.inner.api;
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
    /// Beyond memory safety, two liveness rules from the `ConPTY` documentation
    /// apply, and violating them hangs the process rather than corrupting it:
    ///
    /// - Before Windows 11 24H2 (build 26100), this call waits until every
    ///   client has disconnected. The caller must therefore have closed its
    ///   conout read end first, or keep another thread draining it.
    /// - It must never be called from the thread that reads conout, because
    ///   that thread is exactly the one that would have to make progress for
    ///   the call to return.
    #[cfg(any(feature = "blocking", feature = "tokio", test))]
    pub(super) unsafe fn close(&self, hpc: HPCON) {
        let api = &self.inner.api;

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
    #[cfg(any(feature = "blocking", feature = "tokio", test))]
    pub(super) unsafe fn release(&self, hpc: HPCON) -> Option<io::Result<()>> {
        let release = self.inner.api.release?;

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
    #[cfg(any(feature = "blocking", feature = "tokio", test))]
    pub(super) unsafe fn clear(&self, hpc: HPCON) -> Option<io::Result<()>> {
        let clear = self.inner.api.clear?;

        // SAFETY: `hpc` is live per this function's contract, and the
        // function pointer was resolved from a module this backend keeps
        // mapped. The two-argument call shape is sound on every target that
        // resolves the export at all; see `backend::exports`.
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
            .field("kind", &self.kind())
            .field("supports_release", &self.supports_release())
            .field("supports_clear", &self.supports_clear())
            .field("module_pinned", &self.inner.module_pin.is_some())
            .finish()
    }
}

/// Turns an `HRESULT` into a [`io::Result`], failing on `FAILED(hr)`.
#[cfg(any(feature = "blocking", feature = "tokio", test))]
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
#[cfg(any(feature = "blocking", feature = "tokio", test))]
fn hresult_to_io_error(hr: HRESULT) -> io::Error {
    /// Mask selecting the severity and facility bits of an `HRESULT`.
    const FACILITY_MASK: u32 = 0xFFFF_0000;
    /// Severity `FAILED` plus `FACILITY_WIN32`, i.e. an `HRESULT_FROM_WIN32`.
    const FAILED_FACILITY_WIN32: u32 = 0x8007_0000;

    let bits = u32::from_ne_bytes(hr.to_ne_bytes());
    if bits & FACILITY_MASK == FAILED_FACILITY_WIN32 {
        let code = i32::try_from(bits & 0xFFFF).unwrap_or(i32::MAX);
        io::Error::from_raw_os_error(code)
    } else {
        io::Error::from_raw_os_error(hr)
    }
}

#[cfg(feature = "tracing")]
fn log_missing_system_export(symbol: &'static str) {
    tracing::warn!(
        symbol,
        "the system ConPTY backend is missing a required export"
    );
}

#[cfg(not(feature = "tracing"))]
const fn log_missing_system_export(_symbol: &'static str) {}

#[cfg(test)]
#[path = "backend_tests.rs"]
mod tests;
