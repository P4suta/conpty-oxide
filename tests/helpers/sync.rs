//! Session harness for the blocking front end.
//!
//! Re-exported from the parent module, so tests keep saying `helpers::Session`
//! and `helpers::pty`. It lives in its own file only so that a test binary
//! built without the `blocking` feature still compiles.
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

use conpty_oxide::blocking::{Child, Command, OwnedReadHalf, OwnedWriteHalf, Pty, PtyController};
use conpty_oxide::{ConPtyBackend, ExitStatus, Size};

use super::{lock, strip_escapes, wait_until};

/// Builds a default 24x80 session.
pub fn pty() -> Pty {
    Pty::builder().build().expect("building a pty must succeed")
}

/// Builds a session of the given size.
pub fn pty_with_size(size: Size) -> Pty {
    Pty::builder()
        .size(size)
        .build()
        .expect("building a pty must succeed")
}

/// Builds a default 24x80 session forced onto the legacy shutdown path.
///
/// The backend clone has its `ReleasePseudoConsole` export stripped
/// (`ConPtyBackend::without_release`, a hidden test hook), so the session
/// behaves as on Windows versions before 11 24H2 regardless of the machine it
/// runs on: the console host outlives the child, and only the crate's own
/// legacy shutdown (the watcher, or closing the console at teardown) can
/// produce end-of-file. Tests built on this cover the code paths that a
/// 24H2+ machine would otherwise never take.
pub fn legacy_pty() -> Pty {
    let backend = ConPtyBackend::system()
        .expect("ConPTY must be available")
        .without_release();
    assert!(!backend.supports_release());
    Pty::builder()
        .backend(backend)
        .build()
        .expect("building a forced-legacy pty must succeed")
}

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
    pub child: Child,
    pub output: OutputCollector,
    /// Kept open on purpose. Closing the input pipe makes the console host
    /// send a close event to every client, which terminates the child with
    /// `0xC000013A` and truncates its output — a test that dropped this early
    /// would observe a broken pipe no matter how the crate behaved.
    pub writer: OwnedWriteHalf,
    pub controller: PtyController,
}

impl Session {
    /// Spawns `command` in a fresh 24x80 session.
    pub fn start(command: &mut Command) -> Self {
        Self::start_in(pty(), command)
    }

    /// Spawns `command` in `pty` and starts draining its output.
    pub fn start_in(pty: Pty, command: &mut Command) -> Self {
        let child = command.spawn(&pty).expect("spawning must succeed");
        let (reader, writer, controller) = pty.into_split();
        Self {
            child,
            output: OutputCollector::spawn(reader),
            writer,
            controller,
        }
    }

    /// Types `line` followed by a carriage return, as a terminal would.
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
    pub fn finish(self) -> (String, ExitStatus) {
        let (bytes, status) = self.finish_raw();
        (strip_escapes(&String::from_utf8_lossy(&bytes)), status)
    }

    /// [`Self::finish`] without decoding or filtering, for tests that assert
    /// on the virtual-terminal stream itself.
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
