//! Pseudoconsole lifecycle: `HPCON` ownership and the release/close state
//! machine.
//!
//! This module owns the hardest part of ConPTY: making sure
//! `ClosePseudoConsole` is called exactly once, never from a thread it could
//! deadlock, and never in a state where it can block forever — regardless of
//! the order in which the reader, writer, controller, and child handles are
//! dropped.
//!
//! # The two lifecycle modes
//!
//! Everything revolves around how the conout read end reaches end-of-file:
//!
//! - **Released mode** (Windows 11 24H2+, or a bundled `conpty.dll` with
//!   `ConptyReleasePseudoConsole`): right after the child is spawned,
//!   [`ConsoleShared::release_after_spawn`] calls `ReleasePseudoConsole`.
//!   From then on the console host's lifetime is tied to its clients, not to
//!   the `HPCON`: when the child exits, the host exits, conout breaks with
//!   `ERROR_BROKEN_PIPE`, and the reader sees a natural end-of-file.
//!   `ClosePseudoConsole` still has to run later — releasing does not free
//!   the `HPCON` — but on every implementation that has the release export,
//!   close returns immediately.
//!
//! - **Legacy mode** (Windows 10 1809 .. Windows 11 23H2, Server 2022): the
//!   console host outlives the child, so end-of-file *never* arrives on its
//!   own. `ClosePseudoConsole` is the only thing that can produce it, and
//!   that call can block until either the conout read end disappears or a
//!   reader drains the host's pending output. The legacy watcher (see
//!   `core::wait`) waits for the child to exit, grants a grace period for the
//!   reader to drain the tail, and then requests close from its own dedicated
//!   thread.
//!
//! # When is `ClosePseudoConsole` allowed to run?
//!
//! Close executes only at a state transition, in one of these situations,
//! each with a proof that it cannot block indefinitely:
//!
//! 1. **Reader drained** (`notify_reader_eof`): end-of-file means the console
//!    host is already gone (our own copy of the conout write end was closed
//!    at creation, so the host held the last one). Close has nothing to wait
//!    for. This is the one case where the *reader thread itself* may execute
//!    the close: it has finished reading, so the "never close from the reader
//!    thread" rule — whose entire point is that the reader must stay able to
//!    drain — is vacuously satisfied.
//! 2. **Reader closed** (`notify_reader_closed`): the conout read end is
//!    gone, so the host's writes fail with a broken pipe instead of blocking;
//!    the host exits and close returns. This is the documented "close the
//!    output pipe first" shutdown.
//! 3. **Explicit request with no live reader** (`request_close`): same proof
//!    as 1/2 depending on the reader state.
//! 4. **Explicit request with a live reader, legacy mode** (`request_close`
//!    from the legacy watcher): close runs on the *caller's* thread and may
//!    genuinely block while the reader drains — this is the documented "keep
//!    reading" shutdown, and it is the only way legacy mode can ever generate
//!    end-of-file. The caller contract is therefore: a dedicated, non-reader
//!    thread that may block (the watcher thread qualifies; `Drop` never
//!    calls this).
//! 5. **Explicit request with a live reader, released mode**: close is
//!    *deferred* (`CloseState::Requested`). Natural end-of-file is on its
//!    way, and the reader's own transition (case 1 or 2) finishes the job.
//!    Closing eagerly would also be safe on a released implementation, but
//!    deferring keeps a single rule — "close only after the reader is done,
//!    except on the blocking-capable watcher path" — instead of two.
//!
//! The alternative design — always delegating the close to a short-lived
//! dedicated thread — was considered and rejected for the normal paths: the
//! per-close thread spawn buys nothing when the call is proven prompt (cases
//! 1–3), and it turns deterministic teardown into a race against process
//! exit (a detached closer that has not run yet when `main` returns silently
//! leaks the session). The dedicated-thread trick is kept exactly where it
//! is needed: the final-defense `Drop`, which must never block and may run
//! while a leaked conout read end is still open (see below).
//!
//! # Double-close and drop-order safety
//!
//! Every path funnels through the same mutex-guarded state: whoever flips
//! `CloseState` to `Done` under the lock is the sole closer, and the FFI call
//! itself happens after the lock is dropped so no other lifecycle call can
//! be serialized behind a potentially blocking close. `resize` and
//! `release_after_spawn` check for `Done` under the same lock, so they can
//! never race a close into a use-after-free. If every handle is dropped
//! without anyone requesting a close, the [`Drop`] impl of [`ConsoleShared`]
//! is the final defense: it closes inline when that is proven prompt, and
//! otherwise hands the `HPCON` to a detached thread rather than block.

