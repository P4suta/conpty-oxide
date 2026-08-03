// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The half of a session that does not depend on how bytes are moved.
//!
//! The two front ends differ only in their I/O: the blocking one owns
//! [`std::fs::File`]s, the async one owns tokio named-pipe halves. Everything
//! else about a session is identical — the `HPCON` and the backend behind it,
//! the last accepted size, the one-root-child rule, and above all the exact
//! order in which a child has to be spawned. That shared part lives here, so
//! it cannot be right in one front end and subtly wrong in the other.
//!
//! # The spawn order
//!
//! [`spawn_root`] performs the three steps `ConPTY` requires, in this order and
//! no other:
//!
//! 1. Create the job object and the process. Until the child exists there is
//!    nothing for the console host to serve.
//! 2. `ReleasePseudoConsole`, immediately afterwards. This hands the console
//!    host its own lifetime back so it exits when its last client disconnects
//!    — which is what makes the output pipe reach end-of-file naturally.
//! 3. Register a root wait for every backend. When the root exits it terminates
//!    remaining Job members; on a legacy backend it additionally requests
//!    pseudoconsole close after the drain grace so output reaches EOF.
//!
//! Doing 2 before 1 would release a console that has no client yet. Skipping 3
//! would violate the managed session's root-bounded process-tree and EOF
//! contract.

