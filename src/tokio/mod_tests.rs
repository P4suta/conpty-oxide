// SPDX-FileCopyrightText: 2025 conpty-oxide contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;
use super::{builder::PtyBuilder, pty::Pty};

use std::mem::size_of_val;
use std::time::Duration;

use ::tokio::io::{AsyncReadExt, AsyncWriteExt};
use ::tokio::task::JoinHandle;

use crate::core::session::{CLEAR_FEATURE, KILL_EXIT_CODE};
use crate::core::wait::ProcessWaiter;
use crate::PtyController;

/// Generous per-test budget: spawning `cmd.exe` under a fresh
/// pseudoconsole plus a legacy teardown grace period is comfortably under
/// this, and a hang is the failure mode being guarded against.
const TEST_TIMEOUT: Duration = Duration::from_secs(30);

/// `STATUS_CONTROL_C_EXIT`: the code a client reports when its terminal
/// goes away, i.e. the crate's documented consequence of closing conin on
/// a live session.
const STATUS_CONTROL_C_EXIT: u32 = 0xC000_013A;

/// Awaits `f`, failing the test if it has not finished within
/// [`TEST_TIMEOUT`].
///
/// Every interesting failure in this module is a stall — an undrained
/// output pipe, a `ClosePseudoConsole` that never returns, a wait for a
/// child that can no longer run. Without this the whole test binary would
/// hang instead of one test failing.
async fn complete_within<F: std::future::Future>(name: &str, f: F) -> F::Output {
    tokio::time::timeout(TEST_TIMEOUT, f)
        .await
        .unwrap_or_else(|error| panic!("`{name}` hung for more than {TEST_TIMEOUT:?}: {error}"))
}

