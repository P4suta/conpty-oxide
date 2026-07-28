//! The end-of-file contract.
//!
//! This is the promise the crate exists to make: a reader that keeps reading
//! after the child has exited eventually gets `Ok(0)`, without having to close
//! anything, poll anything, or know which Windows version it is running on.
//! Getting it wrong is not a visible bug — it is a reader thread that never
//! returns — so it is tested directly rather than inferred from a passing
//! `read_to_end`.
//!
//! Every test here keeps the write half open for the child's whole life. That
//! is deliberate: closing the input pipe makes the console host tear the
//! session down by itself, which would produce end-of-file even if the crate's
//! own shutdown path were completely broken.

#![cfg(all(windows, feature = "blocking"))]

mod helpers;

use std::io::Read;
use std::time::{Duration, Instant};

use conpty_oxide::blocking::Command;

use helpers::{pty, strip_escapes, with_timeout, Session};

/// Per-test budget; the legacy shutdown path spends about a second draining
/// before it closes, everything else is milliseconds.
const BUDGET: Duration = Duration::from_secs(30);

/// The bound `wait` is expected to respect for a child that exits at once.
const WAIT_BUDGET: Duration = Duration::from_secs(2);

#[test]
fn reading_past_the_child_exit_reaches_end_of_file() {
    with_timeout(BUDGET, || {
        const MARKER: &str = "conpty-oxide-eof-marker";

        let pty = pty();
        let mut child = Command::new("cmd.exe")
            .args(["/c", "echo", MARKER])
            .spawn(&pty)
            .expect("spawning must succeed");
        let (mut reader, writer, controller) = pty.into_split();

        // `echo` writes far less than a pipe buffer, so the child can run to
        // completion with nobody reading. Waiting first is what makes this
        // test meaningful: the reader below starts only once the child is
        // already gone.
        let status = child.wait().expect("waiting must succeed");
        assert!(status.success(), "unexpected status: {status}");

        let mut collected = Vec::new();
        let mut chunk = [0_u8; 4096];
        let reads_to_eof = loop {
            let read = reader.read(&mut chunk).expect("reading must not fail");
            if read == 0 {
                break true;
            }
            collected.extend_from_slice(&chunk[..read]);
        };
        assert!(reads_to_eof);

        // End-of-file is sticky: once reported it stays reported, so a reader
        // loop cannot be restarted into a hang by a stray extra read.
        assert_eq!(
            reader
                .read(&mut chunk)
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
    });
}

#[test]
fn waiting_for_a_short_child_returns_promptly() {
    with_timeout(BUDGET, || {
        // The output is drained by a collector thread throughout, which is the
        // arrangement ConPTY requires; `wait` must then return as soon as the
        // child is gone rather than waiting for any part of the teardown.
        let mut session = Session::start(Command::new("cmd.exe").args(["/c", "echo", "prompt"]));

        let started = Instant::now();
        let status = session.child.wait().expect("waiting must succeed");
        let elapsed = started.elapsed();

        assert!(status.success(), "unexpected status: {status}");
        assert!(
            elapsed < WAIT_BUDGET,
            "waiting for a child that exits immediately took {elapsed:?}, \
             which is over the {WAIT_BUDGET:?} budget"
        );

        session.finish();
    });
}

#[test]
fn the_output_written_before_exit_survives_the_shutdown() {
    with_timeout(BUDGET, || {
        const LINES: u32 = 15;

        // Fifteen lines fit inside the 24-row viewport, so nothing scrolls
        // away; each carries a unique marker so a truncated tail is visible as
        // a specific missing line rather than as a shorter blob.
        let (output, status) = Session::start(
            Command::new("cmd.exe")
                .raw_arg("/c for /l %i in (1,1,15) do @echo conpty-oxide-line-%i-end"),
        )
        .finish();

        assert!(status.success(), "unexpected status: {status}");
        for line in 1..=LINES {
            let marker = format!("conpty-oxide-line-{line}-end");
            assert!(
                output.contains(&marker),
                "{marker:?} is missing, so the session ended before the reader \
                 had drained the output: {output:?}"
            );
        }
    });
}
