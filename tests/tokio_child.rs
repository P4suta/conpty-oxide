// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Waiting for, cancelling, and killing an async child.
//!
//! Two promises are tested here, and both are invisible until they break.
//!
//! `Child::kill` (and `kill_on_drop`) must terminate the whole *tree*.
//! `TerminateProcess` on the process this crate created would leave every
//! process that child started running — the usual outcome being an orphaned
//! build tool or shell pipeline that keeps holding files open. The only honest
//! way to test that is to look for the *grandchild* in the system process list
//! before and after, which is what the process-snapshot helpers do.
//!
//! `Child::wait` must be cancel-safe. Dropping the future has to lose nothing:
//! a later `wait` must still report the child's real exit status, not an
//! error, not a hang, and not a status invented by a second waiter.
//!
//! `ping` is the tool of choice for both: it is present on every Windows
//! installation, `-t` never exits on its own, and `-n` gives a predictable
//! couple of seconds of work.

#![cfg(all(windows, feature = "tokio"))]

pub mod helpers;

use std::time::Duration;

use conpty_oxide::tokio::Command;

use helpers::tokio_support::{poll_until, wait_for_descendant, within, Session};
use helpers::{process_is_running, watchdog};

/// Outer guard. Only a genuine deadlock gets anywhere near this.
const BUDGET: Duration = Duration::from_secs(60);

/// Per-test budget.
const DEADLINE: Duration = Duration::from_secs(45);

/// How long the grandchild gets to appear in the process list.
const APPEAR: Duration = Duration::from_secs(10);

/// How long the tree gets to disappear after being terminated.
const VANISH: Duration = Duration::from_secs(10);

const ROOT_EXE: &str = "cmd.exe";
const GRANDCHILD_EXE: &str = "ping.exe";

/// Arguments for a root child that spawns a never-ending grandchild.
const NEVER_ENDING: [&str; 4] = ["/c", "ping", "-t", "127.0.0.1"];

/// Arguments for a root child that takes about two seconds and then succeeds.
const A_FEW_SECONDS: [&str; 5] = ["/c", "ping", "-n", "3", "127.0.0.1"];

/// The exit code a terminated tree reports.
const KILL_EXIT_CODE: u32 = 1;

/// Asserts that both the root child and its grandchild are gone.
async fn assert_tree_terminated(root: u32, grandchild: u32) {
    assert!(
        poll_until(VANISH, || !process_is_running(grandchild, GRANDCHILD_EXE)).await,
        "{GRANDCHILD_EXE} ({grandchild}) outlived the kill, so only the root \
         process was terminated instead of the whole tree"
    );
    assert!(
        poll_until(VANISH, || !process_is_running(root, ROOT_EXE)).await,
        "the root child ({root}) outlived the kill"
    );
}

/// The body of the kill test, shared by both lifecycle modes: after a kill the
/// tree must be gone *and* the session must still reach end-of-file, whichever
/// shutdown path produces it.
async fn kill_terminates_the_whole_tree_in() {
    let mut session = Session::start(Command::new(ROOT_EXE).args(NEVER_ENDING));
    let root = session.child.id();
    assert_ne!(root, 0, "a spawned child must have a pid");
    let grandchild = wait_for_descendant(root, GRANDCHILD_EXE, APPEAR).await;

    assert!(
        session
            .child
            .try_wait()
            .expect("polling must succeed")
            .is_none(),
        "a running child must not report a status yet"
    );

    session.child.kill().expect("kill must succeed");
    assert_tree_terminated(root, grandchild).await;

    let status = session.child.wait().await.expect("waiting must succeed");
    assert!(!status.success());
    assert_eq!(
        status.code(),
        KILL_EXIT_CODE,
        "a killed tree reports exit code 1"
    );

    // The session must still reach end-of-file after a kill; joining the
    // reader task inside `finish` is what proves it.
    let (_output, again) = session.finish().await;
    assert_eq!(again, status, "the status must be cached, not re-read");
}

#[tokio::test]
async fn kill_terminates_the_whole_tree() {
    let _watchdog = watchdog(BUDGET);
    within("kill_terminates_the_whole_tree", DEADLINE, async {
        kill_terminates_the_whole_tree_in().await;
    })
    .await;
}

#[tokio::test]
async fn dropping_a_managed_child_terminates_the_whole_tree() {
    let _watchdog = watchdog(BUDGET);
    within(
        "dropping_a_kill_on_drop_child_terminates_the_whole_tree",
        DEADLINE,
        async {
            let session = Session::start(Command::new(ROOT_EXE).args(NEVER_ENDING));
            let Session {
                child,
                output,
                input,
                controller,
            } = session;

            let root = child.id();
            let grandchild = wait_for_descendant(root, GRANDCHILD_EXE, APPEAR).await;

            drop(child);
            assert_tree_terminated(root, grandchild).await;

            // With every client gone the session ends on its own, so the
            // reader task must come back rather than stay parked forever.
            output.join().await;
            input.close().await;
            drop(controller);
        },
    )
    .await;
}

