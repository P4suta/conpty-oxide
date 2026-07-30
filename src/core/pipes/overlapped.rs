// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Overlapped named pipes used by the Tokio frontend.
//!
//! Each stream is a single-instance named-pipe pair. The server end is opened
//! with
//!   `FILE_FLAG_OVERLAPPED` so tokio can register it with its
//!   I/O-completion-port reactor; the client end is opened synchronously and
//!   handed to `CreatePseudoConsole`, meeting the same requirement. The split
//!   also keeps older console hosts working: only recent OpenConsole builds
//!   understand overlapped pipe handles, while every host performs correct
//!   synchronous I/O on the synchronous client ends it receives here.
//!
//! Every handle is created **non-inheritable**. `ConPTY` does not rely on
//! handle inheritance: `CreatePseudoConsole` duplicates the two handles it is
//! given into the console host itself, and the child process is launched with
//! `bInheritHandles = FALSE` and the `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`
//! attribute. Keeping the handles non-inheritable means an unrelated
//! `CreateProcess` call elsewhere in the process cannot leak them and hold a
//! pipe open, which would defeat EOF detection on conout.

use std::io;
use std::iter;
use std::mem::{forget, size_of, zeroed};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_IO_PENDING, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ,
    GENERIC_WRITE, INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING,
    PIPE_ACCESS_INBOUND, PIPE_ACCESS_OUTBOUND,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows_sys::Win32::System::IO::{CancelIo, GetOverlappedResult, OVERLAPPED};

/// Buffer size, in bytes, of each direction of an overlapped named pipe.
///
/// Generous enough that a burst of terminal output does not immediately
/// back-pressure the console host, small enough not to matter.
const PIPE_BUFFER_SIZE: u32 = 128 * 1024;

/// How many pipe names to try before giving up on a colliding name.
///
/// Collisions cannot happen accidentally (see [`unique_pipe_name`]); this
/// bounds the damage a deliberate name squatter can do to "session creation
/// fails" instead of looping forever.
const PIPE_NAME_ATTEMPTS: u32 = 4;

/// How long to wait for a pending `ConnectNamedPipe` to finish.
///
/// The client end is opened *before* the connect is issued, so the connect is
/// expected to complete instantly; this timeout only bounds a defensive path
/// that has never been observed to run.
const CONNECT_TIMEOUT_MS: u32 = 10_000;

/// How long to wait for a *cancelled* connect to acknowledge cancellation
/// before its buffers are leaked instead of freed.
const CANCEL_DRAIN_TIMEOUT_MS: u32 = 5_000;

/// The four ends of one pseudoconsole session's named-pipe pair, with both
/// connections already established.
///
/// Each stream is a single-instance named pipe. The `*_server` ends are ours
/// and are opened for overlapped I/O — what tokio's
/// `NamedPipeServer::from_raw_handle` demands for I/O-completion-port
/// registration. The `*_client` ends are synchronous — what
/// `CreatePseudoConsole` demands of `hInput` and `hOutput` — and must be
/// closed by us as soon as the console has been created: `ConPTY` keeps its own
/// duplicates, and dropping ours is what lets the server ends observe
/// end-of-file once the console host is gone.
#[derive(Debug)]
pub(crate) struct OverlappedPipes {
    /// Overlapped server end of conout. The async front end reads
    /// pseudoconsole output from this.
    pub(crate) conout_server: OwnedHandle,
    /// Synchronous client end of conout. Passed to `CreatePseudoConsole` as
    /// `hOutput`, then closed by us.
    pub(crate) conout_client: OwnedHandle,
    /// Overlapped server end of conin. The async front end writes user input
    /// into this. Closing it does not merely signal end-of-input: the console
    /// host treats a vanishing conin writer as "the terminal is gone" and
    /// tears the whole session down.
    pub(crate) conin_server: OwnedHandle,
    /// Synchronous client end of conin. Passed to `CreatePseudoConsole` as
    /// `hInput`, then closed by us.
    pub(crate) conin_client: OwnedHandle,
}

