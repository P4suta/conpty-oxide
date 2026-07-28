//! The bundled `conpty.dll` backend, driven against a real DLL.
//!
//! Every other test in this directory runs on whatever ConPTY the operating
//! system happens to ship. That leaves the external backend — the loader, the
//! module pin, and entry points resolved out of `conpty.dll` instead of
//! `kernel32.dll` — covered only by unit tests that never load real code, and
//! a loader nobody has ever loaded anything with is not a loader. These tests
//! run the crate's whole contract a second time, on a bundle.
//!
//! They need that bundle, so they are opt-in: set `CONPTY_OXIDE_TEST_DLL_DIR`
//! to a directory holding `conpty.dll` next to its matching `OpenConsole.exe`
//! and they run; leave it unset and each one returns immediately with a note
//! on stderr. `just fetch-conpty` produces such a directory at `vendor/conpty`
//! from the pinned `Microsoft.Windows.Console.ConPTY` package, `just test-dll`
//! runs the suite with the variable already pointed at it, and CI always sets
//! it — so "unset" means "a developer who has not fetched the package", never
//! "the coverage quietly disappeared".
//!
//! Pointing the variable at something that is *not* a loadable bundle is a
//! failure rather than a skip. An opt-in test that passes because the thing it
//! tests could not be loaded is worse than no test at all.
//!
//! One test here needs no bundle and always runs: rejecting a `conpty.dll`
//! whose console host is missing is a property of the loader, and the sharper
//! version of it — a *genuine, loadable* DLL rejected all the same — is worth
//! stating at the integration level even though the unit tests cover the
//! bookkeeping.

#![cfg(all(windows, feature = "blocking"))]

mod helpers;

use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use conpty_oxide::blocking::{Command, Pty};
use conpty_oxide::{BackendError, BackendKind, ConPtyBackend, Error, Size};

use helpers::{
    expected_size, process_is_running, reported_size, strip_escapes, wait_for_descendant,
    wait_until, with_timeout, Session,
};

/// Names the directory holding the `conpty.dll` bundle to test against.
const DLL_DIR_VAR: &str = "CONPTY_OXIDE_TEST_DLL_DIR";

/// Per-test budget. Every test here spawns a real console host out of the
/// bundle, which costs a few hundred milliseconds; anything near this is a
/// deadlock.
const BUDGET: Duration = Duration::from_secs(60);

/// How long an interactive shell gets to answer a typed command.
const ANSWER: Duration = Duration::from_secs(15);

/// How long a process gets to appear in, or disappear from, the process list.
const SETTLE: Duration = Duration::from_secs(10);

/// The directory the suite was pointed at, if any.
fn bundle_dir() -> Option<PathBuf> {
    env::var_os(DLL_DIR_VAR).map(PathBuf::from)
}

/// Loads the bundle, or reports the skip and returns `None`.
///
/// Note the asymmetry: a *missing* variable is a skip, but a variable that
/// points at a bundle which fails to load is a panic. The second case is a
/// broken bundle or a broken loader, and both are exactly what this file
/// exists to catch.
fn bundle() -> Option<ConPtyBackend> {
    let Some(dir) = bundle_dir() else {
        eprintln!(
            "skipped: {DLL_DIR_VAR} is not set, so there is no conpty.dll \
             bundle to test against. Run `just fetch-conpty` and then \
             `just test-dll`, or set {DLL_DIR_VAR} to a directory holding \
             conpty.dll and its matching OpenConsole.exe."
        );
        return None;
    };
    let backend = ConPtyBackend::from_dir(&dir).unwrap_or_else(|err| {
        panic!(
            "{DLL_DIR_VAR} points at {}, which must be a loadable conpty.dll \
             bundle: {err}",
            dir.display()
        )
    });
    Some(backend)
}

/// Builds a default 24x80 session on the bundle.
fn bundle_pty() -> Option<Pty> {
    bundle_pty_with_size(Size::new(24, 80))
}

/// Builds a session of the given size on the bundle.
fn bundle_pty_with_size(size: Size) -> Option<Pty> {
    let pty = Pty::builder()
        .backend(bundle()?)
        .size(size)
        .build()
        .expect("building a session on the bundle must succeed");
    Some(pty)
}

