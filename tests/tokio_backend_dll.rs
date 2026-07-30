// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The bundled `conpty.dll` backend, driven from the asynchronous front end.
//!
//! `backend_dll.rs` already runs the crate's whole contract against a real
//! bundle through the blocking API, so this file does not repeat it. What it
//! covers is the part the two front ends do *not* share: an async session's
//! pipes are overlapped named pipes registered with a Tokio I/O driver, and
//! the console host on the other end of them comes out of `conpty.dll` and its
//! `OpenConsole.exe` rather than out of the operating system. Nothing but a
//! real bundle proves that combination works.
//!
//! It also gives `clear` its only chance to actually succeed: `kernel32.dll`
//! exports no `ClearPseudoConsole`, so every other clear test in the suite can
//! do no more than watch the crate decline.
//!
//! These tests need the bundle, so they are opt-in: set
//! `CONPTY_OXIDE_TEST_DLL_DIR` to a directory holding `conpty.dll` next to its
//! matching `OpenConsole.exe` and they run; leave it unset and each one
//! returns immediately with a note on stderr. `just fetch-conpty` produces such
//! a directory at `vendor/conpty`, `just test-dll` runs the suite with the
//! variable already pointed at it, and CI always sets it. Pointing the variable
//! at something that is not a loadable bundle is a failure rather than a skip.

#![cfg(all(windows, feature = "tokio"))]

pub mod helpers;

use std::env;
use std::path::PathBuf;
use std::time::Duration;

use conpty_oxide::tokio::Command;
use conpty_oxide::{ConPtyBackend, ErrorKind, SessionOptions};

use helpers::tokio_support::{within, Session};
use helpers::watchdog;

/// Names the directory holding the `conpty.dll` bundle to test against.
const DLL_DIR_VAR: &str = "CONPTY_OXIDE_TEST_DLL_DIR";

/// Outer guard. Only a genuine deadlock gets anywhere near this.
const BUDGET: Duration = Duration::from_secs(60);

/// Per-test budget. Every test here spawns a real console host out of the
/// bundle, which costs a few hundred milliseconds.
const DEADLINE: Duration = Duration::from_secs(45);

/// How long an interactive shell gets to answer a typed command.
const ANSWER: Duration = Duration::from_secs(15);

/// Loads the bundle, or reports the skip and returns `None`.
///
/// Note the asymmetry: a *missing* variable is a skip, but a variable that
/// points at a bundle which fails to load is a panic. The second case is a
/// broken bundle or a broken loader, and both are exactly what this file
/// exists to catch.
fn bundle() -> Option<ConPtyBackend> {
    let Some(dir) = env::var_os(DLL_DIR_VAR).map(PathBuf::from) else {
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

/// Builds managed options for an async session on the bundle.
fn bundle_options() -> Option<SessionOptions> {
    Some(SessionOptions::new().backend(bundle()?))
}

/// The smoke test: a child's output comes back through the registered pipes,
/// and the session reaches end-of-file on its own once the child is gone.
///
/// A bundled `conpty.dll` always exports `ReleasePseudoConsole`, so this is
/// the released lifecycle even on a Windows version whose own ConPTY predates
/// it: the console host exits when its last client disconnects, and awaiting
/// the reader task inside `finish` is what proves the reader saw that as a
/// clean `Ok(0)`.
#[tokio::test]
async fn an_async_session_on_the_bundle_echoes_and_reaches_end_of_file() {
    let _watchdog = watchdog(BUDGET);
    let Some(options) = bundle_options() else {
        return;
    };

    within("an_async_session_on_the_bundle", DEADLINE, async {
        const MARKER: &str = "conpty-oxide-async-bundle-echo";

        let session = Session::start_with(
            Command::new("cmd.exe").args(["/c", "echo", MARKER]),
            options,
        );
        assert_eq!(
            session.controller.supports_clear(),
            !cfg!(target_arch = "x86"),
            "a bundled conpty.dll exports ConptyClearPseudoConsole, and the \
             crate offers it everywhere except on 32-bit x86"
        );
        let (output, status) = session.finish().await;

        assert!(
            output.contains(MARKER),
            "the echoed marker is missing from the rendered output: {output:?}"
        );
        assert!(status.success(), "unexpected status: {status}");
        assert_eq!(status.code(), 0);
    })
    .await;
}

/// Clearing a live async session on the bundle, where the call is real.
///
/// What is asserted after a successful clear is that the session survives it:
/// `clear` is a signal to the console host, the child is not told, and the
/// exact repaint the host emits afterwards is its business. A wedged or
/// silently killed session is the failure that matters here.
///
/// 32-bit x86 is the deliberate exception the test is written around: the
/// export changed arity without changing its name (microsoft/terminal#18976),
/// and under `__stdcall` the callee pops the arguments, so the crate declines
/// rather than guess.
#[tokio::test]
async fn clearing_an_async_session_on_the_bundle_agrees_with_the_capability_query() {
    let _watchdog = watchdog(BUDGET);
    let Some(options) = bundle_options() else {
        return;
    };

    within("clearing_an_async_session_on_the_bundle", DEADLINE, async {
        const BEFORE: &str = "conpty-oxide-async-bundle-before-clear";
        const AFTER: &str = "conpty-oxide-async-bundle-after-clear";

        let mut session = Session::start_with(&mut Command::new("cmd.exe"), options);
        let supported = session.controller.supports_clear();

        session.output.wait_for(">", ANSWER).await;
        session.write_line(&format!("echo {BEFORE}"));
        session.output.wait_for(BEFORE, ANSWER).await;

        match session.controller.clear() {
            Ok(()) => assert!(
                supported,
                "clear succeeded on a backend that reports no clear support"
            ),
            Err(err) if err.kind() == ErrorKind::UnsupportedFeature => {
                assert!(
                    !supported,
                    "clear was refused as unsupported on a backend that reports \
                     clear support"
                );
                assert!(err.to_string().contains("ClearPseudoConsole"));
            },
            Err(other) => panic!("clearing the bundle's console failed: {other}"),
        }

        // The child is untouched by a clear, so the session must still carry
        // input to it and output back.
        session.write_line(&format!("echo {AFTER}"));
        session.output.wait_for(AFTER, ANSWER).await;

        session.write_line("exit");
        let (_output, status) = session.finish().await;
        assert!(status.success(), "unexpected status: {status}");
    })
    .await;
}
