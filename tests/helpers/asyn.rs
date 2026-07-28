//! Session harness for the asynchronous (Tokio) front end.
//!
//! The shape is deliberately different from the blocking harness. Async makes
//! it cheap to run *both* directions of a session concurrently, so a
//! [`Session`] here always consists of three parties: a reader task draining
//! the output pipe, a writer task owning the input pipe, and the test itself
//! driving them. That is the arrangement ConPTY requires and the one real
//! callers are expected to use, so testing anything else would be testing a
//! shape nobody ships.
//!
//! Two properties are worth spelling out, because they are what keeps these
//! tests honest rather than merely green:
//!
//! - The write half is owned by the writer task and stays alive for the
//!   child's whole life. Closing the input pipe makes the console host
//!   terminate every client, which would produce an end-of-file even against a
//!   completely broken shutdown path.
//! - The reader task forwards over an *unbounded* channel, so it never stops
//!   reading while a test is busy asserting. A bounded one would let the pipe
//!   buffer fill and wedge the console host, turning every slow assertion into
//!   a deadlock.

use std::future::Future;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use conpty_oxide::{
    Child, Command, ConPtyBackend, ExitStatus, OwnedReadHalf, OwnedWriteHalf, Pty, PtyController,
    Size,
};

use super::{find_descendant, strip_escapes, POLL_INTERVAL};

/// Builds a default 24x80 session.
///
/// Like every constructor here, this has to run inside a Tokio runtime: an
/// async session registers its pipes with the runtime's I/O driver.
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
/// runs on: the console host outlives the child, and end-of-file has to be
/// *produced* by the crate's legacy watcher rather than merely observed.
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

/// Awaits `body`, failing the test if it has not finished within `limit`.
///
/// This is the recoverable half of the timeout story: it turns a stalled
/// session into one failed test with a name attached. It cannot catch a
/// *blocked* thread — a destructor that never returns occupies the runtime
/// thread and its timer with it — which is what [`super::watchdog`] is for.
/// Async tests use both.
pub async fn within<F: Future>(label: &str, limit: Duration, body: F) -> F::Output {
    match tokio::time::timeout(limit, body).await {
        Ok(value) => value,
        Err(_) => panic!("`{label}` did not finish within {limit:?}"),
    }
}

/// Polls `condition` until it holds or `limit` elapses; returns whether it
/// ever held.
///
/// Sleeps with the runtime's timer rather than [`std::thread::sleep`], which
/// on a single-threaded runtime would stop the reader task and let the output
/// pipe fill while the test waits.
pub async fn poll_until(limit: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Waits for `root` to have a descendant named `exe` and returns its pid.
///
/// # Panics
///
/// If no such descendant appears within `limit`.
pub async fn wait_for_descendant(root: u32, exe: &str, limit: Duration) -> u32 {
    let mut found = None;
    let appeared = poll_until(limit, || {
        found = find_descendant(root, exe);
        found.is_some()
    })
    .await;
    assert!(
        appeared,
        "process {root} never spawned a descendant named {exe:?}"
    );
    found.expect("the descendant was just observed").pid
}

/// The reader task, plus everything it has delivered so far.
pub struct OutputStream {
    task: JoinHandle<()>,
    chunks: UnboundedReceiver<Vec<u8>>,
    seen: Vec<u8>,
}

impl OutputStream {
    /// Starts draining `half` until end-of-file.
    pub fn spawn(mut half: OwnedReadHalf) -> Self {
        let (sender, chunks) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut chunk = [0_u8; 4096];
            loop {
                let read = half
                    .read(&mut chunk)
                    .await
                    .expect("reading the session must not fail");
                // The end-of-file contract itself: a zero-byte read, never an
                // error, is what ends this loop.
                if read == 0 {
                    return;
                }
                if sender.send(chunk[..read].to_vec()).is_err() {
                    // The test dropped the stream; nothing left to feed.
                    return;
                }
            }
        });
        Self {
            task,
            chunks,
            seen: Vec::new(),
        }
    }

    /// Everything delivered so far, decoded lossily and left as-is.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.seen).into_owned()
    }

    /// [`Self::text`] with escape sequences removed.
    pub fn rendered(&self) -> String {
        strip_escapes(&self.text())
    }

    /// Reads until `needle` appears in the rendered output.
    ///
    /// # Panics
    ///
    /// If the session reaches end-of-file first, or if `limit` elapses, with
    /// the output so far included so the failure is diagnosable.
    pub async fn wait_for(&mut self, needle: &str, limit: Duration) {
        self.wait_until_rendered(&format!("{needle:?}"), limit, |text| text.contains(needle))
            .await;
    }

    /// Reads until `predicate` holds on the rendered output.
    ///
    /// `what` names the condition for the panic message, e.g.
    /// `"the resized dimensions"`.
    ///
    /// # Panics
    ///
    /// If the session reaches end-of-file first, or if `limit` elapses.
    pub async fn wait_until_rendered(
        &mut self,
        what: &str,
        limit: Duration,
        mut predicate: impl FnMut(&str) -> bool,
    ) {
        let outcome = tokio::time::timeout(limit, self.collect_until(&mut predicate)).await;
        match outcome {
            Ok(true) => {}
            Ok(false) => panic!(
                "the session reached end-of-file before {what} appeared: {:?}",
                self.rendered()
            ),
            Err(_) => panic!(
                "{what} did not appear within {limit:?}: {:?}",
                self.rendered()
            ),
        }
    }

    /// Accumulates chunks until `predicate` holds; `false` means end-of-file
    /// arrived first.
    async fn collect_until(&mut self, predicate: &mut impl FnMut(&str) -> bool) -> bool {
        loop {
            if predicate(&self.rendered()) {
                return true;
            }
            match self.chunks.recv().await {
                Some(chunk) => self.seen.extend_from_slice(&chunk),
                None => return false,
            }
        }
    }

    /// Waits for end-of-file and returns everything the session produced.
    ///
    /// This is the strongest assertion the harness makes: it returns only once
    /// the crate's own shutdown path has produced end-of-file.
    pub async fn join(mut self) -> Vec<u8> {
        self.task.await.expect("the reader task must not panic");
        // The task is gone, so its sender is too and the channel drains to a
        // clean `None` rather than blocking.
        while let Some(chunk) = self.chunks.recv().await {
            self.seen.extend_from_slice(&chunk);
        }
        self.seen
    }
}

