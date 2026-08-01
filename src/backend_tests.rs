// SPDX-FileCopyrightText: 2025 conpty-oxide contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;

use std::os::windows::io::AsHandle;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::{env, fs};

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

#[test]
fn executable_directory_matches_the_current_executable() {
    let expected = env::current_exe()
        .expect("the test executable path must be available")
        .parent()
        .expect("the test executable must have a parent directory")
        .to_path_buf();
    assert_eq!(exe_dir(), Some(expected));
}

/// The standalone package used by external-backend CI exports
/// `ConptyReleasePseudoConsole` on every supported architecture. Assert that
/// known capability directly rather than only comparing two queries backed by
/// the same export table, which would let both answers become wrong together.
#[test]
fn configured_bundle_exports_release() {
    let Some(dir) = env::var_os("CONPTY_OXIDE_TEST_DLL_DIR") else {
        return;
    };
    let backend = ConPtyBackend::from_dir(dir).expect("the configured ConPTY bundle must load");
    assert!(
        backend.supports_release(),
        "the pinned standalone conpty.dll must export ConptyReleasePseudoConsole"
    );
}

#[test]
fn configured_bundle_module_guard_releases_its_loader_reference() {
    let Some(dir) = env::var_os("CONPTY_OXIDE_TEST_DLL_DIR") else {
        return;
    };
    let source = Path::new(&dir).join(CONPTY_DLL);
    let temp = TempDir::new("module-guard");
    let file_name = format!("conpty-oxide-module-guard-{}.dll", std::process::id());
    let probe = temp.path().join(&file_name);
    fs::copy(&source, &probe).expect("copying the loader probe must succeed");

    let module_name = wide_path(Path::new(&file_name)).expect("the probe name must widen");
    // SAFETY: `module_name` is NUL-terminated and outlives each call.
    assert!(unsafe { GetModuleHandleW(module_name.as_ptr()) }.is_null());

    let guard = load_module(&probe).expect("the copied standalone DLL must load");
    // SAFETY: as above. The unique module name is now mapped by `guard`.
    assert_eq!(
        unsafe { GetModuleHandleW(module_name.as_ptr()) },
        guard.module
    );
    drop(guard);

    // SAFETY: as above. Releasing the only loader reference must unmap it.
    assert!(unsafe { GetModuleHandleW(module_name.as_ptr()) }.is_null());
}

