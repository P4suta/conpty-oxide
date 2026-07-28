//! Shared harness for the integration tests.
//!
//! The tests in this directory all guard against the same failure mode: a
//! pseudoconsole session that *stops making progress*. That shapes everything
//! here.
//!
//! - [`with_timeout`] is the outer guard. Unlike a plain watchdog that panics
//!   on another thread, it runs the test body on the calling thread and lets a
//!   watchdog thread kill the whole test process on expiry — the only way to
//!   guarantee termination when the wedged thread is the test thread itself
//!   (a blocked `Drop`, for instance, cannot be interrupted or abandoned).
//! - [`OutputCollector`] provides the dedicated reader thread ConPTY requires,
//!   while still letting the test thread inspect what has arrived so far.
//! - [`Session`] bundles the four things a live session consists of, with the
//!   write half deliberately kept alive: closing the input pipe makes the
//!   console host terminate its clients, so a test that dropped it early would
//!   pass even against a completely broken end-of-file implementation.
//! - The process helpers ([`descendants_of`] and friends) observe the child's
//!   *descendants*, which is what "kill tree" claims are actually about.

// Every test binary compiles this whole module but uses only the part it
// needs, so unused items here are expected rather than a mistake.
#![allow(dead_code)]

use std::io::{self, Read, Write};
use std::mem;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};

use conpty_oxide::blocking::{Child, Command, OwnedReadHalf, OwnedWriteHalf, Pty, PtyController};
use conpty_oxide::{ConPtyBackend, ExitStatus, Size};

/// How often the watchdog and the polling helpers re-check their condition.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Exit code used when a test exceeds its budget.
///
/// `101` is what the standard test harness reports for a failing run, so a
/// killed test looks like a failure to every runner rather than like a crash.
const TIMEOUT_EXIT_CODE: i32 = 101;