/// Creates the conout and conin named-pipe pairs for one pseudoconsole
/// session, with both connections fully established.
///
/// Each direction is one named pipe under a name unique to this process and
/// call (`\\.\pipe\conpty-oxide-{pid}-{seq}-{tick}`): an overlapped server
/// end that we keep, and a synchronous client end destined for
/// `CreatePseudoConsole`. By the time this function returns `Ok`,
/// `ConnectNamedPipe` has confirmed both connections — the caller never
/// waits for, or races against, a connection.
///
/// # Errors
///
/// A name collision — only possible if someone deliberately squats our names,
/// see [`create_pipe_server`] — is retried under fresh names a few times
/// before the error is returned. Any other `CreateNamedPipeW`, `CreateFileW`,
/// or `ConnectNamedPipe` failure is returned as-is. On every failure path the
/// handles created so far are closed by their [`OwnedHandle`] destructors.
pub(crate) fn create_overlapped_pipes() -> io::Result<OverlappedPipes> {
    let (conout_server, conout_client) = create_connected_pair(ServerDirection::Inbound)?;
    let (conin_server, conin_client) = create_connected_pair(ServerDirection::Outbound)?;

    Ok(OverlappedPipes {
        conout_server,
        conout_client,
        conin_server,
        conin_client,
    })
}

/// Which way bytes flow through one named pipe, from the server's viewpoint.
#[derive(Clone, Copy)]
enum ServerDirection {
    /// The server reads and the client writes. Used for conout, where the
    /// client end becomes the pseudoconsole's `hOutput`.
    Inbound,
    /// The server writes and the client reads. Used for conin, where the
    /// client end becomes the pseudoconsole's `hInput`.
    Outbound,
}

impl ServerDirection {
    /// `dwOpenMode` for `CreateNamedPipeW`: the direction plus the two flags
    /// every server end needs (overlapped mode, squat detection).
    const fn server_open_mode(self) -> u32 {
        let access = match self {
            Self::Inbound => PIPE_ACCESS_INBOUND,
            Self::Outbound => PIPE_ACCESS_OUTBOUND,
        };
        access | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE
    }

    /// `dwDesiredAccess` for the client's `CreateFileW`: the mirror image of
    /// the server's direction.
    const fn client_desired_access(self) -> u32 {
        match self {
            Self::Inbound => GENERIC_WRITE,
            Self::Outbound => GENERIC_READ,
        }
    }
}

/// Creates one named pipe and returns its connected `(server, client)` ends.
///
/// The order of operations is: create the server end, open the client end,
/// then run `ConnectNamedPipe` purely to *confirm* the connection that the
/// client's open already established. Opening the client first is what makes
/// the confirmation non-blocking; see [`confirm_client_connected`] for how
/// the confirmation handles each documented outcome.
fn create_connected_pair(direction: ServerDirection) -> io::Result<(OwnedHandle, OwnedHandle)> {
    let (name, server) = retry_name_collisions(|| {
        let name = unique_pipe_name();
        create_pipe_server(&name, direction).map(|server| (name, server))
    })?;
    // Failures from here on drop `server` (and then `client`), closing
    // them; a half-built pair never leaks.
    let client = open_pipe_client(&name, direction)?;
    confirm_client_connected(&server)?;
    Ok((server, client))
}

/// Retries only name-collision failures, under a fresh name each time.
fn retry_name_collisions<T>(mut operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut attempts = 0;
    loop {
        attempts = next_attempt(attempts);
        match operation() {
            Ok(value) => return Ok(value),
            Err(err) if !should_retry_name_collision(attempts, &err) => return Err(err),
            Err(_) => {},
        }
    }
}

const fn next_attempt(attempts: u32) -> u32 {
    attempts + 1
}

fn should_retry_name_collision(attempts: u32, err: &io::Error) -> bool {
    attempts < PIPE_NAME_ATTEMPTS && is_pipe_name_collision(err)
}