#[test]
fn external_dll_search_policy_is_restricted() {
    const DLL_LOAD_DIR: u32 = 0x0000_0100;
    const SYSTEM32: u32 = 0x0000_0800;
    assert_eq!(restricted_search_flags(), DLL_LOAD_DIR | SYSTEM32);
    assert_ne!(restricted_search_flags() & DLL_LOAD_DIR, 0);
    assert_ne!(restricted_search_flags() & SYSTEM32, 0);
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
    let Ok(expected) = env::var("CONPTY_OXIDE_EXPECT_RELEASE") else {
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
fn resolve_default_matches_automatic_selection() {
    let automatic = ConPtyBackend::auto().expect("ConPTY must be available");
    let resolved = ConPtyBackend::resolve_default().expect("ConPTY must be available");
    assert_eq!(resolved.kind(), automatic.kind());
    assert_eq!(resolved.supports_release(), automatic.supports_release());
    assert_eq!(resolved.supports_clear(), automatic.supports_clear());
}

/// The test binary ships no bundled `conpty.dll`, so `auto` must land on
/// the system implementation instead of failing.
#[test]
fn auto_falls_back_to_the_system_backend() {
    if exe_dir().is_some_and(|dir| dir.join(CONPTY_DLL).is_file()) {
        // Somebody dropped a bundle next to the test binary; the fallback
        // is then not the path under test.
        return;
    }
    let backend = ConPtyBackend::auto().expect("system ConPTY must be available");
    assert_eq!(backend.kind(), &BackendKind::System);
    assert!(ConPtyBackend::resolve_default().is_ok());
}

#[test]
fn successful_cache_runs_one_detector_under_contention() {
    const WORKERS: usize = 16;

    let cache = Arc::new(SuccessfulCache::new());
    let calls = Arc::new(AtomicU32::new(0));
    let start = Arc::new(Barrier::new(WORKERS));
    let mut handles = Vec::with_capacity(WORKERS);

    for _ in 0..WORKERS {
        let cache = Arc::clone(&cache);
        let calls = Arc::clone(&calls);
        let start = Arc::clone(&start);
        handles.push(thread::spawn(move || {
            start.wait();
            cache
                .get_or_try_init(|| {
                    calls.fetch_add(1, Ordering::Relaxed);
                    Ok::<u32, &'static str>(42)
                })
                .expect("the detector must succeed")
        }));
    }

    for handle in handles {
        assert_eq!(handle.join().expect("the cache worker must not panic"), 42);
    }
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[test]
fn successful_cache_retries_after_failure_and_then_stays_cached() {
    let cache = SuccessfulCache::new();
    let calls = AtomicU32::new(0);

    let first = cache.get_or_try_init(|| {
        calls.fetch_add(1, Ordering::Relaxed);
        Err::<u32, &'static str>("retryable")
    });
    assert_eq!(first, Err("retryable"));

    let recovered = cache
        .get_or_try_init(|| {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok::<u32, &'static str>(42)
        })
        .expect("the second detector must succeed");
    assert_eq!(recovered, 42);

    let cached = cache
        .get_or_try_init(|| {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok::<u32, &'static str>(99)
        })
        .expect("the cached result must be returned");
    assert_eq!(cached, 42);
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}

#[test]
fn from_dir_reports_a_missing_directory() {
    let temp = TempDir::new("missing-dir");
    let missing = temp.path().join("no-such-directory");

    let err = ConPtyBackend::from_dir(&missing).expect_err("a missing directory must fail");
    assert_eq!(err.kind(), crate::BackendErrorKind::DllNotFound);
    assert_eq!(
        err.io_error().map(io::Error::kind),
        Some(io::ErrorKind::NotFound)
    );
    assert!(err.to_string().contains(&missing.display().to_string()));
}

#[test]
fn from_dir_reports_an_empty_directory() {
    let temp = TempDir::new("empty-dir");

    let err = ConPtyBackend::from_dir(temp.path()).expect_err("an empty directory must fail");
    assert_eq!(err.kind(), crate::BackendErrorKind::DllNotFound);
    assert_eq!(
        err.io_error().map(io::Error::kind),
        Some(io::ErrorKind::NotFound)
    );
    assert!(err.to_string().contains(&temp.path().display().to_string()));
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
    assert_eq!(err.kind(), crate::BackendErrorKind::OpenConsoleMissing);
    assert!(err.to_string().contains(&dll.display().to_string()));

    // The unchecked variant skips only the version comparison, so it must
    // reject the same bundle for the same reason.
    let err = ConPtyBackend::from_dir_unchecked(temp.path())
        .expect_err("a bundle without OpenConsole.exe must fail");
    assert_eq!(err.kind(), crate::BackendErrorKind::OpenConsoleMissing);
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
    assert_eq!(err.kind(), crate::BackendErrorKind::VersionMismatch);
    let rendered = err.to_string();
    assert!(rendered.contains(UNKNOWN_VERSION), "{rendered}");
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
    assert_eq!(err.kind(), crate::BackendErrorKind::OpenConsoleMissing);
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
fn every_supported_machine_code_maps_to_its_package_directory() {
    assert_eq!(machine_arch_subdir(0x8664), Some("x64"));
    assert_eq!(machine_arch_subdir(0xAA64), Some("arm64"));
    assert_eq!(machine_arch_subdir(0x014c), Some("x86"));
    assert_eq!(machine_arch_subdir(0), None);
}

#[test]
fn native_machine_selection_uses_the_probe_only_after_success() {
    assert_eq!(selected_native_machine(1, 0xAA64, 0x8664), 0xAA64);
    assert_eq!(selected_native_machine(0, 0xAA64, 0x8664), 0x8664);
}

#[test]
fn version_resource_lengths_and_padding_are_normalized() {
    assert_eq!(translation_count(0), 0);
    assert_eq!(translation_count(4), 1);
    assert_eq!(translation_count(8), 2);
    assert_eq!(trim_resource_string(concat!("\0", "1.2.3.4\0")), "1.2.3.4");
    assert_eq!(trim_resource_string("  1.2.3.4  "), "1.2.3.4");
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
    assert_eq!(err.kind(), crate::BackendErrorKind::VersionMismatch);

    let err = ConPtyBackend::from_dir_unchecked(temp.path())
        .expect_err("an empty file is not a loadable DLL");
    assert_eq!(err.kind(), crate::BackendErrorKind::DllNotFound);
    // Not `NotFound`: the file exists, the loader rejected it.
    assert_ne!(
        err.io_error()
            .expect("DLL load failures retain an I/O error")
            .kind(),
        io::ErrorKind::NotFound
    );
    assert!(err.to_string().contains(&temp.path().display().to_string()));
}

#[test]
fn from_dir_rejects_a_directory_named_like_the_dll() {
    let temp = TempDir::new("dll-is-a-dir");
    fs::create_dir_all(temp.path().join(CONPTY_DLL))
        .expect("creating the decoy directory must succeed");

    let err = ConPtyBackend::from_dir(temp.path()).expect_err("a directory is not a loadable DLL");
    assert_eq!(err.kind(), crate::BackendErrorKind::DllNotFound);
    assert_eq!(
        err.io_error().map(io::Error::kind),
        Some(io::ErrorKind::InvalidInput)
    );
}

#[test]
fn a_relative_directory_is_resolved_against_the_working_directory() {
    let relative = Path::new("conpty-oxide-no-such-relative-directory");
    let err = ConPtyBackend::from_dir(relative).expect_err("a missing directory must fail");
    assert_eq!(err.kind(), crate::BackendErrorKind::DllNotFound);
    let rendered = err.to_string();
    assert!(rendered.contains(relative.to_string_lossy().as_ref()));
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
    assert_eq!(err.kind(), crate::BackendErrorKind::DllNotFound);
    assert_eq!(
        err.io_error().map(io::Error::kind),
        Some(io::ErrorKind::InvalidInput)
    );
    assert!(err
        .to_string()
        .contains(drive_relative.to_string_lossy().as_ref()));
}

/// The other prefix-less form, a rooted path (`\dir`), *does* absolutize:
/// the join keeps the working directory's drive, which is the resolution
/// the OS itself would perform — made once, here, and then pinned.
#[test]
fn a_rooted_driveless_directory_takes_the_working_directory_drive() {
    let resolved =
        absolute_dir(Path::new("\\conpty-oxide-rooted")).expect("a rooted path must absolutize");
    assert!(resolved.is_absolute(), "unexpected path: {resolved:?}");
    assert!(
        resolved.ends_with("conpty-oxide-rooted"),
        "unexpected path: {resolved:?}"
    );
}

#[test]
fn parse_version_reads_the_numeric_prefix() {
    let cases: [(&str, Option<[u64; 4]>); 14] = [
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
        (concat!("\0", "1.24.1234.0\0"), Some([1, 24, 1234, 0])),
        // A trailing label does not invalidate the numbers before it.
        ("1.24.1234.0-preview", Some([1, 24, 1234, 0])),
        // A labeled component contributes its leading digits and ends the
        // version; the digits must not be discarded, or two different
        // labeled builds would both truncate to their major.minor.
        ("1.24.1234-hotfix", Some([1, 24, 1234, 0])),
        ("1.24.1234-hotfix.7", Some([1, 24, 1234, 0])),
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
    let cases: [(Option<&str>, Option<&str>, bool); 11] = [
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
        // Labeled builds keep their digits, so two different labeled
        // versions must not collapse into the same value.
        (Some("1.24.1234-hotfix"), Some("1.24.20250101-x"), false),
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
fn hresult_ok_rejects_failure_codes() {
    let hr = i32::from_ne_bytes(0x8007_0005_u32.to_ne_bytes());
    let err = hresult_ok(hr).expect_err("a failed HRESULT must stay an error");
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
}

#[cfg(feature = "tracing")]
#[test]
fn rejected_optional_bundle_is_logged() {
    let events = crate::tracing_test_support::count_events(|| {
        log_rejected(Path::new("rejected-bundle"), &BackendError::unsupported());
    });
    assert_eq!(events, 1);
}

#[cfg(feature = "tracing")]
#[test]
fn missing_system_export_is_logged() {
    let events = crate::tracing_test_support::count_events(|| {
        log_missing_system_export("MissingConPtyExport");
    });
    assert_eq!(events, 1);
}

#[test]
fn hresult_win32_facility_unwraps_to_the_os_error() {
    // HRESULT_FROM_WIN32(ERROR_ACCESS_DENIED)
    let err = hresult_to_io_error(i32::from_ne_bytes(0x8007_0005_u32.to_ne_bytes()));
    assert_eq!(err.raw_os_error(), Some(5));
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);

    // E_INVALIDARG is HRESULT_FROM_WIN32(ERROR_INVALID_PARAMETER)
    let err = hresult_to_io_error(i32::from_ne_bytes(0x8007_0057_u32.to_ne_bytes()));
    assert_eq!(err.raw_os_error(), Some(87));
}

#[test]
fn hresult_other_facilities_pass_through() {
    // E_NOTIMPL lives in FACILITY_NULL, so there is nothing to unwrap and
    // the raw code must survive verbatim.
    let hr = i32::from_ne_bytes(0x8000_4001_u32.to_ne_bytes());
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
            crate::size::test_size(24, 80),
            pipes.conin_read.as_handle(),
            pipes.conout_write.as_handle(),
            0,
        )
        .expect("CreatePseudoConsole must succeed");
    assert_ne!(hpc, 0);

    // SAFETY: `hpc` is live and was created by this backend.
    unsafe { backend.resize(hpc, crate::size::test_size(50, 132)) }
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
