//! End-to-end smoke tests for the asynchronous API.
//!
//! These answer the first questions anyone has about an async pseudoconsole
//! library: does the child's output come back, does its exit code come back,
//! is this actually a *console* rather than a plain redirected pipe, and can
//! the two directions really be driven from separate tasks at the same time?
//!
//! Every test here holds two guards. `asyn::within` is the ordinary timeout
//! and reports a stalled session as one named failure; `helpers::watchdog`
//! kills the process, and is the only thing that helps when a destructor
//! blocks the runtime thread and takes the timer with it.

#![cfg(all(windows, feature = "tokio"))]

mod helpers;

use std::time::Duration;

use conpty_oxide::{Command, Error, ExitStatus, Size};

use helpers::asyn::{pty, pty_with_size, within, Session};
use helpers::{expected_size, reported_size, watchdog};

/// Outer guard. Only a genuine deadlock gets anywhere near this.
const BUDGET: Duration = Duration::from_secs(40);

/// Per-test budget. Spawning `cmd.exe` under a fresh pseudoconsole takes a few
/// hundred milliseconds at worst; anything approaching this is a deadlock.
const DEADLINE: Duration = Duration::from_secs(30);

/// How long an interactive shell gets to answer a typed command.
const ANSWER: Duration = Duration::from_secs(15);

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
/// mirror image of the crate's own `Size`, and a swapped pair still succeeds —
/// so nothing but asking the child what size it thinks it has can catch it.
/// `mode con` is that question, and both dimensions are checked from the same
/// reply.
#[tokio::test]
async fn the_child_observes_a_resize() {
    let _watchdog = watchdog(BUDGET);
    within("the_child_observes_a_resize", DEADLINE, async {
        let initial = Size::new(24, 80);
        let resized = Size::new(30, 100);

        let mut session = Session::start_in(pty_with_size(initial), &mut Command::new("cmd.exe"));
        session.output.wait_for(">", ANSWER).await;

        session.write_line("mode con");
        session
            .output
            .wait_until_rendered("the session's initial size", ANSWER, |text| {
                reported_size(text) == expected_size(initial)
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
                reported_size(text) == expected_size(resized)
            })
            .await;

        session.write_line("exit");
        let (_output, status) = session.finish().await;
        assert!(status.success(), "unexpected status: {status}");
    })
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

            let pty = pty();
            let supported = pty.supports_clear();
            let mut session = Session::start_in(pty, &mut Command::new("cmd.exe"));
            assert_eq!(session.controller.supports_clear(), supported);

            session.output.wait_for(">", ANSWER).await;
            session.write_line(&format!("echo {BEFORE}"));
            session.output.wait_for(BEFORE, ANSWER).await;

            match session.controller.clear() {
                Ok(()) => assert!(
                    supported,
                    "clear succeeded on a backend that reports no clear support"
                ),
                Err(Error::UnsupportedFeature { feature }) => {
                    assert!(
                        !supported,
                        "clear was refused as unsupported on a backend that reports \
                     clear support"
                    );
                    assert_eq!(feature, "ClearPseudoConsole");
                }
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
