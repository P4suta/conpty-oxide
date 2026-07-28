//! Shared harness for the integration tests.
//!
//! The tests in this directory all guard against the same failure mode: a
//! pseudoconsole session that *stops making progress*. That shapes everything
//! here.
//!
//! - [`watchdog`] (and [`with_timeout`], which wraps it) is the outer guard.
//!   Unlike a plain watchdog that panics on another thread, it lets a watchdog
//!   thread kill the whole test process on expiry — the only way to guarantee
//!   termination when the wedged thread is the one running the test (a blocked
//!   `Drop`, for instance, cannot be interrupted or abandoned, and it takes a
//!   single-threaded Tokio runtime's timer down with it).
//! - The process helpers ([`descendants_of`] and friends) observe a child's
//!   *descendants*, which is what "kill tree" claims are actually about.
//! - [`strip_escapes`] and [`reported_size`] turn a rendered virtual-terminal
//!   stream back into something a test can assert on.
//!
//! Everything specific to one front end lives in a submodule, so a test binary
//! built with only one of them compiles: the blocking session harness is
//! re-exported from here (`helpers::Session`, `helpers::pty`, ...) and the
//! asynchronous one is reached through [`asyn`] (`helpers::asyn::Session`,
//! `helpers::asyn::pty`, ...).

// Every test binary compiles this whole module but uses only the part it
// needs, so unused items here are expected rather than a mistake.
#![allow(dead_code)]

use std::io::{self, Write};
use std::mem;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};

use conpty_oxide::Size;

#[cfg(feature = "blocking")]
mod sync;
// Same reasoning as the `dead_code` allowance above: a test binary that only
// exercises one front end still compiles the harness for both.
#[cfg(feature = "blocking")]
#[allow(unused_imports)]
pub use sync::*;

#[cfg(feature = "tokio")]
pub mod asyn;

/// How often the watchdog and the polling helpers re-check their condition.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Exit code used when a test exceeds its budget.
///
/// `101` is what the standard test harness reports for a failing run, so a
/// killed test looks like a failure to every runner rather than like a crash.
const TIMEOUT_EXIT_CODE: i32 = 101;

/// Kills the test process if it is still alive when its budget runs out.
///
/// Returned by [`watchdog`] and disarmed by its own [`Drop`], so an assertion
/// failure unwinds normally and is reported by the harness with its original
/// message — only a test that never gets that far is treated as deadlocked.
#[must_use = "the watchdog is disarmed as soon as the guard is dropped"]
pub struct Watchdog {
    finished: Arc<AtomicBool>,
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.finished.store(true, Ordering::SeqCst);
    }
}

/// Arms a watchdog that terminates the test process after `limit`.
///
/// Killing the process is deliberate. The regressions under test are hangs
/// inside teardown, and a hung `Drop` cannot be abandoned: reporting the
/// failure from a watchdog thread and letting the harness continue would leave
/// the wedged thread holding a pseudoconsole open, and the test binary would
/// never exit. Killing turns "the suite hangs forever" into "one test failed",
/// which is the whole value of these tests in CI.
///
/// Async tests hold the guard for the whole test function and use
/// [`asyn::within`] for the ordinary, recoverable timeout. The two are
/// complementary: `tokio::time::timeout` cannot fire while a blocked
/// destructor is occupying the runtime thread, and this can.
pub fn watchdog(limit: Duration) -> Watchdog {
    let finished = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&finished);
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

    Watchdog { finished }
}

/// Runs `body` under a [`watchdog`] armed for `limit`.
///
/// The body runs on the *calling* thread, which is the whole point: a hung
/// destructor has to be able to wedge the thread the test is on for the
/// watchdog to be worth anything.
pub fn with_timeout<T>(limit: Duration, body: impl FnOnce() -> T) -> T {
    let _guard = watchdog(limit);
    body()
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
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
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
