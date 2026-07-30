// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Child-process exit detection and the legacy shutdown wait.
//!
//! Exit detection is handle-based. Blocking waits use [`ProcessWaiter`] and
//! `WaitForSingleObject`; the Tokio frontend uses `RegisteredWait` and
//! `RegisterWaitForSingleObject`. Both read the exit code only *after* the
//! process is signaled. Calling `GetExitCodeProcess` on a running process
//! "succeeds" with `STILL_ACTIVE` (259), indistinguishable from a real exit
//! code of 259, so the ordering is the protocol.
//!
//! The other half of this module is the **legacy shutdown registration**
//! ([`spawn_legacy_watcher`]): on Windows versions without
//! `ReleasePseudoConsole`, the console host outlives the child, so the conout
//! pipe never reaches end-of-file on its own. Windows waits without parking a
//! crate thread; after exit, a short-lived worker grants the reader a drain
//! grace and asks the lifecycle state machine to close the pseudoconsole from
//! a non-reader thread. That breaks conout and surfaces as EOF.

use std::ffi::c_void;
#[cfg(feature = "tokio")]
use std::future::Future;
use std::io;
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, OwnedHandle};
use std::panic::{catch_unwind, AssertUnwindSafe};
#[cfg(feature = "tokio")]
use std::pin::Pin;
use std::ptr;
#[cfg(all(feature = "tokio", test))]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, Weak};
#[cfg(feature = "tokio")]
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::Duration;

#[cfg(feature = "tokio")]
use windows_sys::Win32::Foundation::ERROR_IO_PENDING;
use windows_sys::Win32::Foundation::{
    HANDLE, INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, RegisterWaitForSingleObject, UnregisterWaitEx, WaitForSingleObject,
    INFINITE, WT_EXECUTELONGFUNCTION, WT_EXECUTEONLYONCE,
};

use crate::core::pseudocon::ConsoleShared;

/// Default drain grace between child exit and the legacy close.
///
/// After the child exits, the console host may still hold rendered output
/// that the reader has not consumed yet. Closing immediately would still be
/// correct (the reader keeps draining while the close blocks), but the grace
/// period lets the common case — reader catches up quickly — finish the tail
/// before teardown begins.
#[cfg(any(feature = "blocking", feature = "tokio"))]
pub(super) const LEGACY_CLOSE_GRACE: Duration = Duration::from_secs(1);

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
    pub(crate) const fn new(process: OwnedHandle) -> Self {
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
    #[cfg(any(feature = "blocking", test))]
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

/// Borrows the process handle, e.g. to duplicate it for a watcher or to expose
/// it through the public `Child` API.
impl AsHandle for ProcessWaiter {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.process.as_handle()
    }
}

/// A thread-pool registered wait used by the Tokio child front end.
///
/// Windows owns the callback scheduling. The callback only reads the exit
/// code, stores it under the context mutex, and wakes the current task; it
/// never unregisters itself. Waking may synchronously re-enter an executor, so
/// callback activity and Drop use the state mutex as a cleanup handshake.
#[cfg(feature = "tokio")]
pub(crate) struct RegisteredWait {
    wait_object: HANDLE,
    context: *mut RegisteredWaitContext,
}

#[cfg(feature = "tokio")]
struct RegisteredWaitContext {
    process: ProcessWaiter,
    state: Mutex<RegisteredWaitState>,
}

#[cfg(feature = "tokio")]
#[derive(Default)]
struct RegisteredWaitState {
    result: Option<io::Result<u32>>,
    waker: Option<Waker>,
    /// The callback has crossed its entry handshake and may be invoking a
    /// user-supplied `Waker`.
    callback_active: bool,
    /// The owning `RegisteredWait` is being dropped. A callback that has not
    /// crossed its entry handshake must return without waking user code.
    owner_dropping: bool,
    /// The callback owns reclamation of its context after it returns from the
    /// user-supplied `Waker`.
    cleanup_after_callback: bool,
    /// Lets crate-local tests observe callback-tail reclamation directly.
    #[cfg(test)]
    cleanup_observer: Option<Arc<AtomicBool>>,
}