/// The writer task, and the channel that feeds it console input.
///
/// The write half never leaves the task, which is what guarantees it outlives
/// every test that only sends input: it is closed exactly when [`Input::close`]
/// runs, or when the task is dropped along with the runtime.
pub struct Input {
    sender: UnboundedSender<Vec<u8>>,
    task: JoinHandle<OwnedWriteHalf>,
}

impl Input {
    /// Starts the writer task on `half`.
    pub fn spawn(mut half: OwnedWriteHalf) -> Self {
        let (sender, mut inbox) = mpsc::unbounded_channel::<Vec<u8>>();
        let task = tokio::spawn(async move {
            while let Some(bytes) = inbox.recv().await {
                half.write_all(&bytes)
                    .await
                    .expect("writing console input must succeed");
                half.flush()
                    .await
                    .expect("flush must be a no-op that succeeds");
            }
            // Returned rather than dropped here, so the caller decides when
            // the input pipe closes — which is when the session ends.
            half
        });
        Self { sender, task }
    }

    /// Types `line` followed by a carriage return, as a terminal would.
    pub fn write_line(&self, line: &str) {
        self.write_bytes(format!("{line}\r\n").as_bytes());
    }

    /// Sends raw console input.
    pub fn write_bytes(&self, bytes: &[u8]) {
        self.sender
            .send(bytes.to_vec())
            .expect("the writer task must still be running");
    }

    /// Closes the input pipe, ending the session's input side.
    ///
    /// Call this only once the child has exited: on a live session it makes
    /// the console host terminate every client.
    pub async fn close(self) {
        let Self { sender, task } = self;
        drop(sender);
        let half = task.await.expect("the writer task must not panic");
        drop(half);
    }
}

/// A live session: the child, the task draining its output, the task owning
/// its input, and the controller.
///
/// The fields are public so a test can take the session apart and retire the
/// pieces in whatever order it wants to exercise.
pub struct Session {
    pub child: Child,
    pub output: OutputStream,
    pub input: Input,
    pub controller: PtyController,
}

impl Session {
    /// Spawns `command` in a fresh 24x80 session.
    pub fn start(command: &mut Command) -> Self {
        Self::start_in(pty(), command)
    }

    /// Spawns `command` in `pty` and starts both tasks.
    pub fn start_in(pty: Pty, command: &mut Command) -> Self {
        let child = command.spawn(&pty).expect("spawning must succeed");
        Self::attach(pty, child)
    }

    /// Starts servicing an already-spawned child, which ConPTY requires to
    /// happen while the child runs.
    pub fn attach(pty: Pty, child: Child) -> Self {
        let (reader, writer, controller) = pty.into_split();
        Self {
            child,
            output: OutputStream::spawn(reader),
            input: Input::spawn(writer),
            controller,
        }
    }

    /// Types `line` followed by a carriage return, as a terminal would.
    pub fn write_line(&self, line: &str) {
        self.input.write_line(line);
    }

    /// Waits for the child, then for end-of-file, and returns the rendered
    /// output with escape sequences removed plus the child's status.
    pub async fn finish(self) -> (String, ExitStatus) {
        let (bytes, status) = self.finish_raw().await;
        (strip_escapes(&String::from_utf8_lossy(&bytes)), status)
    }

    /// [`Self::finish`] without decoding or filtering, for tests that assert
    /// on the virtual-terminal stream itself.
    pub async fn finish_raw(self) -> (Vec<u8>, ExitStatus) {
        let Self {
            mut child,
            output,
            input,
            controller,
        } = self;
        let status = child.wait().await.expect("waiting must succeed");
        // Joining is the real assertion: it returns only once the session
        // reached end-of-file, and with the input pipe still open that can
        // only have come from the crate's own shutdown path.
        let bytes = output.join().await;
        input.close().await;
        drop(controller);
        (bytes, status)
    }
}
