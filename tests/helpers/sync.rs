// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Session harness for the blocking front end.
//!
//! Reached through `helpers::sync`; it lives in its own file so that a test
//! binary built without the `blocking` feature still compiles.
//!
//! - [`OutputCollector`] provides the dedicated reader thread ConPTY requires,
//!   while still letting the test thread inspect what has arrived so far.
//! - [`Session`] bundles the four things a live session consists of, with the
//!   write half deliberately kept alive: closing the input pipe makes the
//!   console host terminate its clients, so a test that dropped it early would
//!   pass even against a completely broken end-of-file implementation.

use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use conpty_oxide::blocking::{Child, Command, OwnedReadHalf, OwnedWriteHalf};
use conpty_oxide::{ExitStatus, PtyController, SessionOptions};

use super::{lock, strip_escapes, wait_until};

/// Drains a session's output on its own thread and accumulates it.
///
/// ConPTY requires the output pipe to be serviced by a thread other than the
/// one waiting on the child, so this is not a convenience — a test that read
/// inline would deadlock as soon as the child produced more than a pipe
/// buffer's worth of text. The accumulated bytes stay visible to the test
/// thread while the child runs, which is what makes request/response tests
/// (see `resize.rs`) possible.
pub struct OutputCollector {
    buffer: Arc<Mutex<Vec<u8>>>,
    reader: JoinHandle<io::Result<()>>,
}

impl OutputCollector {
    /// Starts draining `half` until end-of-file.
    ///
    /// # Panics
    ///
    /// If the collector thread cannot be created.
    #[must_use]
    pub fn spawn(mut half: OwnedReadHalf) -> Self {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&buffer);
        let reader = thread::Builder::new()
            .name("conpty-oxide-test-reader".into())
            .spawn(move || {
                let mut chunk = [0_u8; 4096];
                loop {
                    let read = half.read(&mut chunk)?;
                    if read == 0 {
                        return Ok(());
                    }
                    lock(&sink).extend_from_slice(&chunk[..read]);
                }
            })
            .expect("spawning the collector thread must succeed");
        Self { buffer, reader }
    }

    /// Everything collected so far, decoded lossily.
    ///
    /// Safe to call while the child is still running; that is what makes
    /// request/response tests possible.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&lock(&self.buffer)).into_owned()
    }

    /// Waits for `needle` to appear in the output, with escape sequences
    /// removed.
    ///
    /// # Panics
    ///
    /// If `needle` has not appeared within `limit`, with the output included
    /// in the message so the failure is diagnosable.
    pub fn wait_for(&self, needle: &str, limit: Duration) {
        let found = wait_until(limit, || strip_escapes(&self.text()).contains(needle));
        assert!(
            found,
            "{needle:?} never appeared in the output: {:?}",
            strip_escapes(&self.text())
        );
    }

    /// Waits for the reader to reach end-of-file and returns everything it
    /// collected.
    ///
    /// This is the strongest assertion the harness makes: it returns only once
    /// the crate's own shutdown path has produced end-of-file.
    ///
    /// # Panics
    ///
    /// If the collector thread panics, reading fails, or its buffer still has
    /// another owner after the thread exits.
    pub fn join(self) -> Vec<u8> {
        let Self { buffer, reader } = self;
        reader
            .join()
            .expect("the collector thread must not panic")
            .expect("reading to end-of-file must succeed");
        // The collector thread has ended, so its clone of the `Arc` is gone
        // and the buffer can be taken rather than copied.
        Arc::try_unwrap(buffer)
            .expect("the collector thread holds the only other reference")
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// [`Self::join`], decoded lossily.
    #[must_use]
    pub fn join_text(self) -> String {
        String::from_utf8_lossy(&self.join()).into_owned()
    }
}

/// A live session: the child, the thread draining its output, and the two
/// handles that must stay alive while it runs.
///
/// The fields are public so a test can take the session apart and drop the
/// pieces in whatever order it wants to exercise.
pub struct Session {
    /// Root child attached to the pseudoconsole.
    pub child: Child,
    /// Thread-backed collector draining rendered output.
    pub output: OutputCollector,
    /// Kept open on purpose. Closing the input pipe makes the console host
    /// send a close event to every client, which terminates the child with
    /// `0xC000013A` and truncates its output — a test that dropped this early
    /// would observe a broken pipe no matter how the crate behaved.
    pub writer: OwnedWriteHalf,
    /// Cloneable control handle for the session.
    pub controller: PtyController,
}

impl Session {
    /// Spawns `command` in a fresh 24x80 session.
    pub fn start(command: &mut Command) -> Self {
        Self::start_with(command, SessionOptions::default())
    }

    /// Spawns `command` with managed options and starts draining its output.
    ///
    /// # Panics
    ///
    /// If the child or output collector cannot be started.
    pub fn start_with(command: &mut Command, options: SessionOptions) -> Self {
        let parts = command
            .spawn_with(options)
            .expect("spawning must succeed")
            .into_parts();
        Self {
            child: parts.child,
            output: OutputCollector::spawn(parts.output),
            writer: parts.input,
            controller: parts.controller,
        }
    }

    /// Types `line` followed by a carriage return, as a terminal would.
    ///
    /// # Panics
    ///
    /// If writing or flushing console input fails.
    pub fn write_line(&mut self, line: &str) {
        self.writer
            .write_all(line.as_bytes())
            .expect("writing console input must succeed");
        self.writer
            .write_all(b"\r\n")
            .expect("writing console input must succeed");
        self.writer.flush().expect("flush must succeed");
    }

    /// Waits for the child, then for end-of-file, and returns the rendered
    /// output with escape sequences removed plus the child's status.
    #[must_use]
    pub fn finish(self) -> (String, ExitStatus) {
        let (bytes, status) = self.finish_raw();
        (strip_escapes(&String::from_utf8_lossy(&bytes)), status)
    }

    /// [`Self::finish`] without decoding or filtering, for tests that assert
    /// on the virtual-terminal stream itself.
    ///
    /// # Panics
    ///
    /// If waiting for the child or collecting its output fails.
    #[must_use]
    pub fn finish_raw(self) -> (Vec<u8>, ExitStatus) {
        let Self {
            mut child,
            output,
            writer,
            controller,
        } = self;
        let status = child.wait().expect("waiting must succeed");
        // Joining is the real assertion: it returns only once the session
        // reached end-of-file, and with the write half still open that can
        // only have come from the crate's own shutdown path.
        let bytes = output.join();
        drop(writer);
        drop(controller);
        (bytes, status)
    }
}
