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
//! 3. Only if the release did not happen (old Windows, or a failed call),
//!    register the legacy process wait that forces end-of-file after the root
//!    child exits.
//!
//! Doing 2 before 1 would release a console that has no client yet; skipping 3
//! on a legacy backend would leave a reader waiting for an end-of-file that
//! can never arrive.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::backend::BackendKind;
use crate::command::Command;
use crate::core::job::Job;
use crate::core::proc::{self, SpawnedChild};
use crate::core::pseudocon::PseudoConsole;
use crate::core::wait::{spawn_legacy_watcher, ProcessWaiter, LEGACY_CLOSE_GRACE};
use crate::error::{Error, Result};
use crate::size::Size;

/// Exit code reported for a process tree terminated by a front end's
/// `Child::kill`.
///
/// Matches `std::process::Child::kill`, which passes `1` to
/// `TerminateProcess`.
pub(crate) const KILL_EXIT_CODE: u32 = 1;

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
    /// Exit detection for the root process itself.
    pub(super) waiter: ProcessWaiter,
    /// The job object the child and all its descendants belong to.
    pub(super) job: Job,
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
/// that point — arming the legacy watcher — terminates the child and retires
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
    command: &Command,
    kill_on_drop: bool,
) -> Result<RootChild> {
    if session.spawned.swap(true, Ordering::SeqCst) {
        return Err(spawn_error(
            command,
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "this pseudoconsole already hosts a root child process",
            ),
        ));
    }

    let (job, spawned) = match create_child(session, command, kill_on_drop) {
        Ok(started) => started,
        Err(err) => {
            // Nothing was attached to the pseudoconsole and no watcher ran, so
            // the session is untouched and can be used for another attempt.
            session.spawned.store(false, Ordering::SeqCst);
            return Err(spawn_error(command, err));
        },
    };
    session.attached.store(true, Ordering::SeqCst);

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

    // Step three, only for sessions that could not be released: force
    // end-of-file when the root child exits.
    if should_arm_legacy_watcher(released, session.eof_on_root_exit) {
        if let Err(err) = arm_legacy_watcher(session, &spawned) {
            // A legacy session without a watcher could never reach
            // end-of-file, so this is fatal. Terminate the child rather than
            // return a session that can never finish; `spawned` deliberately
            // stays `true`, because a child was attached to the pseudoconsole
            // — the session is retired, not reusable.
            if let Err(kill_err) = job.terminate(KILL_EXIT_CODE) {
                log_spawn_cleanup_failure(&kill_err);
            }
            return Err(spawn_error(command, err));
        }
    }

    Ok(RootChild {
        waiter: ProcessWaiter::new(spawned.process),
        job,
        pid: spawned.pid,
        kill_on_drop,
    })
}

/// Returns whether this session needs the root-exit watcher that forces EOF.
///
/// Keeping the two independent conditions explicit and truth-table tested is
/// important: released sessions reach EOF naturally, while opting out on a
/// legacy session deliberately leaves the reader open after root exit.
const fn should_arm_legacy_watcher(released: bool, eof_on_root_exit: bool) -> bool {
    !released && eof_on_root_exit
}

/// Creates the job object and the process itself.
fn create_child(
    session: &Session,
    command: &Command,
    kill_on_drop: bool,
) -> io::Result<(Job, SpawnedChild)> {
    let job = Job::create(kill_on_drop)?;
    let spawned = proc::spawn(command, session.console.hpcon(), &job)?;
    Ok((job, spawned))
}

/// Starts the watcher that forces end-of-file after the root child exits.
///
/// The watcher gets its own duplicate of the process handle so it is
/// independent of the `Child` the caller receives — which may be dropped long
/// before the child exits.
fn arm_legacy_watcher(session: &Session, spawned: &SpawnedChild) -> io::Result<()> {
    use std::os::windows::io::AsHandle;

    let watched = spawned.process.as_handle().try_clone_to_owned()?;
    spawn_legacy_watcher(
        watched,
        Arc::clone(session.console.shared()),
        LEGACY_CLOSE_GRACE,
    )
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
        "failed to terminate a child after legacy watcher setup failed"
    );
}

#[cfg(not(feature = "tracing"))]
const fn log_spawn_cleanup_failure(_err: &io::Error) {}

#[cfg(test)]
mod tests {
    #[cfg(feature = "tracing")]
    use std::io;

    #[cfg(any(feature = "blocking", feature = "tokio"))]
    use std::sync::atomic::Ordering;

    use super::should_arm_legacy_watcher;
    #[cfg(feature = "tracing")]
    use super::{log_release_failure, log_spawn_cleanup_failure};

    #[cfg(any(feature = "blocking", feature = "tokio"))]
    use crate::backend::ConPtyBackend;
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    use crate::core::pipes::{create_sync_pipes, SyncPipes};
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    use crate::core::pseudocon::PseudoConsole;
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    use crate::size::Size;

    #[test]
    fn legacy_watcher_requires_both_legacy_mode_and_eof_policy() {
        assert!(should_arm_legacy_watcher(false, true));
        assert!(!should_arm_legacy_watcher(false, false));
        assert!(!should_arm_legacy_watcher(true, true));
        assert!(!should_arm_legacy_watcher(true, false));
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
        let session = super::Session::new(console, true);

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
