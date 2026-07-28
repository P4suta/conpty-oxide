//! End-to-end smoke tests for the blocking API.
//!
//! These answer the first three questions anyone has about a pseudoconsole
//! library: does the child's output come back, does its exit code come back,
//! and is this actually a *console* rather than a plain redirected pipe?

#![cfg(all(windows, feature = "blocking"))]

mod helpers;

use std::time::Duration;

use conpty_oxide::blocking::Command;

use helpers::{with_timeout, Session};

/// Per-test budget. Spawning `cmd.exe` under a fresh pseudoconsole takes a few
/// hundred milliseconds at worst; anything approaching this is a deadlock.
const BUDGET: Duration = Duration::from_secs(30);

/// The escape character that introduces every ANSI sequence.
const ESC: u8 = 0x1b;

#[test]
fn echoed_text_comes_back_with_a_successful_status() {
    with_timeout(BUDGET, || {
        const MARKER: &str = "conpty-oxide-basic-echo";
        let (output, status) =
            Session::start(Command::new("cmd.exe").args(["/c", "echo", MARKER])).finish();

        assert!(
            output.contains(MARKER),
            "the echoed marker is missing from the rendered output: {output:?}"
        );
        assert!(status.success(), "unexpected status: {status}");
        assert_eq!(status.code(), 0);
    });
}

#[test]
fn a_nonzero_exit_code_is_reported_verbatim() {
    with_timeout(BUDGET, || {
        let (_output, status) =
            Session::start(Command::new("cmd.exe").args(["/c", "exit", "42"])).finish();

        assert_eq!(status.code(), 42);
        assert!(!status.success());
        // 259 is `STILL_ACTIVE`; seeing it here would mean the exit code was
        // read before the process actually exited.
        assert_ne!(status.code(), 259);
    });
}

#[test]
fn the_output_is_a_virtual_terminal_stream() {
    with_timeout(BUDGET, || {
        let (bytes, status) =
            Session::start(Command::new("cmd.exe").args(["/c", "echo", "vt"])).finish_raw();

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
    });
}
