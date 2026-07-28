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

mod helpers;

use std::time::Duration;

use conpty_oxide::blocking::Command;
use conpty_oxide::Size;

use helpers::{pty_with_size, strip_escapes, wait_until, with_timeout, Session};

const BUDGET: Duration = Duration::from_secs(60);

/// How long the shell gets to answer a typed command.
const ANSWER: Duration = Duration::from_secs(15);

/// Returns the dimensions reported by the most recent `mode con` in `raw`, as
/// `(rows, columns)`.
///
/// The most recent one is what matters: resizing makes the console host
/// repaint the viewport, which re-emits the *previous* answer verbatim. Only
/// the last occurrence of each label is guaranteed to belong to the reply to
/// the last question.
fn reported_size(raw: &str) -> Option<(u32, u32)> {
    let text = strip_escapes(raw);
    Some((
        last_number(&text, "Lines:")?,
        last_number(&text, "Columns:")?,
    ))
}

/// Returns the number that follows the last occurrence of `label`.
fn last_number(text: &str, label: &str) -> Option<u32> {
    text.rmatch_indices(label).find_map(|(at, _)| {
        text[at + label.len()..]
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    })
}

/// The pair `mode con` should report for `size`.
fn expected(size: Size) -> Option<(u32, u32)> {
    Some((u32::from(size.rows()), u32::from(size.cols())))
}

#[test]
fn the_child_observes_a_resize() {
    with_timeout(BUDGET, || {
        let initial = Size::new(24, 80);
        let resized = Size::new(30, 100);

        let mut session = Session::start_in(pty_with_size(initial), &mut Command::new("cmd.exe"));

        // Wait for the prompt before typing, so the command cannot be sent to
        // a shell that has not started reading its console yet.
        session.output.wait_for(">", ANSWER);

        session.write_line("mode con");
        assert!(
            wait_until(ANSWER, || reported_size(&session.output.text())
                == expected(initial)),
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
            wait_until(ANSWER, || reported_size(&session.output.text())
                == expected(resized)),
            "the child never observed the resize to {resized}: {:?}",
            strip_escapes(&session.output.text())
        );

        session.write_line("exit");
        let (_output, status) = session.finish();
        assert!(status.success(), "unexpected status: {status}");
    });
}