use std::io;
use std::os::windows::io::{AsHandle, OwnedHandle};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;

use crate::backend::{ConPtyBackend, HPCON, PSEUDOCONSOLE_INHERIT_CURSOR};
use crate::size::Size;

/// State of the conout read end, as reported by the reader wrapper.
///
/// The state only moves "forward": `Open` → `Drained` → (`Closed`), or
/// `Open` → `Closed`. `Closed` is terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReaderState {
    /// The conout read end may still be read. This is the initial state; it
    /// deliberately over-approximates (a reader might not exist yet), because
    /// treating a possibly-live reader as live is the safe direction.
    Open,
    /// The reader observed end-of-file. The console host is gone, so nothing
    /// will ever be written to conout again.
    Drained,
    /// The conout read end has been dropped.
    Closed,
}

/// Progress of the one-and-only `ClosePseudoConsole` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseState {
    /// Nobody has asked for the pseudoconsole to be closed yet.
    NotRequested,
    /// A close was requested while the reader was still live in released
    /// mode; it runs as soon as the reader reaches `Drained` or `Closed`.
    Requested,
    /// `ClosePseudoConsole` has been claimed (and possibly already executed);
    /// the `HPCON` must never be touched again.
    Done,
}

/// The mutable half of [`ConsoleShared`], guarded by one mutex.
#[derive(Debug)]
struct State {
    /// Whether `ReleasePseudoConsole` succeeded; decides between the natural
    /// end-of-file contract and the legacy watcher-driven one.
    released: bool,
    /// Whether a `ReleasePseudoConsole` attempt failed. Recorded so the
    /// demotion to legacy mode is observable, not silent.
    release_failed: bool,
    reader: ReaderState,
    close: CloseState,
}

impl State {
    fn initial() -> Self {
        Self {
            released: false,
            release_failed: false,
            reader: ReaderState::Open,
            close: CloseState::NotRequested,
        }
    }
}

/// The shared core of one pseudoconsole session.
///
/// Every wrapper that outlives the [`PseudoConsole`] controller — the conout
/// reader, the conin writer, the legacy watcher — holds an
/// `Arc<ConsoleShared>` (or a `Weak` one) and reports its lifecycle events
/// here. The struct owns the `HPCON` and is the only place in the crate that
/// calls `ClosePseudoConsole`.
#[derive(Debug)]
pub(crate) struct ConsoleShared {
    backend: ConPtyBackend,
    hpcon: HPCON,
    state: Mutex<State>,
}

// SAFETY: `hpcon` is an opaque, process-global handle into the pseudoconsole
// subsystem, not a thread-affine resource; Microsoft's reference
// implementations (and node-pty, wezterm, alacritty) resize and close it from
// arbitrary threads. What must not happen is two conflicting FFI calls on it,
// and every call in this module is serialized through `state`: `release` and
// `resize` run under the mutex, and `close` runs only on the thread that
// flipped `CloseState` to `Done` under that mutex. `Send`/`Sync` are not just
// sound but required — `ClosePseudoConsole` must run on a thread other than
// the conout reader, so the shared core inherently crosses threads.
unsafe impl Send for ConsoleShared {}
// SAFETY: see above; all interior mutability is behind the `state` mutex.
unsafe impl Sync for ConsoleShared {}

