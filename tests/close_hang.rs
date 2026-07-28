//! Teardown must never hang, in any drop order, with the pipe buffer full.
//!
//! This is the crate's headline regression. `ClosePseudoConsole` is documented
//! to block indefinitely unless the output pipe is closed first or kept
//! drained, so the dangerous state is exactly the one a naive caller reaches:
//! a child flooding the console, nobody reading, and the session being dropped.
//! A library that gets this wrong does not return an error — it stops.
//!
//! Each test drives a child that writes far more than a pipe buffer can hold,
//! never reads a byte of it, and then destroys the session in one particular
//! order. All four orders must complete promptly.

#![cfg(all(windows, feature = "blocking"))]

mod helpers;

use std::time::{Duration, Instant};

use conpty_oxide::blocking::{Command, Pty};

use helpers::{legacy_pty, pty, wait_until, with_timeout};

/// Roughly 280 KiB of console output, well past any pipe buffer.
///
/// `raw_arg` is required here: `cmd.exe` parses its own command line, so the
/// loop must reach it verbatim rather than quoted as a single argument.
const FLOOD: &str = "/c for /l %i in (1,1,4000) do @echo \
     0123456789012345678901234567890123456789012345678901234567890123456789";

/// Outer guard. Only a genuine deadlock gets anywhere near this.
const BUDGET: Duration = Duration::from_secs(20);

/// How long the child is left flooding before the session is destroyed.
const FILL: Duration = Duration::from_millis(500);

/// What "prompt" means for the teardown itself.
const TEARDOWN_BUDGET: Duration = Duration::from_secs(5);

/// Runs one drop-order case: flood the session built by `build`, read
/// nothing, then hand the `Pty` to `teardown` and require it to finish
/// quickly.
///
/// Every case runs twice — once on the machine's natural lifecycle mode and
/// once on a forced-legacy session — because the two modes tear down through
/// different code paths (a released session's close never blocks; a legacy
/// one relies on the drop order having retired the reader first).
fn drop_order_completes(build: impl FnOnce() -> Pty, teardown: impl FnOnce(Pty)) {
    with_timeout(BUDGET, || {
        let pty = build();
        // `kill_on_drop` is cleanup, not part of the scenario: the child is
        // dropped only after the session is gone, so the tree cannot outlive
        // the test even though the pseudoconsole it was attached to is dead.
        let mut child = Command::new("cmd.exe")
            .raw_arg(FLOOD)
            .kill_on_drop(true)
            .spawn(&pty)
            .expect("spawning must succeed");

        // Give the console host time to fill the pipe, and confirm that it
        // did: a child that already exited would mean the output fit after
        // all, and the teardown below would be testing nothing.
        let exited = wait_until(FILL, || {
            child.try_wait().expect("polling must succeed").is_some()
        });
        assert!(
            !exited,
            "the child finished despite nobody reading its output, so the \
             pipe buffer never filled and this case is not exercising a \
             blocked console host"
        );

        let started = Instant::now();
        teardown(pty);
        let elapsed = started.elapsed();
        assert!(
            elapsed < TEARDOWN_BUDGET,
            "tearing the session down took {elapsed:?}, over the \
             {TEARDOWN_BUDGET:?} budget"
        );

        drop(child);
    });
}

/// Drop order: read half, write half, controller.
fn read_half_first(pty: Pty) {
    let (reader, writer, controller) = pty.into_split();
    drop(reader);
    drop(writer);
    drop(controller);
}

/// Drop order: controller, write half, read half.
///
/// The awkward order: the controller owns the pseudoconsole, so this drops it
/// while a live read half is still registered and the console host is blocked
/// writing.
fn controller_first(pty: Pty) {
    let (reader, writer, controller) = pty.into_split();
    drop(controller);
    drop(writer);
    drop(reader);
}

/// Drop order: write half, controller, read half.
fn write_half_first(pty: Pty) {
    let (reader, writer, controller) = pty.into_split();
    drop(writer);
    drop(controller);
    drop(reader);
}

#[test]
fn dropping_the_whole_session_completes() {
    drop_order_completes(pty, drop);
}

#[test]
fn dropping_the_read_half_first_completes() {
    drop_order_completes(pty, read_half_first);
}

#[test]
fn dropping_the_controller_first_completes() {
    drop_order_completes(pty, controller_first);
}

#[test]
fn dropping_the_write_half_first_completes() {
    drop_order_completes(pty, write_half_first);
}

#[test]
fn dropping_the_whole_forced_legacy_session_completes() {
    drop_order_completes(legacy_pty, drop);
}

#[test]
fn dropping_the_read_half_of_a_forced_legacy_session_first_completes() {
    drop_order_completes(legacy_pty, read_half_first);
}

#[test]
fn dropping_the_controller_of_a_forced_legacy_session_first_completes() {
    drop_order_completes(legacy_pty, controller_first);
}

#[test]
fn dropping_the_write_half_of_a_forced_legacy_session_first_completes() {
    drop_order_completes(legacy_pty, write_half_first);
}