use std::io;
use std::os::windows::io::{AsHandle, OwnedHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::backend::BackendKind;
use crate::command::Command;
pub(crate) use crate::core::job::KILL_EXIT_CODE;
use crate::core::job::{self, Job};
use crate::core::proc;
use crate::core::pseudocon::PseudoConsole;
use crate::core::wait::{spawn_root_watcher, LEGACY_CLOSE_GRACE};
use crate::error::{Error, Result};
use crate::size::Size;

/// Capability name reported with [`crate::ErrorKind::UnsupportedFeature`] when the backend
/// cannot clear the pseudoconsole.
pub(crate) const CLEAR_FEATURE: &str = "ClearPseudoConsole";

/// The console-side state of one session: everything except the pipe ends.
///
/// Both front ends keep this behind an `Arc`, shared between the session
/// handle and the controller a split leaves behind, so the parts of a session
/// may be dropped in any order. The pipe ends are deliberately *not* in here:
/// they belong to the read and write halves so those can move to other tasks
/// or threads independently.
#[derive(Debug)]
pub(crate) struct Session {
    console: PseudoConsole,
    eof_on_root_exit: bool,
    /// Whether a root-child spawn has been reserved for this pseudoconsole.
    ///
    /// One session hosts exactly one root child. Re-using a session would be
    /// unsound rather than merely surprising: on a legacy backend the watcher
    /// closes the pseudoconsole after the first child exits, and a second
    /// `CreateProcessW` would then hand a freed `HPCON` to the kernel.
    spawned: AtomicBool,
    /// Whether `CreateProcessW` attached a child to this pseudoconsole.
    ///
    /// Kept separate from `spawned`: that flag is claimed before the
    /// fallible process creation to serialize competing attempts, while input
    /// retirement must request close only after a child really exists.
    attached: AtomicBool,
}

impl Session {
    /// Wraps a freshly created pseudoconsole together with the settings the
    /// session was built with.
    pub(crate) const fn new(console: PseudoConsole, eof_on_root_exit: bool) -> Self {
        Self {
            console,
            eof_on_root_exit,
            spawned: AtomicBool::new(false),
            attached: AtomicBool::new(false),
        }
    }

    /// Returns the size last accepted by [`Session::resize`], or the size the
    /// session was built with.
    pub(crate) fn size(&self) -> Size {
        self.console.size()
    }

    /// Resizes the pseudoconsole and records the new size.
    pub(crate) fn resize(&self, size: Size) -> Result<()> {
        self.console.resize(size).map_err(Error::resize)
    }

    /// Clears the pseudoconsole's screen and scrollback.
    pub(crate) fn clear(&self) -> Result<()> {
        // Asked before the call so a missing capability is reported as such
        // rather than as an opaque I/O failure.
        if !self.console.supports_clear() {
            return Err(Error::unsupported_feature(CLEAR_FEATURE));
        }
        self.console.clear().map_err(Error::clear)
    }

    /// Returns whether [`Session::clear`] is available on this backend.
    pub(crate) fn supports_clear(&self) -> bool {
        self.console.supports_clear()
    }

    /// Returns whether this session's backend exports
    /// `ReleasePseudoConsole`; see `ConPtyBackend::supports_release`.
    #[cfg(test)]
    pub(crate) fn supports_release(&self) -> bool {
        self.console.supports_release()
    }

    /// Returns whether the conout reader has retired or observed EOF.
    #[cfg(test)]
    pub(crate) fn reader_finished(&self) -> bool {
        self.console.shared().reader_finished()
    }

    /// Returns which `ConPTY` implementation backs this session.
    pub(crate) fn backend_kind(&self) -> &BackendKind {
        self.console.backend_kind()
    }

    /// Ends a spawned session after its input pipe has been closed.
    ///
    /// Closing conin alone does not reliably disconnect console clients on
    /// pre-24H2 Windows. Requesting the pseudoconsole close sends the required
    /// `CTRL_CLOSE_EVENT`; the lifecycle core keeps legacy close off this
    /// caller's thread so output can continue draining concurrently.
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    pub(crate) fn request_close_after_input(&self) {
        if self.attached.load(Ordering::SeqCst) {
            self.console.shared().request_close_detached();
        }
    }
}

/// A freshly spawned root child, in the shape both front ends' `Child` needs.
///
/// The job object travels with the process handle because terminating the job
/// — not the process — is what kills the whole tree.
#[derive(Debug)]
pub(crate) struct RootChild {
    /// The windows-spawn-owned root process and its status cache.
    pub(super) child: windows_spawn::Child,
    /// The job object the child and all its descendants belong to.
    pub(super) job: Arc<Job>,
    /// The root child's process identifier.
    pub(super) pid: u32,
    /// Whether the front end's `Child::drop` should terminate the tree.
    pub(super) kill_on_drop: bool,
}

/// Spawns `command` as the root child of `session` and arms the session's
/// shutdown strategy.
///
/// See the module documentation for why the three steps happen in this order.
/// A session hosts exactly one root child; a second call fails with an
/// [`io::ErrorKind::AlreadyExists`] source. A spawn that fails before the
/// child process exists leaves the session reusable, because nothing was
/// attached to the pseudoconsole and no watcher ran. The one failure past
/// that point — arming the root watcher — terminates the child and retires
/// the session for good: a child was already attached, so later spawns keep
/// failing with `AlreadyExists`.
///
/// # Errors
///
/// [`crate::ErrorKind::Spawn`] carrying the program name and the underlying failure:
/// [`io::ErrorKind::NotFound`] for a missing executable,
/// [`io::ErrorKind::InvalidInput`] for a command line or environment block
/// that cannot be built, [`io::ErrorKind::AlreadyExists`] for a re-used
/// session, or the raw OS error from `CreateProcessW`.
pub(crate) fn spawn_root(
    session: &Session,
    command: &mut Command,
    kill_on_drop: bool,
) -> Result<RootChild> {
    spawn_root_with_watcher_handle(session, command, kill_on_drop, duplicate_watcher_handle)
}

fn spawn_root_with_watcher_handle<F>(
    session: &Session,
    command: &mut Command,
    kill_on_drop: bool,
    duplicate: F,
) -> Result<RootChild>
where
    F: FnOnce(&windows_spawn::Child) -> io::Result<OwnedHandle>,
{
    if session.spawned.swap(true, Ordering::SeqCst) {
        return Err(spawn_error(
            command,
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "this pseudoconsole already hosts a root child process",
            ),
        ));
    }

    let (job, child) = match create_child(session, command, kill_on_drop) {
        Ok(started) => started,
        Err(err) => {
            // Nothing was attached to the pseudoconsole and no watcher ran, so
            // the session is untouched and can be used for another attempt.
            session.spawned.store(false, Ordering::SeqCst);
            return Err(spawn_error(command, err));
        },
    };
    session.attached.store(true, Ordering::SeqCst);
    let pid = child.id();

    // Step two of the lifecycle, immediately after the child exists: hand the
    // pseudoconsole its own lifetime back, so that it exits when the last
    // client disconnects.
    let released = match session.console.release_after_spawn() {
        Ok(released) => released,
        Err(err) => {
            // The session is demoted to legacy mode; from here on it is
            // indistinguishable from a backend without the export.
            log_release_failure(&err);
            false
        },
    };

    // Step three for every backend: root exit bounds the whole managed tree.
    // Legacy sessions additionally use this registration to force EOF.
    let close_legacy = should_close_legacy_on_root_exit(released, session.eof_on_root_exit);
    if let Err(err) = arm_root_watcher(session, &child, &job, close_legacy, duplicate) {
        // A session without a root watcher cannot uphold its managed lifecycle
        // contract. Terminate the child rather than return it; `spawned`
        // deliberately stays `true`, because the session is now retired.
        if let Err(kill_err) = job.terminate(KILL_EXIT_CODE) {
            log_spawn_cleanup_failure(&kill_err);
        }
        session.console.shared().request_close_detached();
        return Err(spawn_error(command, err));
    }

    Ok(RootChild {
        child,
        job,
        pid,
        kill_on_drop,
    })
}