/// Monotonic per-process counter distinguishing pipe names within a process.
static NEXT_PIPE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Returns a pipe name of the form `\\.\pipe\conpty-oxide-{pid}-{seq}-{tick}`.
///
/// The process ID separates concurrently running processes and the counter
/// separates calls within this process, which together already rule out
/// accidental collisions: a named pipe ceases to exist when its last handle
/// closes, so a dead process with our recycled PID cannot have left its names
/// behind. The boot-relative tick count additionally varies the names across
/// runs. Deliberate squatting is not preventable by naming — it is *detected*
/// instead, by `FILE_FLAG_FIRST_PIPE_INSTANCE` in [`create_pipe_server`].
fn unique_pipe_name() -> String {
    let pid = std::process::id();
    let seq = NEXT_PIPE_SEQ.fetch_add(1, Ordering::Relaxed);
    // SAFETY: `GetTickCount64` has no preconditions.
    let tick = unsafe { GetTickCount64() };
    format!(r"\\.\pipe\conpty-oxide-{pid}-{seq}-{tick}")
}

/// Encodes `s` as a NUL-terminated UTF-16 string.
fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(iter::once(0)).collect()
}

/// Security attributes that make the new handle non-inheritable.
///
/// A NULL `SECURITY_ATTRIBUTES` pointer would behave the same, but spelling
/// the attributes out documents the intent at each call site instead of
/// relying on a default.
fn non_inheritable_attributes() -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(u32::MAX),
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 0, // FALSE
    }
}

/// Byte-stream mode plus the local-only policy shared by both directions.
const fn pipe_mode() -> u32 {
    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS
}