/// Runs `body`, killing the test process if it has not finished within
/// `limit`.
///
/// The body runs on the *calling* thread, which is the whole point: the
/// regressions under test are hangs inside teardown, and a hung `Drop` cannot
/// be abandoned. Reporting the failure from a watchdog thread and letting the
/// harness continue would leave the wedged thread holding a pseudoconsole
/// open, and the test binary would never exit. Killing the process instead
/// turns "the suite hangs forever" into "one test failed", which is the whole
/// value of these tests in CI.
///
/// Panics inside `body` are *not* timeouts: the completion flag is set by a
/// drop guard, so an assertion failure unwinds normally and is reported by the
/// harness with its original message.
pub fn with_timeout<T>(limit: Duration, body: impl FnOnce() -> T) -> T {
    /// Marks the test as finished however `body` ends — return or unwind.
    struct Finished(Arc<AtomicBool>);
    impl Drop for Finished {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let flag = Arc::new(AtomicBool::new(false));
    let guard = Finished(Arc::clone(&flag));
    let test = current_test_name();

    thread::Builder::new()
        .name(format!("watchdog-{test}"))
        .spawn(move || {
            let deadline = Instant::now() + limit;
            while !flag.load(Ordering::SeqCst) {
                if Instant::now() >= deadline {
                    eprintln!(
                        "\nconpty-oxide: `{test}` did not finish within {limit:?}. \
                         It is assumed to be deadlocked, so the test process is being \
                         terminated with exit code {TIMEOUT_EXIT_CODE}."
                    );
                    let _ = io::stderr().flush();
                    process::exit(TIMEOUT_EXIT_CODE);
                }
                thread::sleep(POLL_INTERVAL);
            }
        })
        .expect("spawning the watchdog thread must succeed");

    let value = body();
    drop(guard);
    value
}

/// Best-effort name of the running test, for the watchdog's diagnostic.
fn current_test_name() -> String {
    let current = thread::current();
    match current.name() {
        // `cargo test` names each test thread after the test it runs.
        Some(name) if name != "main" => name.to_owned(),
        // `cargo nextest` runs one test per process on the main thread and
        // passes the test's name as the only positional argument.
        _ => std::env::args()
            .skip(1)
            .find(|arg| !arg.starts_with('-'))
            .unwrap_or_else(|| "<unknown test>".to_owned()),
    }
}

/// Polls `condition` until it holds or `limit` elapses; returns whether it
/// ever held.
pub fn wait_until(limit: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Locks a mutex, ignoring poisoning.
///
/// A poisoned buffer in a test is still perfectly readable, and the panic that
/// poisoned it is what the harness should report — not a second panic here.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

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

/// Removes ANSI escape sequences from a rendered console stream.
///
/// Assertions in these tests are about the *text* the console produced, but
/// the stream also carries cursor movement, mode changes and window-title
/// updates. Those would otherwise both hide the text (a line can be split by a
/// reposition) and inject stray digits into it, which matters when a test
/// parses a number out of the output.
pub fn strip_escapes(text: &str) -> String {
    const ESC: char = '\u{1b}';
    const BEL: char = '\u{7}';

    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    loop {
        let Some(ch) = chars.next() else { return out };
        if ch != ESC {
            out.push(ch);
            continue;
        }
        match chars.next() {
            // CSI: parameter and intermediate bytes up to a final byte in
            // `@`..=`~`.
            Some('[') => loop {
                match chars.next() {
                    Some('\u{40}'..='\u{7e}') | None => break,
                    Some(_) => {}
                }
            },
            // String sequences (OSC, DCS, SOS, PM, APC): everything up to a
            // BEL or a string terminator (`ESC \`).
            Some(']' | 'P' | 'X' | '^' | '_') => loop {
                match chars.next() {
                    Some(BEL) | None => break,
                    Some(ESC) => {
                        chars.next();
                        break;
                    }
                    Some(_) => {}
                }
            },
            // Two-character sequence: the second character completes it.
            Some(_) | None => {}
        }
    }
}

/// Returns the dimensions reported by the most recent `mode con` in `raw`, as
/// `(rows, columns)`.
///
/// Asking the child what size it believes it has is the only way to catch a
/// swapped `COORD`: `ResizePseudoConsole` takes `(X = columns, Y = rows)`, the
/// mirror image of the crate's own [`Size`], and a swapped pair still
/// succeeds. `mode con` is that question, on every Windows installation.
///
/// The *most recent* answer is the one that counts. Resizing makes the console
/// host repaint the viewport, which re-emits the previous answer verbatim, so
/// only the last occurrence of each label is guaranteed to belong to the reply
/// to the last question.
pub fn reported_size(raw: &str) -> Option<(u32, u32)> {
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

/// The pair [`reported_size`] must return once `size` has taken effect.
pub fn expected_size(size: Size) -> Option<(u32, u32)> {
    Some((u32::from(size.rows()), u32::from(size.cols())))
}

/// One entry of a process snapshot.
#[derive(Debug, Clone)]
pub struct ProcessEntry {
    pub pid: u32,
    pub parent_pid: u32,
    /// Image name, e.g. `ping.exe`.
    pub name: String,
}

impl ProcessEntry {
    /// Whether this entry is a process with the given image name.
    fn is(&self, exe: &str) -> bool {
        self.name.eq_ignore_ascii_case(exe)
    }
}

/// How many times to retry a snapshot before giving up.
const SNAPSHOT_ATTEMPTS: u32 = 20;

/// Takes a snapshot of every process on the system.
///
/// # Panics
///
/// If `CreateToolhelp32Snapshot` keeps failing. It fails transiently with
/// `ERROR_BAD_LENGTH` when the process list changes mid-capture, which is
/// documented as retryable, so only a persistent failure aborts.
pub fn process_snapshot() -> Vec<ProcessEntry> {
    let snapshot = open_process_snapshot();
    let handle = snapshot.as_raw_handle() as HANDLE;
    let mut entry = PROCESSENTRY32W {
        dwSize: mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };

    let mut processes = Vec::new();
    // SAFETY: `handle` is a live process snapshot and `entry` is a writable,
    // correctly sized `PROCESSENTRY32W`. Both calls only fill it in.
    let mut has_entry = unsafe { Process32FirstW(handle, &mut entry) } != 0;
    while has_entry {
        processes.push(ProcessEntry {
            pid: entry.th32ProcessID,
            parent_pid: entry.th32ParentProcessID,
            name: wide_to_string(&entry.szExeFile),
        });
        // SAFETY: as above.
        has_entry = unsafe { Process32NextW(handle, &mut entry) } != 0;
    }
    processes
}

fn open_process_snapshot() -> OwnedHandle {
    for _ in 0..SNAPSHOT_ATTEMPTS {
        // SAFETY: no pointer arguments; the call either returns a fresh
        // handle or `INVALID_HANDLE_VALUE`.
        let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if raw != INVALID_HANDLE_VALUE {
            // SAFETY: `raw` is a fresh snapshot handle owned by nobody else,
            // and it is released with `CloseHandle` — exactly what dropping an
            // `OwnedHandle` does.
            return unsafe { OwnedHandle::from_raw_handle(raw as RawHandle) };
        }
        thread::sleep(POLL_INTERVAL);
    }
    panic!(
        "CreateToolhelp32Snapshot kept failing: {}",
        io::Error::last_os_error()
    );
}

fn wide_to_string(wide: &[u16]) -> String {
    let end = wide
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..end])
}

/// Returns every process descended from `root`, at any depth.
///
/// Parent identifiers in a snapshot are not cleared when the parent exits, so
/// a recycled identifier could in principle attach an unrelated process to the
/// tree. Over the lifetime of one test, with a process this crate created
/// moments ago, that cannot happen.
pub fn descendants_of(root: u32) -> Vec<ProcessEntry> {
    let all = process_snapshot();
    let mut found: Vec<ProcessEntry> = Vec::new();
    let mut frontier = vec![root];

    while let Some(parent) = frontier.pop() {
        for entry in &all {
            let known = entry.pid == root || found.iter().any(|seen| seen.pid == entry.pid);
            if entry.parent_pid == parent && !known {
                frontier.push(entry.pid);
                found.push(entry.clone());
            }
        }
    }
    found
}

/// Returns a descendant of `root` whose image name is `exe`, if there is one.
pub fn find_descendant(root: u32, exe: &str) -> Option<ProcessEntry> {
    descendants_of(root).into_iter().find(|entry| entry.is(exe))
}

/// Waits for `root` to have a descendant named `exe` and returns its pid.
///
/// # Panics
///
/// If no such descendant appears within `limit`.
pub fn wait_for_descendant(root: u32, exe: &str, limit: Duration) -> u32 {
    let mut found = None;
    let appeared = wait_until(limit, || {
        found = find_descendant(root, exe);
        found.is_some()
    });
    assert!(
        appeared,
        "process {root} never spawned a descendant named {exe:?}"
    );
    found.expect("the descendant was just observed").pid
}

/// Whether a process with this identifier *and* image name still exists.
///
/// Matching the name as well as the identifier keeps a recycled pid from
/// masquerading as a survivor, which would turn a correct "kill tree" into a
/// flaky failure.
pub fn process_is_running(pid: u32, exe: &str) -> bool {
    process_snapshot()
        .iter()
        .any(|entry| entry.pid == pid && entry.is(exe))
}
