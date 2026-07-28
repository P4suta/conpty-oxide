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
//! Each test runs twice, once on the machine's natural lifecycle mode and once
//! on a forced-legacy session. Where `ReleasePseudoConsole` exists the console
//! host exits on its own and end-of-file is merely *observed*; on the legacy
//! path the crate's watcher has to *produce* it, a second or so after the root
//! child exits. The observable contract must not differ.

#![cfg(all(windows, feature = "tokio"))]

mod helpers;

use std::time::Duration;

use tokio::io::AsyncReadExt;

use conpty_oxide::{Command, Pty};

use helpers::asyn::{legacy_pty, pty, within, Session};
use helpers::{strip_escapes, watchdog};

/// Outer guard. Only a genuine deadlock gets anywhere near this.
const BUDGET: Duration = Duration::from_secs(40);

/// Per-test budget; the legacy shutdown path spends about a second draining
/// before it closes, everything else is milliseconds.
const DEADLINE: Duration = Duration::from_secs(30);

/// The body of the read-past-exit test, shared by both lifecycle modes.
async fn reading_past_the_child_exit_reaches_eof_in(pty: Pty) {
    const MARKER: &str = "conpty-oxide-async-eof-marker";

    let mut child = Command::new("cmd.exe")
        .args(["/c", "echo", MARKER])
        .spawn(&pty)
        .expect("spawning must succeed");
    let (mut reader, writer, controller) = pty.into_split();

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
        async { reading_past_the_child_exit_reaches_eof_in(pty()).await },
    )
    .await;
}

#[tokio::test]
async fn reading_past_the_child_exit_reaches_end_of_file_on_the_forced_legacy_path() {
    let _watchdog = watchdog(BUDGET);
    within(
        "reading_past_the_child_exit_reaches_end_of_file_on_the_forced_legacy_path",
        DEADLINE,
        async { reading_past_the_child_exit_reaches_eof_in(legacy_pty()).await },
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
async fn the_output_written_before_exit_survives_in(pty: Pty) {
    const LINES: u32 = 15;

    let (output, status) = Session::start_in(
        pty,
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
        async { the_output_written_before_exit_survives_in(pty()).await },
    )
    .await;
}

#[tokio::test]
async fn the_output_written_before_exit_survives_the_forced_legacy_shutdown() {
    let _watchdog = watchdog(BUDGET);
    within(
        "the_output_written_before_exit_survives_the_forced_legacy_shutdown",
        DEADLINE,
        async { the_output_written_before_exit_survives_in(legacy_pty()).await },
    )
    .await;
}
