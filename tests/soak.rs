// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Scheduled lifecycle soak covering the failure modes that need repetition.

#![cfg(all(windows, feature = "blocking", feature = "tokio"))]

pub mod helpers;

use std::io;
use std::thread;
use std::time::{Duration, Instant};

use conpty_oxide::blocking::Command;
use conpty_oxide::{ErrorKind, Size};
use helpers::sync::Session;
use helpers::{process_is_running, wait_until, watchdog};
use tokio::runtime::Runtime;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};

const ITERATIONS: usize = 64;
const WARM_UP_ITERATIONS: usize = 8;
const HANDLE_GROWTH_LIMIT: u32 = 8;
const SEED: u64 = 0x434f_4e50_5459_2026;
const TEST_BUDGET: Duration = Duration::from_secs(20 * 60);
const APPEAR: Duration = Duration::from_secs(10);
const VANISH: Duration = Duration::from_secs(10);

const ROOT_EXE: &str = "cmd.exe";
const GRANDCHILD_EXE: &str = "ping.exe";
const NEVER_ENDING: [&str; 4] = ["/c", "ping", "-t", "127.0.0.1"];
const FLOOD: &str = "/c for /l %i in (1,1,4000) do @echo \
    0123456789012345678901234567890123456789012345678901234567890123456789";

#[derive(Clone, Copy)]
struct RecordedProcess {
    pid: u32,
    executable: &'static str,
}

/// Repeats the dangerous lifecycle interleavings in one process so handle
/// growth is observable instead of being hidden by test-process exit.
#[test]
fn scheduled_lifecycle_soak() {
    if std::env::var_os("CONPTY_OXIDE_RUN_SOAK").is_none() {
        return;
    }
    let _watchdog = watchdog(TEST_BUDGET);
    let runtime = Runtime::new().expect("creating the soak Tokio runtime must succeed");
    let mut seed = SEED;
    let mut recorded = Vec::new();
    let mut warmed_handles = None;

    for iteration in 0..ITERATIONS {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        match iteration % 5 {
            0 => large_output_drop(&mut recorded),
            1 => resize_close_race(&mut recorded, seed),
            2 => managed_drop_order(&mut recorded, seed),
            3 => tokio_cancellation_kills_tree(&runtime, &mut recorded),
            4 => eof_teardown(&mut recorded),
            _ => unreachable!(),
        }

        if iteration + 1 == WARM_UP_ITERATIONS {
            settle();
            warmed_handles = Some(process_handle_count());
        }
    }

    settle();
    let warmed = warmed_handles.expect("the warm-up iteration must be reached");
    let final_count = process_handle_count();
    assert!(
        final_count <= warmed + HANDLE_GROWTH_LIMIT,
        "process handles grew from {warmed} after warm-up to {final_count}; \
         the allowed increase is {HANDLE_GROWTH_LIMIT}"
    );

    for process in recorded {
        assert!(
            !process_is_running(process.pid, process.executable),
            "recorded {} process {} survived the soak",
            process.executable,
            process.pid
        );
    }
}

fn large_output_drop(recorded: &mut Vec<RecordedProcess>) {
    let mut session = Command::new(ROOT_EXE)
        .raw_arg(FLOOD)
        .spawn()
        .expect("large-output spawning must succeed");
    let root = record(recorded, session.id(), ROOT_EXE);
    thread::sleep(Duration::from_millis(250));
    assert!(
        session
            .try_wait()
            .expect("polling the flood child must succeed")
            .is_none(),
        "the flood did not fill the unread ConPTY output path"
    );
    drop(session);
    assert_gone(root);
}

fn resize_close_race(recorded: &mut Vec<RecordedProcess>, seed: u64) {
    let session =
        Session::start(Command::new(ROOT_EXE).args(["/d", "/c", "ping", "-n", "2", "127.0.0.1"]));
    let Session {
        mut child,
        output,
        writer,
        controller,
    } = session;
    let root = record(recorded, child.id(), ROOT_EXE);
    let row_offset = u16::try_from(seed % 3).expect("row offset fits u16");
    let column_offset = u16::try_from(seed % 5).expect("column offset fits u16");
    let first = size(24 + row_offset, 80 + column_offset);
    let second = size(first.rows() + 1, first.cols() + 1);

    let racer = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut next = first;
        loop {
            match controller.resize(next) {
                Ok(()) => next = if next == first { second } else { first },
                Err(err) if err.kind() == ErrorKind::Resize => {
                    return err
                        .io_error()
                        .expect("resize errors must retain their I/O source")
                        .kind();
                },
                Err(other) => panic!("unexpected resize failure during close: {other}"),
            }
            assert!(
                Instant::now() < deadline,
                "resize never observed the closing pseudoconsole"
            );
            thread::sleep(Duration::from_millis(2));
        }
    });

    assert!(
        child
            .wait()
            .expect("waiting for the resize probe must succeed")
            .success(),
        "the resize probe exited unsuccessfully"
    );
    output.join();
    assert_eq!(
        racer.join().expect("the resize racer must not panic"),
        io::ErrorKind::NotConnected
    );
    drop(writer);
    assert_gone(root);
}