impl ConsoleShared {
    /// Locks the state, recovering from poisoning.
    ///
    /// No invariant-breaking panic can happen while the lock is held (the
    /// critical sections contain no user code), so a poisoned mutex still
    /// holds a consistent `State` and it is always safe to continue — which
    /// matters because the final line of defense runs in `Drop`, possibly
    /// during a panic unwind.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Calls `ReleasePseudoConsole`, switching the session to released mode.
    ///
    /// Call this once, right after the child process has been spawned.
    /// Returns `Ok(true)` if the session is now (or already was) released,
    /// and `Ok(false)` if the backend has no release export — the caller must
    /// then run a legacy watcher to guarantee end-of-file.
    ///
    /// # Errors
    ///
    /// If `ReleasePseudoConsole` itself fails, the failure is recorded (see
    /// [`Self::release_failed`]) and returned; the session is demoted to
    /// legacy mode, so the caller must treat `Err` like `Ok(false)` for
    /// shutdown purposes — spawn the watcher — and may additionally log the
    /// error.
    pub(crate) fn release_after_spawn(&self) -> io::Result<bool> {
        let mut state = self.lock();
        if state.close == CloseState::Done {
            // The HPCON is gone; there is nothing left to release. Report
            // "not released" so a (buggy) caller still runs the legacy path,
            // which is harmless after a close.
            return Ok(false);
        }
        if state.released {
            return Ok(true);
        }
        // SAFETY: `hpcon` came from `backend.create` and `close` is not
        // `Done`; holding the state lock keeps it that way for the duration
        // of the call. `ReleasePseudoConsole` only closes a reference handle,
        // so it cannot block and may run under the lock.
        match unsafe { self.backend.release(self.hpcon) } {
            None => Ok(false),
            Some(Ok(())) => {
                state.released = true;
                Ok(true)
            }
            Some(Err(err)) => {
                state.release_failed = true;
                Err(err)
            }
        }
    }

    /// Records that the conout reader observed end-of-file, and performs a
    /// pending close if one was requested.
    ///
    /// This is intended to be called from the reader thread itself, and the
    /// close it may perform is safe there: end-of-file proves the console
    /// host is gone (it held the last conout write end), so
    /// `ClosePseudoConsole` has nothing left to wait for, and the reader has
    /// nothing left to read.
    pub(crate) fn notify_reader_eof(&self) {
        let mut state = self.lock();
        if state.reader == ReaderState::Open {
            state.reader = ReaderState::Drained;
        }
        self.close_if_due(state);
    }

    /// Records that the conout read end has been dropped, and performs a
    /// pending close if one was requested.
    ///
    /// With the read end gone the console host's conout writes fail instead
    /// of blocking, so a close from here is prompt (the documented
    /// "close the output pipe first" shutdown).
    pub(crate) fn notify_reader_closed(&self) {
        let mut state = self.lock();
        state.reader = ReaderState::Closed;
        self.close_if_due(state);
    }

    /// Requests that the pseudoconsole be closed, closing it immediately when
    /// that is safe.
    ///
    /// Behaviour by state:
    ///
    /// - Reader `Drained`/`Closed`: closes now, on this thread (prompt; see
    ///   the module docs).
    /// - Reader `Open`, released mode: defers; the reader's own
    ///   end-of-file/close transition finishes the close.
    /// - Reader `Open`, legacy mode: closes now, on this thread, which may
    ///   **block** until the reader has drained the console host's remaining
    ///   output. This is the legacy watcher's path and the only way legacy
    ///   mode ever produces end-of-file.
    ///
    /// # Caller contract
    ///
    /// Never call this from the thread that reads conout — in the blocking
    /// case that thread must stay free to drain. Call it from a dedicated
    /// thread that may tolerate blocking (the legacy watcher does).
    pub(crate) fn request_close(&self) {
        let mut state = self.lock();
        if state.close == CloseState::Done {
            return;
        }
        state.close = CloseState::Requested;
        if state.reader == ReaderState::Open && state.released {
            // Deferred: natural EOF is coming, and closing is the reader
            // transition's job. See the module docs for why eager-close was
            // rejected even though it would be prompt here.
            return;
        }
        self.execute_close(state);
    }

