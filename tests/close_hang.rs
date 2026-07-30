// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

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

pub mod helpers;

use std::time::{Duration, Instant};

use conpty_oxide::blocking::{Command, Session};

use helpers::{wait_until, with_timeout};

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

/// Runs one drop-order case: flood a managed session, read nothing, then hand
/// it to `teardown` and require it to finish quickly.
///
/// The CI matrix runs this on both legacy and released Windows versions.
fn drop_order_completes(teardown: impl FnOnce(Session)) {
    with_timeout(BUDGET, || {
        let mut session = Command::new("cmd.exe")
            .raw_arg(FLOOD)
            .spawn()
            .expect("spawning must succeed");

        // Give the console host time to fill the pipe, and confirm that it
        // did: a child that already exited would mean the output fit after
        // all, and the teardown below would be testing nothing.
        let exited = wait_until(FILL, || {
            session.try_wait().expect("polling must succeed").is_some()
        });
        assert!(
            !exited,
            "the child finished despite nobody reading its output, so the \
             pipe buffer never filled and this case is not exercising a \
             blocked console host"
        );

        let started = Instant::now();
        teardown(session);
        let elapsed = started.elapsed();
        assert!(
            elapsed < TEARDOWN_BUDGET,
            "tearing the session down took {elapsed:?}, over the \
             {TEARDOWN_BUDGET:?} budget"
        );
    });
}

/// Drop order: read half, write half, controller.
fn read_half_first(session: Session) {
    let parts = session.into_parts();
    drop(parts.output);
    drop(parts.input);
    drop(parts.controller);
    drop(parts.child);
}

/// Drop order: controller, write half, read half.
///
/// The awkward order: the controller owns the pseudoconsole, so this drops it
/// while a live read half is still registered and the console host is blocked
/// writing.
fn controller_first(session: Session) {
    let parts = session.into_parts();
    drop(parts.controller);
    drop(parts.input);
    drop(parts.output);
    drop(parts.child);
}

/// Drop order: write half, controller, read half.
fn write_half_first(session: Session) {
    let parts = session.into_parts();
    drop(parts.input);
    drop(parts.controller);
    drop(parts.output);
    drop(parts.child);
}

#[test]
fn dropping_the_whole_session_completes() {
    drop_order_completes(drop);
}

#[test]
fn dropping_the_read_half_first_completes() {
    drop_order_completes(read_half_first);
}

#[test]
fn dropping_the_controller_first_completes() {
    drop_order_completes(controller_first);
}

#[test]
fn dropping_the_write_half_first_completes() {
    drop_order_completes(write_half_first);
}