/// Builds a session on the bundle with `ReleasePseudoConsole` stripped.
///
/// The original backend is dropped before the session is built, which makes
/// this the pointed test of the module pin: `without_release` copies raw entry
/// point addresses out of the loaded `conpty.dll`, and the only thing keeping
/// that DLL mapped afterwards is the pin the stripped clone shares. If the
/// pin were missing, `FreeLibrary` would run here and every later call would
/// jump into unmapped memory.
fn legacy_bundle_pty() -> Option<Pty> {
    let backend = bundle()?;
    let legacy = backend.without_release();
    drop(backend);

    assert!(
        !legacy.supports_release(),
        "the forced-legacy clone must not report a release export"
    );
    let pty = Pty::builder()
        .backend(legacy)
        .build()
        .expect("building a forced-legacy session on the bundle must succeed");
    Some(pty)
}

/// A scratch directory that is removed when the guard is dropped.
///
/// The crate has no `tempfile` dependency and needs none for this: the one
/// test that wants a directory only ever puts a single file in it.
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
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn the_bundle_loads_and_names_the_dll_it_loaded() {
    let Some(backend) = bundle() else { return };

    let BackendKind::External { dll } = backend.kind() else {
        panic!("a backend loaded from a directory must report itself as external");
    };
    assert!(
        dll.is_absolute(),
        "the reported path must be absolute so it stays meaningful after a \
         change of working directory: {}",
        dll.display()
    );
    assert!(dll.is_file(), "{} must exist", dll.display());
    assert_eq!(
        dll.file_name().and_then(|name| name.to_str()),
        Some("conpty.dll")
    );
}

/// `ReleasePseudoConsole` has been part of `conpty.dll` since long before the
/// package this suite pins, so a bundle that does not export it is a bundle
/// that was assembled wrongly — the entry point was resolved from the wrong
/// file, or the wrong architecture's DLL was laid out.
#[test]
fn the_bundle_exports_release_whatever_the_operating_system_supports() {
    let Some(backend) = bundle() else { return };

    assert!(
        backend.supports_release(),
        "a bundled conpty.dll must export ReleasePseudoConsole, so a session \
         on it uses the released lifecycle even on a Windows version whose \
         own ConPTY predates it"
    );
}

/// `ConptyClearPseudoConsole` is declared in the package's own `conpty.h` and
/// exported by the `conpty.dll` it ships, so wherever the crate is willing to
/// call it, a bundle must offer it — otherwise the clear tests below would
/// pass by never performing a clear at all.
///
/// 32-bit x86 is the deliberate exception: the export changed arity without
/// changing its name (microsoft/terminal#18976 added `keepCursorRow`), and
/// under `__stdcall` the callee pops the arguments, so calling either shape
/// against the other corrupts the stack. The crate declines rather than guess,
/// and that has to be asserted too.
#[test]
fn the_bundle_exports_clear_except_on_32_bit_x86() {
    let Some(backend) = bundle() else { return };

    assert_eq!(
        backend.supports_clear(),
        !cfg!(target_arch = "x86"),
        "a bundled conpty.dll exports ConptyClearPseudoConsole, and the crate \
         offers it everywhere except on 32-bit x86"
    );
}

#[test]
fn echoed_text_comes_back_with_a_successful_status_from_the_bundle() {
    let Some(pty) = bundle_pty() else { return };
    with_timeout(BUDGET, || {
        const MARKER: &str = "conpty-oxide-bundle-echo";

        let (output, status) =
            Session::start_in(pty, Command::new("cmd.exe").args(["/c", "echo", MARKER])).finish();

        assert!(
            output.contains(MARKER),
            "the echoed marker is missing from the rendered output: {output:?}"
        );
        assert!(status.success(), "unexpected status: {status}");
        assert_eq!(status.code(), 0);
    });
}

#[test]
fn a_nonzero_exit_code_is_reported_verbatim_from_the_bundle() {
    let Some(pty) = bundle_pty() else { return };
    with_timeout(BUDGET, || {
        let (_output, status) =
            Session::start_in(pty, Command::new("cmd.exe").args(["/c", "exit", "42"])).finish();

        assert_eq!(status.code(), 42);
        assert!(!status.success());
    });
}