    /// Closes now if a request is pending and the reader is out of the way.
    fn close_if_due(&self, state: MutexGuard<'_, State>) {
        if state.close == CloseState::Requested && state.reader != ReaderState::Open {
            self.execute_close(state);
        }
    }

    /// Claims the close under the lock, then performs it outside the lock.
    ///
    /// Claiming first makes this the sole closer (double-close is
    /// impossible); unlocking before the FFI call keeps a potentially slow
    /// `ClosePseudoConsole` from stalling unrelated lifecycle calls, which
    /// observe `Done` and refuse to touch the handle instead.
    fn execute_close(&self, mut state: MutexGuard<'_, State>) {
        debug_assert_ne!(state.close, CloseState::Done);
        state.close = CloseState::Done;
        drop(state);
        // SAFETY: `hpcon` came from `backend.create` on this backend and the
        // `Done` claim above guarantees no other close (or any further FFI
        // call) on it. The liveness rules are upheld by the callers, each of
        // which documents its case from the module-level list.
        unsafe { self.backend.close(self.hpcon) };
    }

    /// Calls `ResizePseudoConsole`.
    ///
    /// Resizing stays valid after a release (the signal channel to the
    /// console host is only torn down by the close).
    ///
    /// # Errors
    ///
    /// Fails with [`io::ErrorKind::NotConnected`] once the pseudoconsole has
    /// been closed, or with the backend's error.
    pub(crate) fn resize(&self, size: Size) -> io::Result<()> {
        let state = self.lock();
        if state.close == CloseState::Done {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "the pseudoconsole has been closed",
            ));
        }
        // SAFETY: `hpcon` is live — `close` is not `Done`, and holding the
        // state lock prevents any close from claiming it during the call.
        // `ResizePseudoConsole` is a quick signal write, safe under the lock.
        unsafe { self.backend.resize(self.hpcon, size) }
    }

    /// Returns whether the session is in released mode.
    pub(crate) fn is_released(&self) -> bool {
        self.lock().released
    }

    /// Returns whether a `ReleasePseudoConsole` attempt failed.
    ///
    /// The session then behaves exactly like one on a release-less backend.
    ///
    /// Test-only: the front ends learn about a failed release from
    /// [`Self::release_after_spawn`]'s `Err`, which they must already handle,
    /// so nothing in the shipped library queries this after the fact.
    #[cfg(test)]
    pub(crate) fn release_failed(&self) -> bool {
        self.lock().release_failed
    }

    /// Returns whether `ClosePseudoConsole` has run (or been claimed).
    ///
    /// Test-only: closing is driven entirely by state transitions, so no
    /// production path needs to poll for it — the observable effect is
    /// end-of-file on the output pipe.
    #[cfg(test)]
    pub(crate) fn is_closed(&self) -> bool {
        self.lock().close == CloseState::Done
    }

    /// Returns whether the conout reader is done (drained or closed).
    ///
    /// The legacy watcher uses this to skip its drain grace period when
    /// there is no reader left to drain.
    pub(crate) fn reader_finished(&self) -> bool {
        self.lock().reader != ReaderState::Open
    }
}