#[cfg(all(feature = "tokio", test))]
impl Drop for RegisteredWaitContext {
    fn drop(&mut self) {
        let state = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        if let Some(observer) = &state.cleanup_observer {
            observer.store(true, Ordering::Release);
        }
    }
}

// SAFETY: `wait_object` is an opaque kernel handle and `context` points to a
// heap allocation whose mutable state is synchronized by `Mutex`. Drop either
// performs a blocking unregister before freeing it or, when a callback is
// active, successfully requests nonblocking deletion and transfers reclamation
// to that callback's tail.
#[cfg(feature = "tokio")]
unsafe impl Send for RegisteredWait {}
// SAFETY: polling still requires `Pin<&mut Self>`; shared access only observes
// the opaque fields. Callback-visible mutation is protected by the mutex.
#[cfg(feature = "tokio")]
unsafe impl Sync for RegisteredWait {}

#[cfg(feature = "tokio")]
impl RegisteredWait {
    /// Registers an infinite, one-shot wait on a duplicate process handle.
    pub(crate) fn new(process: BorrowedHandle<'_>) -> io::Result<Self> {
        let process = ProcessWaiter::new(process.try_clone_to_owned()?);
        let context = Box::into_raw(Box::new(RegisteredWaitContext {
            process,
            state: Mutex::new(RegisteredWaitState::default()),
        }));
        let mut wait_object: HANDLE = ptr::null_mut();

        // SAFETY: `context` points to the stable allocation created above.
        let process_handle = unsafe { (*context).process.as_handle().as_raw_handle() };
        // SAFETY: `context` is a stable heap allocation and remains live until
        // either Drop has synchronously unregistered the wait or an active
        // callback has accepted cleanup ownership. The duplicated process
        // handle is live inside that allocation. The callback signature and
        // flags match RegisterWaitForSingleObject's contract.
        let registered = unsafe {
            RegisterWaitForSingleObject(
                &mut wait_object,
                process_handle,
                Some(registered_wait_callback),
                context.cast(),
                INFINITE,
                WT_EXECUTEONLYONCE,
            )
        };
        if registered == 0 {
            let err = io::Error::last_os_error();
            // SAFETY: registration failed, so Windows retained neither the
            // pointer nor a callback reference to it.
            drop(unsafe { Box::from_raw(context) });
            return Err(err);
        }

        Ok(Self {
            wait_object,
            context,
        })
    }

    /// Installs a crate-local observer for context reclamation.
    #[cfg(test)]
    fn observe_cleanup(&self, observer: Arc<AtomicBool>) {
        // SAFETY: successful construction keeps the allocation live until
        // Drop or the callback tail reclaims it. The state mutex synchronizes
        // a callback that may already have started.
        let context = unsafe { &*self.context };
        context
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .cleanup_observer = Some(observer);
    }
}

#[cfg(feature = "tokio")]
impl Future for RegisteredWait {
    type Output = io::Result<u32>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: successful construction keeps the allocation live under the
        // Drop/callback cleanup handshake.
        let context = unsafe { &*self.context };
        let mut state = context.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(result) = state.result.take() {
            return Poll::Ready(result);
        }
        if should_replace_waker(state.waker.as_ref(), cx.waker()) {
            state.waker = Some(cx.waker().clone());
        }
        Poll::Pending
    }
}

#[cfg(feature = "tokio")]
fn should_replace_waker(registered: Option<&Waker>, candidate: &Waker) -> bool {
    registered.map_or(true, |registered| !registered.will_wake(candidate))
}

#[cfg(feature = "tokio")]
fn callback_unregister_transfers_cleanup(result: i32, error_code: Option<i32>) -> bool {
    result != 0 || error_code == i32::try_from(ERROR_IO_PENDING).ok()
}

