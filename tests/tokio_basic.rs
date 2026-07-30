// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end smoke tests for the asynchronous API.
//!
//! These answer the first questions anyone has about an async pseudoconsole
//! library: does the child's output come back, does its exit code come back,
//! is this actually a *console* rather than a plain redirected pipe, and can
//! the two directions really be driven from separate tasks at the same time?
//!
//! Every test here holds two guards. `tokio_support::within` is the ordinary
//! timeout
//! and reports a stalled session as one named failure; `helpers::watchdog`
//! kills the process, and is the only thing that helps when a destructor
//! blocks the runtime thread and takes the timer with it.

#![cfg(all(windows, feature = "tokio"))]

pub mod helpers;

use std::io;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use conpty_oxide::tokio::Command;
use conpty_oxide::{ErrorKind, ExitStatus, SessionOptions, Size};

use helpers::tokio_support::{within, Session};
use helpers::{expected_size, reported_size, watchdog};

/// Outer guard. Only a genuine deadlock gets anywhere near this.
const BUDGET: Duration = Duration::from_secs(40);

/// Per-test budget. Spawning `cmd.exe` under a fresh pseudoconsole takes a few
/// hundred milliseconds at worst; anything approaching this is a deadlock.
const DEADLINE: Duration = Duration::from_secs(30);

/// How long an interactive shell gets to answer a typed command.
const ANSWER: Duration = Duration::from_secs(15);

/// Number of independently synchronized resize pairs used to exercise the
/// controller's cross-thread ordering.
const CONCURRENT_ROUNDS: u16 = 12;

/// Maximum time the resize racer may take to observe session shutdown.
const CLOSE_RACE_BUDGET: Duration = Duration::from_secs(10);

fn size(rows: u16, cols: u16) -> Size {
    Size::try_new(cols, rows).expect("test dimensions must be valid")
}

/// The escape character that introduces every ANSI sequence.
const ESC: u8 = 0x1b;

/// Runs `cmd.exe` with `args` to completion in a fresh session.
async fn run_cmd(args: &[&str]) -> (String, ExitStatus) {
    Session::start(Command::new("cmd.exe").args(args))
        .finish()
        .await
}

#[tokio::test]
async fn echoed_text_comes_back_with_a_successful_status() {
    let _watchdog = watchdog(BUDGET);
    within("echoed_text_comes_back", DEADLINE, async {
        const MARKER: &str = "conpty-oxide-async-basic-echo";
        let (output, status) = run_cmd(&["/c", "echo", MARKER]).await;

        assert!(
            output.contains(MARKER),
            "the echoed marker is missing from the rendered output: {output:?}"
        );
        assert!(status.success(), "unexpected status: {status}");
        assert_eq!(status.code(), 0);
    })
    .await;
}

#[tokio::test]
async fn a_nonzero_exit_code_is_reported_verbatim() {
    let _watchdog = watchdog(BUDGET);
    within(
        "a_nonzero_exit_code_is_reported_verbatim",
        DEADLINE,
        async {
            let (_output, status) = run_cmd(&["/c", "exit", "42"]).await;

            assert_eq!(status.code(), 42);
            assert!(!status.success());
            // 259 is `STILL_ACTIVE`; seeing it here would mean the exit code was
            // read before the process actually exited.
            assert_ne!(status.code(), 259);
        },
    )
    .await;
}

#[tokio::test]
async fn the_output_is_a_virtual_terminal_stream() {
    let _watchdog = watchdog(BUDGET);
    within("the_output_is_a_virtual_terminal_stream", DEADLINE, async {
        let (bytes, status) = Session::start(Command::new("cmd.exe").args(["/c", "echo", "vt"]))
            .finish_raw()
            .await;

        // A plain redirected pipe carries the child's bytes and nothing else.
        // A console host renders into a virtual terminal, so the stream also
        // carries the sequences it used to do so — this is the proof that the
        // child really ran attached to a pseudoconsole.
        assert!(
            bytes.contains(&ESC),
            "no escape sequences in the output, so this was not a pseudoconsole: {:?}",
            String::from_utf8_lossy(&bytes)
        );
        assert!(status.success());
    })
    .await;
}

/// A full interactive conversation with a shell, with the two directions of
/// the session owned by different tasks.
///
/// This is the shape the async front end exists for, and the one where a
/// mistake is invisible in the simpler tests: the reader task must keep
/// draining while the test blocks on an answer, the writer task must keep the
/// input pipe alive while the shell is running, and the session has to reach
/// end-of-file once the shell exits on its own — not because anything was
/// closed from this side.
///
/// `dir` is answered by looking for `Cargo.toml`, which is in the directory
/// the shell is started in and is spelled the same in every Windows display
/// language. The headers `dir` prints around it are not. `DIRCMD` is cleared
/// because `dir` reads its default switches from there, and a developer who
/// has `/p` in it would otherwise get a paged listing that waits for a
/// keypress this test never sends.
#[tokio::test]
async fn an_interactive_session_ends_when_the_shell_exits() {
    let _watchdog = watchdog(BUDGET);
    within("an_interactive_session", DEADLINE, async {
        let mut session = Session::start(
            Command::new("cmd.exe")
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .env_remove("DIRCMD"),
        );

        // Wait for the prompt before typing, so the command cannot be sent to
        // a shell that has not started reading its console yet.
        session.output.wait_for(">", ANSWER).await;

        session.write_line("dir");
        session.output.wait_for("Cargo.toml", ANSWER).await;

        session.write_line("exit");
        let (output, status) = session.finish().await;

        assert!(status.success(), "unexpected status: {status}");
        assert!(
            output.contains("Cargo.toml"),
            "the directory listing is missing from the collected output: {output:?}"
        );
    })
    .await;
}

