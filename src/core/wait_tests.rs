// SPDX-FileCopyrightText: 2025 conpty-oxide contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;

use std::fs::File;
use std::io::Read;
#[cfg(feature = "tokio")]
use std::mem::ManuallyDrop;
use std::os::windows::io::AsHandle;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
#[cfg(feature = "tokio")]
use std::task::{RawWaker, RawWakerVTable};

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

fn watcher_job() -> Arc<Job> {
    Arc::new(Job::create(true).expect("creating an empty watcher test Job must succeed"))
}

/// A minimal executor whose Waker polls the registered wait immediately on
/// the Windows callback thread.
///
/// This is deliberately more reentrant than Tokio's scheduler. The Future
/// contract permits it, and it exercises the lifetime edge where observing
/// `Ready` drops `RegisteredWait` before `Waker::wake` returns.
#[cfg(feature = "tokio")]
struct InlineWaitTask {
    wait: Mutex<Option<Pin<Box<RegisteredWait>>>>,
    completed: mpsc::Sender<io::Result<u32>>,
}

#[cfg(feature = "tokio")]
impl InlineWaitTask {
    fn poll_once(self: &Arc<Self>) {
        let waker = Waker::from(Arc::clone(self));
        let mut cx = Context::from_waker(&waker);
        let mut slot = self.wait.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(wait) = slot.as_mut() else {
            return;
        };

        if let Poll::Ready(result) = wait.as_mut().poll(&mut cx) {
            // This runs inside `registered_wait_callback` when invoked by
            // `Wake::wake`. Dropping here must select nonblocking
            // unregistration; the old unconditional blocking form waited
            // for the callback that was executing this very line.
            drop(slot.take());
            drop(slot);
            let _ = self.completed.send(result);
        }
    }
}

#[cfg(feature = "tokio")]
impl std::task::Wake for InlineWaitTask {
    fn wake(self: Arc<Self>) {
        self.poll_once();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.poll_once();
    }
}

/// Clones the strong task reference stored in a test `RawWaker`.
///
/// # Safety
///
/// `data` must come from `Arc::into_raw` for an [`InlineWaitTask`].
#[cfg(feature = "tokio")]
unsafe fn panicking_waker_clone(data: *const ()) -> RawWaker {
    // SAFETY: guaranteed by this vtable function's contract.
    let task = ManuallyDrop::new(unsafe { Arc::<InlineWaitTask>::from_raw(data.cast()) });
    let cloned = Arc::clone(&task);
    RawWaker::new(Arc::into_raw(cloned).cast(), &PANICKING_INLINE_WAKER_VTABLE)
}

/// Polls and drops the ready wait reentrantly, then deliberately panics.
///
/// # Safety
///
/// `data` must come from `Arc::into_raw` for an [`InlineWaitTask`], and this
/// call consumes that strong reference.
#[cfg(feature = "tokio")]
unsafe fn panicking_waker_wake(data: *const ()) {
    // SAFETY: guaranteed by this vtable function's contract.
    let task = unsafe { Arc::<InlineWaitTask>::from_raw(data.cast()) };
    task.poll_once();
    panic!("intentional panic from the registered-wait RawWaker");
}

/// Performs the same deliberately panicking wake without consuming the
/// vtable's strong task reference.
///
/// # Safety
///
/// `data` must come from `Arc::into_raw` for an [`InlineWaitTask`].
#[cfg(feature = "tokio")]
unsafe fn panicking_waker_wake_by_ref(data: *const ()) {
    // SAFETY: guaranteed by this vtable function's contract.
    let task = ManuallyDrop::new(unsafe { Arc::<InlineWaitTask>::from_raw(data.cast()) });
    task.poll_once();
    panic!("intentional panic from the registered-wait RawWaker");
}