#[cfg(feature = "tokio")]
impl Drop for RegisteredWait {
    fn drop(&mut self) {
        // A Waker is allowed to poll its task inline. Consequently the callback
        // can re-enter `Child::wait`, observe Ready, and drop this registration
        // before `Waker::wake` returns. A blocking unregister in that situation
        // would wait for the current callback to finish while the callback was
        // waiting for this Drop: the exact self-deadlock Microsoft forbids.
        //
        // The state mutex is an entry handshake. If the callback is active,
        // request nonblocking deletion and transfer context reclamation to the
        // callback. If it is not active, mark the owner as dropping before
        // releasing the mutex; a concurrently queued callback then skips the
        // Waker and returns promptly, making the blocking unregister safe.
        //
        // SAFETY: successful construction keeps the allocation live until one
        // of the two reclamation paths below has completed.
        let context = unsafe { &*self.context };
        let mut state = context.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.owner_dropping = true;
        if state.callback_active {
            // A null completion event is the callback-safe, nonblocking form.
            // Keep the state mutex held so the callback cannot finish and
            // inspect `cleanup_after_callback` until the unregister result has
            // selected its cleanup owner.
            //
            // SAFETY: `wait_object` came from successful registration.
            let unregistered = unsafe { UnregisterWaitEx(self.wait_object, ptr::null_mut()) };
            let err = io::Error::last_os_error();
            if callback_unregister_transfers_cleanup(unregistered, err.raw_os_error()) {
                state.cleanup_after_callback = true;
            } else {
                // An unexpected failure leaves the registration's
                // relationship with its context uncertain. Retain the
                // allocation rather than risk a use-after-free.
                log_unregister_failure(&err);
            }
            return;
        }
        drop(state);

        // WT_EXECUTEONLYONCE stops future callbacks but does not release the
        // wait object. INVALID_HANDLE_VALUE waits for a callback that raced the
        // handshake above; that callback sees `owner_dropping`, invokes no
        // Waker, and returns without depending on this thread.
        //
        // SAFETY: `wait_object` came from successful registration.
        let unregistered = unsafe { UnregisterWaitEx(self.wait_object, INVALID_HANDLE_VALUE) };
        if unregistered == 0 {
            // Freeing after a failed unregister could race a callback and
            // become a use-after-free. Leak the small context and duplicated
            // handle instead; this is the only memory-safe recovery.
            log_unregister_failure(&io::Error::last_os_error());
            return;
        }

        // SAFETY: the blocking unregister proves Windows can no longer touch
        // the callback context.
        drop(unsafe { Box::from_raw(self.context) });
    }
}

/// Completes one registered wait.
///
/// # Safety
///
/// `raw` must be the live `RegisteredWaitContext` pointer passed at
/// registration. Windows may call this at most once due to
/// `WT_EXECUTEONLYONCE`.
#[cfg(feature = "tokio")]
unsafe extern "system" fn registered_wait_callback(raw: *mut c_void, _timed_out: bool) {
    // RawWaker vtables and tracing subscribers are arbitrary user code. No
    // unwind may cross this Windows callback ABI: Rust aborts the process if
    // it does. The inner callback catches a Waker panic before its cleanup
    // tail; this outer boundary contains every other unexpected panic.
    match catch_callback_unwind(|| {
        // SAFETY: guaranteed by the registration and callback contract above.
        unsafe { registered_wait_callback_inner(raw) }
    }) {
        Ok(true) => log_callback_panic_safely("registered-wait Waker"),
        Ok(false) => {},
        Err(()) => log_callback_panic_safely("registered-wait callback"),
    }
}