/// Kills the test process if the guarded test has not finished within
/// [`TEST_TIMEOUT`]; disarmed by its own [`Drop`].
///
/// [`complete_within`] cannot report a destructor that blocks: on the
/// current-thread runtime `#[tokio::test]` uses, a wedged `Drop` occupies
/// the only thread and takes the runtime's timer down with it. The
/// teardown-heavy tests below — the ones whose interesting statements are
/// bare `drop`s outside any timeout scope — arm this process-killing
/// guard instead (the integration harness's watchdog pattern), so a
/// future teardown regression fails the `--lib` run rather than hanging
/// it forever.
struct ProcessWatchdog {
    finished: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for ProcessWatchdog {
    fn drop(&mut self) {
        self.finished
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

fn process_watchdog(name: &'static str) -> ProcessWatchdog {
    use std::sync::atomic::{AtomicBool, Ordering};

    let finished = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&finished);
    std::thread::Builder::new()
        .name(format!("watchdog-{name}"))
        .spawn(move || {
            let deadline = std::time::Instant::now() + TEST_TIMEOUT;
            while !flag.load(Ordering::SeqCst) {
                if std::time::Instant::now() >= deadline {
                    // 101 is what the harness reports for a failing run,
                    // so a killed test reads as a failure, not a crash.
                    eprintln!(
                        "conpty-oxide: `{name}` did not finish within \
                         {TEST_TIMEOUT:?}; assuming a wedged destructor \
                         and killing the test process"
                    );
                    std::process::exit(101);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        })
        .expect("spawning the watchdog thread must succeed");
    ProcessWatchdog { finished }
}

fn pty() -> Pty {
    Pty::builder().build().expect("building a pty must succeed")
}

/// A session forced onto the legacy shutdown path, whatever the OS.
///
/// On machines whose ConPTY exports `ReleasePseudoConsole` (Windows 11
/// 24H2 and later), every ordinary session in this module runs in released
/// mode and `Command::spawn_in` never arms the legacy watcher. Stripping the
/// export from the backend makes the spawn path take the watcher route for
/// real, so its regressions fail here instead of only on pre-24H2 CI.
fn legacy_pty() -> Pty {
    let backend = ConPtyBackend::system()
        .expect("ConPTY must be available")
        .without_release();
    assert!(!backend.supports_release());
    Pty::builder()
        .backend(backend)
        .build()
        .expect("building a forced-legacy pty must succeed")
}

/// A session under test: a running child, a task draining its output, and
/// the two halves that must stay alive while it runs.
///
/// Keeping the write half open for the child's whole life is not
/// housekeeping — closing the input pipe makes the console host terminate
/// its clients, which would both corrupt the exit status and hide a broken
/// end-of-file contract behind a trivially broken pipe.
struct Running {
    child: Child,
    reader: JoinHandle<Vec<u8>>,
    writer: OwnedWriteHalf,
    controller: PtyController,
}

impl Running {
    /// Spawns `command` in a fresh 80x24 session.
    fn start(command: &mut Command) -> Self {
        Self::start_in(pty(), command)
    }

    /// Spawns `command` in `pty`.
    fn start_in(pty: Pty, command: &mut Command) -> Self {
        let child = command.spawn_in(&pty).expect("spawning must succeed");
        Self::attach(pty, child)
    }

    /// Starts draining the output of an already-spawned child, which
    /// ConPTY requires to happen while the child runs.
    fn attach(pty: Pty, child: Child) -> Self {
        let controller = pty.controller();
        let (mut read_half, writer) = pty.into_split();
        let reader = ::tokio::spawn(async move {
            let mut sink = Vec::new();
            read_half
                .read_to_end(&mut sink)
                .await
                .expect("reading to end-of-file must succeed");
            sink
        });
        Self {
            child,
            reader,
            writer,
            controller,
        }
    }

    /// Waits for the child, then for end-of-file, and returns the rendered
    /// output together with the exit status.
    async fn finish(self) -> (String, ExitStatus) {
        let Self {
            mut child,
            reader,
            writer,
            controller,
        } = self;
        let status = child.wait().await.expect("waiting must succeed");
        // Joining is the real assertion: it returns only once the session
        // reached end-of-file, and since the write half is still open,
        // that end-of-file can only have come from the crate's own
        // shutdown path (a natural release, or the legacy watcher).
        let output = reader.await.expect("the reader task must not panic");
        drop(writer);
        drop(controller);
        (String::from_utf8_lossy(&output).into_owned(), status)
    }
}

/// Runs `cmd.exe` with `args` to completion in a fresh session.
async fn run_cmd(args: &[&str]) -> (String, ExitStatus) {
    Running::start(Command::new("cmd.exe").args(args))
        .finish()
        .await
}

#[test]
fn owned_parts_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Pty>();
    assert_send_sync::<OwnedReadHalf>();
    assert_send_sync::<OwnedWriteHalf>();
    assert_send_sync::<PtyController>();
    assert_send_sync::<Child>();
    assert_send_sync::<Command>();
    assert_send_sync::<PtyBuilder>();
}

/// The one misuse the front end is required to turn into an error rather
/// than a panic, because it is the easy mistake to make: building a
/// session from ordinary synchronous code.
#[test]
fn building_outside_a_runtime_is_an_error() {
    let err = Pty::builder()
        .build()
        .expect_err("building without a runtime must fail");
    assert_eq!(err.kind(), crate::ErrorKind::CreateConsole);
    let source = err
        .io_error()
        .expect("console creation errors carry an I/O error");
    assert!(
        source.to_string().contains("Tokio runtime"),
        "the error must name the cause, got: {source}"
    );
}

#[test]
fn building_with_a_disabled_io_driver_is_an_error() {
    let runtime = ::tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("building a runtime must succeed");
    let err = runtime.block_on(async {
        Pty::builder()
            .build()
            .expect_err("building without an I/O driver must fail")
    });

    assert_eq!(err.kind(), crate::ErrorKind::CreateConsole);
    let source = err
        .io_error()
        .expect("console creation errors carry an I/O error");
    assert!(
        source.to_string().contains("I/O driver is disabled"),
        "the error must name the cause, got: {source}"
    );
}

#[tokio::test]
async fn managed_output_future_stays_below_clippys_stack_threshold() {
    const LARGE_FUTURE_THRESHOLD: usize = 16 * 1024;

    let session = Command::new("cmd.exe")
        .args(["/d", "/c", "exit", "0"])
        .spawn()
        .expect("managed spawning must succeed");
    let future = session.collect_output();
    let bytes = size_of_val(&future);
    assert!(
        bytes < LARGE_FUTURE_THRESHOLD,
        "Session::collect_output() is {bytes} bytes; keep it below Clippy's \
         {LARGE_FUTURE_THRESHOLD}-byte large-future threshold without boxing"
    );
    future.await.expect("managed collection must complete");
}

/// `Debug` output ends up in logs and bug reports, so it must show a
/// session's identity — and must not leak raw handle values or the
/// private lifecycle state machine, which would make internals part of
/// the observable surface.
#[tokio::test]
async fn debug_shows_identity_not_internals() {
    let mut pty = pty();
    let rendered = format!("{pty:?}");
    assert!(rendered.starts_with("Pty"), "{rendered}");
    assert!(rendered.contains("size"), "{rendered}");
    assert!(rendered.contains("System"), "{rendered}");
    for leak in ["hpcon", "pipe", "handle", "state", "released"] {
        assert!(!rendered.contains(leak), "`{leak}` leaked: {rendered}");
    }

    let (read_half, write_half) = pty.split();
    assert_eq!(format!("{read_half:?}"), "ReadHalf { .. }");
    assert_eq!(format!("{write_half:?}"), "WriteHalf { .. }");

    let mut running = Running::start(Command::new("cmd.exe").args(["/c", "exit", "0"]));
    let rendered = format!("{:?}", running.child);
    assert!(rendered.contains("pid"), "{rendered}");
    assert!(!rendered.contains("handle"), "{rendered}");
    running.child.wait().await.expect("waiting must succeed");
    let rendered = format!("{:?}", running.child);
    assert!(
        rendered.contains("status"),
        "a reaped child must show its cached status: {rendered}"
    );
    running.finish().await;

    let controller = pty.controller();
    let (reader, writer) = pty.into_split();
    assert_eq!(format!("{reader:?}"), "OwnedReadHalf { .. }");
    assert_eq!(format!("{writer:?}"), "OwnedWriteHalf { .. }");
    let rendered = format!("{controller:?}");
    assert!(rendered.starts_with("PtyController"), "{rendered}");
    assert!(rendered.contains("size"), "{rendered}");
    assert!(rendered.contains("supports_clear"), "{rendered}");
    assert!(!rendered.contains("backend_kind"), "{rendered}");
}

#[tokio::test]
async fn builder_defaults_to_24_by_80_on_the_automatic_backend() {
    let expected = ConPtyBackend::auto().expect("automatic backend detection must succeed");
    let pty = pty();
    assert_eq!(pty.size(), Size::default());
    assert_eq!(pty.backend_kind(), expected.kind());
}

#[tokio::test]
async fn builder_honours_an_explicit_size_and_backend() {
    let backend = ConPtyBackend::system().expect("ConPTY must be available");
    let pty = Pty::builder()
        .size(crate::size::test_size(50, 132))
        .backend(backend)
        .inherit_cursor(false)
        .eof_on_root_exit(true)
        .build()
        .expect("building must succeed");
    assert_eq!(pty.size(), crate::size::test_size(50, 132));
}

/// `eof_on_root_exit`'s documented behaviour depends on whether the
/// backend has `ReleasePseudoConsole`, so a caller must be able to ask a
/// built session which lifecycle it runs — and the answer has to match
/// the backend's own, session by session rather than machine-wide.
#[tokio::test]
async fn supports_release_matches_the_backend() {
    let backend = ConPtyBackend::system().expect("ConPTY must be available");
    let expected = backend.supports_release();
    let pty = Pty::builder()
        .backend(backend)
        .build()
        .expect("building must succeed");
    assert_eq!(pty.supports_release(), expected);
    let controller = pty.controller();
    let (_reader, _writer) = pty.into_split();
    assert_eq!(controller.supports_release(), expected);

    // The query reflects the session's own backend, not the machine.
    let legacy = legacy_pty();
    assert!(!legacy.supports_release());
    let controller = legacy.controller();
    let (_reader, _writer) = legacy.into_split();
    assert!(!controller.supports_release());
}

#[tokio::test]
async fn dropping_the_reader_notifies_the_lifecycle_core() {
    let pty = pty();
    let controller = pty.controller();
    let (reader, writer) = pty.into_split();
    assert!(!controller.reader_finished());

    drop(reader);

    assert!(controller.reader_finished());
    drop(writer);
}

#[cfg(not(target_arch = "x86"))]
#[tokio::test]
async fn managed_session_reports_the_configured_bundle_clear_capability() {
    let Some(dir) = std::env::var_os("CONPTY_OXIDE_TEST_DLL_DIR") else {
        return;
    };
    let backend =
        ConPtyBackend::from_dir(dir).expect("the configured standalone backend must load");
    assert!(backend.supports_clear());
    let options = crate::SessionOptions::new().backend(backend);
    let session = Command::new("cmd.exe")
        .args(["/c", "exit", "0"])
        .spawn_with(options)
        .expect("managed spawn must succeed");

    assert!(session.supports_clear());
    assert!(session
        .collect_output()
        .await
        .expect("the managed session must finish")
        .status()
        .success());
}

#[tokio::test]
async fn resize_updates_the_reported_size() {
    let pty = pty();
    pty.resize(crate::size::test_size(40, 120))
        .expect("resize must succeed");
    assert_eq!(pty.size(), crate::size::test_size(40, 120));

    let controller = pty.controller();
    let (_reader, _writer) = pty.into_split();
    assert_eq!(controller.size(), crate::size::test_size(40, 120));
    controller
        .resize(Size::default())
        .expect("resize must succeed");
    assert_eq!(controller.size(), Size::default());
    assert_eq!(controller.backend_kind(), &BackendKind::System);
}

/// Runs a short child in `pty` to completion, then checks the documented
/// resize contract for a finished session.
///
/// Both lifecycle modes must report the same thing: on a released backend
/// the console host is gone but the `HPCON` is still open, so the error is
/// the normalized disconnect from the resize FFI; on a legacy backend the
/// watcher has closed the pseudoconsole, so it comes from the close-state
/// check. Either way the caller must see `NotConnected`.
async fn assert_resize_after_session_end_is_not_connected(pty: Pty) {
    let Running {
        mut child,
        reader,
        writer,
        controller,
    } = Running::start_in(pty, Command::new("cmd.exe").args(["/c", "exit", "0"]));
    child.wait().await.expect("waiting must succeed");
    // End-of-file proves the session is over (and, on a legacy backend,
    // that the watcher has already closed the pseudoconsole).
    reader.await.expect("the reader task must not panic");

    let err = controller
        .resize(crate::size::test_size(30, 100))
        .expect_err("resizing a finished session must fail");
    assert_eq!(err.kind(), crate::ErrorKind::Resize);
    let source = err.io_error().expect("resize errors carry an I/O error");
    assert_eq!(
        source.kind(),
        io::ErrorKind::NotConnected,
        "a finished session must report NotConnected on every backend, got: {source:?}"
    );
    drop(writer);
}

#[tokio::test]
async fn resize_after_the_session_ends_reports_not_connected() {
    complete_within(
        "resize_after_the_session_ends_reports_not_connected",
        assert_resize_after_session_end_is_not_connected(pty()),
    )
    .await;
}

#[tokio::test]
async fn forced_legacy_resize_after_the_session_ends_reports_not_connected() {
    complete_within(
        "forced_legacy_resize_after_the_session_ends_reports_not_connected",
        assert_resize_after_session_end_is_not_connected(legacy_pty()),
    )
    .await;
}

/// The system backend exports no `ClearPseudoConsole`, so on an ordinary
/// machine this exercises the typed refusal. On a session backed by a
/// bundled `conpty.dll` the same test proves the call goes through — the
/// assertion is that the capability query and the operation agree.
#[tokio::test]
async fn clear_agrees_with_the_reported_capability() {
    let pty = pty();
    let supported = pty.supports_clear();
    let from_pty = pty.clear();
    let controller = pty.controller();
    let (_reader, _writer) = pty.into_split();
    assert_eq!(controller.supports_clear(), supported);

    assert_eq!(from_pty.is_ok(), supported);
    match controller.clear() {
        Ok(()) => assert!(supported, "clear succeeded without a clear export"),
        Err(err) if err.kind() == crate::ErrorKind::UnsupportedFeature => {
            assert!(!supported, "clear refused although the export is present");
            assert!(err.to_string().contains(CLEAR_FEATURE));
        },
        Err(other) => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn echoed_output_reaches_the_reader_and_the_session_ends() {
    const MARKER: &str = "conpty-oxide-async-marker";
    let (output, status) = complete_within(
        "echoed_output_reaches_the_reader",
        run_cmd(&["/c", "echo", MARKER]),
    )
    .await;
    assert!(
        output.contains(MARKER),
        "marker missing from the rendered output: {output:?}"
    );
    assert!(status.success(), "unexpected status: {status}");
    assert_eq!(status.code(), 0);
}

#[tokio::test]
async fn a_forced_legacy_session_reaches_end_of_file() {
    const MARKER: &str = "conpty-oxide-async-forced-legacy-marker";
    // `finish` awaiting the reader is the real assertion: the session was
    // never released, so only the legacy watcher's close can produce the
    // end-of-file the reader task waits for.
    let (output, status) = complete_within(
        "a_forced_legacy_session_reaches_end_of_file",
        Running::start_in(
            legacy_pty(),
            Command::new("cmd.exe").args(["/c", "echo", MARKER]),
        )
        .finish(),
    )
    .await;
    assert!(
        output.contains(MARKER),
        "marker missing from the rendered output: {output:?}"
    );
    assert!(status.success(), "unexpected status: {status}");
}

#[tokio::test]
async fn exit_code_is_reported_verbatim() {
    let (_output, status) = complete_within(
        "exit_code_is_reported_verbatim",
        run_cmd(&["/c", "exit", "7"]),
    )
    .await;
    assert_eq!(status.code(), 7);
    assert!(!status.success());
}

#[tokio::test]
async fn registered_wait_preserves_sentinel_and_high_bit_exit_codes() {
    let (_output, sentinel) = complete_within(
        "registered_wait_preserves_259",
        run_cmd(&["/c", "exit", "259"]),
    )
    .await;
    assert_eq!(sentinel.code(), 259);

    // STATUS_CONTROL_C_EXIT expressed as a signed decimal value. The
    // high bit must survive the callback, cache, and public status type.
    let (_output, high_bit) = complete_within(
        "registered_wait_preserves_high_bit",
        run_cmd(&["/c", "exit", "-1073741510"]),
    )
    .await;
    assert_eq!(high_bit.code(), 0xC000_013A);
}

#[tokio::test]
async fn the_environment_reaches_the_child() {
    const MARKER: &str = "conpty-oxide-async-env-9182";
    let (output, _status) = complete_within(
        "the_environment_reaches_the_child",
        Running::start(
            Command::new("cmd.exe")
                .args(["/c", "echo", "%CONPTY_OXIDE_ASYNC_MARKER%"])
                .env("CONPTY_OXIDE_ASYNC_MARKER", MARKER),
        )
        .finish(),
    )
    .await;
    // An unexpanded `%CONPTY_OXIDE_ASYNC_MARKER%` here would mean the
    // environment block never reached the child.
    assert!(
        output.contains(MARKER),
        "marker missing from the rendered output: {output:?}"
    );
}

#[tokio::test]
async fn written_input_reaches_the_child() {
    // An interactive `cmd.exe` only exits when it reads the `exit` command
    // from its console input, so the child terminating with that exact
    // code proves the bytes travelled through conin.
    let mut running = Running::start(&mut Command::new("cmd.exe"));
    running
        .writer
        .write_all(b"exit 3\r\n")
        .await
        .expect("writing console input must succeed");
    running
        .writer
        .flush()
        .await
        .expect("flush must be a no-op that succeeds");

    let (_output, status) =
        complete_within("written_input_reaches_the_child", running.finish()).await;
    assert_eq!(status.code(), 3);
}

#[tokio::test]
async fn kill_terminates_the_tree_and_reports_a_status() {
    let _watchdog = process_watchdog("kill_terminates_the_tree_and_reports_a_status");
    // `pause` blocks on console input that this test never sends.
    let mut running = Running::start(Command::new("cmd.exe").args(["/c", "pause"]));

    assert_ne!(running.child.id(), 0, "a spawned child must have a pid");
    assert!(
        running
            .child
            .try_wait()
            .expect("polling must succeed")
            .is_none(),
        "a blocked child must not report a status yet"
    );
    assert!(!running.child.as_handle().as_raw_handle().is_null());

    running.child.kill().expect("kill must succeed");
    let status = complete_within("kill_terminates_the_tree", running.child.wait())
        .await
        .expect("waiting must succeed");
    assert_eq!(status.code(), KILL_EXIT_CODE);
    // A second kill of a dead tree is a documented no-op.
    running
        .child
        .kill()
        .expect("killing a dead tree must succeed");

    let (_output, again) = complete_within("kill_teardown", running.finish()).await;
    assert_eq!(again, status, "the status must be cached, not re-read");
}

#[tokio::test]
async fn wait_is_repeatable_and_matches_try_wait() {
    let mut running = Running::start(Command::new("cmd.exe").args(["/c", "exit", "5"]));
    let first = complete_within("wait_is_repeatable", running.child.wait())
        .await
        .expect("waiting must succeed");
    assert_eq!(
        running
            .child
            .wait()
            .await
            .expect("waiting again must succeed"),
        first
    );
    assert_eq!(
        running.child.try_wait().expect("polling must succeed"),
        Some(first)
    );
    assert_eq!(running.child.cached_status(), Some(first));
    assert_eq!(first.code(), 5);
    complete_within("wait_is_repeatable_teardown", running.finish()).await;
}

/// A dropped `wait` future must lose nothing: the next `wait` has to
/// return the real exit status, not an error and not a hang.
#[tokio::test]
async fn a_cancelled_wait_can_be_retried() {
    let mut running = Running::start(Command::new("cmd.exe").args(["/c", "pause"]));

    // The child is blocked on input, so this wait cannot possibly finish
    // and is guaranteed to be cancelled mid-flight.
    assert!(
        tokio::time::timeout(Duration::from_millis(100), running.child.wait())
            .await
            .is_err(),
        "a blocked child must not report a status yet"
    );

    running.child.kill().expect("kill must succeed");
    let status = complete_within("a_cancelled_wait_can_be_retried", running.child.wait())
        .await
        .expect("a retried wait must succeed");
    assert_eq!(status.code(), KILL_EXIT_CODE);
    complete_within("a_cancelled_wait_teardown", running.finish()).await;
}

#[tokio::test]
async fn kill_on_drop_terminates_the_tree() {
    let running = Running::start(
        Command::new("cmd.exe")
            .args(["/c", "pause"])
            .kill_on_drop(true),
    );

    // An independent handle, so the process can still be observed after
    // the `Child` — and with it the job object — is gone.
    let watched = ProcessWaiter::new(
        running
            .child
            .as_handle()
            .try_clone_to_owned()
            .expect("duplicating the process handle must succeed"),
    );
    assert!(watched.try_wait().expect("polling must succeed").is_none());

    let Running {
        child,
        reader,
        writer,
        controller,
    } = running;
    drop(child);
    assert_eq!(
        watched.wait().expect("waiting must succeed"),
        KILL_EXIT_CODE,
        "dropping a kill-on-drop child must terminate the tree"
    );
    complete_within("kill_on_drop_terminates_the_tree", reader)
        .await
        .expect("the reader task must not panic");
    drop(writer);
    drop(controller);
}

#[tokio::test]
async fn a_second_spawn_into_the_same_pty_is_rejected() {
    let pty = pty();
    let child = Command::new("cmd.exe")
        .args(["/c", "exit", "0"])
        .spawn_in(&pty)
        .expect("the first spawn must succeed");

    let err = Command::new("cmd.exe")
        .args(["/c", "exit", "0"])
        .spawn_in(&pty)
        .expect_err("a second spawn must be rejected");
    assert_eq!(err.kind(), crate::ErrorKind::Spawn);
    assert_eq!(
        err.io_error()
            .expect("spawn errors carry an I/O error")
            .kind(),
        io::ErrorKind::AlreadyExists
    );

    let (_output, status) = complete_within(
        "a_second_spawn_is_rejected",
        Running::attach(pty, child).finish(),
    )
    .await;
    assert!(status.success());
}

#[tokio::test]
async fn a_failed_spawn_leaves_the_session_reusable() {
    let pty = pty();
    let err = Command::new("conpty-oxide-no-such-program.exe")
        .spawn_in(&pty)
        .expect_err("spawning a missing program must fail");
    assert_eq!(err.kind(), crate::ErrorKind::Spawn);
    assert!(err.to_string().contains("conpty-oxide-no-such-program.exe"));
    assert_eq!(
        err.io_error()
            .expect("spawn errors carry an I/O error")
            .kind(),
        io::ErrorKind::NotFound
    );

    // The failed attempt attached nothing, so the session is still good
    // for a real child.
    let (_output, status) = complete_within(
        "a_failed_spawn_leaves_the_session_reusable",
        Running::start_in(pty, Command::new("cmd.exe").args(["/c", "exit", "0"])).finish(),
    )
    .await;
    assert!(status.success());
}

#[tokio::test]
async fn reading_an_empty_buffer_is_not_end_of_file() {
    let mut pty = pty();
    let (mut reader, _writer) = pty.split();
    assert_eq!(
        reader
            .read(&mut [])
            .await
            .expect("a zero-length read must succeed"),
        0
    );
    // A zero-length read must not have reported end-of-file, so the
    // session is still open and still resizable.
    pty.resize(crate::size::test_size(30, 100))
        .expect("the session must still be open");
}

/// Shutting the write half down is the documented way to end a session
/// from the input side: it must close the input pipe without blocking,
/// refuse further writes, and bring the whole session down with it.
#[tokio::test]
async fn shutting_down_the_write_half_ends_the_session() {
    complete_within("shutting_down_the_write_half_ends_the_session", async {
        write_half_ends_the_session_in(pty()).await;
    })
    .await;
}

#[tokio::test]
async fn shutting_down_the_write_half_ends_a_forced_legacy_session() {
    complete_within(
        "shutting_down_the_write_half_ends_a_forced_legacy_session",
        write_half_ends_the_session_in(legacy_pty()),
    )
    .await;
}

async fn write_half_ends_the_session_in(pty: Pty) {
    const MARKER: &str = "conpty-oxide-async-ready-marker";

    let mut child = Command::new("cmd.exe")
        .spawn_in(&pty)
        .expect("spawning must succeed");
    let controller = pty.controller();
    let (mut reader, mut writer) = pty.into_split();

    // First prove the child is attached and reading console input:
    // `cmd.exe` cannot echo this line back before it has done both.
    writer
        .write_all(format!("echo {MARKER}\r\n").as_bytes())
        .await
        .expect("writing console input must succeed");
    let mut seen = String::new();
    let mut buf = [0u8; 4096];
    while !seen.contains(MARKER) {
        let read = reader.read(&mut buf).await.expect("reading must succeed");
        assert_ne!(read, 0, "the session ended before the child started");
        seen.push_str(&String::from_utf8_lossy(&buf[..read]));
    }

    writer.shutdown().await.expect("shutdown must succeed");
    let err = writer
        .write_all(b"exit\r\n")
        .await
        .expect_err("writing after a shutdown must fail");
    assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    writer
        .shutdown()
        .await
        .expect("a repeated shutdown must succeed");

    // Writer retirement closes conin and requests pseudoconsole close. The
    // latter sends CTRL_CLOSE_EVENT portably, including on legacy Windows,
    // while this reader remains available to drain the final output.
    let mut sink = Vec::new();
    reader
        .read_to_end(&mut sink)
        .await
        .expect("reading to end-of-file must succeed");
    let status = child.wait().await.expect("waiting must succeed");
    assert_eq!(
        status.code(),
        STATUS_CONTROL_C_EXIT,
        "a child whose terminal went away must report \
         STATUS_CONTROL_C_EXIT, got: {status}"
    );
    drop(controller);
}

#[tokio::test]
async fn a_session_without_the_eof_watcher_still_tears_down() {
    let _watchdog = process_watchdog("a_session_without_the_eof_watcher_still_tears_down");
    let pty = Pty::builder()
        .eof_on_root_exit(false)
        .build()
        .expect("building must succeed");
    let mut child = Command::new("cmd.exe")
        .args(["/c", "exit", "0"])
        .spawn_in(&pty)
        .expect("spawning must succeed");
    assert!(complete_within("without_the_eof_watcher", child.wait())
        .await
        .expect("waiting must succeed")
        .success());

    // Without a watcher a legacy session never reaches end-of-file on its
    // own, so the reader is retired by dropping the session instead.
    // Dropping must not hang, on any backend.
    drop(pty);
}

#[tokio::test]
async fn the_controller_keeps_an_idle_session_alive() {
    let _watchdog = process_watchdog("the_controller_keeps_an_idle_session_alive");
    let second_pty = pty();
    let controller = second_pty.controller();
    let (reader, writer) = second_pty.into_split();
    // Retiring both pipe ends does not end the session: nothing has asked
    // for a close, and the controller still owns the console.
    drop(reader);
    drop(writer);
    controller
        .resize(crate::size::test_size(30, 100))
        .expect("a session with a live controller must still resize");
    assert_eq!(controller.size(), crate::size::test_size(30, 100));
}

#[tokio::test]
async fn dropping_the_parts_in_any_order_completes() {
    let _watchdog = process_watchdog("dropping_the_parts_in_any_order_completes");
    // Controller first, then the write half, then the reader: the
    // pseudoconsole outlives its controller and is closed by the last part
    // standing.
    let first_pty = pty();
    let controller = first_pty.controller();
    let (reader, writer) = first_pty.into_split();
    drop(controller);
    drop(writer);
    drop(reader);

    // And the reverse order.
    let second_pty = pty();
    let controller = second_pty.controller();
    let (reader, writer) = second_pty.into_split();
    drop(reader);
    drop(writer);
    drop(controller);
}

#[tokio::test]
async fn managed_output_drains_more_than_pipe_capacity() {
    let output = Box::pin(complete_within(
        "managed_output_drains_more_than_pipe_capacity",
        Command::new("cmd.exe")
            .args([
                "/d",
                "/q",
                "/c",
                "for /L %i in (1,1,6000) do @echo managed-output-%i-01234567890123456789",
            ])
            .spawn()
            .expect("managed spawning must succeed")
            .collect_output(),
    ))
    .await
    .expect("managed output must complete");

    assert!(output.status().success());
    assert!(
        output.as_bytes().len() > 64 * 1024,
        "the fixture must exceed ordinary pipe capacity"
    );
    let rendered = String::from_utf8_lossy(output.as_bytes());
    let sequence = rendered
        .split("managed-output-")
        .skip(1)
        .map(|tail| {
            tail.split('-')
                .next()
                .expect("every marker has an index")
                .parse::<u32>()
                .expect("every marker index is numeric")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        sequence,
        (1..=6000).collect::<Vec<_>>(),
        "the complete ordered VT payload must be collected without gaps or duplicates"
    );
}

#[tokio::test]
async fn managed_wait_drains_more_than_pipe_capacity_without_collecting() {
    let status = Box::pin(complete_within(
        "managed_wait_drains_more_than_pipe_capacity",
        Command::new("cmd.exe")
            .raw_arg(
                r#"/d /q /c "for /L %i in (1,1,6000) do @echo managed-wait-%i-01234567890123456789 & exit /b 37""#,
            )
            .spawn()
            .expect("managed spawning must succeed")
            .wait(),
    ))
    .await
    .expect("managed wait must drain output and complete");
    assert_eq!(status.code(), 37);
}

#[tokio::test]
async fn managed_output_keeps_input_open_until_the_real_exit() {
    let output = Box::pin(complete_within(
        "managed_output_keeps_input_open",
        Command::new("cmd.exe")
            .raw_arg(r#"/d /q /c "ping -n 2 127.0.0.1 >nul & exit 42""#)
            .spawn()
            .expect("managed spawning must succeed")
            .collect_output(),
    ))
    .await
    .expect("managed output must complete");
    assert_eq!(output.status().code(), 42);
}

async fn assert_root_bounded_collection(backend: ConPtyBackend) {
    const MARKER: &str = "tokio-root-bounded-tail";

    let options = crate::SessionOptions::new().backend(backend);
    let mut session = Command::new("cmd.exe")
        .args(["/d", "/q"])
        .spawn_with(options)
        .expect("managed spawning must succeed");
    session
        .write_all(
            format!("start \"\" /b ping -t 127.0.0.1 >nul & echo {MARKER} & exit /b 23\r\n")
                .as_bytes(),
        )
        .await
        .expect("the root command must reach the session");

    let output = session
        .collect_output()
        .await
        .expect("root-bounded collection must finish");
    assert_eq!(output.status().code(), 23);
    assert!(
        String::from_utf8_lossy(output.as_bytes()).contains(MARKER),
        "the root's teardown tail must be preserved"
    );
}

#[tokio::test]
async fn managed_collection_has_the_same_root_boundary_on_both_lifecycles() {
    complete_within("managed_collection_root_boundary", async {
        let system = ConPtyBackend::system().expect("ConPTY must be available");
        assert_root_bounded_collection(system.without_release()).await;
        assert_root_bounded_collection(system).await;

        #[cfg(not(target_arch = "x86"))]
        if let Some(dir) = std::env::var_os("CONPTY_OXIDE_TEST_DLL_DIR") {
            let bundle =
                ConPtyBackend::from_dir(dir).expect("the configured standalone backend must load");
            assert!(bundle.supports_release());
            assert_root_bounded_collection(bundle).await;
        }
    })
    .await;
}

#[test]
fn command_builder_delegates_every_configuration_category() {
    let system_root =
        std::env::var_os("SystemRoot").expect("supported Windows installations define SystemRoot");
    let current = std::env::current_dir().expect("reading the current directory must succeed");
    let mut command = Command::new("cmd.exe");
    command
        .arg("/d")
        .args(["/q", "/c"])
        .raw_arg("echo %CONPTY_COV_ONE%,%CONPTY_COV_TWO%,%CONPTY_COV_REMOVED%,%CD%")
        .env_clear()
        .env("SystemRoot", system_root)
        .env("CONPTY_COV_ONE", "first")
        .envs([
            ("CONPTY_COV_ONE", "one"),
            ("CONPTY_COV_TWO", "two"),
            ("CONPTY_COV_REMOVED", "remove-me"),
        ])
        .env_remove("CONPTY_COV_REMOVED")
        .current_dir(&current)
        .kill_on_drop(false);

    let _watchdog = process_watchdog("command_builder_delegates_every_configuration_category");
    let runtime = ::tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("building the test runtime must succeed");
    let output = runtime
        .block_on(async {
            command
                .spawn()
                .expect("managed spawning must succeed")
                .collect_output()
                .await
        })
        .expect("the fully configured command must complete");
    assert!(output.status().success());
    let text = String::from_utf8_lossy(output.as_bytes());
    assert!(text.contains("one,two,%CONPTY_COV_REMOVED%"), "{text}");
    assert!(
        text.to_ascii_lowercase()
            .contains(&current.display().to_string().to_ascii_lowercase()),
        "{text}"
    );
}

#[tokio::test]
async fn shutting_down_direct_pty_retires_its_input() {
    complete_within("shutting_down_direct_pty_retires_its_input", async {
        let mut direct = pty();
        direct
            .shutdown()
            .await
            .expect("shutting down direct Pty input must succeed");
        let error = direct
            .write_all(b"after-shutdown")
            .await
            .expect_err("direct Pty writes after shutdown must fail");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    })
    .await;
}

#[tokio::test]
async fn low_level_pty_and_borrowed_halves_delegate_io() {
    const DIRECT: &str = "tokio-direct-pty-marker";
    const BORROWED: &str = "tokio-borrowed-half-marker";

    let mut direct = pty();
    let mut child = Command::new("cmd.exe")
        .args(["/d", "/q"])
        .spawn_in(&direct)
        .expect("spawning the direct-I/O shell must succeed");
    assert!(!AsHandle::as_handle(&child).as_raw_handle().is_null());
    direct
        .write_all(format!("echo {DIRECT}\r\nexit\r\n").as_bytes())
        .await
        .expect("writing through Pty must succeed");
    direct.flush().await.expect("flushing Pty must succeed");
    let mut output = String::new();
    direct
        .read_to_string(&mut output)
        .await
        .expect("reading through Pty must reach EOF");
    assert_eq!(
        direct
            .read(&mut [0])
            .await
            .expect("reading Pty again after EOF must succeed"),
        0
    );
    direct
        .shutdown()
        .await
        .expect("shutting down Pty input after EOF must succeed");
    direct
        .shutdown()
        .await
        .expect("repeated Pty input shutdown must be idempotent");
    let direct_error = direct
        .write_all(b"after-shutdown")
        .await
        .expect_err("writes through Pty after shutdown must fail");
    assert_eq!(direct_error.kind(), io::ErrorKind::BrokenPipe);
    assert!(child.wait().await.expect("waiting must succeed").success());
    assert!(output.contains(DIRECT), "{output}");

    let mut split_pty = pty();
    let mut split_child = Command::new("cmd.exe")
        .args(["/d", "/q"])
        .spawn_in(&split_pty)
        .expect("spawning the borrowed-I/O shell must succeed");
    let mut split_output = String::new();
    {
        let (mut reader, mut writer) = split_pty.split();
        writer
            .write_all(format!("echo {BORROWED}\r\nexit\r\n").as_bytes())
            .await
            .expect("writing through WriteHalf must succeed");
        writer
            .flush()
            .await
            .expect("flushing WriteHalf must succeed");
        reader
            .read_to_string(&mut split_output)
            .await
            .expect("reading through ReadHalf must reach EOF");
        assert_eq!(
            reader
                .read(&mut [0])
                .await
                .expect("reading ReadHalf again after EOF must succeed"),
            0
        );
        writer
            .shutdown()
            .await
            .expect("shutting down WriteHalf after EOF must succeed");
        writer
            .shutdown()
            .await
            .expect("repeated WriteHalf shutdown must be idempotent");
        let split_error = writer
            .write_all(b"after-shutdown")
            .await
            .expect_err("writes through WriteHalf after shutdown must fail");
        assert_eq!(split_error.kind(), io::ErrorKind::BrokenPipe);
    }
    assert!(split_child
        .wait()
        .await
        .expect("waiting for the split child must succeed")
        .success());
    assert!(split_output.contains(BORROWED), "{split_output}");
}

#[tokio::test]
async fn managed_session_try_wait_reports_a_completed_child() {
    const MARKER: &str = "tokio-completed-root-tail";
    let mut session = Command::new("cmd.exe")
        .raw_arg(format!(r#"/d /q /c "echo {MARKER} & exit /b 23""#))
        .spawn()
        .expect("managed spawning must succeed");
    let expected = complete_within("managed_session_child_completion", session.child.wait())
        .await
        .expect("waiting for the managed child must succeed");

    assert_eq!(
        session
            .try_wait()
            .expect("polling the completed managed session must succeed"),
        Some(expected)
    );

    let output = complete_within(
        "managed_session_try_wait_teardown",
        session.collect_output(),
    )
    .await
    .expect("draining the completed managed session must succeed");
    assert_eq!(output.status(), expected);
    assert!(
        String::from_utf8_lossy(output.as_bytes()).contains(MARKER),
        "output buffered after root completion must still be drained"
    );
}

#[tokio::test]
async fn managed_session_delegates_io_and_debugs_named_parts() {
    const MARKER: &str = "tokio-managed-session-io";
    let mut session = Command::new("cmd.exe")
        .args(["/d", "/q"])
        .spawn()
        .expect("managed spawning must succeed");
    let session_debug = format!("{session:?}");
    assert!(session_debug.starts_with("Session"), "{session_debug}");

    session
        .write_all(format!("echo {MARKER}\r\nexit\r\n").as_bytes())
        .await
        .expect("writing through Session must succeed");
    session
        .flush()
        .await
        .expect("flushing Session must succeed");
    let mut output = String::new();
    session
        .read_to_string(&mut output)
        .await
        .expect("reading through Session must reach EOF");
    assert!(output.contains(MARKER), "{output}");
    // Conout EOF and the root process handle becoming signaled are
    // independent kernel events. Released ConPTY can expose EOF a scheduling
    // instant first, so `try_wait` may still return None; the async wait
    // through SessionParts below proves completion.
    if let Some(status) = session
        .try_wait()
        .expect("polling the managed session must succeed")
    {
        assert!(status.success());
    }

    session
        .shutdown()
        .await
        .expect("shutting down input after exit must succeed");
    session
        .shutdown()
        .await
        .expect("repeated shutdown must be idempotent");
    let error = session
        .write_all(b"after-shutdown")
        .await
        .expect_err("writes after shutdown must fail");
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);

    let mut parts = session.into_parts();
    let parts_debug = format!("{parts:?}");
    assert!(parts_debug.starts_with("SessionParts"), "{parts_debug}");
    assert!(parts
        .child
        .wait()
        .await
        .expect("waiting through SessionParts must succeed")
        .success());
}

#[tokio::test]
async fn shutting_down_a_live_managed_session_retires_its_input() {
    complete_within(
        "shutting_down_a_live_managed_session_retires_its_input",
        async {
            const MARKER: &str = "tokio-managed-shutdown-ready";
            let mut session = Command::new("cmd.exe")
                .args(["/d", "/q"])
                .spawn()
                .expect("managed spawning must succeed");

            // Prove the client has finished initialization before closing its
            // terminal. On legacy Windows, closing during loader startup can
            // legitimately surface STATUS_DLL_INIT_FAILED instead of the
            // CTRL_CLOSE_EVENT status this test is specifically checking.
            session
                .write_all(format!("echo {MARKER}\r\n").as_bytes())
                .await
                .expect("writing the readiness marker must succeed");
            session
                .flush()
                .await
                .expect("flushing the readiness marker must succeed");
            let mut seen = String::new();
            let mut buffer = [0_u8; 4096];
            while !seen.contains(MARKER) {
                let read = session
                    .read(&mut buffer)
                    .await
                    .expect("reading the readiness marker must succeed");
                assert_ne!(read, 0, "the session ended before the child was ready");
                seen.push_str(&String::from_utf8_lossy(&buffer[..read]));
            }

            session.shutdown().await.expect("shutdown must succeed");
            let error = session
                .write_all(b"after-shutdown")
                .await
                .expect_err("writes after live-session shutdown must fail");
            assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);

            let mut parts = session.into_parts();
            let mut sink = Vec::new();
            parts
                .output
                .read_to_end(&mut sink)
                .await
                .expect("shutdown must let output reach EOF");
            let status = parts.child.wait().await.expect("waiting must succeed");
            assert_eq!(status.code(), STATUS_CONTROL_C_EXIT);
        },
    )
    .await;
}

#[tokio::test]
async fn cancelling_collect_output_kills_the_managed_tree() {
    let session = Command::new("cmd.exe")
        .spawn()
        .expect("managed spawn must succeed");
    let watched = ProcessWaiter::new(
        session
            .child
            .as_handle()
            .try_clone_to_owned()
            .expect("duplicating the process handle must succeed"),
    );

    assert!(
        ::tokio::time::timeout(Duration::from_millis(50), session.collect_output())
            .await
            .is_err(),
        "the interactive child must still be running when collection is cancelled"
    );
    assert_eq!(
        watched.wait().expect("waiting must succeed"),
        KILL_EXIT_CODE
    );
}

#[tokio::test]
async fn cancelling_session_wait_kills_the_managed_tree() {
    let session = Command::new("cmd.exe")
        .spawn()
        .expect("managed spawn must succeed");
    let watched = ProcessWaiter::new(
        session
            .child
            .as_handle()
            .try_clone_to_owned()
            .expect("duplicating the process handle must succeed"),
    );

    assert!(
        ::tokio::time::timeout(Duration::from_millis(50), session.wait())
            .await
            .is_err(),
        "the interactive child must still be running when wait is cancelled"
    );
    assert_eq!(
        watched.wait().expect("waiting must succeed"),
        KILL_EXIT_CODE
    );
}

#[test]
fn dropping_a_runtime_does_not_wait_for_a_registered_child_wait() {
    let runtime = ::tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("building a runtime must succeed");

    let (parts, watched) = runtime.block_on(async {
        let mut parts = Command::new("cmd.exe")
            .spawn()
            .expect("managed spawn must succeed")
            .into_parts();
        let watched = ProcessWaiter::new(
            parts
                .child
                .as_handle()
                .try_clone_to_owned()
                .expect("duplicating the process handle must succeed"),
        );
        assert!(
            ::tokio::time::timeout(Duration::from_millis(50), parts.child.wait())
                .await
                .is_err(),
            "the child wait must be pending before runtime shutdown"
        );
        (parts, watched)
    });

    let started = std::time::Instant::now();
    drop(runtime);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "runtime shutdown was held open by the child wait"
    );

    drop(parts.child);
    assert_eq!(
        watched.wait().expect("waiting must succeed"),
        KILL_EXIT_CODE
    );
    drop(parts.output);
    drop(parts.input);
    drop(parts.controller);
}