/// Returns whether the root watcher must force legacy EOF after killing the tree.
///
/// Keeping the two independent conditions explicit and truth-table tested is
/// important: released sessions reach EOF naturally, while opting out on a
/// legacy session deliberately leaves the reader open after root exit.
const fn should_close_legacy_on_root_exit(released: bool, eof_on_root_exit: bool) -> bool {
    !released && eof_on_root_exit
}

/// Creates the job object and the process itself.
fn create_child(
    session: &Session,
    command: &mut Command,
    kill_on_drop: bool,
) -> io::Result<(Arc<Job>, windows_spawn::Child)> {
    let job = Arc::new(job::create(kill_on_drop)?);
    let pseudoconsole = session.console.spawn_capability()?;
    let child = proc::spawn(command, &pseudoconsole, &job)?;
    Ok((job, child))
}

/// Starts the watcher that bounds the managed tree and, when needed, forces EOF.
///
/// The watcher gets its own duplicate of the process handle so it is
/// independent of the `Child` the caller receives — which may be dropped long
/// before the child exits.
fn arm_root_watcher<F>(
    session: &Session,
    child: &windows_spawn::Child,
    job: &Arc<Job>,
    close_legacy: bool,
    duplicate: F,
) -> io::Result<()>
where
    F: FnOnce(&windows_spawn::Child) -> io::Result<OwnedHandle>,
{
    let watched = duplicate(child)?;
    spawn_root_watcher(
        watched,
        Arc::downgrade(job),
        Arc::clone(session.console.shared()),
        LEGACY_CLOSE_GRACE,
        close_legacy,
    )
}

fn duplicate_watcher_handle(child: &windows_spawn::Child) -> io::Result<OwnedHandle> {
    child.as_handle().try_clone_to_owned()
}

/// Wraps a spawn failure with the program that could not be started.
fn spawn_error(command: &Command, source: io::Error) -> Error {
    Error::spawn(command.get_program().to_os_string(), source)
}

/// Reports a failed `ReleasePseudoConsole`.
///
/// Not an error for the caller: the session simply falls back to the legacy
/// shutdown path, which restores the same end-of-file contract. It is logged
/// (with the `tracing` feature) because a silent demotion would make the
/// resulting one-second teardown delay look like a bug.
#[cfg(feature = "tracing")]
fn log_release_failure(err: &io::Error) {
    tracing::warn!(
        error = %err,
        "ReleasePseudoConsole failed; the session falls back to the legacy shutdown path"
    );
}