/// The end-of-file contract, on the bundle: a reader that starts *after* the
/// child is gone still reaches `Ok(0)` without anything being closed.
///
/// The write half is deliberately kept open throughout. Closing it would make
/// the console host tear the session down by itself, which produces
/// end-of-file even when the crate's own shutdown path is broken.
fn reading_past_the_child_exit_reaches_eof_in(pty: Pty) {
    const MARKER: &str = "conpty-oxide-bundle-eof-marker";

    let mut child = Command::new("cmd.exe")
        .args(["/c", "echo", MARKER])
        .spawn(&pty)
        .expect("spawning must succeed");
    let (mut reader, writer, controller) = pty.into_split();

    let status = child.wait().expect("waiting must succeed");
    assert!(status.success(), "unexpected status: {status}");

    let mut collected = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = reader.read(&mut chunk).expect("reading must not fail");
        if read == 0 {
            break;
        }
        collected.extend_from_slice(&chunk[..read]);
    }

    let text = strip_escapes(&String::from_utf8_lossy(&collected));
    assert!(
        text.contains(MARKER),
        "output written before exit was lost: {text:?}"
    );

    drop(writer);
    drop(controller);
}

#[test]
fn reading_past_the_child_exit_reaches_end_of_file_on_the_bundle() {
    let Some(pty) = bundle_pty() else { return };
    with_timeout(BUDGET, || reading_past_the_child_exit_reaches_eof_in(pty));
}

/// The same contract on the legacy shutdown path.
///
/// A bundled `conpty.dll` always exports `ReleasePseudoConsole`, so without
/// the strip this path would be unreachable through a bundle no matter which
/// Windows version the suite runs on — and it is the path where end-of-file
/// has to be *produced* by the crate rather than observed.
#[test]
fn reading_past_the_child_exit_reaches_end_of_file_on_the_bundles_legacy_path() {
    let Some(pty) = legacy_bundle_pty() else {
        return;
    };
    with_timeout(BUDGET, || reading_past_the_child_exit_reaches_eof_in(pty));
}

#[test]
fn the_child_observes_a_resize_on_the_bundle() {
    let initial = Size::new(24, 80);
    let resized = Size::new(30, 100);
    let Some(pty) = bundle_pty_with_size(initial) else {
        return;
    };
    with_timeout(BUDGET, || {
        let mut session = Session::start_in(pty, &mut Command::new("cmd.exe"));

        // Wait for the prompt before typing: a shell that has not started
        // reading its console yet would drop the command.
        session.output.wait_for(">", ANSWER);

        session.write_line("mode con");
        assert!(
            wait_until(ANSWER, || reported_size(&session.output.text())
                == expected_size(initial)),
            "the child never reported the session's initial size {initial}: {:?}",
            strip_escapes(&session.output.text())
        );

        session
            .controller
            .resize(resized)
            .expect("resizing a live session on the bundle must succeed");
        assert_eq!(session.controller.size(), resized);

        session.write_line("mode con");
        assert!(
            wait_until(ANSWER, || reported_size(&session.output.text())
                == expected_size(resized)),
            "the child never observed the resize to {resized}: {:?}",
            strip_escapes(&session.output.text())
        );

        session.write_line("exit");
        let (_output, status) = session.finish();
        assert!(status.success(), "unexpected status: {status}");
    });
}

#[test]
fn kill_terminates_the_whole_tree_on_the_bundle() {
    const ROOT_EXE: &str = "cmd.exe";
    const GRANDCHILD_EXE: &str = "ping.exe";

    let Some(pty) = bundle_pty() else { return };
    with_timeout(BUDGET, || {
        let mut session = Session::start_in(
            pty,
            Command::new(ROOT_EXE).args(["/c", "ping", "-t", "127.0.0.1"]),
        );
        let root = session.child.id();
        let grandchild = wait_for_descendant(root, GRANDCHILD_EXE, SETTLE);

        session.child.kill().expect("kill must succeed");
        assert!(
            wait_until(SETTLE, || !process_is_running(grandchild, GRANDCHILD_EXE)),
            "{GRANDCHILD_EXE} ({grandchild}) outlived the kill, so only the \
             root process was terminated instead of the whole tree"
        );
        assert!(
            wait_until(SETTLE, || !process_is_running(root, ROOT_EXE)),
            "the root child ({root}) outlived the kill"
        );

        let status = session.child.wait().expect("waiting must succeed");
        assert_eq!(status.code(), 1, "a killed tree reports exit code 1");

        // Still has to reach end-of-file afterwards; `finish` joins the
        // collector, which is what proves it.
        session.finish();
    });
}