/// Final line of defense: nothing else closed the pseudoconsole, so do it
/// here — without ever blocking the dropping thread.
///
/// Running at all means every `Arc<ConsoleShared>` is gone, so no wrapper
/// that could still transition the state exists. Two cases:
///
/// - Reader `Drained`/`Closed`, or released mode: the close is proven prompt
///   (module docs, cases 1/2; in released mode close never blocks), so run
///   it inline.
/// - Reader still `Open` in legacy mode: state claims a conout read end may
///   exist (e.g. a leaked raw handle, or ends that were dropped without a
///   notification — with all `Arc`s gone nobody can tell us which), so
///   `ClosePseudoConsole` could block until that end disappears. Delegate to
///   a detached short-lived thread and do not join it: a `Drop` must never
///   block. If even spawning the thread fails, leak the `HPCON` — a leak is
///   reclaimed at process exit, a wedged destructor is not.
impl Drop for ConsoleShared {
    fn drop(&mut self) {
        // `get_mut`: no other reference exists, so no lock is needed; recover
        // from poisoning for the same reason `Self::lock` does.
        let state = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        if state.close == CloseState::Done {
            return;
        }
        state.close = CloseState::Done;

        if state.reader == ReaderState::Open && !state.released {
            let backend = self.backend.clone();
            // Send the pointer as an integer: `HPCON` is a raw pointer and
            // deliberately not `Send`; this one crosses threads soundly
            // because the `Done` claim above made the receiver the sole user.
            let hpcon = self.hpcon as usize;
            let spawned = thread::Builder::new()
                .name("conpty-oxide-close".into())
                .spawn(move || {
                    // SAFETY: sole closer (claimed above); the pseudoconsole
                    // object lives in the OS, not in `ConsoleShared`, so it
                    // is still valid after the `ConsoleShared` is freed.
                    unsafe { backend.close(hpcon as HPCON) };
                });
            if spawned.is_err() {
                // Deliberate leak; see the doc comment above.
            }
            return;
        }

        // SAFETY: sole closer (claimed above); prompt per the case analysis
        // in the doc comment above.
        unsafe { self.backend.close(self.hpcon) };
    }
}

/// A live pseudoconsole, created with `CreatePseudoConsole`.
///
/// This is the controller half of a session: it creates the console, exposes
/// the raw `HPCON` for `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`, resizes, and
/// releases. The teardown logic lives in the [`ConsoleShared`] it hands out
/// via [`Self::shared`], so it survives the controller: dropping a
/// `PseudoConsole` only drops one `Arc`, and the close runs at whichever
/// state transition (or final `Drop`) the module docs prescribe.
#[derive(Debug)]
pub(crate) struct PseudoConsole {
    shared: Arc<ConsoleShared>,
}

impl PseudoConsole {
    /// Creates a pseudoconsole of `size` over the client ends of the session
    /// pipes.
    ///
    /// `conin_read` and `conout_write` are consumed and closed *immediately*
    /// after `CreatePseudoConsole` returns, before any child is spawned —
    /// the same order as the official ConPTY sample. The console host holds
    /// its own duplicates; keeping ours would hold the conout pipe open
    /// forever and defeat end-of-file detection.
    ///
    /// `inherit_cursor` maps to `PSEUDOCONSOLE_INHERIT_CURSOR`. Leave it
    /// `false` unless both pipes are being actively pumped: the flag makes
    /// the console emit a cursor-position query on conout and stall all input
    /// processing until the reply arrives on conin
    /// (microsoft/terminal#17688).
    ///
    /// # Errors
    ///
    /// Returns the `CreatePseudoConsole` failure mapped to an [`io::Error`].
    pub(crate) fn new(
        backend: ConPtyBackend,
        size: Size,
        conin_read: OwnedHandle,
        conout_write: OwnedHandle,
        inherit_cursor: bool,
    ) -> io::Result<Self> {
        let flags = if inherit_cursor {
            PSEUDOCONSOLE_INHERIT_CURSOR
        } else {
            0
        };
        let hpcon = backend.create(
            size,
            conin_read.as_handle(),
            conout_write.as_handle(),
            flags,
        )?;
        // Both client ends drop here (see the doc comment).
        drop(conin_read);
        drop(conout_write);

        Ok(Self {
            shared: Arc::new(ConsoleShared {
                backend,
                hpcon,
                state: Mutex::new(State::initial()),
            }),
        })
    }

    /// Returns the raw `HPCON`, for `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`.
    ///
    /// The value stays valid for as long as this `PseudoConsole` is alive and
    /// un-closed; do not store it beyond that.
    pub(crate) fn hpcon(&self) -> HPCON {
        self.shared.hpcon
    }

