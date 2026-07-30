// SPDX-FileCopyrightText: 2025 conpty-oxide contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;

use std::fs::File;
use std::io::Read;
use std::os::windows::io::{AsHandle, AsRawHandle};
use std::panic;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use windows_sys::Win32::System::JobObjects::IsProcessInJob;

use crate::backend::ConPtyBackend;
use crate::core::pipes::{create_sync_pipes, SyncPipes};
use crate::core::pseudocon::PseudoConsole;
use crate::core::wait::{spawn_root_watcher, ProcessWaiter};
use crate::size::Size;

/// Grace period the legacy watcher gets in tests; short enough to keep the
/// suite fast, long enough for a reader to drain a few kilobytes.
const TEST_GRACE: Duration = Duration::from_millis(200);

/// Runs `f` on a helper thread and fails the test if it has not finished
/// within `timeout`.
///
/// Every failure mode this module can hit is a hang — an unserviced conout
/// pipe, a `ClosePseudoConsole` that never returns — which without a
/// watchdog would stall the entire test binary instead of failing one
/// test. A panic inside `f` is re-raised on the test thread, so ordinary
/// assertion failures still report themselves normally.
fn complete_within(name: &str, timeout: Duration, f: impl FnOnce() + Send + 'static) {
    let (done_tx, done_rx) = mpsc::channel();
    let handle = thread::Builder::new()
        .name(format!("watchdog-subject-{name}"))
        .spawn(move || {
            f();
            let _ = done_tx.send(());
        })
        .expect("spawning the test subject thread must succeed");

    match done_rx.recv_timeout(timeout) {
        // The sender was dropped without sending: `f` panicked, and the
        // join below re-raises it with its original message.
        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {},
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("`{name}` hung for more than {timeout:?}")
        },
    }
    if let Err(payload) = handle.join() {
        panic::resume_unwind(payload);
    }
}

/// One pseudoconsole session, ready for a child.
///
/// Field order is the teardown order: the console (and with it the last
/// `Arc<ConsoleShared>`, hence `ClosePseudoConsole`) goes first, the pipe
/// ends last.
struct Session {
    console: PseudoConsole,
    job: Arc<Job>,
    /// Taken by [`Session::drain_conout`]; still here when a test never
    /// starts a reader, so dropping the session retires it either way.
    conout_read: Option<OwnedHandle>,
    /// Never read and never written to: holding the handle open is what
    /// keeps a child blocked in `pause` instead of seeing end-of-file on
    /// its console input.
    _conin_write: OwnedHandle,
}

impl Session {
    fn new(kill_on_close: bool) -> Self {
        let backend = ConPtyBackend::system().expect("ConPTY must be available");
        let SyncPipes {
            conout_read,
            conout_write,
            conin_read,
            conin_write,
        } = create_sync_pipes().expect("creating pipes must succeed");
        let console = PseudoConsole::new(backend, Size::default(), conin_read, conout_write, false)
            .expect("CreatePseudoConsole must succeed");
        Self {
            console,
            job: Arc::new(Job::create(kill_on_close).expect("creating a job must succeed")),
            conout_read: Some(conout_read),
            _conin_write: conin_write,
        }
    }

    /// Starts the mandatory conout reader thread.
    ///
    /// ConPTY's documentation is explicit that the I/O channels must be
    /// serviced from a separate thread or a full pipe buffer deadlocks the
    /// session. The thread also reports the two reader-side lifecycle
    /// events the close state machine waits for.
    fn drain_conout(&mut self) -> thread::JoinHandle<Vec<u8>> {
        let mut conout = File::from(
            self.conout_read
                .take()
                .expect("conout may only be drained once"),
        );
        let shared = Arc::clone(self.console.shared());
        thread::Builder::new()
            .name("test-conout-reader".into())
            .spawn(move || {
                let mut sink = Vec::new();
                // A broken pipe reads as end-of-file, so this returns once
                // the console host is gone.
                let _ = conout.read_to_end(&mut sink);
                shared.notify_reader_eof();
                drop(conout);
                shared.notify_reader_closed();
                sink
            })
            .expect("spawning the reader thread must succeed")
    }

    /// Performs the post-spawn lifecycle step: release if the backend can,
    /// otherwise start the legacy watcher.
    fn arm_shutdown(&self, child: &SpawnedChild) {
        // An `Err` from the release call means the session was demoted to
        // legacy mode, so it is handled exactly like "no release export".
        let released = self.console.release_after_spawn().unwrap_or(false);
        self.start_root_watcher(child, !released);
    }

