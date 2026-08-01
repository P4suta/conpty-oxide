// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The end-of-file contract, for the asynchronous front end.
//!
//! This is the promise the crate exists to make: a reader that keeps reading
//! after the child has exited eventually gets `Ok(0)`, without having to close
//! anything, poll anything, or know which Windows version it is running on.
//! Getting it wrong is not a visible bug — it is a task that never completes —
//! so it is tested directly rather than inferred from a passing `read_to_end`.
//!
//! Every test here keeps the input pipe open for the child's whole life. That
//! is deliberate: closing it makes the console host tear the session down by
//! itself, which would produce end-of-file even if the crate's own shutdown
//! path were completely broken.
//!
//! The CI matrix runs these tests on both lifecycle modes. Where
//! `ReleasePseudoConsole` exists EOF is observed naturally; on legacy Windows
//! the crate's registered wait and close worker must produce it.

#![cfg(all(windows, feature = "tokio"))]

pub mod helpers;

use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;

use conpty_oxide::tokio::Command;

use helpers::tokio_support::{within, Session};
use helpers::{strip_escapes, watchdog};

/// Outer guard. Only a genuine deadlock gets anywhere near this.
const BUDGET: Duration = Duration::from_secs(40);

/// Per-test budget; the legacy shutdown path spends about a second draining
/// before it closes, everything else is milliseconds.
const DEADLINE: Duration = Duration::from_secs(30);

/// The bound `wait` is expected to respect for a child that exits at once.
const WAIT_BUDGET: Duration = Duration::from_secs(2);

/// The body of the read-past-exit test, shared by both lifecycle modes.
async fn reading_past_the_child_exit_reaches_eof_in() {
    const MARKER: &str = "conpty-oxide-async-eof-marker";

    let parts = Command::new("cmd.exe")
        .args(["/c", "echo", MARKER])
        .spawn()
        .expect("spawning must succeed")
        .into_parts();
    let mut child = parts.child;
    let mut reader = parts.output;
    let writer = parts.input;
    let controller = parts.controller;

    // `echo` writes far less than a pipe buffer, so the child can run to
    // completion with nobody reading. Waiting first is what makes this test
    // meaningful: the reads below start only once the child is already gone.
    let status = child.wait().await.expect("waiting must succeed");
    assert!(status.success(), "unexpected status: {status}");

    let mut collected = Vec::new();
    let mut chunk = [0_u8; 4096];
    let reads_to_eof = loop {
        let read = reader
            .read(&mut chunk)
            .await
            .expect("reading must not fail");
        if read == 0 {
            break true;
        }
        collected.extend_from_slice(&chunk[..read]);
    };
    assert!(reads_to_eof);

    // End-of-file is sticky: once reported it stays reported, so a reader loop
    // cannot be restarted into a hang by a stray extra read.
    assert_eq!(
        reader
            .read(&mut chunk)
            .await
            .expect("a read after EOF must not fail"),
        0,
        "end-of-file must be reported for every subsequent read"
    );

    // The output produced before the child exited is still there — the
    // shutdown that produced end-of-file did not swallow it.
    let text = strip_escapes(&String::from_utf8_lossy(&collected));
    assert!(
        text.contains(MARKER),
        "output written before exit was lost: {text:?}"
    );

    drop(writer);
    drop(controller);
}

#[tokio::test]
async fn reading_past_the_child_exit_reaches_end_of_file() {
    let _watchdog = watchdog(BUDGET);
    within(
        "reading_past_the_child_exit_reaches_end_of_file",
        DEADLINE,
        reading_past_the_child_exit_reaches_eof_in(),
    )
    .await;
}

/// `Child::wait` must resolve as soon as the child is gone, not after any
/// part of the session's teardown. The async wait is a stored Windows
/// registered wait, so the latency bound from `eof_semantics.rs` is asserted
/// here as well.
#[tokio::test]
async fn waiting_for_a_short_child_resolves_promptly() {
    let _watchdog = watchdog(BUDGET);
    within(
        "waiting_for_a_short_child_resolves_promptly",
        DEADLINE,
        async {
            // The output is drained by the session's reader task throughout,
            // which is the arrangement ConPTY requires; `wait` must then
            // resolve as soon as the child is gone.
            let mut session =
                Session::start(Command::new("cmd.exe").args(["/c", "echo", "prompt"]));

            let started = Instant::now();
            let status = session.child.wait().await.expect("waiting must succeed");
            let elapsed = started.elapsed();

            assert!(status.success(), "unexpected status: {status}");
            assert!(
                elapsed < WAIT_BUDGET,
                "waiting for a child that exits immediately took {elapsed:?}, \
                 which is over the {WAIT_BUDGET:?} budget"
            );

            let (_output, status) = session.finish().await;
            assert!(status.success(), "exit 0 must report success: {status:?}");
        },
    )
    .await;
}

/// Nothing the child wrote before exiting may be lost to the shutdown.
///
/// Fifteen lines fit inside the 24-row viewport, so nothing scrolls away; each
/// carries a unique marker so a truncated tail shows up as a specific missing
/// line rather than as a shorter blob. On the legacy path this is the pointed
/// test of the watcher's grace period, which is the window in which the reader
/// has to have drained the console host's remaining output.
async fn the_output_written_before_exit_survives_in() {
    const LINES: u32 = 15;

    let (output, status) = Session::start(
        Command::new("cmd.exe")
            .raw_arg("/c for /l %i in (1,1,15) do @echo conpty-oxide-async-line-%i-end"),
    )
    .finish()
    .await;

    assert!(status.success(), "unexpected status: {status}");
    for line in 1..=LINES {
        let marker = format!("conpty-oxide-async-line-{line}-end");
        assert!(
            output.contains(&marker),
            "{marker:?} is missing, so the session ended before the reader had \
             drained the output: {output:?}"
        );
    }
}

#[tokio::test]
async fn the_output_written_before_exit_survives_the_shutdown() {
    let _watchdog = watchdog(BUDGET);
    within(
        "the_output_written_before_exit_survives_the_shutdown",
        DEADLINE,
        the_output_written_before_exit_survives_in(),
    )
    .await;
}