/// A resize is visible to the program running inside the session.
///
/// `ResizePseudoConsole` takes a `COORD` of `(X = columns, Y = rows)`, the
/// same order as `Size::try_new`, and a swapped pair still succeeds —
/// so nothing but asking the child what size it thinks it has can catch it.
/// `mode con` is that question, and both dimensions are checked from the same
/// reply.
#[tokio::test]
async fn the_child_observes_a_resize() {
    let _watchdog = watchdog(BUDGET);
    within("the_child_observes_a_resize", DEADLINE, async {
        let initial = size(24, 80);
        let resized = size(30, 100);

        let mut session = Session::start_with(
            &mut Command::new("cmd.exe"),
            SessionOptions::new().size(initial),
        );
        session.output.wait_for(">", ANSWER).await;

        session.write_line("mode con");
        session
            .output
            .wait_until_rendered("the session's initial size", ANSWER, |text| {
                reported_size(text) == Some(expected_size(initial))
            })
            .await;

        session
            .controller
            .resize(resized)
            .expect("resizing a live session must succeed");
        assert_eq!(session.controller.size(), resized);

        session.write_line("mode con");
        session
            .output
            .wait_until_rendered("the resized dimensions", ANSWER, |text| {
                reported_size(text) == Some(expected_size(resized))
            })
            .await;

        session.write_line("exit");
        let (_output, status) = session.finish().await;
        assert!(status.success(), "unexpected status: {status}");
    })
    .await;
}

/// Concurrent successful resizes must have one total order shared by the
/// controller's cached size and the console host.
#[tokio::test]
async fn concurrent_resizes_keep_the_controller_and_child_in_sync() {
    let _watchdog = watchdog(BUDGET);
    within(
        "concurrent_resizes_keep_the_controller_and_child_in_sync",
        DEADLINE,
        async {
            let initial = size(18, 70);
            let mut session = Session::start_with(
                &mut Command::new("cmd.exe"),
                SessionOptions::new().size(initial),
            );
            session.output.wait_for(">", ANSWER).await;

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
                session
                    .output
                    .wait_until_rendered("one of the concurrent sizes", ANSWER, |text| {
                        let observed = reported_size(text);
                        observed == Some(expected_size(first))
                            || observed == Some(expected_size(second))
                    })
                    .await;
                assert_eq!(
                    reported_size(&session.output.text()),
                    Some(expected_size(recorded)),
                    "controller.size() diverged from the child after concurrent resizes"
                );
            }

            session.write_line("exit");
            let (_output, status) = session.finish().await;
            assert!(status.success(), "unexpected status: {status}");
        },
    )
    .await;
}

/// Root exit and resize are allowed to overlap, but teardown must serialize
/// the backend call and normalize the first post-close failure.
#[tokio::test]
async fn resize_racing_session_close_transitions_to_not_connected() {
    let _watchdog = watchdog(BUDGET);
    within(
        "resize_racing_session_close_transitions_to_not_connected",
        DEADLINE,
        async {
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
                input,
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

            let status = child
                .wait()
                .await
                .expect("waiting for the probe must succeed");
            assert!(status.success(), "unexpected status: {status}");
            output.join().await;

            assert_eq!(
                racer.join().expect("the resize racer must not panic"),
                io::ErrorKind::NotConnected
            );
            input.close().await;
        },
    )
    .await;
}

/// Clearing the buffer, and the promise that the capability query does not lie
/// in either direction.
///
/// The system backend exports no `ClearPseudoConsole`, so on an ordinary
/// machine this exercises the typed refusal; against a bundled `conpty.dll`
/// (see `tokio_backend_dll.rs`) the same shape performs a real clear. Either
/// way the session must survive the call and still carry input to the child.
#[tokio::test]
async fn clearing_agrees_with_the_capability_query() {
    let _watchdog = watchdog(BUDGET);
    within(
        "clearing_agrees_with_the_capability_query",
        DEADLINE,
        async {
            const BEFORE: &str = "conpty-oxide-async-before-clear";
            const AFTER: &str = "conpty-oxide-async-after-clear";

            let mut session = Session::start(&mut Command::new("cmd.exe"));
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
                Err(other) => panic!("clearing the console failed: {other}"),
            }

            // The child is untouched by a clear, so the session must still carry
            // input to it and output back.
            session.write_line(&format!("echo {AFTER}"));
            session.output.wait_for(AFTER, ANSWER).await;

            session.write_line("exit");
            let (_output, status) = session.finish().await;
            assert!(status.success(), "unexpected status: {status}");
        },
    )
    .await;
}
