// SPDX-FileCopyrightText: 2025 conpty-oxide contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;
use super::{builder::PtyBuilder, pty::Pty};

use crate::core::session::{CLEAR_FEATURE, KILL_EXIT_CODE};
use crate::core::wait::ProcessWaiter;
use crate::PtyController;

use std::panic;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Generous per-test budget: spawning `cmd.exe` under a fresh
/// pseudoconsole plus a legacy teardown grace period is comfortably under
/// this, and a hang is the failure mode being guarded against.
const TEST_TIMEOUT: Duration = Duration::from_secs(30);

/// `STATUS_CONTROL_C_EXIT`: the code a client reports when its terminal
/// goes away, i.e. the crate's documented consequence of closing conin on
/// a live session.
const STATUS_CONTROL_C_EXIT: u32 = 0xC000_013A;

/// Runs `f` on a helper thread and fails the test if it has not finished
/// within [`TEST_TIMEOUT`].
///
/// Every interesting failure in this module is a deadlock — an undrained
/// output pipe, a `ClosePseudoConsole` that never returns, a `wait` for a
/// child that can no longer run. Without a watchdog those would stall the
/// whole test binary instead of failing one test. A panic inside `f` is
/// re-raised here so assertion failures keep their original message.
fn complete_within(name: &str, f: impl FnOnce() + Send + 'static) {
    let (done_tx, done_rx) = mpsc::channel();
    let handle = thread::Builder::new()
        .name(format!("watchdog-subject-{name}"))
        .spawn(move || {
            f();
            let _ = done_tx.send(());
        })
        .expect("spawning the test subject thread must succeed");

    match done_rx.recv_timeout(TEST_TIMEOUT) {
        // The sender was dropped without sending: `f` panicked, and the
        // join below re-raises it with its original message.
        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {},
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("`{name}` hung for more than {TEST_TIMEOUT:?}")
        },
    }
    if let Err(payload) = handle.join() {
        panic::resume_unwind(payload);
    }
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

/// A session under test: a running child, a thread draining its output,
/// and the two halves that must stay alive while it runs.
///
/// Keeping the write half open for the child's whole life is not
/// housekeeping — closing the input pipe makes the console host terminate
/// its clients, which would both corrupt the exit status and hide a broken
/// end-of-file contract behind a trivially broken pipe.
struct Running {
    child: Child,
    reader: thread::JoinHandle<Vec<u8>>,
    writer: OwnedWriteHalf,
    controller: PtyController,
}

impl Running {
    /// Spawns `command` in a fresh 24x80 session.
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
        let reader = thread::Builder::new()
            .name("test-conout-reader".into())
            .spawn(move || {
                let mut sink = Vec::new();
                read_half
                    .read_to_end(&mut sink)
                    .expect("reading to end-of-file must succeed");
                sink
            })
            .expect("spawning the reader thread must succeed");
        Self {
            child,
            reader,
            writer,
            controller,
        }
    }

    /// Waits for the child, then for end-of-file, and returns the rendered
    /// output together with the exit status.
    fn finish(self) -> (String, ExitStatus) {
        let Self {
            mut child,
            reader,
            writer,
            controller,
        } = self;
        let status = child.wait().expect("waiting must succeed");
        // Joining is the real assertion: it returns only once the session
        // reached end-of-file, and since the write half is still open,
        // that end-of-file can only have come from the crate's own
        // shutdown path (a natural release, or the legacy watcher).
        let output = reader.join().expect("the reader thread must not panic");
        drop(writer);
        drop(controller);
        (String::from_utf8_lossy(&output).into_owned(), status)
    }
}

