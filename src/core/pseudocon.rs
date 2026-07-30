// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pseudoconsole lifecycle: `HPCON` ownership and the release/close state
//! machine.
//!
//! This module owns the hardest part of `ConPTY`: making sure
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
//!   `core::wait`) uses a Windows registered wait while the child lives. Only
//!   after exit does a short-lived worker grant the reader a grace period and
//!   request close; if that worker cannot be created, the registered
//!   long-function callback completes the post-exit work itself.
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
//!    output pipe first" shutdown. An async front end cannot always deliver
//!    this proof — dropping a Tokio pipe closes the OS handle only once the
//!    I/O driver retires its in-flight read — so such sessions are marked
//!    with [`ConsoleShared::set_reader_close_deferred`] and the final-defense
//!    [`Drop`] refuses to lean on case 2 for them. The notification-driven
//!    closes reached from `notify_reader_closed` itself only ever run in
//!    released mode (a deferred `request_close`), where close is prompt
//!    regardless of the reader.
//! 3. **Explicit request with no live reader** (`request_close`): same proof
//!    as 1/2 depending on the reader state.
//! 4. **Explicit request with a live reader, legacy mode** (`request_close`
//!    from the legacy watcher): close runs on the *caller's* thread and may
//!    genuinely block while the reader drains — this is the documented "keep
//!    reading" shutdown, and it is the only way legacy mode can ever generate
//!    end-of-file. The caller contract is therefore: a dedicated, non-reader
//!    thread that may block (the post-exit close worker qualifies; `Drop` never
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
#[cfg(test)]
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;

#[cfg(any(feature = "blocking", feature = "tokio"))]
use crate::backend::BackendKind;
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
    /// Last size successfully applied by `CreatePseudoConsole` or
    /// `ResizePseudoConsole`.
    size: Size,
    /// Whether `ReleasePseudoConsole` succeeded; decides between the natural
    /// end-of-file contract and the legacy watcher-driven one.
    released: bool,
    /// Whether a `ReleasePseudoConsole` attempt failed. Recorded so the
    /// demotion to legacy mode is observable, not silent.
    release_failed: bool,
    reader: ReaderState,
    close: CloseState,
    /// Whether a `Closed` reader may lag the OS-level close of the conout
    /// read end; see [`ConsoleShared::set_reader_close_deferred`].
    reader_close_deferred: bool,
    /// Records whether final-defense Drop chose detached (1) or inline (2).
    #[cfg(test)]
    drop_observer: Option<Arc<AtomicU8>>,
}

impl State {
    const fn initial(size: Size) -> Self {
        Self {
            size,
            released: false,
            release_failed: false,
            reader: ReaderState::Open,
            close: CloseState::NotRequested,
            reader_close_deferred: false,
            #[cfg(test)]
            drop_observer: None,
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

    #[cfg(test)]
    fn observe_drop_mode(&self, observer: Arc<AtomicU8>) {
        self.lock().drop_observer = Some(observer);
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
            },
            Some(Err(err)) => {
                state.release_failed = true;
                Err(err)
            },
        }
    }

    /// Records that this session's conout read end may still exist at the OS
    /// level after [`Self::notify_reader_closed`] has been called.
    ///
    /// The async front end calls this once per session, right after creation.
    /// Dropping a Tokio named pipe does not synchronously close the handle
    /// while an overlapped operation is in flight: the drop only cancels the
    /// pending read, and the `CloseHandle` runs when the runtime's I/O driver
    /// retires the cancelled operation. A `Closed` reader therefore proves
    /// the reader will never read again, but *not* that the conout read end
    /// is gone at the OS level — so the final-defense [`Drop`] must not use
    /// "reader closed" as a promptness proof for `ClosePseudoConsole` (see
    /// there). The blocking front end never calls this: its read end is a
    /// plain `OwnedHandle` whose drop closes synchronously.
    #[cfg(feature = "tokio")]
    pub(crate) fn set_reader_close_deferred(&self) {
        self.lock().reader_close_deferred = true;
    }

