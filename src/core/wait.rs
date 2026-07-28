//! Child-process exit detection and the legacy shutdown watcher.
//!
//! Exit detection is handle-based: [`ProcessWaiter`] waits on the process
//! handle with `WaitForSingleObject` and reads the exit code only *after*
//! the wait has confirmed the process is gone. Calling `GetExitCodeProcess`
//! on a running process "succeeds" with the sentinel `STILL_ACTIVE` (259),
//! which is indistinguishable from a real exit code of 259 — sequencing the
//! two calls is the only correct protocol.
//!
//! The other half of this module is the **legacy watcher**
//! ([`spawn_legacy_watcher`]): on Windows versions without
//! `ReleasePseudoConsole`, the console host outlives the child, so the conout
//! pipe never reaches end-of-file on its own. The watcher restores the EOF
//! contract: it waits for the child to exit on a dedicated thread, grants the
//! reader a grace period to drain the host's remaining output, and then asks
//! the lifecycle state machine to close the pseudoconsole — from its own
//! thread, never the reader's — which breaks the conout pipe and surfaces as
//! end-of-file (`ERROR_BROKEN_PIPE`) to the reader.

use std::io;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};

use crate::core::pseudocon::ConsoleShared;

/// Default drain grace between child exit and the legacy close.
///
/// After the child exits, the console host may still hold rendered output
/// that the reader has not consumed yet. Closing immediately would still be
/// correct (the reader keeps draining while the close blocks), but the grace
/// period lets the common case — reader catches up quickly — finish the tail
/// before teardown begins.
pub(crate) const LEGACY_CLOSE_GRACE: Duration = Duration::from_millis(1000);

/// Waits on a process handle and reads its exit code.
///
/// Works with any process handle that has `SYNCHRONIZE` and
/// `PROCESS_QUERY_(LIMITED_)INFORMATION` access — which a handle from
/// `CreateProcessW`, or a `DuplicateHandle`/`try_clone_to_owned` copy of
/// one, always does.
#[derive(Debug)]
pub(crate) struct ProcessWaiter {
    process: OwnedHandle,
}

impl ProcessWaiter {
    /// Wraps an owned process handle.
    pub(crate) fn new(process: OwnedHandle) -> Self {
        Self { process }
    }

    /// Blocks until the process exits and returns its exit code.
    ///
    /// Safe to call repeatedly and from multiple threads: a process handle
    /// stays signaled forever once the process has exited.
    ///
    /// # Errors
    ///
    /// Returns the OS error from `WaitForSingleObject` or
    /// `GetExitCodeProcess`.
    pub(crate) fn wait(&self) -> io::Result<u32> {
        // SAFETY: the handle is owned by `self` and thus live for the call.
        let waited = unsafe { WaitForSingleObject(self.process.as_raw_handle(), INFINITE) };
        match waited {
            WAIT_OBJECT_0 => self.exit_code(),
            WAIT_FAILED => Err(io::Error::last_os_error()),
            // WAIT_ABANDONED cannot happen (the handle is not a mutex) and
            // WAIT_TIMEOUT cannot happen with INFINITE; treat defensively.
            other => Err(io::Error::other(format!(
                "unexpected WaitForSingleObject result {other:#x} while waiting for child exit"
            ))),
        }
    }

    /// Returns the exit code if the process has already exited, without
    /// blocking.
    ///
    /// # Errors
    ///
    /// Returns the OS error from `WaitForSingleObject` or
    /// `GetExitCodeProcess`.
    pub(crate) fn try_wait(&self) -> io::Result<Option<u32>> {
        // SAFETY: the handle is owned by `self` and thus live for the call.
        let waited = unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) };
        match waited {
            WAIT_OBJECT_0 => self.exit_code().map(Some),
            WAIT_TIMEOUT => Ok(None),
            WAIT_FAILED => Err(io::Error::last_os_error()),
            other => Err(io::Error::other(format!(
                "unexpected WaitForSingleObject result {other:#x} while polling child exit"
            ))),
        }
    }

    /// Reads the exit code of a process known to have exited.
    ///
    /// Only called after a wait has confirmed the exit; on a still-running
    /// process the OS would "successfully" report `STILL_ACTIVE` (259) here.
    fn exit_code(&self) -> io::Result<u32> {
        let mut code: u32 = 0;
        // SAFETY: the handle is owned by `self`, and `code` is a valid
        // out-parameter for the duration of the call.
        let ok = unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &mut code) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(code)
    }
}