/// Releases the strong task reference stored in a test `RawWaker`.
///
/// # Safety
///
/// `data` must come from `Arc::into_raw` for an [`InlineWaitTask`], and this
/// call consumes that strong reference.
#[cfg(feature = "tokio")]
unsafe fn panicking_waker_drop(data: *const ()) {
    // SAFETY: guaranteed by this vtable function's contract.
    drop(unsafe { Arc::<InlineWaitTask>::from_raw(data.cast()) });
}

#[cfg(feature = "tokio")]
static PANICKING_INLINE_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    panicking_waker_clone,
    panicking_waker_wake,
    panicking_waker_wake_by_ref,
    panicking_waker_drop,
);

#[cfg(feature = "tokio")]
fn panicking_inline_waker(task: Arc<InlineWaitTask>) -> Waker {
    let raw = RawWaker::new(Arc::into_raw(task).cast(), &PANICKING_INLINE_WAKER_VTABLE);
    // SAFETY: the vtable above preserves exactly one `Arc` strong reference
    // per RawWaker clone and consumes it from `wake` or `drop`.
    unsafe { Waker::from_raw(raw) }
}

#[cfg(feature = "tokio")]
#[test]
fn registered_wait_drop_from_an_inline_waker_does_not_deadlock() {
    // Keep the child alive until after the first poll installs the inline
    // Waker, then terminate it explicitly to make the callback deterministic.
    let mut child = spawn_cmd(&["/D", "/C", "ping -t 127.0.0.1 >nul"]);
    assert!(
        child
            .try_wait()
            .expect("polling the fixture child must succeed")
            .is_none(),
        "the fixture child must still be running"
    );

    let wait =
        RegisteredWait::new(child.as_handle()).expect("registering the process wait must work");
    let (completed_tx, completed_rx) = mpsc::channel();
    let task = Arc::new(InlineWaitTask {
        wait: Mutex::new(Some(Box::pin(wait))),
        completed: completed_tx,
    });

    task.poll_once();
    assert!(
        matches!(completed_rx.try_recv(), Err(mpsc::TryRecvError::Empty)),
        "the first poll must leave the process wait pending"
    );

    child
        .kill()
        .expect("terminating the fixture child must succeed");
    let code = completed_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("inline polling and reentrant Drop must not deadlock")
        .expect("the registered wait must report an exit code");
    assert_ne!(
        code, STILL_ACTIVE,
        "the callback must read the code only after the process is signaled"
    );
    assert!(
        task.wait
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none(),
        "the inline Ready path must have dropped the registration"
    );

    child.wait().expect("reaping via std must also succeed");
}

#[cfg(feature = "tokio")]
#[test]
fn registered_wait_contains_a_panicking_waker_at_the_ffi_boundary() {
    let mut child = spawn_cmd(&["/D", "/C", "ping -t 127.0.0.1 >nul"]);
    let wait =
        RegisteredWait::new(child.as_handle()).expect("registering the process wait must work");
    let cleanup_observed = Arc::new(AtomicBool::new(false));
    wait.observe_cleanup(Arc::clone(&cleanup_observed));
    let (completed_tx, completed_rx) = mpsc::channel();
    let task = Arc::new(InlineWaitTask {
        wait: Mutex::new(Some(Box::pin(wait))),
        completed: completed_tx,
    });
    let panicking = panicking_inline_waker(Arc::clone(&task));
    let mut panicking_cx = Context::from_waker(&panicking);
    {
        let mut slot = task.wait.lock().unwrap_or_else(PoisonError::into_inner);
        let wait = slot
            .as_mut()
            .expect("the registered wait must still be owned by the task");
        assert!(matches!(
            wait.as_mut().poll(&mut panicking_cx),
            Poll::Pending
        ));
    }

    child
        .kill()
        .expect("terminating the fixture child must succeed");
    let code = completed_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the panicking RawWaker must poll and drop the wait reentrantly")
        .expect("the registered wait must report an exit code");
    assert_ne!(
        code, STILL_ACTIVE,
        "the callback must read the code only after the process is signaled"
    );

    let cleanup_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !cleanup_observed.load(Ordering::Acquire) {
        assert!(
            std::time::Instant::now() < cleanup_deadline,
            "the callback tail must reclaim the context after containing the RawWaker panic"
        );
        thread::yield_now();
    }
    assert!(
        task.wait
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none(),
        "the inline Ready path must have dropped the registration"
    );

    child.wait().expect("reaping via std must also succeed");
}