    /// Records that the conout reader observed end-of-file and closes the
    /// remaining `HPCON` reference.
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
        if state.close != CloseState::Done {
            state.close = CloseState::Requested;
            self.execute_close(state);
        }
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
    ///   the module docs). On a session with a deferred reader close (the
    ///   async front end), a `Closed` reader's OS handle may briefly outlive
    ///   the notification, so this can block until the runtime's I/O driver
    ///   retires the reader's last operation — which the caller contract
    ///   below already tolerates.
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
    /// Fails with [`io::ErrorKind::NotConnected`] once the session is over —
    /// whether the pseudoconsole has been closed or the console host of a
    /// released session has already exited on its own — or with the backend's
    /// error.
    pub(crate) fn resize(&self, size: Size) -> io::Result<()> {
        let mut state = self.lock();
        if state.close == CloseState::Done {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "the pseudoconsole has been closed",
            ));
        }
        // SAFETY: `hpcon` is live — `close` is not `Done`, and holding the
        // state lock prevents any close from claiming it during the call.
        // `ResizePseudoConsole` is a quick signal write, safe under the lock.
        match unsafe { self.backend.resize(self.hpcon, size) } {
            Ok(()) => {
                state.size = size;
                Ok(())
            },
            Err(err) => Err(normalize_session_end(err)),
        }
    }

    /// Returns the initial size or the last size accepted by
    /// [`Self::resize`].
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    pub(crate) fn size(&self) -> Size {
        self.lock().size
    }

    /// Returns whether the backend provides `ClearPseudoConsole`.
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    pub(crate) fn supports_clear(&self) -> bool {
        self.backend.supports_clear()
    }

    /// Returns whether the backend provides `ReleasePseudoConsole`.
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    #[cfg(test)]
    pub(crate) fn supports_release(&self) -> bool {
        self.backend.supports_release()
    }

    /// Returns which `ConPTY` implementation backs the session.
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    pub(crate) fn backend_kind(&self) -> &BackendKind {
        self.backend.kind()
    }

    /// Calls `ClearPseudoConsole`, discarding the console host's scrollback
    /// and screen.
    ///
    /// Follows exactly the same discipline as [`Self::resize`]: the FFI call
    /// runs under the state lock so no close can claim the `HPCON` underneath
    /// it, and it is a quick write to the signal pipe rather than anything
    /// that can block.
    ///
    /// # Errors
    ///
    /// Fails with [`io::ErrorKind::Unsupported`] when the backend has no
    /// `ClearPseudoConsole` export, with [`io::ErrorKind::NotConnected`] once
    /// the session is over — whether the pseudoconsole has been closed or the
    /// console host of a released session has already exited on its own — or
    /// with the backend's error.
    pub(crate) fn clear(&self) -> io::Result<()> {
        let state = self.lock();
        if state.close == CloseState::Done {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "the pseudoconsole has been closed",
            ));
        }
        // SAFETY: `hpcon` is live — `close` is not `Done`, and holding the
        // state lock prevents any close from claiming it during the call.
        unsafe { self.backend.clear(self.hpcon) }.map_or_else(
            // The front ends check `supports_clear` first and report a typed
            // error, so this is the defensive answer for a direct caller.
            || {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "this ConPTY backend does not export ClearPseudoConsole",
                ))
            },
            |result| result.map_err(normalize_session_end),
        )
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
/// - Released mode, reader `Drained`, or reader `Closed` by a synchronous
///   drop: the close is proven prompt (module docs, cases 1/2; in released
///   mode close never blocks), so run it inline.
/// - Reader still `Open` in legacy mode — or `Closed` in legacy mode on a
///   session whose reader close is *deferred* (the async front end, see
///   [`ConsoleShared::set_reader_close_deferred`]): state claims a conout
///   read end may still exist at the OS level (a leaked raw handle, ends
///   dropped without a notification, or an overlapped read the runtime's I/O
///   driver has not retired yet), so `ClosePseudoConsole` could block until
///   that end disappears — and on a current-thread runtime the dropping
///   thread may be the only one that can make it disappear. Delegate to a
///   detached short-lived thread and do not join it: a `Drop` must never
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

        if should_close_on_detached_thread(
            state.reader,
            state.reader_close_deferred,
            state.released,
        ) {
            #[cfg(test)]
            if let Some(observer) = &state.drop_observer {
                observer.store(1, Ordering::SeqCst);
            }
            let backend = self.backend.clone();
            // `windows-sys` represents `HPCON` as its pointer-sized integer
            // handle type. The `Done` claim above makes the receiver the sole
            // remaining user.
            let hpcon = self.hpcon;
            let spawned = thread::Builder::new()
                .name("conpty-oxide-close".into())
                .spawn(move || {
                    // SAFETY: sole closer (claimed above); the pseudoconsole
                    // object lives in the OS, not in `ConsoleShared`, so it
                    // is still valid after the `ConsoleShared` is freed.
                    unsafe { backend.close(hpcon) };
                });
            if spawned.is_err() {
                // Deliberate leak; see the doc comment above.
            }
            return;
        }

        #[cfg(test)]
        if let Some(observer) = &state.drop_observer {
            observer.store(2, Ordering::SeqCst);
        }
        // SAFETY: sole closer (claimed above); prompt per the case analysis
        // in the doc comment above.
        unsafe { self.backend.close(self.hpcon) };
    }
}

/// Decides whether final-defense close may block the dropping thread.
fn should_close_on_detached_thread(
    reader: ReaderState,
    reader_close_deferred: bool,
    released: bool,
) -> bool {
    !released
        && (reader == ReaderState::Open || (reader == ReaderState::Closed && reader_close_deferred))
}

/// Normalizes "the console host is already gone" into
/// [`io::ErrorKind::NotConnected`].
///
/// In released mode nothing closes the `HPCON` when the session ends by natural
/// end-of-file, so a signal sent after that point reaches a console host that
/// is already gone and fails with a disconnect-class code (observed:
/// `ERROR_NO_DATA`). Legacy mode reports the same situation through the
/// `CloseState::Done` check the callers make first; normalizing here keeps the
/// `NotConnected` contract identical across Windows versions. The original
/// error stays available as the source.
fn normalize_session_end(err: io::Error) -> io::Error {
    if crate::core::is_disconnect_error(&err) {
        io::Error::new(io::ErrorKind::NotConnected, err)
    } else {
        err
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
    /// the same order as the official `ConPTY` sample. The console host holds
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
                state: Mutex::new(State::initial(size)),
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
    pub(crate) const fn shared(&self) -> &Arc<ConsoleShared> {
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

    /// Returns the initial size or the last size accepted by
    /// [`Self::resize`].
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    pub(crate) fn size(&self) -> Size {
        self.shared.size()
    }

    /// See [`ConsoleShared::clear`].
    pub(crate) fn clear(&self) -> io::Result<()> {
        self.shared.clear()
    }

    /// Returns whether the backend provides `ClearPseudoConsole`.
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    pub(crate) fn supports_clear(&self) -> bool {
        self.shared.supports_clear()
    }

    /// Returns whether the backend provides `ReleasePseudoConsole`.
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    #[cfg(test)]
    pub(crate) fn supports_release(&self) -> bool {
        self.shared.supports_release()
    }

    /// Returns which `ConPTY` implementation backs the session.
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    pub(crate) fn backend_kind(&self) -> &BackendKind {
        self.shared.backend_kind()
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
#[path = "pseudocon_tests.rs"]
mod tests;