/// Runs `cmd.exe` with `args` to completion in a fresh session.
fn run_cmd(args: &[&str]) -> (String, ExitStatus) {
    Running::start(Command::new("cmd.exe").args(args)).finish()
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

/// `Debug` output ends up in logs and bug reports, so it must show a
/// session's identity — and must not leak raw handle values or the
/// private lifecycle state machine, which would make internals part of
/// the observable surface.
#[test]
fn debug_shows_identity_not_internals() {
    complete_within("debug_shows_identity_not_internals", || {
        let mut pty = pty();
        let rendered = format!("{pty:?}");
        assert!(rendered.starts_with("Pty"), "{rendered}");
        assert!(rendered.contains("size"), "{rendered}");
        assert!(rendered.contains("System"), "{rendered}");
        for leak in ["hpcon", "File", "handle", "state", "released"] {
            assert!(!rendered.contains(leak), "`{leak}` leaked: {rendered}");
        }

        let (read_half, write_half) = pty.split();
        assert_eq!(format!("{read_half:?}"), "ReadHalf { .. }");
        assert_eq!(format!("{write_half:?}"), "WriteHalf { .. }");

        let mut running = Running::start(Command::new("cmd.exe").args(["/c", "exit", "0"]));
        let rendered = format!("{:?}", running.child);
        assert!(rendered.contains("pid"), "{rendered}");
        assert!(!rendered.contains("handle"), "{rendered}");
        running.child.wait().expect("waiting must succeed");
        let rendered = format!("{:?}", running.child);
        assert!(
            rendered.contains("status"),
            "a reaped child must show its cached status: {rendered}"
        );

        assert_eq!(format!("{:?}", running.writer), "OwnedWriteHalf { .. }");
        let rendered = format!("{:?}", running.controller);
        assert!(rendered.starts_with("PtyController"), "{rendered}");
        assert!(rendered.contains("size"), "{rendered}");
        assert!(rendered.contains("supports_clear"), "{rendered}");
        assert!(!rendered.contains("backend_kind"), "{rendered}");
        running.finish();
    });
}

#[test]
fn builder_defaults_to_24_by_80_with_automatic_backend_selection() {
    let pty = pty();
    let automatic = ConPtyBackend::auto().expect("automatic backend selection must remain usable");
    assert_eq!(pty.size(), Size::default());
    assert_eq!(pty.backend_kind(), automatic.kind());
}

#[test]
fn builder_honours_an_explicit_size_and_backend() {
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
#[test]
fn supports_release_matches_the_backend() {
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

#[test]
fn dropping_the_reader_notifies_the_lifecycle_core() {
    let pty = pty();
    let controller = pty.controller();
    let (reader, writer) = pty.into_split();
    assert!(!controller.reader_finished());

    drop(reader);

    assert!(controller.reader_finished());
    drop(writer);
}

#[cfg(not(target_arch = "x86"))]
#[test]
fn managed_session_reports_the_configured_bundle_clear_capability() {
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
        .expect("the managed session must finish")
        .status()
        .success());
}

#[test]
fn resize_updates_the_reported_size() {
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
fn assert_resize_after_session_end_is_not_connected(pty: Pty) {
    let Running {
        mut child,
        reader,
        writer,
        controller,
    } = Running::start_in(pty, Command::new("cmd.exe").args(["/c", "exit", "0"]));
    child.wait().expect("waiting must succeed");
    // End-of-file proves the session is over (and, on a legacy backend,
    // that the watcher has already closed the pseudoconsole).
    reader.join().expect("the reader thread must not panic");

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

/// The system backend exports no `ClearPseudoConsole`, so on an ordinary
/// machine this exercises the typed refusal. On a session backed by a
/// bundled `conpty.dll` the same test proves the call goes through — the
/// assertion is that the capability query and the operation agree.
#[test]
fn clear_agrees_with_the_reported_capability() {
    complete_within("clear_agrees_with_the_reported_capability", || {
        let pty = pty();
        let supported = pty.supports_clear();
        let controller = pty.controller();
        let (_reader, _writer) = pty.into_split();
        assert_eq!(controller.supports_clear(), supported);

        match controller.clear() {
            Ok(()) => assert!(supported, "clear succeeded without a clear export"),
            Err(err) if err.kind() == crate::ErrorKind::UnsupportedFeature => {
                assert!(!supported, "clear refused although the export is present");
                assert!(err.to_string().contains(CLEAR_FEATURE));
            },
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    });
}

#[test]
fn clear_on_a_pty_matches_clear_on_its_controller() {
    complete_within("clear_on_a_pty_matches_clear_on_its_controller", || {
        let pty = pty();
        let from_pty = pty.clear();
        let supported = pty.supports_clear();
        let controller = pty.controller();
        let (_reader, _writer) = pty.into_split();
        let from_controller = controller.clear();
        assert_eq!(from_pty.is_ok(), supported);
        assert_eq!(from_controller.is_ok(), supported);
    });
}

#[test]
fn resize_after_the_session_ends_reports_not_connected() {
    complete_within(
        "resize_after_the_session_ends_reports_not_connected",
        || {
            assert_resize_after_session_end_is_not_connected(pty());
        },
    );
}

#[test]
fn forced_legacy_resize_after_the_session_ends_reports_not_connected() {
    complete_within(
        "forced_legacy_resize_after_the_session_ends_reports_not_connected",
        || {
            assert_resize_after_session_end_is_not_connected(legacy_pty());
        },
    );
}

#[test]
fn a_forced_legacy_session_reaches_end_of_file() {
    complete_within("a_forced_legacy_session_reaches_end_of_file", || {
        const MARKER: &str = "conpty-oxide-forced-legacy-marker";
        let (output, status) = Running::start_in(
            legacy_pty(),
            Command::new("cmd.exe").args(["/c", "echo", MARKER]),
        )
        .finish();
        // `finish` joining the reader is the real assertion: the session
        // was never released, so only the legacy watcher's close can
        // produce the end-of-file the reader thread waits for. A
        // regression in arming the watcher (handle duplication, grace
        // handling, the release/legacy decision) hangs here and is killed
        // by the watchdog instead of passing silently on a 24H2 machine.
        assert!(
            output.contains(MARKER),
            "marker missing from the rendered output: {output:?}"
        );
        assert!(status.success(), "unexpected status: {status}");
    });
}

#[test]
fn echoed_output_reaches_the_reader_and_the_session_ends() {
    complete_within("echoed_output_reaches_the_reader", || {
        const MARKER: &str = "conpty-oxide-blocking-marker";
        let (output, status) = run_cmd(&["/c", "echo", MARKER]);
        assert!(
            output.contains(MARKER),
            "marker missing from the rendered output: {output:?}"
        );
        assert!(status.success(), "unexpected status: {status}");
        assert_eq!(status.code(), 0);
    });
}

#[test]
fn exit_code_is_reported_verbatim() {
    complete_within("exit_code_is_reported_verbatim", || {
        let (_output, status) = run_cmd(&["/c", "exit", "7"]);
        assert_eq!(status.code(), 7);
        assert!(!status.success());
    });
}

#[test]
fn the_environment_reaches_the_child() {
    complete_within("the_environment_reaches_the_child", || {
        const MARKER: &str = "conpty-oxide-blocking-env-9182";
        let (output, _status) = Running::start(
            Command::new("cmd.exe")
                .args(["/c", "echo", "%CONPTY_OXIDE_BLOCKING_MARKER%"])
                .env("CONPTY_OXIDE_BLOCKING_MARKER", MARKER),
        )
        .finish();
        // An unexpanded `%CONPTY_OXIDE_BLOCKING_MARKER%` here would mean
        // the environment block never reached the child.
        assert!(
            output.contains(MARKER),
            "marker missing from the rendered output: {output:?}"
        );
    });
}

#[test]
fn the_working_directory_reaches_the_child() {
    complete_within("the_working_directory_reaches_the_child", || {
        let dir = std::env::temp_dir();
        let (output, status) =
            Running::start(Command::new("cmd.exe").args(["/c", "cd"]).current_dir(&dir)).finish();
        assert!(status.success());
        // `cd` without an argument prints the working directory. Comparing
        // the last component avoids depending on 8.3 short paths; without
        // `current_dir` the child would inherit the test runner's
        // directory, whose name is different.
        let leaf = dir
            .file_name()
            .expect("the temp directory must have a name")
            .to_string_lossy()
            .into_owned();
        assert!(
            output.contains(&leaf),
            "working directory missing from the rendered output: {output:?}"
        );
    });
}

#[test]
fn written_input_reaches_the_child() {
    complete_within("written_input_reaches_the_child", || {
        // An interactive `cmd.exe` only exits when it reads the `exit`
        // command from its console input, so the child terminating with
        // that exact code proves the bytes travelled through conin.
        let mut running = Running::start(&mut Command::new("cmd.exe"));
        running
            .writer
            .write_all(b"exit 3\r\n")
            .expect("writing console input must succeed");
        running
            .writer
            .flush()
            .expect("flush must be a no-op that succeeds");

        let (_output, status) = running.finish();
        assert_eq!(status.code(), 3);
    });
}

#[test]
fn kill_terminates_the_tree_and_reports_a_status() {
    complete_within("kill_terminates_the_tree", || {
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
        let status = running.child.wait().expect("waiting must succeed");
        assert_eq!(status.code(), KILL_EXIT_CODE);
        // A second kill of a dead tree is a documented no-op.
        running
            .child
            .kill()
            .expect("killing a dead tree must succeed");

        let (_output, again) = running.finish();
        assert_eq!(again, status, "the status must be cached, not re-read");
    });
}

#[test]
fn wait_is_repeatable_and_matches_try_wait() {
    complete_within("wait_is_repeatable", || {
        let mut running = Running::start(Command::new("cmd.exe").args(["/c", "exit", "5"]));
        let first = running.child.wait().expect("waiting must succeed");
        assert_eq!(
            running.child.wait().expect("waiting again must succeed"),
            first
        );
        assert_eq!(
            running.child.try_wait().expect("polling must succeed"),
            Some(first)
        );
        assert_eq!(first.code(), 5);
        running.finish();
    });
}

#[test]
fn try_wait_observes_exit_and_populates_the_status_cache() {
    complete_within("try_wait_observes_exit", || {
        let mut running = Running::start(Command::new("cmd.exe").args(["/c", "exit", "37"]));
        let deadline = std::time::Instant::now() + TEST_TIMEOUT;
        let status = loop {
            if let Some(status) = running.child.try_wait().expect("polling must succeed") {
                break status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the short-lived child did not exit within {TEST_TIMEOUT:?}"
            );
            thread::sleep(Duration::from_millis(5));
        };

        assert_eq!(status.code(), 37);
        assert_eq!(
            running
                .child
                .try_wait()
                .expect("polling again must succeed"),
            Some(status)
        );
        let (_output, waited) = running.finish();
        assert_eq!(waited, status, "wait must return try_wait's cached status");
    });
}

#[test]
fn kill_on_drop_terminates_the_tree() {
    complete_within("kill_on_drop_terminates_the_tree", || {
        let running = Running::start(
            Command::new("cmd.exe")
                .args(["/c", "pause"])
                .kill_on_drop(true),
        );

        // An independent handle, so the process can still be observed
        // after the `Child` — and with it the job object — is gone.
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
        reader.join().expect("the reader thread must not panic");
        drop(writer);
        drop(controller);
    });
}

#[test]
fn a_second_spawn_into_the_same_pty_is_rejected() {
    complete_within("a_second_spawn_is_rejected", || {
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

        let (_output, status) = Running::attach(pty, child).finish();
        assert!(status.success());
    });
}

#[test]
fn a_failed_spawn_leaves_the_session_reusable() {
    complete_within("a_failed_spawn_leaves_the_session_reusable", || {
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

        // The failed attempt attached nothing, so the session is still
        // good for a real child.
        let (_output, status) =
            Running::start_in(pty, Command::new("cmd.exe").args(["/c", "exit", "0"])).finish();
        assert!(status.success());
    });
}

#[test]
fn an_unbuildable_command_line_is_rejected() {
    let pty = pty();
    let err = Command::new("cmd.exe")
        .arg("embedded\0nul")
        .spawn_in(&pty)
        .expect_err("an unbuildable command line must fail");
    assert_eq!(err.kind(), crate::ErrorKind::Spawn);
    assert_eq!(
        err.io_error()
            .expect("spawn errors carry an I/O error")
            .kind(),
        io::ErrorKind::InvalidInput
    );
}

#[test]
fn reading_an_empty_buffer_is_not_end_of_file() {
    complete_within("reading_an_empty_buffer_is_not_end_of_file", || {
        let mut pty = pty();
        let (mut reader, _writer) = pty.split();
        assert_eq!(
            reader
                .read(&mut [])
                .expect("a zero-length read must succeed"),
            0
        );
        // A zero-length read must not have reported end-of-file, so the
        // session is still open and still resizable.
        pty.resize(crate::size::test_size(30, 100))
            .expect("the session must still be open");
    });
}

/// The input-side contract the docs state in four places: dropping the
/// owned write half of a live session closes conin, the console host
/// reads that as the terminal being closed, and the child is terminated
/// with `STATUS_CONTROL_C_EXIT` — dropping this half is a way to *stop* a
/// session, not to signal end of input.
#[test]
fn dropping_the_write_half_terminates_the_child() {
    complete_within("dropping_the_write_half_terminates_the_child", || {
        write_half_terminates_the_child_in(pty());
    });
}

#[test]
fn dropping_the_write_half_terminates_a_forced_legacy_child() {
    complete_within(
        "dropping_the_write_half_terminates_a_forced_legacy_child",
        || write_half_terminates_the_child_in(legacy_pty()),
    );
}

fn write_half_terminates_the_child_in(pty: Pty) {
    const MARKER: &str = "conpty-oxide-conin-drop-marker";

    let mut child = Command::new("cmd.exe")
        .spawn_in(&pty)
        .expect("spawning must succeed");
    let _controller = pty.controller();
    let (mut reader, mut writer) = pty.into_split();

    // First prove the child is attached and reading console input: an
    // interactive `cmd.exe` cannot echo this line before it has done both.
    writer
        .write_all(format!("echo {MARKER}\r\n").as_bytes())
        .expect("writing console input must succeed");
    let mut seen = String::new();
    let mut buf = [0u8; 4096];
    while !seen.contains(MARKER) {
        let read = reader.read(&mut buf).expect("reading must succeed");
        assert_ne!(read, 0, "the session ended before the child started");
        seen.push_str(&String::from_utf8_lossy(&buf[..read]));
    }

    drop(writer);

    // Writer retirement closes conin and requests pseudoconsole close. The
    // latter sends CTRL_CLOSE_EVENT portably, including on legacy Windows,
    // while this reader remains available to drain the final output.
    let mut sink = Vec::new();
    reader
        .read_to_end(&mut sink)
        .expect("reading to end-of-file must succeed");
    let status = child.wait().expect("waiting must succeed");
    assert_eq!(
        status.code(),
        STATUS_CONTROL_C_EXIT,
        "a child whose terminal went away must report \
         STATUS_CONTROL_C_EXIT, got: {status}"
    );
}

#[test]
fn a_session_without_the_eof_watcher_still_tears_down() {
    complete_within("a_session_without_the_eof_watcher_still_tears_down", || {
        let pty = Pty::builder()
            .eof_on_root_exit(false)
            .build()
            .expect("building must succeed");
        let mut child = Command::new("cmd.exe")
            .args(["/c", "exit", "0"])
            .spawn_in(&pty)
            .expect("spawning must succeed");
        assert!(child.wait().expect("waiting must succeed").success());

        // Without a watcher a legacy session never reaches end-of-file on
        // its own, so the reader is retired by dropping the session
        // instead. Dropping must not hang, on any backend.
        drop(pty);
    });
}

#[test]
fn the_controller_keeps_an_idle_session_alive() {
    complete_within("the_controller_keeps_an_idle_session_alive", || {
        let second_pty = pty();
        let controller = second_pty.controller();
        let (reader, writer) = second_pty.into_split();
        // Retiring both pipe ends does not end the session: nothing has
        // asked for a close, and the controller still owns the console.
        drop(reader);
        drop(writer);
        controller
            .resize(crate::size::test_size(30, 100))
            .expect("a session with a live controller must still resize");
        assert_eq!(controller.size(), crate::size::test_size(30, 100));
    });
}

#[test]
fn dropping_the_parts_in_any_order_completes() {
    complete_within("dropping_the_parts_in_any_order_completes", || {
        // Controller first, then the write half, then the reader: the
        // pseudoconsole outlives its controller and is closed by the last
        // part standing.
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
    });
}

#[test]
fn managed_output_drains_more_than_pipe_capacity() {
    complete_within("managed_output_drains_more_than_pipe_capacity", || {
        let output = Command::new("cmd.exe")
            .args([
                "/d",
                "/q",
                "/c",
                "for /L %i in (1,1,6000) do @echo managed-output-%i-01234567890123456789",
            ])
            .spawn()
            .expect("managed spawning must succeed")
            .collect_output()
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
    });
}

#[test]
fn managed_output_keeps_input_open_until_the_real_exit() {
    complete_within("managed_output_keeps_input_open", || {
        let output = Command::new("cmd.exe")
            .raw_arg(r#"/d /q /c "ping -n 2 127.0.0.1 >nul & exit 42""#)
            .spawn()
            .expect("managed spawning must succeed")
            .collect_output()
            .expect("managed output must complete");
        assert_eq!(output.status().code(), 42);
    });
}

fn assert_root_bounded_collection(backend: ConPtyBackend) {
    const MARKER: &str = "blocking-root-bounded-tail";

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
        .expect("the root command must reach the session");

    let output = session
        .collect_output()
        .expect("root-bounded collection must finish");
    assert_eq!(output.status().code(), 23);
    assert!(
        String::from_utf8_lossy(output.as_bytes()).contains(MARKER),
        "the root's teardown tail must be preserved"
    );
}

#[test]
fn managed_collection_has_the_same_root_boundary_on_both_lifecycles() {
    complete_within("managed_collection_root_boundary", || {
        let system = ConPtyBackend::system().expect("ConPTY must be available");
        assert_root_bounded_collection(system.without_release());
        assert_root_bounded_collection(system);

        #[cfg(not(target_arch = "x86"))]
        if let Some(dir) = std::env::var_os("CONPTY_OXIDE_TEST_DLL_DIR") {
            let bundle =
                ConPtyBackend::from_dir(dir).expect("the configured standalone backend must load");
            assert!(bundle.supports_release());
            assert_root_bounded_collection(bundle);
        }
    });
}

#[test]
fn command_builder_delegates_every_configuration_category() {
    complete_within(
        "command_builder_delegates_every_configuration_category",
        || {
            let system_root = std::env::var_os("SystemRoot")
                .expect("supported Windows installations define SystemRoot");
            let current =
                std::env::current_dir().expect("reading the current directory must succeed");
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

            let output = command
                .spawn()
                .expect("managed spawning must succeed")
                .collect_output()
                .expect("the fully configured command must complete");
            assert!(output.status().success());
            let text = String::from_utf8_lossy(output.as_bytes());
            assert!(text.contains("one,two,%CONPTY_COV_REMOVED%"), "{text}");
            assert!(
                text.to_ascii_lowercase()
                    .contains(&current.display().to_string().to_ascii_lowercase()),
                "{text}"
            );
        },
    );
}

#[test]
fn low_level_pty_and_borrowed_halves_delegate_io() {
    complete_within("low_level_pty_and_borrowed_halves_delegate_io", || {
        const DIRECT: &str = "blocking-direct-pty-marker";
        const BORROWED: &str = "blocking-borrowed-half-marker";

        let mut direct = pty();
        let direct_controller = direct.controller();
        let mut child = Command::new("cmd.exe")
            .args(["/d", "/q"])
            .spawn_in(&direct)
            .expect("spawning the direct-I/O shell must succeed");
        assert!(!AsHandle::as_handle(&child).as_raw_handle().is_null());
        direct
            .write_all(format!("echo {DIRECT}\r\nexit\r\n").as_bytes())
            .expect("writing through Pty must succeed");
        direct.flush().expect("flushing Pty must succeed");
        let mut output = String::new();
        direct
            .read_to_string(&mut output)
            .expect("reading through Pty must reach EOF");
        assert!(
            direct_controller.reader_finished(),
            "observing EOF must notify the lifecycle core before reader drop"
        );
        assert_eq!(
            direct
                .read(&mut [0])
                .expect("reading Pty again after EOF must succeed"),
            0
        );
        assert!(child.wait().expect("waiting must succeed").success());
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
                .expect("writing through WriteHalf must succeed");
            writer.flush().expect("flushing WriteHalf must succeed");
            reader
                .read_to_string(&mut split_output)
                .expect("reading through ReadHalf must reach EOF");
            assert_eq!(
                reader
                    .read(&mut [0])
                    .expect("reading ReadHalf again after EOF must succeed"),
                0
            );
        }
        assert!(split_child
            .wait()
            .expect("waiting for the split child must succeed")
            .success());
        assert!(split_output.contains(BORROWED), "{split_output}");

        let owned = pty();
        let (reader, writer) = owned.into_split();
        let reader_debug = format!("{reader:?}");
        assert!(reader_debug.starts_with("OwnedReadHalf"), "{reader_debug}");
        let writer_debug = format!("{writer:?}");
        assert!(writer_debug.starts_with("OwnedWriteHalf"), "{writer_debug}");
    });
}

#[test]
fn managed_session_try_wait_reports_a_completed_child() {
    complete_within("managed_session_try_wait_reports_a_completed_child", || {
        const MARKER: &str = "blocking-completed-root-tail";
        let mut session = Command::new("cmd.exe")
            .raw_arg(format!(r#"/d /q /c "echo {MARKER} & exit /b 23""#))
            .spawn()
            .expect("managed spawning must succeed");
        let expected = session
            .child
            .wait()
            .expect("waiting for the managed child must succeed");

        assert_eq!(
            session
                .try_wait()
                .expect("polling the completed managed session must succeed"),
            Some(expected)
        );

        let output = session
            .collect_output()
            .expect("draining the completed managed session must succeed");
        assert_eq!(output.status(), expected);
        assert!(
            String::from_utf8_lossy(output.as_bytes()).contains(MARKER),
            "output buffered after root completion must still be drained"
        );
    });
}

#[test]
fn managed_session_delegates_io_and_debugs_named_parts() {
    complete_within(
        "managed_session_delegates_io_and_debugs_named_parts",
        || {
            const MARKER: &str = "blocking-managed-session-io";
            let mut session = Command::new("cmd.exe")
                .args(["/d", "/q"])
                .spawn()
                .expect("managed spawning must succeed");
            let session_debug = format!("{session:?}");
            assert!(session_debug.starts_with("Session"), "{session_debug}");

            session
                .write_all(format!("echo {MARKER}\r\nexit\r\n").as_bytes())
                .expect("writing through Session must succeed");
            session.flush().expect("flushing Session must succeed");
            let mut output = String::new();
            session
                .read_to_string(&mut output)
                .expect("reading through Session must reach EOF");
            assert!(output.contains(MARKER), "{output}");
            // Conout EOF and the root process handle becoming signaled are
            // independent kernel events. Released ConPTY can expose EOF a
            // scheduling instant first, so `try_wait` may still return None;
            // the blocking wait through SessionParts below proves completion.
            if let Some(status) = session
                .try_wait()
                .expect("polling the managed session must succeed")
            {
                assert!(status.success());
            }

            let mut parts = session.into_parts();
            let parts_debug = format!("{parts:?}");
            assert!(parts_debug.starts_with("SessionParts"), "{parts_debug}");
            assert!(parts
                .child
                .wait()
                .expect("waiting through SessionParts must succeed")
                .success());
        },
    );
}

#[test]
fn dropping_a_managed_session_kills_its_tree() {
    complete_within("dropping_a_managed_session_kills_its_tree", || {
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
        assert!(watched.try_wait().expect("polling must succeed").is_none());
        drop(session);
        assert_eq!(
            watched.wait().expect("waiting must succeed"),
            KILL_EXIT_CODE
        );
    });
}

#[test]
fn dropping_the_child_from_managed_parts_kills_its_tree() {
    complete_within("dropping_managed_parts_child_kills_tree", || {
        let parts = Command::new("cmd.exe")
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
        drop(parts.child);
        assert_eq!(
            watched.wait().expect("waiting must succeed"),
            KILL_EXIT_CODE
        );
        drop(parts.output);
        drop(parts.input);
        drop(parts.controller);
    });
}