/// Discards a release failure when diagnostics are disabled.
#[cfg(not(feature = "tracing"))]
const fn log_release_failure(_err: &io::Error) {}

#[cfg(feature = "tracing")]
fn log_spawn_cleanup_failure(err: &io::Error) {
    tracing::error!(
        error = %err,
        "failed to terminate a child after root watcher setup failed"
    );
}

#[cfg(not(feature = "tracing"))]
const fn log_spawn_cleanup_failure(_err: &io::Error) {}

#[cfg(test)]
mod tests {
    use std::io;
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    use std::mem::size_of;
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};

    #[cfg(any(feature = "blocking", feature = "tokio"))]
    use std::sync::atomic::Ordering;
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    use std::time::{Duration, Instant};

    use super::should_close_legacy_on_root_exit;
    #[cfg(feature = "tracing")]
    use super::{log_release_failure, log_spawn_cleanup_failure};
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    use super::{spawn_root, spawn_root_with_watcher_handle, Session};

    #[cfg(any(feature = "blocking", feature = "tokio"))]
    use crate::backend::ConPtyBackend;
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    use crate::core::pipes::{create_sync_pipes, SyncPipes};
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    use crate::core::pseudocon::PseudoConsole;
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    use crate::error::ErrorKind;
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    use crate::size::Size;
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    use windows_sys::Win32::Foundation::{INVALID_HANDLE_VALUE, WAIT_TIMEOUT};
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};

    #[cfg(any(feature = "blocking", feature = "tokio"))]
    fn process_is_running(pid: u32) -> bool {
        // SAFETY: OpenProcess receives a process identifier observed from a
        // child we created and requests only synchronization access.
        let raw = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
        if raw.is_null() {
            return false;
        }
        // SAFETY: a non-null successful OpenProcess result is a uniquely owned
        // handle and is adopted exactly once.
        let process = unsafe { OwnedHandle::from_raw_handle(raw as RawHandle) };
        // SAFETY: the owned process handle remains valid for this zero-timeout
        // wait and needs no writable output buffers.
        unsafe { WaitForSingleObject(process.as_raw_handle(), 0) == WAIT_TIMEOUT }
    }

    #[cfg(any(feature = "blocking", feature = "tokio"))]
    fn direct_child_of(root: u32) -> Option<u32> {
        // SAFETY: this requests an owned snapshot of the current process list
        // and carries no caller-provided pointers.
        let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if raw == INVALID_HANDLE_VALUE {
            return None;
        }
        // SAFETY: a successful snapshot call returns one uniquely owned handle.
        let snapshot = unsafe { OwnedHandle::from_raw_handle(raw as RawHandle) };
        let mut entry = PROCESSENTRY32W {
            dwSize: u32::try_from(size_of::<PROCESSENTRY32W>())
                .expect("PROCESSENTRY32W size fits in u32"),
            ..Default::default()
        };
        // SAFETY: snapshot remains live and entry is a correctly sized,
        // writable PROCESSENTRY32W output buffer.
        let mut present = unsafe { Process32FirstW(snapshot.as_raw_handle(), &mut entry) } != 0;
        while present {
            if entry.th32ParentProcessID == root {
                return Some(entry.th32ProcessID);
            }
            // SAFETY: the same snapshot and output buffer remain live.
            present = unsafe { Process32NextW(snapshot.as_raw_handle(), &mut entry) } != 0;
        }
        None
    }

    #[test]
    fn legacy_close_requires_both_legacy_mode_and_eof_policy() {
        assert!(should_close_legacy_on_root_exit(false, true));
        assert!(!should_close_legacy_on_root_exit(false, false));
        assert!(!should_close_legacy_on_root_exit(true, true));
        assert!(!should_close_legacy_on_root_exit(true, false));
    }

    #[cfg(any(feature = "blocking", feature = "tokio"))]
    #[test]
    fn input_retirement_requests_close_only_after_a_child_attaches() {
        let backend = ConPtyBackend::system().expect("ConPTY must be available");
        let SyncPipes {
            conout_read,
            conout_write,
            conin_read,
            conin_write,
        } = create_sync_pipes().expect("creating pipes must succeed");
        let console = PseudoConsole::new(backend, Size::default(), conin_read, conout_write, false)
            .expect("CreatePseudoConsole must succeed");
        let shared = std::sync::Arc::clone(console.shared());
        let session = Session::new(console, true);

        // Retire the raw reader so either backend can claim close promptly;
        // this test observes the Session-to-lifecycle request, not host I/O.
        drop(conout_read);
        shared.notify_reader_closed();

        session.request_close_after_input();
        assert!(
            !shared.is_closed(),
            "an idle session must stay usable when its writer is dropped"
        );

        session.attached.store(true, Ordering::SeqCst);
        session.request_close_after_input();
        assert!(
            shared.is_closed(),
            "input retirement after attachment must claim pseudoconsole close"
        );

        drop(conin_write);
    }

    #[cfg(any(feature = "blocking", feature = "tokio"))]
    #[test]
    fn watcher_handle_duplication_failure_retires_and_kills_the_tree() {
        let backend = ConPtyBackend::system().expect("ConPTY must be available");
        let SyncPipes {
            conout_read,
            conout_write,
            conin_read,
            conin_write,
        } = create_sync_pipes().expect("creating pipes must succeed");
        let console = PseudoConsole::new(backend, Size::default(), conin_read, conout_write, false)
            .expect("CreatePseudoConsole must succeed");
        let session = Session::new(console, true);

        let mut command = crate::command::Command::new("cmd.exe");
        command.args(["/D", "/S", "/C"]).raw_arg(
            "start \"\" /b ping.exe -t 127.0.0.1 >nul & +             ping.exe -n 30 127.0.0.1 >nul",
        );

        let mut observed = None;
        let error = spawn_root_with_watcher_handle(&session, &mut command, false, |child| {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut descendant = direct_child_of(child.id());
            while descendant.is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
                descendant = direct_child_of(child.id());
            }
            let root = child.id();
            let descendant =
                descendant.ok_or_else(|| io::Error::other("no descendant appeared"))?;
            observed = Some((root, descendant));
            Err(io::Error::other(
                "injected watcher handle duplication failure",
            ))
        })
        .expect_err("watcher handle duplication failure must fail the spawn");
        assert_eq!(error.kind(), ErrorKind::Spawn);
        assert_eq!(
            error.io_error().map(io::Error::kind),
            Some(io::ErrorKind::Other)
        );
        assert!(error
            .io_error()
            .is_some_and(|source| source.to_string().contains("injected watcher")));

        let mut second = crate::command::Command::new("cmd.exe");
        let reused = spawn_root(&session, &mut second, false)
            .expect_err("a post-creation failure must retire the pseudoconsole");
        assert_eq!(reused.kind(), ErrorKind::Spawn);
        assert_eq!(
            reused.io_error().map(io::Error::kind),
            Some(io::ErrorKind::AlreadyExists)
        );

        let (root, descendant) = observed.expect("the injected failure observed both pids");
        let deadline = Instant::now() + Duration::from_secs(5);
        while (process_is_running(root) || process_is_running(descendant))
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !process_is_running(root),
            "the root process survived cleanup"
        );
        assert!(
            !process_is_running(descendant),
            "the descendant process survived cleanup"
        );

        drop(conin_write);
        drop(conout_read);
    }

    #[cfg(feature = "tracing")]
    #[test]
    fn lifecycle_fallback_failures_are_logged() {
        let error = io::Error::other("injected lifecycle failure");
        let events = crate::tracing_test_support::count_events(|| {
            log_release_failure(&error);
            log_spawn_cleanup_failure(&error);
        });
        assert_eq!(events, 2);
    }
}
