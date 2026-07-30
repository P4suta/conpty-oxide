// SPDX-FileCopyrightText: 2025 conpty-oxide contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;

use std::sync::mpsc;
use std::time::Duration;

use crate::backend::ConPtyBackend;
use crate::core::pipes::{create_sync_pipes, SyncPipes};

/// Runs `f` on a helper thread and fails the test if it does not finish
/// within five seconds.
///
/// A watchdog is essential here: the failure mode under test is a hang
/// (e.g. `ClosePseudoConsole` blocking), which would otherwise stall the
/// whole test run forever instead of failing one test. The helper thread
/// signals completion over a channel; on timeout the *test* thread
/// panics, and the wedged helper is abandoned (the test harness can still
/// finish and report).
fn complete_within_5s(name: &str, f: impl FnOnce() + Send + 'static) {
    let (done_tx, done_rx) = mpsc::channel();
    thread::Builder::new()
        .name(format!("watchdog-subject-{name}"))
        .spawn(move || {
            f();
            let _ = done_tx.send(());
        })
        .expect("spawning the test subject thread must succeed");
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|_| panic!("`{name}` hung for more than 5 seconds"));
}

fn backend() -> ConPtyBackend {
    ConPtyBackend::system().expect("ConPTY must be available on a test machine")
}

/// Creates a console plus the two user-side pipe ends that remain ours.
fn console_and_user_ends(backend: ConPtyBackend) -> (PseudoConsole, OwnedHandle, OwnedHandle) {
    let SyncPipes {
        conout_read,
        conout_write,
        conin_read,
        conin_write,
    } = create_sync_pipes().expect("creating pipes must succeed");
    let console = PseudoConsole::new(backend, Size::default(), conin_read, conout_write, false)
        .expect("CreatePseudoConsole must succeed");
    (console, conout_read, conin_write)
}

#[test]
fn shared_core_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ConsoleShared>();
    assert_send_sync::<PseudoConsole>();
}

#[test]
fn create_yields_a_live_console() {
    let (console, _conout_read, _conin_write) = console_and_user_ends(backend());
    assert_ne!(console.hpcon(), 0);
    assert!(!console.is_released());
    assert!(!console.shared().is_closed());
}

#[test]
fn resize_succeeds_while_open_and_fails_after_close() {
    let (console, conout_read, conin_write) = console_and_user_ends(backend());
    console
        .resize(crate::size::test_size(50, 132))
        .expect("resize on a live console must succeed");

    // Retire the reader, then close.
    drop(conout_read);
    console.shared().notify_reader_closed();
    console.shared().request_close();
    assert!(console.shared().is_closed());

    let err = console
        .resize(crate::size::test_size(30, 100))
        .expect_err("resize after close must fail");
    assert_eq!(err.kind(), io::ErrorKind::NotConnected);
    drop(conin_write);
}

/// Whether `clear` works at all is a property of the backend, but the
/// *state machine* around it must behave the same everywhere: refuse once
/// the pseudoconsole has been closed, exactly as `resize` does.
#[test]
fn clear_matches_the_backend_capability_and_fails_after_close() {
    let backend = backend();
    let supported = backend.supports_clear();
    let (console, conout_read, conin_write) = console_and_user_ends(backend);

    match console.clear() {
        Ok(()) => assert!(supported, "clear succeeded without a clear export"),
        Err(err) if err.kind() == io::ErrorKind::Unsupported => {
            assert!(!supported, "clear refused although the export is present");
        },
        Err(err) => panic!("unexpected error: {err:?}"),
    }

    // Retire the reader, then close.
    drop(conout_read);
    console.shared().notify_reader_closed();
    console.shared().request_close();
    assert!(console.shared().is_closed());

    let err = console.clear().expect_err("clear after close must fail");
    assert_eq!(err.kind(), io::ErrorKind::NotConnected);
    drop(conin_write);
}

#[test]
fn drop_console_before_user_pipe_ends() {
    complete_within_5s("drop_console_before_user_pipe_ends", || {
        let (console, conout_read, conin_write) = console_and_user_ends(backend());
        let mode = Arc::new(AtomicU8::new(0));
        console.shared().observe_drop_mode(Arc::clone(&mode));
        drop(console);
        assert_eq!(
            mode.load(Ordering::SeqCst),
            1,
            "an open legacy reader requires the detached close path"
        );
        drop(conout_read);
        drop(conin_write);
    });
}

#[test]
fn final_defense_detaches_legacy_close_until_reader_eof() {
    for reader in [ReaderState::Open, ReaderState::Drained, ReaderState::Closed] {
        let legacy = backend().without_release();
        let (console, conout_read, conin_write) = console_and_user_ends(legacy);
        let mode = Arc::new(AtomicU8::new(0));
        console.shared().observe_drop_mode(Arc::clone(&mode));
        console.shared().lock().reader = reader;

        drop(console);
        let expected = if reader == ReaderState::Drained { 2 } else { 1 };
        assert_eq!(mode.load(Ordering::SeqCst), expected, "reader={reader:?}");
        drop(conout_read);
        drop(conin_write);
    }
}

#[test]
fn drop_user_pipe_ends_before_console() {
    complete_within_5s("drop_user_pipe_ends_before_console", || {
        let (console, conout_read, conin_write) = console_and_user_ends(backend());
        drop(conout_read);
        drop(conin_write);
        drop(console);
    });
}

#[test]
fn drop_after_reader_closed_notification() {
    complete_within_5s("drop_after_reader_closed_notification", || {
        let (console, conout_read, conin_write) = console_and_user_ends(backend());
        drop(conout_read);
        console.shared().notify_reader_closed();
        drop(console);
        drop(conin_write);
    });
}