/// Completes one registered wait while preserving callback-tail cleanup.
///
/// Returns whether the installed `Waker` panicked.
///
/// # Safety
///
/// `raw` must be the live `RegisteredWaitContext` pointer passed at
/// registration. Windows may call this at most once due to
/// `WT_EXECUTEONLYONCE`.
#[cfg(feature = "tokio")]
unsafe fn registered_wait_callback_inner(raw: *mut c_void) -> bool {
    // SAFETY: guaranteed by the registration and callback contract above.
    let context = unsafe { &*raw.cast::<RegisteredWaitContext>() };
    {
        let mut state = context.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.owner_dropping {
            // Drop won the entry handshake. It is already performing a
            // blocking unregister, so do not invoke a Waker that could depend
            // on that thread; returning lets the unregister finish.
            return false;
        }
        state.callback_active = true;
    }

    let result = context.process.exit_code();
    let waker = {
        let mut state = context.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.owner_dropping {
            None
        } else {
            state.result = Some(result);
            state.waker.take()
        }
    };
    // `Waker` delegates to a user-supplied RawWaker vtable. Catch its panic
    // before the callback tail so reentrant Drop still transfers and completes
    // context reclamation.
    let wake_panicked = waker.is_some_and(|waker| catch_callback_unwind(|| waker.wake()).is_err());

    let cleanup = {
        let mut state = context.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.callback_active = false;
        state.cleanup_after_callback
    };
    if cleanup {
        // SAFETY: Drop requested nonblocking deletion while this callback was
        // active and transferred sole reclamation ownership here. This is the
        // callback tail, so the allocation is not accessed again.
        drop(unsafe { Box::from_raw(raw.cast::<RegisteredWaitContext>()) });
    }
    wake_panicked
}

/// Runs callback code without permitting an unwind to cross FFI.
///
/// A panic payload can contain a user-defined destructor that panics again.
/// Retaining that payload on the already-exceptional path is the only way to
/// make this boundary independent of such a destructor.
fn catch_callback_unwind<T>(operation: impl FnOnce() -> T) -> Result<T, ()> {
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(value) => Ok(value),
        Err(payload) => {
            std::mem::forget(payload);
            Err(())
        },
    }
}

/// Logs a contained callback panic without trusting the tracing subscriber.
fn log_callback_panic_safely(callback: &'static str) {
    let _contained = catch_callback_unwind(|| log_callback_panic(callback));
}

#[cfg(feature = "tracing")]
fn log_callback_panic(callback: &'static str) {
    tracing::error!(
        callback,
        "contained a panic at a Windows thread-pool callback boundary"
    );
}