fn managed_drop_order(recorded: &mut Vec<RecordedProcess>, seed: u64) {
    let parts = Command::new(ROOT_EXE)
        .args(["/d", "/c", "pause"])
        .spawn()
        .expect("managed spawning must succeed")
        .into_parts();
    let root = record(recorded, parts.child.id(), ROOT_EXE);
    let mut child = Some(parts.child);
    let mut output = Some(parts.output);
    let mut input = Some(parts.input);
    let mut controller = Some(parts.controller);
    let start = usize::try_from(seed % 4).expect("drop-order offset fits usize");

    for offset in 0..4 {
        match (start + offset) % 4 {
            0 => drop(child.take()),
            1 => drop(output.take()),
            2 => drop(input.take()),
            3 => drop(controller.take()),
            _ => unreachable!(),
        }
    }
    assert_gone(root);
}

fn tokio_cancellation_kills_tree(runtime: &Runtime, recorded: &mut Vec<RecordedProcess>) {
    runtime.block_on(async {
        let session = conpty_oxide::tokio::Command::new(ROOT_EXE)
            .args(NEVER_ENDING)
            .spawn()
            .expect("Tokio managed spawning must succeed");
        let root = record(recorded, session.id(), ROOT_EXE);
        let grandchild_pid =
            helpers::tokio_support::wait_for_descendant(root.pid, GRANDCHILD_EXE, APPEAR).await;
        let grandchild = record(recorded, grandchild_pid, GRANDCHILD_EXE);

        assert!(
            tokio::time::timeout(Duration::from_millis(50), session.collect_output())
                .await
                .is_err(),
            "the never-ending tree must still be collecting when cancelled"
        );
        assert_gone_async(root).await;
        assert_gone_async(grandchild).await;
    });
}

fn eof_teardown(recorded: &mut Vec<RecordedProcess>) {
    let session =
        Session::start(Command::new(ROOT_EXE).args(["/d", "/c", "echo", "conpty-oxide-soak-eof"]));
    let root = record(recorded, session.child.id(), ROOT_EXE);
    let (output, status) = session.finish();
    assert!(status.success(), "EOF probe failed with {status}");
    assert!(
        output.contains("conpty-oxide-soak-eof"),
        "EOF probe output was truncated"
    );
    assert_gone(root);
}

fn size(rows: u16, cols: u16) -> Size {
    Size::try_new(cols, rows).expect("soak dimensions must be valid")
}

fn record(
    recorded: &mut Vec<RecordedProcess>,
    pid: u32,
    executable: &'static str,
) -> RecordedProcess {
    assert_ne!(pid, 0, "a spawned process must have a PID");
    let process = RecordedProcess { pid, executable };
    recorded.push(process);
    process
}

fn assert_gone(process: RecordedProcess) {
    assert!(
        wait_until(VANISH, || !process_is_running(
            process.pid,
            process.executable
        )),
        "{} process {} survived teardown",
        process.executable,
        process.pid
    );
}

async fn assert_gone_async(process: RecordedProcess) {
    assert!(
        helpers::tokio_support::poll_until(VANISH, || {
            !process_is_running(process.pid, process.executable)
        })
        .await,
        "{} process {} survived teardown",
        process.executable,
        process.pid
    );
}

fn settle() {
    thread::sleep(Duration::from_millis(250));
}

fn process_handle_count() -> u32 {
    let mut count = 0;
    // SAFETY: GetCurrentProcess returns the current process's immortal pseudo
    // handle and `count` is a valid out-parameter for the duration of the call.
    let succeeded = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) };
    assert_ne!(
        succeeded,
        0,
        "GetProcessHandleCount failed: {}",
        io::Error::last_os_error()
    );
    count
}
