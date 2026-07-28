//! Teardown must never hang, in any drop order, with the pipe buffer full.
//!
//! This is the crate's headline regression, restated for the async front end.
//! `ClosePseudoConsole` is documented to block indefinitely unless the output
//! pipe is closed first or kept drained, so the dangerous state is exactly the
//! one a naive caller reaches: a child flooding the console, nobody reading,
//! and the session being dropped. A library that gets this wrong does not
//! return an error — it stops.
//!
//! Async raises the stakes. The destructors run on a runtime thread, and a
//! blocked destructor there takes the runtime's timer with it, so
//! `tokio::time::timeout` cannot report the failure: only the process-killing
//! watchdog can. Two arrangements go beyond the drop-order matrix: the
//! no-intervening-await cases drop the session while the I/O driver has never
//! polled since the spawn (so the conout OS handle is still pinned by an
//! in-flight overlapped read — the interleaving where async teardown differs
//! from blocking teardown), and the last test drops the *runtime* with a
//! live, flooding session still parked in a task, which is where the
//! destructors run on Tokio's shutdown path rather than on an ordinary poll.

#![cfg(all(windows, feature = "tokio"))]

mod helpers;

use std::time::{Duration, Instant};

use conpty_oxide::{Command, Pty};

use helpers::asyn::{legacy_pty, pty, within};
use helpers::watchdog;

/// Roughly 280 KiB of console output, well past any pipe buffer.
///
/// `raw_arg` is required here: `cmd.exe` parses its own command line, so the
/// loop must reach it verbatim rather than quoted as a single argument.
const FLOOD: &str = "/c for /l %i in (1,1,4000) do @echo \
     0123456789012345678901234567890123456789012345678901234567890123456789";

/// Outer guard. Only a genuine deadlock gets anywhere near this.
const BUDGET: Duration = Duration::from_secs(20);

/// Per-test budget for everything except the runtime-shutdown case.
const DEADLINE: Duration = Duration::from_secs(15);

/// How long the child is left flooding before the session is destroyed.
const FILL: Duration = Duration::from_millis(500);

/// What "prompt" means for the teardown itself.
const TEARDOWN_BUDGET: Duration = Duration::from_secs(5);

/// Runs one drop-order case: flood the session built by `build`, read nothing,
/// then hand the `Pty` to `teardown` and require it to finish quickly.
///
/// Every case runs twice — once on the machine's natural lifecycle mode and
/// once on a forced-legacy session — because the two modes tear down through
/// different code paths (a released session's close never blocks; a legacy one
/// relies on the drop order having retired the reader first).
async fn drop_order_completes(build: impl FnOnce() -> Pty, teardown: impl FnOnce(Pty)) {
    let pty = build();
    // `kill_on_drop` is cleanup, not part of the scenario: the child is
    // dropped only after the session is gone, so the tree cannot outlive the
    // test even though the pseudoconsole it was attached to is dead.
    let mut child = Command::new("cmd.exe")
        .raw_arg(FLOOD)
        .kill_on_drop(true)
        .spawn(&pty)
        .expect("spawning must succeed");

    // Give the console host time to fill the pipe, and confirm that it did: a
    // child that already exited would mean the output fit after all, and the
    // teardown below would be testing nothing.
    tokio::time::sleep(FILL).await;
    assert!(
        child.try_wait().expect("polling must succeed").is_none(),
        "the child finished despite nobody reading its output, so the pipe \
         buffer never filled and this case is not exercising a blocked \
         console host"
    );

    let started = Instant::now();
    teardown(pty);
    let elapsed = started.elapsed();
    assert!(
        elapsed < TEARDOWN_BUDGET,
        "tearing the session down took {elapsed:?}, over the \
         {TEARDOWN_BUDGET:?} budget"
    );

    drop(child);
}

/// [`drop_order_completes`] under the recoverable timeout.
async fn case(name: &str, build: impl FnOnce() -> Pty, teardown: impl FnOnce(Pty)) {
    within(name, DEADLINE, drop_order_completes(build, teardown)).await;
}

/// [`drop_order_completes`] with **no await between the spawn and the drop**.
///
/// Every case above parks the runtime in `tokio::time::sleep` while the pipe
/// fills, which lets the I/O driver retire the overlapped conout read that
/// mio schedules eagerly at registration; the later drop then closes the
/// conout OS handle synchronously. This variant blocks the thread with
/// `std::thread::sleep` instead, so the driver never runs between building
/// the session and tearing it down, and the drop happens with that read still
/// in flight — the one interleaving where dropping a Tokio pipe does *not*
/// close the OS handle before returning. A teardown that treats "reader
/// dropped" as "read end gone at the OS level" runs `ClosePseudoConsole`
/// against a still-open, undrained conout pipe; on hosts before Windows 11
/// 24H2 that close blocks until the read end disappears, which on this
/// current-thread runtime only the now-blocked thread could make happen — a
/// permanent deadlock. On 24H2 and later `ClosePseudoConsole` returns
/// promptly even with the read end open, so on modern machines this case
/// exercises the interleaving without being able to prove the deadlock
/// absent: its failure mode is only observable on pre-24H2 CI.
///
/// No `within` wrapper here — the runtime's timer is deliberately starved, so
/// only the process watchdog and the elapsed-time check can report a failure.
async fn drop_order_completes_unpolled(build: impl FnOnce() -> Pty, teardown: impl FnOnce(Pty)) {
    let pty = build();
    let mut child = Command::new("cmd.exe")
        .raw_arg(FLOOD)
        .kill_on_drop(true)
        .spawn(&pty)
        .expect("spawning must succeed");

    // Starve the runtime — I/O driver included — while the console host
    // fills the pipe behind the parked read.
    std::thread::sleep(FILL);
    assert!(
        child.try_wait().expect("polling must succeed").is_none(),
        "the child finished despite nobody reading its output, so the pipe \
         buffer never filled and this case is not exercising a blocked \
         console host"
    );

    let started = Instant::now();
    teardown(pty);
    let elapsed = started.elapsed();
    assert!(
        elapsed < TEARDOWN_BUDGET,
        "tearing the session down took {elapsed:?}, over the \
         {TEARDOWN_BUDGET:?} budget"
    );

    drop(child);
}