    /// Performs the post-spawn lifecycle step, forced onto the legacy
    /// path.
    ///
    /// Skipping the release call makes a Windows 11 24H2 machine behave
    /// like Windows 10 or Server 2022: the console host outlives the
    /// child, so nothing but `ClosePseudoConsole` can ever produce
    /// end-of-file on conout. Without this, the fallback the crate depends
    /// on for older systems would go untested on every modern machine.
    fn arm_shutdown_as_legacy(&self, child: &SpawnedChild) {
        self.start_root_watcher(child, true);
    }

    fn start_root_watcher(&self, child: &SpawnedChild, close_legacy: bool) {
        let watched = child
            .process
            .as_handle()
            .try_clone_to_owned()
            .expect("duplicating the process handle must succeed");
        spawn_root_watcher(
            watched,
            Arc::downgrade(&self.job),
            Arc::clone(self.console.shared()),
            TEST_GRACE,
            close_legacy,
        )
        .expect("spawning the root watcher must succeed");
    }
}

/// Builds a `cmd.exe` command line without relying on `cmd`'s quote
/// stripping rules.
fn cmd_exe(args: &[&str]) -> Command {
    let mut command = Command::new("cmd.exe");
    command.args(args);
    command
}

#[test]
fn spawn_reports_the_child_exit_code() {
    complete_within(
        "spawn_reports_the_child_exit_code",
        Duration::from_secs(30),
        || {
            let mut session = Session::new(false);
            let mut command = cmd_exe(&["/c", "exit", "7"]);
            // Exercises the `lpCurrentDirectory` path; a bad directory
            // would fail `CreateProcessW` outright.
            command.current_dir(std::env::temp_dir());

            let child = spawn(&command, session.console.hpcon(), &session.job)
                .expect("spawning under the pseudoconsole must succeed");
            assert_ne!(child.pid, 0, "a spawned child must have a pid");

            let waiter = ProcessWaiter::new(
                child
                    .process
                    .as_handle()
                    .try_clone_to_owned()
                    .expect("duplicating the process handle must succeed"),
            );
            let reader = session.drain_conout();
            session.arm_shutdown(&child);

            assert_eq!(waiter.wait().expect("waiting must succeed"), 7);
            reader.join().expect("the reader thread must not panic");
            drop(session);
        },
    );
}

#[test]
fn spawn_reaches_end_of_file_on_the_forced_legacy_path() {
    complete_within(
        "spawn_reaches_end_of_file_on_the_forced_legacy_path",
        Duration::from_secs(30),
        || {
            let mut session = Session::new(false);
            let command = cmd_exe(&["/c", "exit", "7"]);

            let child = spawn(&command, session.console.hpcon(), &session.job)
                .expect("spawning must succeed");
            let waiter = ProcessWaiter::new(
                child
                    .process
                    .as_handle()
                    .try_clone_to_owned()
                    .expect("duplicating the process handle must succeed"),
            );
            let reader = session.drain_conout();
            session.arm_shutdown_as_legacy(&child);

            assert_eq!(waiter.wait().expect("waiting must succeed"), 7);
            // The join is the real assertion: with the pseudoconsole never
            // released, the console host survives the child, so the reader
            // can only finish once the watcher's `ClosePseudoConsole`
            // breaks conout. If that contract were broken, this would hang
            // until the watchdog fires.
            reader.join().expect("the reader thread must not panic");
            assert!(!session.console.is_released());
            assert!(session.console.shared().is_closed());
            drop(session);
        },
    );
}

#[test]
fn spawn_passes_the_environment_block_to_the_child() {
    complete_within(
        "spawn_passes_the_environment_block_to_the_child",
        Duration::from_secs(30),
        || {
            const MARKER: &str = "conpty-oxide-env-marker-4711";

            let mut session = Session::new(false);
            let mut command = cmd_exe(&["/c", "echo", "%CONPTY_OXIDE_TEST_MARKER%"]);
            command.env("CONPTY_OXIDE_TEST_MARKER", MARKER);

            let child = spawn(&command, session.console.hpcon(), &session.job)
                .expect("spawning must succeed");
            let waiter = ProcessWaiter::new(
                child
                    .process
                    .as_handle()
                    .try_clone_to_owned()
                    .expect("duplicating the process handle must succeed"),
            );
            let reader = session.drain_conout();
            session.arm_shutdown(&child);

            waiter.wait().expect("waiting must succeed");
            let output = reader.join().expect("the reader thread must not panic");
            // The pseudoconsole renders the child's output as a UTF-8 VT
            // stream; the marker appears verbatim between escape
            // sequences. An unexpanded `%CONPTY_OXIDE_TEST_MARKER%` here
            // would mean the environment block never reached the child.
            let rendered = String::from_utf8_lossy(&output);
            assert!(
                rendered.contains(MARKER),
                "marker missing from pseudoconsole output: {rendered:?}"
            );
            drop(session);
        },
    );
}