    /// Returns the shared lifecycle core, for wiring up the reader wrapper
    /// and the legacy watcher.
    pub(crate) fn shared(&self) -> &Arc<ConsoleShared> {
        &self.shared
    }

    /// See [`ConsoleShared::release_after_spawn`].
    pub(crate) fn release_after_spawn(&self) -> io::Result<bool> {
        self.shared.release_after_spawn()
    }

    /// See [`ConsoleShared::resize`].
    pub(crate) fn resize(&self, size: Size) -> io::Result<()> {
        self.shared.resize(size)
    }

    /// See [`ConsoleShared::is_released`].
    ///
    /// Test-only: the spawn path learns the mode from
    /// [`Self::release_after_spawn`]'s return value, and the legacy watcher
    /// asks the shared core directly.
    #[cfg(test)]
    pub(crate) fn is_released(&self) -> bool {
        self.shared.is_released()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::mpsc;
    use std::time::Duration;

    use crate::backend::ConPtyBackend;
    use crate::core::pipes::{create_sync_pipes, SyncPipes};

    /// Runs `f` on a helper thread and fails the test if it does not finish
    /// within five seconds.
    ///
    /// A watchdog is essential here: the failure mode under test is a hang
    /// (e.g. `ClosePseudoConsole` blocking), which would otherwise stall the
    /// whole test run forever instead of failing one test. The helper thread
    /// signals completion over a channel; on timeout the *test* thread
    /// panics, and the wedged helper is abandoned (the test harness can still
    /// finish and report).
    fn complete_within_5s(name: &str, f: impl FnOnce() + Send + 'static) {
        let (done_tx, done_rx) = mpsc::channel();
        thread::Builder::new()
            .name(format!("watchdog-subject-{name}"))
            .spawn(move || {
                f();
                let _ = done_tx.send(());
            })
            .expect("spawning the test subject thread must succeed");
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("`{name}` hung for more than 5 seconds"));
    }

    fn backend() -> ConPtyBackend {
        ConPtyBackend::system().expect("ConPTY must be available on a test machine")
    }

    /// Creates a console plus the two user-side pipe ends that remain ours.
    fn console_and_user_ends(backend: ConPtyBackend) -> (PseudoConsole, OwnedHandle, OwnedHandle) {
        let SyncPipes {
            conout_read,
            conout_write,
            conin_read,
            conin_write,
        } = create_sync_pipes().expect("creating pipes must succeed");
        let console = PseudoConsole::new(backend, Size::default(), conin_read, conout_write, false)
            .expect("CreatePseudoConsole must succeed");
        (console, conout_read, conin_write)
    }