/// Drop order: read half, write half, controller.
fn read_half_first(pty: Pty) {
    let (reader, writer, controller) = pty.into_split();
    drop(reader);
    drop(writer);
    drop(controller);
}

/// Drop order: write half, controller, read half.
fn write_half_first(pty: Pty) {
    let (reader, writer, controller) = pty.into_split();
    drop(writer);
    drop(controller);
    drop(reader);
}

/// Drop order: controller, write half, read half.
///
/// The awkward order: the controller owns the pseudoconsole, so this drops it
/// while a live read half is still registered and the console host is blocked
/// writing.
fn controller_first(pty: Pty) {
    let (reader, writer, controller) = pty.into_split();
    drop(controller);
    drop(writer);
    drop(reader);
}

#[tokio::test]
async fn dropping_the_whole_session_completes() {
    let _watchdog = watchdog(BUDGET);
    case("dropping_the_whole_session_completes", pty, drop).await;
}

#[tokio::test]
async fn dropping_the_read_half_first_completes() {
    let _watchdog = watchdog(BUDGET);
    case(
        "dropping_the_read_half_first_completes",
        pty,
        read_half_first,
    )
    .await;
}

#[tokio::test]
async fn dropping_the_write_half_first_completes() {
    let _watchdog = watchdog(BUDGET);
    case(
        "dropping_the_write_half_first_completes",
        pty,
        write_half_first,
    )
    .await;
}

#[tokio::test]
async fn dropping_the_controller_first_completes() {
    let _watchdog = watchdog(BUDGET);
    case(
        "dropping_the_controller_first_completes",
        pty,
        controller_first,
    )
    .await;
}

#[tokio::test]
async fn dropping_the_whole_forced_legacy_session_completes() {
    let _watchdog = watchdog(BUDGET);
    case(
        "dropping_the_whole_forced_legacy_session_completes",
        legacy_pty,
        drop,
    )
    .await;
}

#[tokio::test]
async fn dropping_the_read_half_of_a_forced_legacy_session_first_completes() {
    let _watchdog = watchdog(BUDGET);
    case(
        "dropping_the_read_half_of_a_forced_legacy_session_first_completes",
        legacy_pty,
        read_half_first,
    )
    .await;
}

#[tokio::test]
async fn dropping_the_write_half_of_a_forced_legacy_session_first_completes() {
    let _watchdog = watchdog(BUDGET);
    case(
        "dropping_the_write_half_of_a_forced_legacy_session_first_completes",
        legacy_pty,
        write_half_first,
    )
    .await;
}

#[tokio::test]
async fn dropping_the_controller_of_a_forced_legacy_session_first_completes() {
    let _watchdog = watchdog(BUDGET);
    case(
        "dropping_the_controller_of_a_forced_legacy_session_first_completes",
        legacy_pty,
        controller_first,
    )
    .await;
}

#[tokio::test]
async fn dropping_the_session_with_no_intervening_await_completes() {
    let _watchdog = watchdog(BUDGET);
    drop_order_completes_unpolled(pty, drop).await;
}

#[tokio::test]
async fn dropping_the_forced_legacy_session_with_no_intervening_await_completes() {
    let _watchdog = watchdog(BUDGET);
    drop_order_completes_unpolled(legacy_pty, drop).await;
}

/// Dropping the runtime with a live, flooding session still parked in a task.
///
/// The session's destructors run on Tokio's shutdown path here, not on an
/// ordinary poll: the runtime drops the task, which drops the `Pty`, which
/// closes the pipes and the pseudoconsole while the console host is blocked
/// writing into a full buffer. This is also the one arrangement where a
/// blocked destructor cannot be reported by anything inside the runtime, which
/// is why the whole test is written around the process watchdog and an elapsed
/// time check rather than around a timeout future.
#[test]
fn dropping_the_runtime_with_a_live_session_completes() {
    let _watchdog = watchdog(BUDGET);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("building a runtime must succeed");

    runtime.block_on(async {
        let pty = pty();
        let child = Command::new("cmd.exe")
            .raw_arg(FLOOD)
            .kill_on_drop(true)
            .spawn(&pty)
            .expect("spawning must succeed");

        // Parked forever: the only thing that will ever retire this session is
        // the runtime shutdown below.
        tokio::spawn(async move {
            let _session = pty;
            let _child = child;
            std::future::pending::<()>().await;
        });

        // Let the task reach its parking point and the console host fill the
        // pipe buffer behind it.
        tokio::time::sleep(FILL).await;
    });

    let started = Instant::now();
    drop(runtime);
    let elapsed = started.elapsed();
    assert!(
        elapsed < TEARDOWN_BUDGET,
        "shutting the runtime down took {elapsed:?}, over the \
         {TEARDOWN_BUDGET:?} budget"
    );
}
