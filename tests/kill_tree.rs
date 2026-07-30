// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Killing a session kills the whole process tree.
//!
//! `TerminateProcess` on the process this crate created would leave every
//! process that child started running — the usual outcome being an orphaned
//! build tool or shell pipeline that keeps holding files open. The crate
//! promises otherwise, and the only honest way to test that promise is to
//! look for the *grandchild* in the system process list before and after.
//!
//! `ping -t` is the grandchild of choice: it never exits on its own, it is
//! present on every Windows installation, and it emits a line a second rather
//! than a flood.

#![cfg(all(windows, feature = "blocking"))]

pub mod helpers;

use std::time::Duration;

use conpty_oxide::blocking::Command;

use helpers::sync::Session;
use helpers::{process_is_running, wait_for_descendant, wait_until, with_timeout};

const BUDGET: Duration = Duration::from_secs(30);

/// How long the grandchild gets to appear in the process list.
const APPEAR: Duration = Duration::from_secs(10);

/// How long the tree gets to disappear after being terminated.
const VANISH: Duration = Duration::from_secs(10);

const ROOT_EXE: &str = "cmd.exe";
const GRANDCHILD_EXE: &str = "ping.exe";

/// Arguments for a root child that spawns a never-ending grandchild.
const NEVER_ENDING: [&str; 4] = ["/c", "ping", "-t", "127.0.0.1"];

/// Asserts that both the root child and its grandchild are gone.
fn assert_tree_terminated(root: u32, grandchild: u32) {
    assert!(
        wait_until(VANISH, || !process_is_running(grandchild, GRANDCHILD_EXE)),
        "{GRANDCHILD_EXE} ({grandchild}) outlived the kill, so only the root \
         process was terminated instead of the whole tree"
    );
    assert!(
        wait_until(VANISH, || !process_is_running(root, ROOT_EXE)),
        "the root child ({root}) outlived the kill"
    );
}

/// After a kill, the tree must be gone and the session must reach EOF. The CI
/// matrix runs this against both lifecycle modes.
fn kill_terminates_the_whole_tree_in() {
    let mut session = Session::start(Command::new(ROOT_EXE).args(NEVER_ENDING));
    let root = session.child.id();
    let grandchild = wait_for_descendant(root, GRANDCHILD_EXE, APPEAR);

    session.child.kill().expect("kill must succeed");
    assert_tree_terminated(root, grandchild);

    let status = session.child.wait().expect("waiting must succeed");
    assert!(!status.success());
    assert_eq!(status.code(), 1, "a killed tree reports exit code 1");

    // The session must still reach end-of-file after a kill; joining the
    // collector inside `finish` is what proves it.
    let (_output, again) = session.finish();
    assert_eq!(again, status, "the exit status must remain cached");
}

#[test]
fn kill_terminates_the_whole_tree() {
    with_timeout(BUDGET, || {
        kill_terminates_the_whole_tree_in();
    });
}

#[test]
fn dropping_a_managed_child_terminates_the_whole_tree() {
    with_timeout(BUDGET, || {
        let session = Session::start(Command::new(ROOT_EXE).args(NEVER_ENDING));
        let Session {
            child,
            output,
            writer,
            controller,
        } = session;

        let root = child.id();
        let grandchild = wait_for_descendant(root, GRANDCHILD_EXE, APPEAR);

        drop(child);
        assert_tree_terminated(root, grandchild);

        // With every client gone the session ends on its own, so the reader
        // thread must come back rather than block forever.
        output.join();
        drop(writer);
        drop(controller);
    });
}

#[test]
fn dropping_a_managed_session_terminates_the_whole_tree() {
    with_timeout(BUDGET, || {
        let session = Command::new(ROOT_EXE)
            .args(NEVER_ENDING)
            .spawn()
            .expect("managed spawning must succeed");
        let root = session.id();
        let grandchild = wait_for_descendant(root, GRANDCHILD_EXE, APPEAR);

        drop(session);
        assert_tree_terminated(root, grandchild);
    });
}

#[test]
fn dropping_the_child_from_managed_parts_terminates_the_whole_tree() {
    with_timeout(BUDGET, || {
        let parts = Command::new(ROOT_EXE)
            .args(NEVER_ENDING)
            .spawn()
            .expect("managed spawning must succeed")
            .into_parts();
        let root = parts.child.id();
        let grandchild = wait_for_descendant(root, GRANDCHILD_EXE, APPEAR);

        drop(parts.child);
        assert_tree_terminated(root, grandchild);
        drop(parts.output);
        drop(parts.input);
        drop(parts.controller);
    });
}