#[test]
fn request_close_after_reader_closed_closes_inline() {
    complete_within_5s("request_close_after_reader_closed_closes_inline", || {
        let (console, conout_read, conin_write) = console_and_user_ends(backend());
        let shared = Arc::clone(console.shared());
        drop(conout_read);
        shared.notify_reader_closed();
        shared.request_close();
        assert!(shared.is_closed());
        // A second request is a no-op, not a double close.
        shared.request_close();
        drop(console);
        drop(conin_write);
    });
}

#[test]
fn legacy_request_close_with_live_reader_completes() {
    complete_within_5s("legacy_request_close_with_live_reader_completes", || {
        let (console, conout_read, conin_write) = console_and_user_ends(backend());
        let shared = Arc::clone(console.shared());
        // Reader still "Open" and the session was never released: this is
        // the legacy watcher's blocking-capable path. With no client and
        // an un-drained conout it must still complete, because closing is
        // exactly what ends a clientless session.
        shared.request_close();
        assert!(shared.is_closed());
        drop(console);
        drop(conout_read);
        drop(conin_write);
    });
}

#[cfg(any(feature = "blocking", feature = "tokio"))]
#[test]
fn input_close_worker_spawn_failure_keeps_close_retryable() {
    complete_within_5s(
        "input_close_worker_spawn_failure_keeps_close_retryable",
        || {
            let legacy = backend().without_release();
            let (console, conout_read, conin_write) = console_and_user_ends(legacy);
            let shared = Arc::clone(console.shared());

            shared.request_close_detached_with_worker_spawn_failure();
            assert!(
                !shared.is_closed(),
                "a failed handoff must not claim the live HPCON"
            );
            console
                .resize(crate::size::test_size(40, 100))
                .expect("the live HPCON must remain usable after the failed handoff");

            // A later request can claim the same HPCON. Retire the raw reader
            // first so the real worker has no output pipe to wait on.
            drop(conout_read);
            shared.notify_reader_closed();
            shared.request_close_detached();
            assert!(shared.is_closed());

            drop(conin_write);
            drop(console);
        },
    );
}

#[cfg(feature = "tracing")]
#[test]
fn close_worker_spawn_failure_is_logged() {
    let events = crate::tracing_test_support::count_events(|| {
        log_close_worker_spawn_failure(
            &io::Error::other("injected close worker failure"),
            "test-close-worker",
            true,
        );
    });
    assert_eq!(events, 1);
}

#[test]
fn notify_eof_executes_a_deferred_close() {
    let backend = backend();
    if !backend.supports_release() {
        // Deferral only happens in released mode; nothing to test here.
        return;
    }
    complete_within_5s("notify_eof_executes_a_deferred_close", || {
        let (console, conout_read, conin_write) = console_and_user_ends(backend);
        let shared = Arc::clone(console.shared());
        assert!(shared
            .release_after_spawn()
            .expect("ReleasePseudoConsole must succeed"));
        assert!(console.is_released());

        // With the reader open, a close request in released mode defers.
        shared.request_close();
        assert!(!shared.is_closed());

        // The reader hitting EOF executes the pending close.
        shared.notify_reader_eof();
        assert!(shared.is_closed());
        drop(console);
        drop(conout_read);
        drop(conin_write);
    });
}

#[test]
fn reader_close_executes_a_deferred_close() {
    let backend = std::env::var_os("CONPTY_OXIDE_TEST_DLL_DIR")
        .map(ConPtyBackend::from_dir)
        .transpose()
        .expect("the configured standalone backend must load")
        .unwrap_or_else(backend);
    if !backend.supports_release() {
        return;
    }
    complete_within_5s("reader_close_executes_a_deferred_close", || {
        let (console, conout_read, conin_write) = console_and_user_ends(backend);
        let shared = Arc::clone(console.shared());
        assert!(shared
            .release_after_spawn()
            .expect("ReleasePseudoConsole must succeed"));

        shared.request_close();
        assert!(!shared.is_closed());

        drop(conout_read);
        shared.notify_reader_closed();
        assert!(shared.is_closed());
        drop(console);
        drop(conin_write);
    });
}

#[test]
fn release_after_spawn_is_idempotent() {
    let backend = backend();
    if !backend.supports_release() {
        return;
    }
    complete_within_5s("release_after_spawn_is_idempotent", || {
        let (console, conout_read, conin_write) = console_and_user_ends(backend);
        assert!(console.release_after_spawn().expect("release must succeed"));
        assert!(console.release_after_spawn().expect("release must succeed"));
        assert!(!console.shared().release_failed());
        drop(conout_read);
        drop(conin_write);
        drop(console);
    });
}

#[test]
fn released_console_drops_cleanly_in_any_order() {
    let backend = backend();
    if !backend.supports_release() {
        return;
    }
    complete_within_5s("released_console_drops_cleanly_in_any_order", || {
        let (console, conout_read, conin_write) = console_and_user_ends(backend);
        assert!(console.release_after_spawn().expect("release must succeed"));
        drop(console);
        drop(conin_write);
        drop(conout_read);
    });
}

#[test]
fn reader_finished_tracks_notifications() {
    let (console, conout_read, conin_write) = console_and_user_ends(backend());
    let shared = Arc::clone(console.shared());
    assert!(!shared.reader_finished());
    // This test injects EOF directly instead of obtaining it from a read.
    // Retire the raw read end first so the resulting close is prompt even on
    // a legacy backend.
    drop(conout_read);
    shared.notify_reader_eof();
    assert!(shared.reader_finished());
    assert!(
        shared.is_closed(),
        "EOF proves the host exited and must retire the remaining HPCON"
    );
    // Drained -> Closed is a valid forward transition.
    shared.notify_reader_closed();
    assert!(shared.reader_finished());
    drop(conin_write);
    drop(console);
}