#[tokio::test]
async fn dropping_a_managed_session_terminates_the_whole_tree() {
    let _watchdog = watchdog(BUDGET);
    within(
        "dropping_a_managed_session_terminates_the_whole_tree",
        DEADLINE,
        async {
            let session = Command::new(ROOT_EXE)
                .args(NEVER_ENDING)
                .spawn()
                .expect("managed spawning must succeed");
            let root = session.id();
            let grandchild = wait_for_descendant(root, GRANDCHILD_EXE, APPEAR).await;

            drop(session);
            assert_tree_terminated(root, grandchild).await;
        },
    )
    .await;
}

#[tokio::test]
async fn dropping_the_child_from_managed_parts_terminates_the_whole_tree() {
    let _watchdog = watchdog(BUDGET);
    within(
        "dropping_the_child_from_managed_parts_terminates_the_whole_tree",
        DEADLINE,
        async {
            let parts = Command::new(ROOT_EXE)
                .args(NEVER_ENDING)
                .spawn()
                .expect("managed spawning must succeed")
                .into_parts();
            let root = parts.child.id();
            let grandchild = wait_for_descendant(root, GRANDCHILD_EXE, APPEAR).await;

            drop(parts.child);
            assert_tree_terminated(root, grandchild).await;
            drop(parts.output);
            drop(parts.input);
            drop(parts.controller);
        },
    )
    .await;
}

#[tokio::test]
async fn cancelling_managed_output_collection_terminates_the_whole_tree() {
    let _watchdog = watchdog(BUDGET);
    Box::pin(within(
        "cancelling_managed_output_collection_terminates_the_whole_tree",
        DEADLINE,
        async {
            let session = Command::new(ROOT_EXE)
                .args(NEVER_ENDING)
                .spawn()
                .expect("managed spawning must succeed");
            let root = session.id();
            let grandchild = wait_for_descendant(root, GRANDCHILD_EXE, APPEAR).await;

            assert!(
                tokio::time::timeout(Duration::from_millis(50), session.wait_with_output())
                    .await
                    .is_err(),
                "the never-ending tree must still be collecting when cancelled"
            );
            assert_tree_terminated(root, grandchild).await;
        },
    ))
    .await;
}

/// A dropped `wait` future must lose nothing: the next `wait` has to return
/// the child's real exit status.
///
/// The child is chosen to outlive the cancellation by a comfortable margin and
/// then to exit *on its own* with a status worth checking — a killed child
/// would report the kill code whether or not the wait had been resumed
/// correctly, which is exactly the failure this is meant to catch.
#[tokio::test]
async fn a_cancelled_wait_still_reports_the_real_exit_status() {
    let _watchdog = watchdog(BUDGET);
    within(
        "a_cancelled_wait_still_reports_the_real_exit_status",
        DEADLINE,
        async {
            let mut session = Session::start(Command::new(ROOT_EXE).args(A_FEW_SECONDS));

            let cancelled = tokio::select! {
                _ = session.child.wait() => false,
                () = tokio::time::sleep(Duration::from_millis(200)) => true,
            };
            assert!(
                cancelled,
                "the child exited before its wait could be cancelled, so this \
                 test never exercised cancellation"
            );
            assert!(
                session
                    .child
                    .try_wait()
                    .expect("polling must succeed")
                    .is_none(),
                "a cancelled wait must not have reaped the child"
            );

            let status = session
                .child
                .wait()
                .await
                .expect("a retried wait must succeed");
            assert!(
                status.success(),
                "a retried wait must report the child's real status, got: {status}"
            );
            assert_eq!(status.code(), 0);

            let (_output, again) = session.finish().await;
            assert_eq!(again, status, "the status must be cached, not re-read");
        },
    )
    .await;
}

/// The Windows value `259` is also `STILL_ACTIVE`, and a high-bit status can
/// be corrupted by an accidental signed conversion. Both must survive the
/// registered-wait callback and the public cache verbatim.
#[tokio::test]
async fn registered_wait_preserves_sentinel_and_high_bit_exit_codes() {
    let _watchdog = watchdog(BUDGET);
    within(
        "registered_wait_preserves_sentinel_and_high_bit_exit_codes",
        DEADLINE,
        async {
            let (_output, sentinel) =
                Session::start(Command::new(ROOT_EXE).args(["/c", "exit", "259"]))
                    .finish()
                    .await;
            assert_eq!(sentinel.code(), 259);

            let (_output, high_bit) =
                Session::start(Command::new(ROOT_EXE).args(["/c", "exit", "-1073741510"]))
                    .finish()
                    .await;
            assert_eq!(high_bit.code(), 0xC000_013A);
        },
    )
    .await;
}

/// Registering a wait after the process is already signaled must complete
/// immediately; the callback may run during registration on this path.
#[tokio::test]
async fn wait_registered_after_exit_reports_the_cached_os_status() {
    let _watchdog = watchdog(BUDGET);
    within(
        "wait_registered_after_exit_reports_the_cached_os_status",
        DEADLINE,
        async {
            let mut session = Session::start(Command::new(ROOT_EXE).args(["/c", "exit", "37"]));
            tokio::time::sleep(Duration::from_millis(300)).await;

            let status = session
                .child
                .wait()
                .await
                .expect("waiting on an already-signaled process must succeed");
            assert_eq!(status.code(), 37);

            let (_output, again) = session.finish().await;
            assert_eq!(again, status, "the status must remain cached");
        },
    )
    .await;
}