#[cfg(feature = "tokio")]
#[test]
fn registered_wait_observes_a_process_that_already_exited() {
    let mut child = spawn_cmd(&["/D", "/C", "exit 37"]);
    let expected = child
        .wait()
        .expect("the fixture process must exit before registration")
        .code()
        .map(u32::try_from)
        .expect("Windows exit statuses always carry a code")
        .expect("the fixture uses a nonnegative exit code");
    let wait =
        RegisteredWait::new(child.as_handle()).expect("registering a signaled handle must work");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("building the test runtime must succeed");

    assert_eq!(
        runtime.block_on(wait).expect("the wait must complete"),
        expected
    );
}

#[cfg(feature = "tokio")]
#[test]
fn dropping_an_untriggered_registered_wait_is_prompt() {
    let mut child = spawn_cmd(&["/D", "/C", "ping -t 127.0.0.1 >nul"]);
    let wait =
        RegisteredWait::new(child.as_handle()).expect("registering the process wait must work");
    let cleanup_observed = Arc::new(AtomicBool::new(false));
    wait.observe_cleanup(Arc::clone(&cleanup_observed));
    let started = std::time::Instant::now();

    drop(wait);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "unregistering a pending process wait must not wait for process exit"
    );
    assert!(
        cleanup_observed.load(Ordering::Acquire),
        "a successful blocking unregister must reclaim its callback context"
    );

    child
        .kill()
        .expect("terminating the fixture child must succeed");
    child.wait().expect("reaping via std must also succeed");
}

#[cfg(feature = "tokio")]
#[test]
fn registered_wait_reuses_only_an_equivalent_waker() {
    struct WakeTarget;
    impl std::task::Wake for WakeTarget {
        fn wake(self: Arc<Self>) {}
    }

    let first = Waker::from(Arc::new(WakeTarget));
    let same = first.clone();
    let different = Waker::from(Arc::new(WakeTarget));
    assert!(!should_replace_waker(Some(&first), &same));
    assert!(should_replace_waker(Some(&first), &different));
    assert!(should_replace_waker(None, &first));
}

#[cfg(feature = "tokio")]
#[test]
fn callback_unregister_cleanup_classifier_covers_every_result() {
    let pending = i32::try_from(ERROR_IO_PENDING).expect("the Win32 code fits i32");
    assert!(callback_unregister_transfers_cleanup(1, None));
    assert!(callback_unregister_transfers_cleanup(0, Some(pending)));
    assert!(!callback_unregister_transfers_cleanup(0, Some(5)));
}

#[cfg(feature = "tracing")]
#[test]
fn callback_and_cleanup_failures_are_logged() {
    let events = crate::tracing_test_support::count_events(|| {
        log_callback_panic_safely("injected callback");
        log_unregister_failure(&io::Error::other("injected unregister failure"));
        log_root_watcher_kill_failure(&io::Error::other("injected kill failure"));
        log_legacy_worker_failure(&io::Error::other("injected worker failure"));
    });
    assert_eq!(events, 4);
}

#[test]
fn legacy_close_grace_only_waits_for_an_active_reader() {
    assert!(should_wait_for_legacy_reader(false));
    assert!(!should_wait_for_legacy_reader(true));
}