/// Clearing the buffer, and the promise that the capability query does not
/// lie in either direction.
///
/// A bundled `conpty.dll` normally exports `ConptyClearPseudoConsole`, so this
/// is where the operation is actually performed rather than refused — the
/// system backend has no such export and every other test of `clear` can only
/// watch it decline. The test is written around the query rather than around
/// a hardcoded answer so that it stays honest on 32-bit x86, where the crate
/// deliberately does not offer the call.
///
/// What is asserted after a successful clear is that the session survives it:
/// `clear` is a signal to the console host, the child is not told, and the
/// exact repaint the host emits afterwards is its business. A wedged or
/// silently killed session is the failure that matters here.
#[test]
fn clearing_agrees_with_the_capability_query_on_the_bundle() {
    const BEFORE: &str = "conpty-oxide-before-clear";
    const AFTER: &str = "conpty-oxide-after-clear";

    let Some(pty) = bundle_pty() else { return };
    with_timeout(BUDGET, || {
        let supported = pty.supports_clear();
        let mut session = Session::start_in(pty, &mut Command::new("cmd.exe"));
        assert_eq!(session.controller.supports_clear(), supported);

        session.output.wait_for(">", ANSWER);
        session.write_line(&format!("echo {BEFORE}"));
        session.output.wait_for(BEFORE, ANSWER);

        match session.controller.clear() {
            Ok(()) => assert!(
                supported,
                "clear succeeded on a backend that reports no clear support"
            ),
            Err(Error::UnsupportedFeature { feature }) => {
                assert!(
                    !supported,
                    "clear was refused as unsupported on a backend that \
                     reports clear support"
                );
                assert_eq!(feature, "ClearPseudoConsole");
                return;
            }
            Err(other) => panic!("clearing the bundle's console failed: {other}"),
        }

        // The child is untouched by a clear, so the session must still carry
        // input to it and output back.
        session.write_line(&format!("echo {AFTER}"));
        session.output.wait_for(AFTER, ANSWER);

        session.write_line("exit");
        let (_output, status) = session.finish();
        assert!(status.success(), "unexpected status: {status}");
    });
}

/// The unchecked constructor skips the version comparison and nothing else, so
/// it must accept a bundle the checked one already accepted.
#[test]
fn the_unchecked_constructor_accepts_a_matching_bundle() {
    let Some(dir) = bundle_dir() else {
        eprintln!("skipped: {DLL_DIR_VAR} is not set");
        return;
    };
    let Some(checked) = bundle() else { return };

    let unchecked = ConPtyBackend::from_dir_unchecked(&dir)
        .expect("the unchecked constructor must accept a bundle from_dir accepted");
    assert_eq!(unchecked.kind(), checked.kind());
    assert_eq!(unchecked.supports_release(), checked.supports_release());
    assert_eq!(unchecked.supports_clear(), checked.supports_clear());
}

/// A `conpty.dll` with no `OpenConsole.exe` anywhere near it is rejected —
/// before it is loaded, and even when it is a perfectly good DLL.
///
/// The ordering is the point. `conpty.dll` launches `OpenConsole.exe` rather
/// than the system `conhost.exe`, so a lone DLL is not a degraded bundle but a
/// broken one, and the failure it would otherwise produce is a session that
/// never emits anything. Copying the real DLL when there is one to copy makes
/// this a statement about a *loadable* file rather than about a placeholder
/// the loader could have rejected for some other reason.
#[test]
fn a_dll_without_its_console_host_is_rejected() {
    let temp = TempDir::new("lone-dll");
    let dll = temp.path().join("conpty.dll");
    match bundle_dir() {
        Some(dir) => {
            fs::copy(dir.join("conpty.dll"), &dll).expect("copying the bundled dll must succeed");
        }
        None => fs::write(&dll, b"").expect("creating the placeholder dll must succeed"),
    }

    let err = ConPtyBackend::from_dir(temp.path())
        .expect_err("a bundle without a console host must be rejected");
    match err {
        BackendError::OpenConsoleMissing { dll: reported } => assert_eq!(reported, dll),
        other => panic!("expected OpenConsoleMissing, got {other:?}"),
    }
}