#[test]
fn spawn_assigns_the_child_to_the_job_and_terminate_kills_it() {
    complete_within(
        "spawn_assigns_the_child_to_the_job_and_terminate_kills_it",
        Duration::from_secs(30),
        || {
            const KILL_CODE: u32 = 42;

            let mut session = Session::new(false);
            // `pause` blocks until a key arrives on the pseudoconsole's
            // input, which this test never writes: the child stays alive
            // until the job is terminated.
            let command = cmd_exe(&["/c", "pause"]);

            let child = spawn(&command, session.console.hpcon(), &session.job)
                .expect("spawning must succeed");

            let mut in_job: i32 = 0;
            // SAFETY: both handles are live, and `in_job` is a valid
            // out-parameter.
            let ok = unsafe {
                IsProcessInJob(
                    child.process.as_raw_handle(),
                    session.job.raw_handle(),
                    &mut in_job,
                )
            };
            assert_ne!(
                ok,
                0,
                "IsProcessInJob failed: {}",
                io::Error::last_os_error()
            );
            assert_ne!(in_job, 0, "the child must be a member of the job");

            let waiter = ProcessWaiter::new(
                child
                    .process
                    .as_handle()
                    .try_clone_to_owned()
                    .expect("duplicating the process handle must succeed"),
            );
            let reader = session.drain_conout();
            session.arm_shutdown(&child);

            assert_eq!(
                waiter.try_wait().expect("polling must succeed"),
                None,
                "the child must still be running before the kill"
            );
            session
                .job
                .terminate(KILL_CODE)
                .expect("terminating the job must succeed");

            assert_eq!(waiter.wait().expect("waiting must succeed"), KILL_CODE);
            reader.join().expect("the reader thread must not panic");
            drop(session);
        },
    );
}

#[test]
fn spawn_reports_a_missing_program_as_not_found() {
    complete_within(
        "spawn_reports_a_missing_program_as_not_found",
        Duration::from_secs(30),
        || {
            let mut session = Session::new(false);
            let command = Command::new("conpty-oxide-no-such-program.exe");

            let err = spawn(&command, session.console.hpcon(), &session.job)
                .expect_err("spawning a missing program must fail");
            assert_eq!(err.kind(), io::ErrorKind::NotFound);

            // No child ever attached, so no end-of-file is coming: retire
            // the reader by hand, which is what makes the close prompt.
            let conout_read = session.conout_read.take().expect("conout is still ours");
            drop(conout_read);
            session.console.shared().notify_reader_closed();
            drop(session);
        },
    );
}

#[test]
fn spawn_rejects_a_command_line_it_cannot_build() {
    complete_within(
        "spawn_rejects_a_command_line_it_cannot_build",
        Duration::from_secs(30),
        || {
            let mut session = Session::new(false);
            let mut command = Command::new("cmd.exe");
            command.arg("embedded\0nul");

            let err = spawn(&command, session.console.hpcon(), &session.job)
                .expect_err("an unbuildable command line must fail");
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

            let conout_read = session.conout_read.take().expect("conout is still ours");
            drop(conout_read);
            session.console.shared().notify_reader_closed();
            drop(session);
        },
    );
}

#[test]
fn attribute_list_initializes_and_deletes_cleanly() {
    // Exercised end to end by the spawn tests; this pins the standalone
    // two-call protocol, including the deliberately failing size probe.
    let deleted = Arc::new(AtomicBool::new(false));
    let mut list = AttributeList::new(ATTRIBUTE_COUNT).expect("initialization must succeed");
    list.drop_observer = Some(Arc::clone(&deleted));
    assert!(!list.as_ptr().is_null());
    // The `Drop` impl deletes the list; running it here (rather than at
    // the end of the test) keeps the failure localized if it misbehaves.
    drop(list);
    assert!(deleted.load(Ordering::SeqCst));
}

#[test]
fn startup_info_blanks_every_standard_handle() {
    let mut list = AttributeList::new(ATTRIBUTE_COUNT).expect("initialization must succeed");
    let expected_list = list.as_ptr();
    let startup = startup_info(&mut list);

    assert_eq!(
        startup.StartupInfo.cb,
        u32::try_from(size_of::<STARTUPINFOEXW>()).expect("STARTUPINFOEXW size must fit")
    );
    assert_eq!(startup.StartupInfo.dwFlags, STARTF_USESTDHANDLES);
    assert_eq!(startup.StartupInfo.hStdInput, INVALID_HANDLE_VALUE);
    assert_eq!(startup.StartupInfo.hStdOutput, INVALID_HANDLE_VALUE);
    assert_eq!(startup.StartupInfo.hStdError, INVALID_HANDLE_VALUE);
    assert_eq!(startup.lpAttributeList, expected_list);
}