#[test]
fn legacy_wait_object_returns_the_published_registration() {
    let mut child = spawn_cmd(&["/C", "exit 0"]);
    let expected = child.as_handle().as_raw_handle();
    let context = LegacyWaitContext {
        process: ProcessWaiter::new(duplicated_handle(&child)),
        job: Weak::new(),
        shared: Weak::new(),
        grace: Duration::ZERO,
        close_legacy: false,
        spawn_close_worker: true,
        wait_object: Mutex::new(Some(expected)),
        wait_object_ready: Condvar::new(),
        cleanup_observer: None,
    };

    assert_eq!(legacy_wait_object(&context), expected);
    child.wait().expect("reaping via std must succeed");
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
    let job = watcher_job();
    spawn_root_watcher(
        duplicated_handle(&child),
        Arc::downgrade(&job),
        Arc::clone(&shared),
        Duration::from_millis(50),
        true,
    )
    .expect("spawning the watcher must succeed");

    // Drain conout on its own thread and treat *its* end-of-file as the
    // pass condition. Polling `is_closed` alone would not do: the state
    // flips to `Done` before the `ClosePseudoConsole` FFI call runs, so
    // it only proves the watcher *requested* a close. End-of-file, by
    // contrast, can only come from the console host exiting — that is,
    // from the close actually completing — so a regression that wedges
    // the close inside the post-exit worker times out here instead of
    // passing. The live reader also makes this the watcher's hardest
    // configuration: it must close from its own thread, with the conout
    // read end still open, and (on a real pre-24H2 host) block until this
    // reader has drained.
    let (eof_tx, eof_rx) = mpsc::channel();
    let reader = thread::Builder::new()
        .name("test-conout-reader".into())
        .spawn(move || {
            let mut conout = File::from(pipes.conout_read);
            let mut sink = Vec::new();
            // A broken pipe reads as end-of-file, so this returns once
            // the console host is gone.
            let _ = conout.read_to_end(&mut sink);
            let _ = eof_tx.send(());
        })
        .expect("spawning the reader thread must succeed");

    eof_rx.recv_timeout(Duration::from_secs(5)).expect(
        "the watcher must complete the close — observed as conout \
         end-of-file — within 5 seconds",
    );
    assert!(shared.is_closed());

    reader.join().expect("the reader thread must not panic");
    child.wait().expect("reaping via std must succeed");
    drop(console);
}

#[test]
fn legacy_watcher_closes_in_callback_when_worker_spawn_fails() {
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

    let mut child = spawn_cmd(&["/C", "exit 0"]);
    let job = watcher_job();
    let cleanup = Arc::new(AtomicBool::new(false));
    spawn_root_watcher_with_worker_spawn_failure(
        duplicated_handle(&child),
        Arc::downgrade(&job),
        Arc::clone(&shared),
        Duration::from_millis(10),
        true,
        Arc::clone(&cleanup),
    )
    .expect("registering the forced-fallback watcher must succeed");

    let (eof_tx, eof_rx) = mpsc::channel();
    let reader = thread::Builder::new()
        .name("test-fallback-conout-reader".into())
        .spawn(move || {
            let mut conout = File::from(pipes.conout_read);
            let mut sink = Vec::new();
            let read = conout.read_to_end(&mut sink);
            let _ = eof_tx.send(read);
        })
        .expect("spawning the reader thread must succeed");

    eof_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the callback fallback must close conout without hanging")
        .expect("reading conout through the fallback close must succeed");
    assert!(shared.is_closed());
    for _ in 0..100 {
        if cleanup.load(Ordering::Acquire) {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        cleanup.load(Ordering::Acquire),
        "the callback fallback retained its context and duplicated process handle"
    );

    reader.join().expect("the reader thread must not panic");
    child.wait().expect("reaping via std must succeed");
    drop(console);
}

#[test]
fn root_watcher_does_not_close_released_sessions() {
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
    let job = watcher_job();
    spawn_root_watcher(
        duplicated_handle(&child),
        Arc::downgrade(&job),
        Arc::clone(console.shared()),
        Duration::from_millis(10),
        false,
    )
    .expect("the released watcher path must succeed");
    child.wait().expect("reaping via std must succeed");

    // Give the registered callback time to terminate the (empty) Job, then
    // confirm the released backend did not close the console behind our back.
    thread::sleep(Duration::from_millis(100));
    assert!(!console.shared().is_closed());
    drop(console);
}
