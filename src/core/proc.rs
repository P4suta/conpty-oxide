//! Spawning the child process attached to a pseudoconsole.
//!
//! This module is one function, [`spawn`], plus the RAII scaffolding that
//! makes it leak-free. It turns a [`Command`] into a running process that is
//! (a) connected to a pseudoconsole and (b) a member of a job object, both
//! established atomically at creation through the extended startup
//! information's attribute list.
//!
//! # The attribute list
//!
//! `CreateProcessW` learns about the pseudoconsole and the job from a
//! `PROC_THREAD_ATTRIBUTE_LIST` reached via `STARTUPINFOEXW`. Building one has
//! a two-call protocol that reads like a bug: the first
//! `InitializeProcThreadAttributeList` is passed a NULL list and is *expected
//! to fail* with `ERROR_INSUFFICIENT_BUFFER`, having written the required byte
//! count; the caller allocates that many bytes and calls again to initialize
//! them. The two attributes then set are:
//!
//! - `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`, whose value is the `HPCON`
//!   *itself* — the handle is passed by value, not by pointer, matching the
//!   official ConPTY sample.
//! - `PROC_THREAD_ATTRIBUTE_JOB_LIST`, whose value is a pointer to an array of
//!   job handles. The array must stay alive and unmoved until the attribute
//!   list is destroyed, which is why [`spawn`] keeps the handle in a local
//!   declared before the list.
//!
//! # Why the child gets no inherited handles
//!
//! `bInheritHandles` is `FALSE`, unconditionally. ConPTY does not need
//! inheritance — the console host received its own duplicates of the pipe ends
//! when the pseudoconsole was created — and any handle that did leak into the
//! child would keep the conout pipe open past the child's death, destroying
//! the end-of-file contract this crate is built around.
//!
//! For the same reason the standard handles are set to `INVALID_HANDLE_VALUE`
//! with `STARTF_USESTDHANDLES` (the approach wezterm takes). Leaving
//! `dwFlags` clear would make the child inherit *our* process's standard
//! handles, so a parent whose stdout is redirected to a file would silently
//! hand that file to a child that is supposed to talk only to the
//! pseudoconsole. Console applications attached to a ConPTY open their
//! `CONIN$`/`CONOUT$` through the console connection rather than through these
//! fields, so blanking them costs nothing.

use std::ffi::c_void;
use std::io;
use std::mem;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList,
    UpdateProcThreadAttribute, CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_JOB_LIST,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
};

use crate::backend::HPCON;
use crate::command::{to_wide_nul, Command};
use crate::core::job::Job;

/// Number of attributes [`spawn`] puts in the process attribute list.
const ATTRIBUTE_COUNT: u32 = 2;

/// A freshly created child process.
///
/// Only the process handle is kept: the primary thread handle that
/// `CreateProcessW` also returns is closed immediately, since this crate never
/// resumes or inspects the thread.
#[derive(Debug)]
pub(crate) struct SpawnedChild {
    /// Handle to the child process, used for waiting and for querying the
    /// exit code.
    pub(crate) process: OwnedHandle,
    /// The child's process identifier.
    pub(crate) pid: u32,
}

