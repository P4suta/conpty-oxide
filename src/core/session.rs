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
//! [`spawn_root`] performs the three steps ConPTY requires, in this order and
//! no other:
//!
//! 1. Create the job object and the process. Until the child exists there is
//!    nothing for the console host to serve.
//! 2. `ReleasePseudoConsole`, immediately afterwards. This hands the console
//!    host its own lifetime back so it exits when its last client disconnects
//!    — which is what makes the output pipe reach end-of-file naturally.
//! 3. Only if the release did not happen (old Windows, or a failed call),
//!    start the legacy watcher that forces end-of-file when the root child
//!    exits.
//!
//! Doing 2 before 1 would release a console that has no client yet; skipping 3
//! on a legacy backend would leave a reader waiting for an end-of-file that
//! can never arrive.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::backend::{BackendKind, ConPtyBackend};
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

/// Capability name reported by [`Error::UnsupportedFeature`] when the backend
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
    backend: ConPtyBackend,
    /// Last size that `ResizePseudoConsole` accepted.
    size: Mutex<Size>,
    eof_on_root_exit: bool,
    /// Whether a root child has been spawned into this pseudoconsole.
    ///
    /// One session hosts exactly one root child. Re-using a session would be
    /// unsound rather than merely surprising: on a legacy backend the watcher
    /// closes the pseudoconsole after the first child exits, and a second
    /// `CreateProcessW` would then hand a freed `HPCON` to the kernel.
    spawned: AtomicBool,
}

impl Session {
    /// Wraps a freshly created pseudoconsole together with the settings the
    /// session was built with.
    pub(crate) fn new(
        console: PseudoConsole,
        backend: ConPtyBackend,
        size: Size,
        eof_on_root_exit: bool,
    ) -> Self {
        Self {
            console,
            backend,
            size: Mutex::new(size),
            eof_on_root_exit,
            spawned: AtomicBool::new(false),
        }
    }

    /// Returns the size last accepted by [`Session::resize`], or the size the
    /// session was built with.
    pub(crate) fn size(&self) -> Size {
        *self.size.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Resizes the pseudoconsole and records the new size.
    pub(crate) fn resize(&self, size: Size) -> Result<()> {
        self.console.resize(size).map_err(Error::Resize)?;
        *self.size.lock().unwrap_or_else(PoisonError::into_inner) = size;
        Ok(())
    }

    /// Clears the pseudoconsole's screen and scrollback.
    pub(crate) fn clear(&self) -> Result<()> {
        // Asked before the call so a missing capability is reported as such
        // rather than as an opaque I/O failure.
        if !self.backend.supports_clear() {
            return Err(Error::UnsupportedFeature {
                feature: CLEAR_FEATURE,
            });
        }
        self.console.clear().map_err(Error::Clear)
    }

    /// Returns whether [`Session::clear`] is available on this backend.
    pub(crate) fn supports_clear(&self) -> bool {
        self.backend.supports_clear()
    }

    /// Returns which ConPTY implementation backs this session.
    pub(crate) fn backend_kind(&self) -> &BackendKind {
        self.backend.kind()
    }
}

/// A freshly spawned root child, in the shape both front ends' `Child` needs.
///
/// The job object travels with the process handle because terminating the job
/// — not the process — is what kills the whole tree.
#[derive(Debug)]
pub(crate) struct RootChild {
    /// Exit detection for the root process itself.
    pub(crate) waiter: ProcessWaiter,
    /// The job object the child and all its descendants belong to.
    pub(crate) job: Job,
    /// The root child's process identifier.
    pub(crate) pid: u32,
    /// Whether the front end's `Child::drop` should terminate the tree.
    pub(crate) kill_on_drop: bool,
}

/// Spawns `command` as the root child of `session` and arms the session's
/// shutdown strategy.
///
/// See the module documentation for why the three steps happen in this order.
/// A session hosts exactly one root child; a second call fails with an
/// [`io::ErrorKind::AlreadyExists`] source. A *failed* spawn leaves the
/// session reusable, because nothing was attached to the pseudoconsole and no
/// watcher ran.
///
/// # Errors
///
/// [`Error::Spawn`] carrying the program name and the underlying failure:
/// [`io::ErrorKind::NotFound`] for a missing executable,
/// [`io::ErrorKind::InvalidInput`] for a command line or environment block
/// that cannot be built, [`io::ErrorKind::AlreadyExists`] for a re-used
/// session, or the raw OS error from `CreateProcessW`.
pub(crate) fn spawn_root(session: &Session, command: &Command) -> Result<RootChild> {
    if session.spawned.swap(true, Ordering::SeqCst) {
        return Err(spawn_error(
            command,
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "this pseudoconsole already hosts a root child process",
            ),
        ));
    }

    let kill_on_drop = command.get_kill_on_drop();
    let (job, spawned) = match create_child(session, command, kill_on_drop) {
        Ok(started) => started,
        Err(err) => {
            // Nothing was attached to the pseudoconsole and no watcher ran, so
            // the session is untouched and can be used for another attempt.
            session.spawned.store(false, Ordering::SeqCst);
            return Err(spawn_error(command, err));
        }
    };

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
        }
    };

    // Step three, only for sessions that could not be released: force
    // end-of-file when the root child exits.
    if !released && session.eof_on_root_exit {
        if let Err(err) = arm_legacy_watcher(session, &spawned) {
            // A legacy session without a watcher could never reach
            // end-of-file, so this is fatal. Undo the spawn rather than return
            // a session that can never finish.
            let _ = job.terminate(KILL_EXIT_CODE);
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
    Error::Spawn {
        program: command.get_program().to_os_string(),
        source,
    }
}

/// Reports a failed `ReleasePseudoConsole`.
///
/// Not an error for the caller: the session simply falls back to the legacy
/// shutdown path, which restores the same end-of-file contract. It is logged
/// (with the `tracing` feature) because a silent demotion would make the
/// resulting one-second teardown delay look like a bug.
fn log_release_failure(err: &io::Error) {
    #[cfg(feature = "tracing")]
    tracing::warn!(
        error = %err,
        "ReleasePseudoConsole failed; the session falls back to the legacy shutdown path"
    );
    #[cfg(not(feature = "tracing"))]
    {
        let _ = err;
    }
}