/// Spawns the legacy shutdown watcher for one session.
///
/// `process` must be a *duplicate* of the child's process handle (e.g. from
/// `BorrowedHandle::try_clone_to_owned`, which wraps `DuplicateHandle` with
/// `DUPLICATE_SAME_ACCESS`), so the watcher's wait is independent of the
/// lifetime of the handle the caller keeps for its own `wait`/`kill` API.
///
/// The watcher thread, detached and never joined:
///
/// 1. Waits for the child to exit. Only a `Weak` reference to the lifecycle
///    core is held during this potentially unbounded wait, so an abandoned
///    session (every user handle dropped while the child lives on) can still
///    reach [`ConsoleShared`]'s final-defense drop instead of being pinned
///    by the watcher.
/// 2. Sleeps for `grace` (skipped when the reader is already done) to let
///    the reader drain output the console host produced before the exit.
/// 3. Calls [`ConsoleShared::request_close`]. The watcher thread satisfies
///    that method's contract — a dedicated, non-reader thread that may
///    block — and the close breaks the conout pipe, which the reader
///    observes as end-of-file.
///
/// If the session is already released, no thread is spawned: end-of-file
/// arrives naturally and closing is handled by the reader-side transitions.
///
/// # Errors
///
/// Returns the OS error if the watcher thread cannot be spawned. The caller
/// must treat that as fatal for the session (without a watcher, a legacy
/// session's reader would never see end-of-file).
pub(crate) fn spawn_legacy_watcher(
    process: OwnedHandle,
    shared: Arc<ConsoleShared>,
    grace: Duration,
) -> io::Result<()> {
    if shared.is_released() {
        return Ok(());
    }

    // Downgrade before the wait; see the doc comment (step 1).
    let weak = Arc::downgrade(&shared);
    drop(shared);

    thread::Builder::new()
        .name("conpty-oxide-legacy-watcher".into())
        .spawn(move || {
            let waiter = ProcessWaiter::new(process);
            // The result is deliberately ignored: whether the child exited or
            // the wait itself failed (e.g. a corrupted handle), the only
            // remedy is the same — tear the console down so the reader is not
            // left blocked forever.
            let _ = waiter.wait();

            let Some(shared) = weak.upgrade() else {
                // Every strong reference is gone; the final-defense drop has
                // already taken (or is taking) care of the close.
                return;
            };
            if !shared.reader_finished() {
                thread::sleep(grace);
            }
            shared.request_close();
        })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::windows::io::AsHandle;
    use std::process::{Child, Command, Stdio};
    use std::time::Instant;

    use crate::backend::ConPtyBackend;
    use crate::core::pipes::create_sync_pipes;
    use crate::core::pseudocon::PseudoConsole;
    use crate::size::Size;

    /// `GetExitCodeProcess`'s sentinel for "still running".
    const STILL_ACTIVE: u32 = 259;

    /// Spawns a `cmd.exe` child with all stdio detached from the test runner.
    fn spawn_cmd(args: &[&str]) -> Child {
        Command::new("cmd.exe")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawning cmd.exe must succeed")
    }

    /// Duplicates a child's process handle, as the spawn layer will for the
    /// watcher.
    fn duplicated_handle(child: &Child) -> OwnedHandle {
        child
            .as_handle()
            .try_clone_to_owned()
            .expect("DuplicateHandle must succeed")
    }

    #[test]
    fn wait_reports_the_exit_code() {
        let mut child = spawn_cmd(&["/C", "exit 7"]);
        let waiter = ProcessWaiter::new(duplicated_handle(&child));

        assert_eq!(waiter.wait().expect("wait must succeed"), 7);
        // A process handle stays signaled: waiting again is fine.
        assert_eq!(waiter.wait().expect("second wait must succeed"), 7);
        // And try_wait after exit reports the same code.
        assert_eq!(waiter.try_wait().expect("try_wait must succeed"), Some(7));

        child.wait().expect("reaping via std must also succeed");
    }

    #[test]
    fn try_wait_is_none_while_running_and_some_after_kill() {
        // `cmd /C pause` blocks reading stdin; the pipe is held open (and
        // never written) by the test, so the child stays alive until killed.
        let mut child = Command::new("cmd.exe")
            .args(["/C", "pause"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawning cmd.exe must succeed");
        let waiter = ProcessWaiter::new(duplicated_handle(&child));

        assert_eq!(
            waiter.try_wait().expect("try_wait must succeed"),
            None,
            "a blocked child must not report an exit code"
        );

        child.kill().expect("kill must succeed");
        let code = waiter.wait().expect("wait after kill must succeed");
        assert_ne!(
            code, STILL_ACTIVE,
            "an exit code read after the wait must never be STILL_ACTIVE"
        );
        child.wait().expect("reaping via std must also succeed");
    }

    #[test]
    fn legacy_watcher_closes_the_console_after_child_exit() {
        let backend = ConPtyBackend::system().expect("ConPTY must be available");
        let pipes = create_sync_pipes().expect("creating pipes must succeed");
        let console = PseudoConsole::new(
            backend,
            Size::default(),
            pipes.conin_read,
            pipes.conout_write,
            false,
        )
        .expect("CreatePseudoConsole must succeed");
        let shared = Arc::clone(console.shared());

        // The child is not attached to the pseudoconsole — the watcher only
        // watches the process handle, so any process exercises it. The
        // session is never released, so this is the legacy path even on a
        // machine whose backend supports release.
        let mut child = spawn_cmd(&["/C", "exit 0"]);
        spawn_legacy_watcher(
            duplicated_handle(&child),
            Arc::clone(&shared),
            Duration::from_millis(50),
        )
        .expect("spawning the watcher must succeed");

        // The reader (conout_read) stays open the whole time: the watcher
        // must close from its own thread regardless.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !shared.is_closed() {
            assert!(
                Instant::now() < deadline,
                "the watcher must close the console within 5 seconds"
            );
            thread::sleep(Duration::from_millis(10));
        }

        child.wait().expect("reaping via std must succeed");
        drop(console);
    }

    #[test]
    fn legacy_watcher_is_a_no_op_for_released_sessions() {
        let backend = ConPtyBackend::system().expect("ConPTY must be available");
        if !backend.supports_release() {
            return;
        }
        let pipes = create_sync_pipes().expect("creating pipes must succeed");
        let console = PseudoConsole::new(
            backend,
            Size::default(),
            pipes.conin_read,
            pipes.conout_write,
            false,
        )
        .expect("CreatePseudoConsole must succeed");
        assert!(console
            .release_after_spawn()
            .expect("ReleasePseudoConsole must succeed"));

        let mut child = spawn_cmd(&["/C", "exit 0"]);
        spawn_legacy_watcher(
            duplicated_handle(&child),
            Arc::clone(console.shared()),
            Duration::from_millis(10),
        )
        .expect("the released no-op path must succeed");
        child.wait().expect("reaping via std must succeed");

        // Give a (hypothetical, buggy) watcher thread time to act, then
        // confirm nothing closed the console behind our back.
        thread::sleep(Duration::from_millis(100));
        assert!(!console.shared().is_closed());
        drop(console);
    }
}