/// Spawns `cmd` attached to the pseudoconsole `hpcon` and assigned to `job`.
///
/// The caller is expected to have created the pseudoconsole (which already
/// closed its copies of the two client pipe ends) and the job object, and must
/// afterwards drive the pseudoconsole lifecycle: call
/// [`release_after_spawn`](crate::core::pseudocon::ConsoleShared::release_after_spawn)
/// and, when it reports that the backend cannot release, start a legacy
/// watcher with [`spawn_legacy_watcher`](crate::core::wait::spawn_legacy_watcher).
///
/// Creation flags are `EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT`
/// OR'ed with whatever [`Command::creation_flags`] recorded. The Unicode flag
/// is set even when the child inherits our environment, matching the standard
/// library: it describes the format of `lpEnvironment` and is harmless when
/// that pointer is NULL.
///
/// # Errors
///
/// Returns [`io::ErrorKind::InvalidInput`] if the command line or environment
/// block cannot be built (embedded NUL, malformed variable name), otherwise
/// the OS error from the failing Win32 call. Every error path unwinds the
/// attribute list and any handle acquired so far; nothing is leaked.
pub(crate) fn spawn(cmd: &Command, hpcon: HPCON, job: &Job) -> io::Result<SpawnedChild> {
    // `CreateProcessW` may modify the command-line buffer in place, so it must
    // be a mutable, NUL-terminated copy we own.
    let mut command_line = cmd.build_command_line()?;
    let environment = cmd.build_environment_block()?;
    let working_dir = cmd
        .get_current_dir()
        .map(|dir| to_wide_nul(dir.as_os_str()))
        .transpose()?;

    // Declared before the attribute list so it is still alive when the list is
    // destroyed: `PROC_THREAD_ATTRIBUTE_JOB_LIST` stores this variable's
    // address, and the documentation requires the pointed-to array to outlive
    // the list.
    let job_handle: HANDLE = job.raw_handle();

    let mut attributes = AttributeList::new(ATTRIBUTE_COUNT)?;
    // SAFETY: `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` takes the `HPCON` by
    // value, so the "pointer" is the handle itself and there is no pointee
    // whose lifetime could end early. `hpcon` is live for the whole call per
    // this function's contract.
    unsafe {
        attributes.set(
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            hpcon.cast_const(),
            mem::size_of::<HPCON>(),
        )?;
    }
    // SAFETY: `job_handle` is a live job handle that outlives `attributes`
    // (see its declaration above), and one handle is exactly the one-element
    // array this attribute expects.
    unsafe {
        attributes.set(
            PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
            ptr::addr_of!(job_handle).cast(),
            mem::size_of::<HANDLE>(),
        )?;
    }

    let startup_info = STARTUPINFOEXW {
        StartupInfo: STARTUPINFOW {
            // `cb` describes the *extended* structure, which is how
            // `EXTENDED_STARTUPINFO_PRESENT` tells the kernel an attribute
            // list follows the classic fields.
            cb: mem::size_of::<STARTUPINFOEXW>() as u32,
            dwFlags: STARTF_USESTDHANDLES,
            hStdInput: INVALID_HANDLE_VALUE,
            hStdOutput: INVALID_HANDLE_VALUE,
            hStdError: INVALID_HANDLE_VALUE,
            ..Default::default()
        },
        lpAttributeList: attributes.as_ptr(),
    };

    let flags =
        EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | cmd.get_creation_flags();
    let mut process_info = PROCESS_INFORMATION::default();

    // SAFETY: every pointer passed here is either NULL or points to a live
    // buffer owned by this frame for the duration of the call:
    // `command_line` is a mutable NUL-terminated UTF-16 buffer, `environment`
    // a double-NUL-terminated block matching `CREATE_UNICODE_ENVIRONMENT`,
    // `working_dir` a NUL-terminated path, and `startup_info` an initialized
    // `STARTUPINFOEXW` whose `cb` and attribute list match `flags`.
    let created = unsafe {
        CreateProcessW(
            ptr::null(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0, // bInheritHandles = FALSE; see the module docs.
            flags,
            environment
                .as_ref()
                .map_or(ptr::null(), |block| block.as_ptr().cast()),
            working_dir.as_ref().map_or(ptr::null(), |dir| dir.as_ptr()),
            ptr::addr_of!(startup_info).cast::<STARTUPINFOW>(),
            &mut process_info,
        )
    };
    if created == 0 {
        // `attributes` is destroyed by its `Drop` on the way out.
        return Err(io::Error::last_os_error());
    }

    // SAFETY: `CreateProcessW` succeeded, so both handles are open and owned
    // by this process. Wrapping each in an `OwnedHandle` immediately makes it
    // closed exactly once even if anything below panics.
    let process = unsafe { OwnedHandle::from_raw_handle(process_info.hProcess) };
    // SAFETY: as above, for the primary thread handle.
    let thread = unsafe { OwnedHandle::from_raw_handle(process_info.hThread) };
    // The thread was never suspended and this crate has no use for it; close
    // the handle now so the thread object is freed the moment it exits.
    drop(thread);

    Ok(SpawnedChild {
        process,
        pid: process_info.dwProcessId,
    })
}

/// An initialized `PROC_THREAD_ATTRIBUTE_LIST` and the storage behind it.
///
/// `DeleteProcThreadAttributeList` runs in [`Drop`], so every early return in
/// [`spawn`] — including a failed `UpdateProcThreadAttribute` or
/// `CreateProcessW` — unwinds the list correctly.
struct AttributeList {
    /// Backing storage for the opaque list.
    ///
    /// The element type is `usize` rather than `u8` on purpose: the list holds
    /// pointers internally, and a `Vec<u8>` only guarantees byte alignment.
    /// The allocation is never resized after `InitializeProcThreadAttributeList`
    /// has run, so the list's own internal references stay valid.
    buffer: Vec<usize>,
}

impl AttributeList {
    /// Allocates and initializes a list with room for `attributes` entries.
    ///
    /// # Errors
    ///
    /// Returns the OS error from either `InitializeProcThreadAttributeList`
    /// call. A *successful* size probe is also an error: it would mean the
    /// required size was never reported and the buffer size is unknown.
    fn new(attributes: u32) -> io::Result<Self> {
        let mut size: usize = 0;
        // SAFETY: the documented size probe. A NULL list is explicitly allowed
        // here; the call writes the required byte count to `size` and fails.
        let probed =
            unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), attributes, 0, &mut size) };
        if probed != 0 {
            return Err(io::Error::other(
                "InitializeProcThreadAttributeList unexpectedly succeeded while probing \
                 for the attribute list size",
            ));
        }
        let probe_error = io::Error::last_os_error();
        if probe_error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
            return Err(probe_error);
        }

        // Round up to whole `usize` words, and never allocate zero words: an
        // empty `Vec` has a dangling pointer, which must not reach the API.
        let words = size.div_ceil(mem::size_of::<usize>()).max(1);
        let mut buffer = vec![0usize; words];

        // SAFETY: `buffer` is at least `size` bytes long and pointer-aligned,
        // and `size` is the value the probe reported for this many attributes.
        let ok = unsafe {
            InitializeProcThreadAttributeList(buffer.as_mut_ptr().cast(), attributes, 0, &mut size)
        };
        if ok == 0 {
            // Nothing was initialized, so `DeleteProcThreadAttributeList` must
            // not run; returning before constructing `Self` guarantees that.
            return Err(io::Error::last_os_error());
        }

        Ok(Self { buffer })
    }

    /// Returns the pointer to hand to `STARTUPINFOEXW::lpAttributeList`.
    fn as_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buffer.as_mut_ptr().cast()
    }

    /// Adds or replaces one attribute.
    ///
    /// # Errors
    ///
    /// Returns the OS error from `UpdateProcThreadAttribute`.
    ///
    /// # Safety
    ///
    /// `value` must be what the attribute identified by `attribute` expects,
    /// and `size` its size in bytes. Attributes that take a pointer require
    /// the pointee to stay valid and unmoved until this list is dropped,
    /// because `UpdateProcThreadAttribute` stores the pointer rather than
    /// copying the data.
    unsafe fn set(
        &mut self,
        attribute: usize,
        value: *const c_void,
        size: usize,
    ) -> io::Result<()> {
        // SAFETY: `self` is an initialized attribute list with room for the
        // configured number of attributes; `value` and `size` are valid per
        // this function's contract. The two optional out-parameters are
        // documented as reserved and must be NULL.
        let ok = unsafe {
            UpdateProcThreadAttribute(
                self.as_ptr(),
                0,
                attribute,
                value,
                size,
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: `Self` is only ever constructed after
        // `InitializeProcThreadAttributeList` succeeded, and `Drop` runs once,
        // so this deletes an initialized list exactly once. The call only
        // releases the list's internal references; the `Vec` frees the memory
        // immediately afterwards.
        unsafe { DeleteProcThreadAttributeList(self.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
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
    use crate::core::wait::{spawn_legacy_watcher, ProcessWaiter};
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
            Ok(()) => {}
            // The sender was dropped without sending: `f` panicked, and the
            // join below re-raises it with its original message.
            Err(mpsc::RecvTimeoutError::Disconnected) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("`{name}` hung for more than {timeout:?}")
            }
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
        job: Job,
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
            let console =
                PseudoConsole::new(backend, Size::new(24, 80), conin_read, conout_write, false)
                    .expect("CreatePseudoConsole must succeed");
            Self {
                console,
                job: Job::create(kill_on_close).expect("creating a job must succeed"),
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
            if !released {
                self.start_legacy_watcher(child);
            }
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
            self.start_legacy_watcher(child);
        }

        fn start_legacy_watcher(&self, child: &SpawnedChild) {
            let watched = child
                .process
                .as_handle()
                .try_clone_to_owned()
                .expect("duplicating the process handle must succeed");
            spawn_legacy_watcher(watched, Arc::clone(self.console.shared()), TEST_GRACE)
                .expect("spawning the legacy watcher must succeed");
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
        let mut list = AttributeList::new(ATTRIBUTE_COUNT).expect("initialization must succeed");
        assert!(!list.as_ptr().is_null());
        // The `Drop` impl deletes the list; running it here (rather than at
        // the end of the test) keeps the failure localized if it misbehaves.
        drop(list);
    }
}