    #[test]
    fn shared_core_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ConsoleShared>();
        assert_send_sync::<PseudoConsole>();
    }

    #[test]
    fn create_yields_a_live_console() {
        let (console, _conout_read, _conin_write) = console_and_user_ends(backend());
        assert!(!console.hpcon().is_null());
        assert!(!console.is_released());
        assert!(!console.shared().is_closed());
    }

    #[test]
    fn resize_succeeds_while_open_and_fails_after_close() {
        let (console, conout_read, conin_write) = console_and_user_ends(backend());
        console
            .resize(Size::new(50, 132))
            .expect("resize on a live console must succeed");

        // Retire the reader, then close.
        drop(conout_read);
        console.shared().notify_reader_closed();
        console.shared().request_close();
        assert!(console.shared().is_closed());

        let err = console
            .resize(Size::new(30, 100))
            .expect_err("resize after close must fail");
        assert_eq!(err.kind(), io::ErrorKind::NotConnected);
        drop(conin_write);
    }

    #[test]
    fn drop_console_before_user_pipe_ends() {
        complete_within_5s("drop_console_before_user_pipe_ends", || {
            let (console, conout_read, conin_write) = console_and_user_ends(backend());
            drop(console);
            drop(conout_read);
            drop(conin_write);
        });
    }

    #[test]
    fn drop_user_pipe_ends_before_console() {
        complete_within_5s("drop_user_pipe_ends_before_console", || {
            let (console, conout_read, conin_write) = console_and_user_ends(backend());
            drop(conout_read);
            drop(conin_write);
            drop(console);
        });
    }

    #[test]
    fn drop_after_reader_closed_notification() {
        complete_within_5s("drop_after_reader_closed_notification", || {
            let (console, conout_read, conin_write) = console_and_user_ends(backend());
            drop(conout_read);
            console.shared().notify_reader_closed();
            drop(console);
            drop(conin_write);
        });
    }

    #[test]
    fn request_close_after_reader_closed_closes_inline() {
        complete_within_5s("request_close_after_reader_closed_closes_inline", || {
            let (console, conout_read, conin_write) = console_and_user_ends(backend());
            let shared = Arc::clone(console.shared());
            drop(conout_read);
            shared.notify_reader_closed();
            shared.request_close();
            assert!(shared.is_closed());
            // A second request is a no-op, not a double close.
            shared.request_close();
            drop(console);
            drop(conin_write);
        });
    }

    #[test]
    fn legacy_request_close_with_live_reader_completes() {
        complete_within_5s("legacy_request_close_with_live_reader_completes", || {
            let (console, conout_read, conin_write) = console_and_user_ends(backend());
            let shared = Arc::clone(console.shared());
            // Reader still "Open" and the session was never released: this is
            // the legacy watcher's blocking-capable path. With no client and
            // an un-drained conout it must still complete, because closing is
            // exactly what ends a clientless session.
            shared.request_close();
            assert!(shared.is_closed());
            drop(console);
            drop(conout_read);
            drop(conin_write);
        });
    }

    #[test]
    fn notify_eof_executes_a_deferred_close() {
        let backend = backend();
        if !backend.supports_release() {
            // Deferral only happens in released mode; nothing to test here.
            return;
        }
        complete_within_5s("notify_eof_executes_a_deferred_close", || {
            let (console, conout_read, conin_write) = console_and_user_ends(backend);
            let shared = Arc::clone(console.shared());
            assert!(shared
                .release_after_spawn()
                .expect("ReleasePseudoConsole must succeed"));
            assert!(console.is_released());

            // With the reader open, a close request in released mode defers.
            shared.request_close();
            assert!(!shared.is_closed());

            // The reader hitting EOF executes the pending close.
            shared.notify_reader_eof();
            assert!(shared.is_closed());
            drop(console);
            drop(conout_read);
            drop(conin_write);
        });
    }

    #[test]
    fn release_after_spawn_is_idempotent() {
        let backend = backend();
        if !backend.supports_release() {
            return;
        }
        complete_within_5s("release_after_spawn_is_idempotent", || {
            let (console, conout_read, conin_write) = console_and_user_ends(backend);
            assert!(console.release_after_spawn().expect("release must succeed"));
            assert!(console.release_after_spawn().expect("release must succeed"));
            assert!(!console.shared().release_failed());
            drop(conout_read);
            drop(conin_write);
            drop(console);
        });
    }

    #[test]
    fn released_console_drops_cleanly_in_any_order() {
        let backend = backend();
        if !backend.supports_release() {
            return;
        }
        complete_within_5s("released_console_drops_cleanly_in_any_order", || {
            let (console, conout_read, conin_write) = console_and_user_ends(backend);
            assert!(console.release_after_spawn().expect("release must succeed"));
            drop(console);
            drop(conin_write);
            drop(conout_read);
        });
    }

    #[test]
    fn reader_finished_tracks_notifications() {
        let (console, conout_read, conin_write) = console_and_user_ends(backend());
        let shared = Arc::clone(console.shared());
        assert!(!shared.reader_finished());
        shared.notify_reader_eof();
        assert!(shared.reader_finished());
        // Drained -> Closed is a valid forward transition.
        shared.notify_reader_closed();
        assert!(shared.reader_finished());
        drop(conout_read);
        drop(conin_write);
        drop(console);
    }
}
