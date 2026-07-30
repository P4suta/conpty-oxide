// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A resize is visible to the program running inside the session.
//!
//! `ResizePseudoConsole` is easy to call and easy to get wrong — the `COORD`
//! it takes is `(X = columns, Y = rows)`, the mirror image of the
//! `(rows, cols)` order the crate's own `Size` uses. A swapped pair still
//! succeeds, so nothing but asking the child what size it thinks it has can
//! catch it; that is why both dimensions are checked, and checked from the
//! same reply.
//!
//! `mode con` is that question: it prints the console screen buffer's
//! dimensions, which is exactly what a resize is supposed to change.

#![cfg(all(windows, feature = "blocking"))]

pub mod helpers;

use std::io;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use conpty_oxide::blocking::Command;
use conpty_oxide::{ErrorKind, SessionOptions, Size};

use helpers::sync::Session;
use helpers::{expected_size, reported_size, strip_escapes, wait_until, with_timeout};

const BUDGET: Duration = Duration::from_secs(60);

/// How long the shell gets to answer a typed command.
const ANSWER: Duration = Duration::from_secs(15);

/// Number of independently synchronized resize pairs used to exercise the
/// controller's cross-thread ordering.
const CONCURRENT_ROUNDS: u16 = 12;

/// Maximum time the resize racer may take to observe session shutdown.
const CLOSE_RACE_BUDGET: Duration = Duration::from_secs(10);

fn size(rows: u16, cols: u16) -> Size {
    Size::try_new(rows, cols).expect("test dimensions must be valid")
}

#[test]
fn the_child_observes_a_resize() {
    with_timeout(BUDGET, || {
        let initial = size(24, 80);
        let resized = size(30, 100);

        let mut session = Session::start_with(
            &mut Command::new("cmd.exe"),
            SessionOptions::new().size(initial),
        );

        // Wait for the prompt before typing, so the command cannot be sent to
        // a shell that has not started reading its console yet.
        session.output.wait_for(">", ANSWER);

        session.write_line("mode con");
        assert!(
            wait_until(ANSWER, || {
                reported_size(&session.output.text()) == Some(expected_size(initial))
            }),
            "the child never reported the session's initial size {initial}: {:?}",
            strip_escapes(&session.output.text())
        );

        session
            .controller
            .resize(resized)
            .expect("resizing a live session must succeed");
        assert_eq!(session.controller.size(), resized);

        session.write_line("mode con");
        assert!(
            wait_until(ANSWER, || {
                reported_size(&session.output.text()) == Some(expected_size(resized))
            }),
            "the child never observed the resize to {resized}: {:?}",
            strip_escapes(&session.output.text())
        );

        session.write_line("exit");
        let (_output, status) = session.finish();
        assert!(status.success(), "unexpected status: {status}");
    });
}

/// Concurrent successful resizes must have one total order shared by the
/// controller's cached size and the console host.
///
/// Calling the backend and updating a separate size lock would let two calls
/// complete as `A, B` but record themselves as `B, A`. Each round uses fresh
/// dimensions so an old `mode con` response cannot satisfy the assertion.
#[test]
fn concurrent_resizes_keep_the_controller_and_child_in_sync() {
    with_timeout(BUDGET, || {
        let initial = size(18, 70);
        let mut session = Session::start_with(
            &mut Command::new("cmd.exe"),
            SessionOptions::new().size(initial),
        );
        session.output.wait_for(">", ANSWER);

        for round in 0..CONCURRENT_ROUNDS {
            let first = size(20 + round * 2, 80 + round * 2);
            let second = size(21 + round * 2, 81 + round * 2);
            let gate = Arc::new(Barrier::new(3));

            let first_controller = session.controller.clone();
            let first_gate = Arc::clone(&gate);
            let first_resize = thread::spawn(move || {
                first_gate.wait();
                first_controller.resize(first)
            });

            let second_controller = session.controller.clone();
            let second_gate = Arc::clone(&gate);
            let second_resize = thread::spawn(move || {
                second_gate.wait();
                second_controller.resize(second)
            });

            gate.wait();
            first_resize
                .join()
                .expect("the first resize thread must not panic")
                .expect("the first concurrent resize must succeed");
            second_resize
                .join()
                .expect("the second resize thread must not panic")
                .expect("the second concurrent resize must succeed");

            let recorded = session.controller.size();
            assert!(
                recorded == first || recorded == second,
                "the controller recorded a size no caller submitted: {recorded}"
            );

            session.write_line("mode con");
            assert!(
                wait_until(ANSWER, || {
                    let observed = reported_size(&session.output.text());
                    observed == Some(expected_size(first))
                        || observed == Some(expected_size(second))
                }),
                "the child did not report either concurrent size: {:?}",
                strip_escapes(&session.output.text())
            );
            assert_eq!(
                reported_size(&session.output.text()),
                Some(expected_size(recorded)),
                "controller.size() diverged from the child after concurrent resizes"
            );
        }

        session.write_line("exit");
        let (_output, status) = session.finish();
        assert!(status.success(), "unexpected status: {status}");
    });
}

/// A resize racing root-process exit must either finish before close or
/// observe the normalized `NotConnected` result afterwards, never touch a
/// closed `HPCON` or hang behind teardown.
#[test]
fn resize_racing_session_close_transitions_to_not_connected() {
    with_timeout(BUDGET, || {
        let session = Session::start(Command::new("cmd.exe").args([
            "/d",
            "/c",
            "ping",
            "-n",
            "3",
            "127.0.0.1",
        ]));
        let Session {
            mut child,
            output,
            writer,
            controller,
        } = session;

        let racer = thread::spawn(move || {
            let deadline = Instant::now() + CLOSE_RACE_BUDGET;
            let mut next = size(24, 80);
            loop {
                match controller.resize(next) {
                    Ok(()) => {
                        next = if next == size(24, 80) {
                            size(25, 81)
                        } else {
                            size(24, 80)
                        };
                    },
                    Err(err) if err.kind() == ErrorKind::Resize => {
                        return err
                            .io_error()
                            .expect("resize failures retain their I/O error")
                            .kind();
                    },
                    Err(other) => panic!("unexpected resize failure during close: {other}"),
                }
                assert!(
                    Instant::now() < deadline,
                    "resize never observed the closing session"
                );
                thread::sleep(Duration::from_millis(2));
            }
        });

        let status = child.wait().expect("waiting for the probe must succeed");
        assert!(status.success(), "unexpected status: {status}");
        output.join();

        assert_eq!(
            racer.join().expect("the resize racer must not panic"),
            io::ErrorKind::NotConnected
        );
        drop(writer);
    });
}