#[cfg(not(feature = "tracing"))]
const fn log_callback_panic(_callback: &'static str) {}

fn log_unregister_failure(err: &io::Error) {
    let _contained = catch_callback_unwind(|| log_unregister_failure_inner(err));
}

#[cfg(feature = "tracing")]
fn log_unregister_failure_inner(err: &io::Error) {
    tracing::error!(
        error = %err,
        "failed to unregister process wait; retaining its callback context for safety"
    );
}

#[cfg(not(feature = "tracing"))]
const fn log_unregister_failure_inner(_err: &io::Error) {}

/// Spawns the legacy shutdown watcher for one session.
///
/// `process` must be a *duplicate* of the child's process handle (e.g. from
/// `BorrowedHandle::try_clone_to_owned`, which wraps `DuplicateHandle` with
/// `DUPLICATE_SAME_ACCESS`), so the watcher's wait is independent of the
/// lifetime of the handle the caller keeps for its own `wait`/`kill` API.
///
/// The registered wait and its post-exit worker:
///
/// 1. Windows waits for the child in its wait pool. No crate thread is parked
///    for the lifetime of the process. Only a `Weak` reference to the
///    lifecycle core is retained, so an abandoned session can still reach
///    [`ConsoleShared`]'s final-defense drop.
/// 2. After process exit, a short-lived worker sleeps for `grace` (skipped
///    when the reader is already done) to let
///    the reader drain output the console host produced before the exit.
/// 3. Calls [`ConsoleShared::request_close`]. The post-exit worker satisfies
///    that method's contract — a dedicated, non-reader thread that may
///    block — and the close breaks the conout pipe, which the reader
///    observes as end-of-file.
///
/// If the session is already released, no wait is registered: end-of-file
/// arrives naturally and closing is handled by the reader-side transitions.
///
/// # Errors
///
/// Returns the OS error if `RegisterWaitForSingleObject` fails. Failure to
/// create the post-exit worker is handled inside the registered callback,
/// which performs the grace and close work itself.
pub(super) fn spawn_legacy_watcher(
    process: OwnedHandle,
    shared: Arc<ConsoleShared>,
    grace: Duration,
) -> io::Result<()> {
    spawn_legacy_watcher_inner(process, shared, grace, true)
}

#[cfg(test)]
fn spawn_legacy_watcher_with_worker_spawn_failure(
    process: OwnedHandle,
    shared: Arc<ConsoleShared>,
    grace: Duration,
) -> io::Result<()> {
    spawn_legacy_watcher_inner(process, shared, grace, false)
}

fn spawn_legacy_watcher_inner(
    process: OwnedHandle,
    shared: Arc<ConsoleShared>,
    grace: Duration,
    spawn_close_worker: bool,
) -> io::Result<()> {
    if shared.is_released() {
        return Ok(());
    }

    let weak = Arc::downgrade(&shared);
    drop(shared);

    let context = Box::into_raw(Box::new(LegacyWaitContext {
        process: ProcessWaiter::new(process),
        shared: weak,
        grace,
        spawn_close_worker,
        wait_object: Mutex::new(None),
        wait_object_ready: Condvar::new(),
    }));
    let mut wait_object: HANDLE = ptr::null_mut();

    // SAFETY: `context` points to the stable allocation created above.
    let process_handle = unsafe { (*context).process.as_handle().as_raw_handle() };
    // SAFETY: the callback context is a stable allocation. A successful
    // registration transfers its cleanup to the callback's worker; failure
    // below reclaims it immediately.
    let registered = unsafe {
        RegisterWaitForSingleObject(
            &mut wait_object,
            process_handle,
            Some(legacy_wait_callback),
            context.cast(),
            INFINITE,
            WT_EXECUTEONLYONCE | WT_EXECUTELONGFUNCTION,
        )
    };
    if registered == 0 {
        let err = io::Error::last_os_error();
        // SAFETY: registration failed and Windows retained no pointer.
        drop(unsafe { Box::from_raw(context) });
        return Err(err);
    }

    // Publish the wait handle after registration. A process that was already
    // signaled may invoke the callback concurrently; the Condvar closes that
    // narrow race without spinning or reading a partially initialized handle.
    // SAFETY: the context remains owned by the registered callback.
    let context_ref = unsafe { &*context };
    let mut slot = context_ref
        .wait_object
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    *slot = Some(wait_object);
    context_ref.wait_object_ready.notify_all();

    Ok(())
}

struct LegacyWaitContext {
    /// Keeps the duplicated process handle live for the registered wait.
    process: ProcessWaiter,
    shared: Weak<ConsoleShared>,
    grace: Duration,
    /// `false` only in the crate-local failure-injection test.
    spawn_close_worker: bool,
    wait_object: Mutex<Option<HANDLE>>,
    wait_object_ready: Condvar,
}

/// A raw callback context transferred to the post-exit worker.
struct LegacyContextPtr(*mut LegacyWaitContext);

// SAFETY: the allocation is synchronized internally and is freed only after a
// blocking unregister proves the Windows callback has returned.
unsafe impl Send for LegacyContextPtr {}

/// Runs after the root process becomes signaled.
///
/// # Safety
///
/// `raw` is the live `LegacyWaitContext` supplied during registration.
unsafe extern "system" fn legacy_wait_callback(raw: *mut c_void, _timed_out: bool) {
    // Thread creation, teardown diagnostics, and tracing can all reach
    // external code. Keep every unwind on the Rust side of the Windows ABI.
    if catch_callback_unwind(|| {
        // SAFETY: guaranteed by the registration and callback contract above.
        unsafe { legacy_wait_callback_inner(raw) }
    })
    .is_err()
    {
        log_callback_panic_safely("legacy-wait callback");
    }
}

/// Runs the legacy post-exit transition from its registered callback.
///
/// # Safety
///
/// `raw` is the live `LegacyWaitContext` supplied during registration.
unsafe fn legacy_wait_callback_inner(raw: *mut c_void) {
    let pointer = LegacyContextPtr(raw.cast());
    let worker_pointer = LegacyContextPtr(pointer.0);
    // SAFETY: the callback contract keeps the context live.
    let context = unsafe { &*pointer.0 };
    let spawned = if context.spawn_close_worker {
        thread::Builder::new()
            .name("conpty-oxide-legacy-close".into())
            .spawn(move || {
                // SAFETY: ownership of cleanup was transferred to this worker.
                unsafe { finish_legacy_wait(&worker_pointer) };
            })
    } else {
        Err(io::Error::other(
            "legacy close worker spawn failure injected by a crate-local test",
        ))
    };

    if let Err(err) = spawned {
        // Thread creation failure must not leave legacy conout waiting forever.
        // The LONGFUNCTION callback is permitted to perform the grace and close
        // work itself. It cannot use blocking UnregisterWaitEx from inside its
        // own callback, so request nonblocking unregistration and retain the
        // context: freeing it before this callback returns would be unsound.
        finish_legacy_close(context);
        let wait_object = legacy_wait_object(context);
        // SAFETY: the wait object belongs to this registration. A null
        // completion event is the callback-safe, nonblocking form.
        let _ = unsafe { UnregisterWaitEx(wait_object, ptr::null_mut()) };
        log_legacy_worker_failure(&err);
    }
}

/// Performs post-exit close and reclaims the registered wait.
///
/// # Safety
///
/// `pointer` must name the live context for the current registration, and the
/// calling worker
/// function must be its sole cleanup owner.
unsafe fn finish_legacy_wait(pointer: &LegacyContextPtr) {
    // SAFETY: guaranteed by this function's contract.
    let context = unsafe { &*pointer.0 };
    finish_legacy_close(context);
    let wait_object = legacy_wait_object(context);
    // SAFETY: this worker is not the registered callback. The blocking form
    // waits until that callback returns before permitting the context to free.
    let unregistered = unsafe { UnregisterWaitEx(wait_object, INVALID_HANDLE_VALUE) };
    if unregistered != 0 {
        // SAFETY: no callback can access the allocation after successful
        // blocking unregistration.
        drop(unsafe { Box::from_raw(pointer.0) });
    } else {
        log_unregister_failure(&io::Error::last_os_error());
    }
}

fn finish_legacy_close(context: &LegacyWaitContext) {
    let Some(shared) = context.shared.upgrade() else {
        return;
    };
    if should_wait_for_legacy_reader(shared.reader_finished()) {
        thread::sleep(context.grace);
    }
    shared.request_close();
}

const fn should_wait_for_legacy_reader(reader_finished: bool) -> bool {
    !reader_finished
}

fn legacy_wait_object(context: &LegacyWaitContext) -> HANDLE {
    let mut slot = context
        .wait_object
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    loop {
        if let Some(wait_object) = *slot {
            return wait_object;
        }
        slot = context
            .wait_object_ready
            .wait(slot)
            .unwrap_or_else(PoisonError::into_inner);
    }
}

fn log_legacy_worker_failure(err: &io::Error) {
    let _contained = catch_callback_unwind(|| {
        log_legacy_worker_failure_inner(err);
    });
}

#[cfg(feature = "tracing")]
fn log_legacy_worker_failure_inner(err: &io::Error) {
    tracing::error!(
        error = %err,
        "failed to spawn legacy close worker; completed close in callback and retained context"
    );
}

#[cfg(not(feature = "tracing"))]
const fn log_legacy_worker_failure_inner(_err: &io::Error) {}

#[cfg(test)]
#[path = "wait_tests.rs"]
mod tests;