/// Creates the overlapped server end of the named pipe `name`.
///
/// The pipe is byte-typed, byte-read, blocking-wait, local-only
/// (`PIPE_REJECT_REMOTE_CLIENTS`), limited to a single instance, and buffers
/// [`PIPE_BUFFER_SIZE`] bytes in each direction. `FILE_FLAG_FIRST_PIPE_INSTANCE`
/// makes name squatting detectable: if the name already exists, this call
/// fails (with `ERROR_ACCESS_DENIED`) instead of silently attaching a second
/// instance to a stranger's pipe.
fn create_pipe_server(name: &str, direction: ServerDirection) -> io::Result<OwnedHandle> {
    let wide_name = to_wide_null(name);
    let attributes = non_inheritable_attributes();

    // SAFETY: `wide_name` is NUL-terminated and, like `attributes`, outlives
    // the call. `nDefaultTimeOut = 0` only affects `WaitNamedPipe`, which is
    // never used on this pipe.
    let handle = unsafe {
        CreateNamedPipeW(
            wide_name.as_ptr(),
            direction.server_open_mode(),
            pipe_mode(),
            1, // nMaxInstances: exactly one client, ever.
            PIPE_BUFFER_SIZE,
            PIPE_BUFFER_SIZE,
            0,
            &attributes,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: `CreateNamedPipeW` reported success, so `handle` is a valid,
    // open handle this process exclusively owns. `OwnedHandle` closes it
    // exactly once.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// Returns whether `err` from `CreateNamedPipeW` means the generated name is
/// already taken, so a retry under a fresh name is worthwhile.
///
/// `FILE_FLAG_FIRST_PIPE_INSTANCE` turns a squatted name into
/// `ERROR_ACCESS_DENIED`; `ERROR_PIPE_BUSY` is the instance-limit flavour of
/// the same situation.
fn is_pipe_name_collision(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error().and_then(|code| u32::try_from(code).ok()),
        Some(ERROR_ACCESS_DENIED | ERROR_PIPE_BUSY)
    )
}

/// Opens the synchronous client end of the named pipe `name`.
///
/// `dwFlagsAndAttributes` is zero — in particular *not*
/// `FILE_FLAG_OVERLAPPED` — so the handle performs synchronous I/O, which is
/// what `CreatePseudoConsole` requires of the handles it is given.
fn open_pipe_client(name: &str, direction: ServerDirection) -> io::Result<OwnedHandle> {
    let wide_name = to_wide_null(name);
    let attributes = non_inheritable_attributes();

    // SAFETY: `wide_name` is NUL-terminated and, like `attributes`, outlives
    // the call.
    let handle = unsafe {
        CreateFileW(
            wide_name.as_ptr(),
            direction.client_desired_access(),
            0, // dwShareMode: pipes are point-to-point, nothing to share.
            &attributes,
            OPEN_EXISTING,
            0, // dwFlagsAndAttributes: synchronous, no attributes.
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: `CreateFileW` reported success, so `handle` is a valid, open
    // handle this process exclusively owns. `OwnedHandle` closes it exactly
    // once.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// Creates an unnamed, non-inheritable, manual-reset event, initially
/// non-signalled.
///
/// `ConnectNamedPipe`'s documentation requires specifically a *manual-reset*
/// event in the `OVERLAPPED` structure passed to it.
fn create_manual_reset_event() -> io::Result<OwnedHandle> {
    // SAFETY: all pointer arguments are permitted to be NULL: default
    // security (non-inheritable), no name.
    let handle = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: `CreateEventW` reported success, so `handle` is a valid, open
    // handle this process exclusively owns. `OwnedHandle` closes it exactly
    // once.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// Returns a zeroed `OVERLAPPED` carrying `event` as its completion event.
fn overlapped_with(event: &OwnedHandle) -> OVERLAPPED {
    // SAFETY: `OVERLAPPED` is a plain C struct for which all-zeroes is the
    // valid idle state.
    let mut overlapped: OVERLAPPED = unsafe { zeroed() };
    overlapped.hEvent = event.as_raw_handle();
    overlapped
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConnectDisposition {
    Complete,
    Pending,
    Error,
}

const fn classify_connect_result(connected: i32, error: Option<u32>) -> ConnectDisposition {
    if connected != 0 || matches!(error, Some(ERROR_PIPE_CONNECTED)) {
        ConnectDisposition::Complete
    } else if matches!(error, Some(ERROR_IO_PENDING)) {
        ConnectDisposition::Pending
    } else {
        ConnectDisposition::Error
    }
}

/// Confirms that the client end just opened by [`open_pipe_client`] is
/// connected to `server`, returning only once the connection is established.
///
/// `server` was opened with `FILE_FLAG_OVERLAPPED`, and `ConnectNamedPipe` on
/// such a handle must be given an `OVERLAPPED` structure — with a NULL
/// `lpOverlapped` "the function can incorrectly report that the connect
/// operation is complete" — carrying a manual-reset event. The documented
/// outcomes map as follows:
///
/// - `FALSE` + `ERROR_PIPE_CONNECTED`: the client connected between
///   `CreateNamedPipeW` and this call. Because the client end is deliberately
///   opened first, this is the *expected* outcome, and the documentation
///   defines it as "a good connection between client and server, even though
///   the function returns zero".
/// - `FALSE` + `ERROR_IO_PENDING`: the connect was queued. Handled anyway —
///   by waiting on the event with a timeout — so this function's contract
///   holds even if a console filter driver reorders the client's open.
/// - `TRUE`: not expected from an overlapped handle, but it means the
///   connection completed, so it is treated as success.
/// - `FALSE` + anything else (e.g. `ERROR_NO_DATA`, meaning some client
///   connected and already disconnected): failure.
fn confirm_client_connected(server: &OwnedHandle) -> io::Result<()> {
    let event = create_manual_reset_event()?;
    // Boxed because the kernel writes into the structure when the operation
    // completes: if a pending connect could not be cancelled in time, the
    // box is leaked rather than freed (see `await_pending_connect`).
    let mut overlapped = Box::new(overlapped_with(&event));

    // SAFETY: `server` is a live named-pipe server handle opened with
    // FILE_FLAG_OVERLAPPED, and `overlapped` is a valid structure carrying a
    // manual-reset event, as the documentation requires. Both stay alive
    // until the operation is known to be finished: every path below either
    // observes completion or hands the allocations to
    // `await_pending_connect`, which drains or leaks them.
    let connected = unsafe { ConnectNamedPipe(server.as_raw_handle(), &mut *overlapped) };
    let err = io::Error::last_os_error();
    let error = err.raw_os_error().and_then(|code| u32::try_from(code).ok());
    match classify_connect_result(connected, error) {
        ConnectDisposition::Complete => Ok(()),
        ConnectDisposition::Pending => await_pending_connect(server, overlapped, event),
        ConnectDisposition::Error => Err(err),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingWaitDisposition {
    Completed,
    TimedOut,
    Failed,
    Unexpected(u32),
}

const fn classify_pending_wait(wait: u32) -> PendingWaitDisposition {
    match wait {
        WAIT_OBJECT_0 => PendingWaitDisposition::Completed,
        WAIT_TIMEOUT => PendingWaitDisposition::TimedOut,
        WAIT_FAILED => PendingWaitDisposition::Failed,
        other => PendingWaitDisposition::Unexpected(other),
    }
}

const fn overlapped_result_succeeded(ok: i32) -> bool {
    ok != 0
}

const fn cancellation_is_still_pending(wait: u32) -> bool {
    wait != WAIT_OBJECT_0
}

/// Waits for a pending `ConnectNamedPipe` on `server` to complete.
///
/// Takes ownership of the operation's `OVERLAPPED` and event because their
/// lifetime is the crux of this path: the kernel writes the final status into
/// the structure and signals the event when the operation completes, so
/// neither may be freed while the operation is in flight. On the timeout and
/// wait-failure paths the operation is cancelled with `CancelIo`; since
/// cancellation is only a *request*, a bounded second wait follows, and if
/// even that expires the allocations are deliberately leaked instead of
/// freed — a leak is safe, a use-after-free is not. This is also why the
/// function never blocks unboundedly.
fn await_pending_connect(
    server: &OwnedHandle,
    overlapped: Box<OVERLAPPED>,
    event: OwnedHandle,
) -> io::Result<()> {
    // SAFETY: `event` is a live event handle.
    let wait = unsafe { WaitForSingleObject(event.as_raw_handle(), CONNECT_TIMEOUT_MS) };
    let disposition = classify_pending_wait(wait);
    let primary = match disposition {
        PendingWaitDisposition::Completed => {
            let mut transferred = 0u32;
            // SAFETY: the event is signalled, so `overlapped` describes a
            // completed operation on `server`; `bWait = FALSE` as no wait is
            // needed.
            let ok = unsafe {
                GetOverlappedResult(server.as_raw_handle(), &*overlapped, &mut transferred, 0)
            };
            if overlapped_result_succeeded(ok) {
                return Ok(());
            }
            return Err(io::Error::last_os_error());
        },
        // Capture the primary error before CancelIo can clobber the thread's
        // last-error value.
        PendingWaitDisposition::TimedOut => io::Error::new(
            io::ErrorKind::TimedOut,
            "the named pipe client did not finish connecting in time",
        ),
        PendingWaitDisposition::Failed => io::Error::last_os_error(),
        PendingWaitDisposition::Unexpected(other) => io::Error::other(format!(
            "unexpected wait result {other:#x} while waiting for the pipe client to connect"
        )),
    };

    // SAFETY: `server` is live; this requests cancellation of the I/O this
    // thread issued on it. The result is deliberately ignored — the bounded
    // wait below decides whether the operation actually finished.
    unsafe { CancelIo(server.as_raw_handle()) };
    // SAFETY: `event` is a live event handle.
    let drained = unsafe { WaitForSingleObject(event.as_raw_handle(), CANCEL_DRAIN_TIMEOUT_MS) };
    if cancellation_is_still_pending(drained) {
        // The operation is still in flight and refuses to die. Leak the
        // structure it will eventually write to, and the event it will
        // eventually signal, so that completion lands in memory we still own.
        Box::leak(overlapped);
        forget(event);
    }

    Err(primary)
}

#[cfg(test)]
#[path = "overlapped_tests.rs"]
mod tests;
