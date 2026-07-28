//! Pipes that carry a pseudoconsole session's I/O streams.
//!
//! A pseudoconsole session needs two unidirectional byte streams:
//!
//! - **conout** — the pseudoconsole writes rendered output (UTF-8 text
//!   interleaved with virtual terminal sequences) into its end; we read ours.
//! - **conin** — we write user input into our end; the pseudoconsole reads
//!   its end.
//!
//! This module builds those streams in two shapes, one per front end:
//!
//! - [`create_sync_pipes`] — anonymous pipes from `CreatePipe`, for the
//!   blocking front end. Anonymous pipe handles are always synchronous (they
//!   never take an `OVERLAPPED`), which is exactly what `CreatePseudoConsole`
//!   requires of `hInput` and `hOutput`. The flip side is the documented
//!   deadlock hazard: whoever owns these pipes must service them from a
//!   thread that is not blocked on anything else, or a full pipe buffer will
//!   wedge the session.
//! - [`create_overlapped_pipes`] — single-instance named-pipe pairs, for the
//!   async front end. The server end of each pipe is opened with
//!   `FILE_FLAG_OVERLAPPED` so tokio can register it with its
//!   I/O-completion-port reactor; the client end is opened synchronously and
//!   handed to `CreatePseudoConsole`, meeting the same requirement. The split
//!   also keeps older console hosts working: only recent OpenConsole builds
//!   understand overlapped pipe handles, while every host performs correct
//!   synchronous I/O on the synchronous client ends it receives here.
//!
//! Every handle is created **non-inheritable**. ConPTY does not rely on
//! handle inheritance: `CreatePseudoConsole` duplicates the two handles it is
//! given into the console host itself, and the child process is launched with
//! `bInheritHandles = FALSE` and the `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`
//! attribute. Keeping the handles non-inheritable means an unrelated
//! `CreateProcess` call elsewhere in the process cannot leak them and hold a
//! pipe open, which would defeat EOF detection on conout.

use std::io;
use std::iter;
use std::mem;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_IO_PENDING, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ,
    GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING,
    PIPE_ACCESS_INBOUND, PIPE_ACCESS_OUTBOUND,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, CreatePipe, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows_sys::Win32::System::IO::{CancelIo, GetOverlappedResult, OVERLAPPED};

/// The four pipe ends of a pseudoconsole session.
///
/// Field names describe the stream, and the doc comment on each field says who
/// owns it after the pseudoconsole has been created. The two ends handed to
/// `CreatePseudoConsole` must be closed by us as soon as the child has been
/// spawned: ConPTY keeps its own duplicates, and dropping ours is what lets
/// the pipe reach end-of-file once the console host is gone.
#[derive(Debug)]
pub(crate) struct SyncPipes {
    /// Read end of conout. **We** read pseudoconsole output from this.
    pub(crate) conout_read: OwnedHandle,
    /// Write end of conout. Passed to `CreatePseudoConsole` as `hOutput`,
    /// then closed by us.
    pub(crate) conout_write: OwnedHandle,
    /// Read end of conin. Passed to `CreatePseudoConsole` as `hInput`, then
    /// closed by us.
    pub(crate) conin_read: OwnedHandle,
    /// Write end of conin. **We** write user input into this.
    pub(crate) conin_write: OwnedHandle,
}

/// Creates the conout and conin pipes for one pseudoconsole session.
///
/// Both pipes use the system default buffer size and are non-inheritable.
///
/// # Errors
///
/// Returns the OS error from `CreatePipe`. If the second pipe fails, the
/// first pipe's handles are closed by their [`OwnedHandle`] destructors before
/// the error is returned.
pub(crate) fn create_sync_pipes() -> io::Result<SyncPipes> {
    let (conout_read, conout_write) = create_pipe()?;
    let (conin_read, conin_write) = create_pipe()?;

    Ok(SyncPipes {
        conout_read,
        conout_write,
        conin_read,
        conin_write,
    })
}

/// Creates one non-inheritable anonymous pipe, returning `(read, write)`.
fn create_pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let attributes = non_inheritable_attributes();

    let mut read: HANDLE = ptr::null_mut();
    let mut write: HANDLE = ptr::null_mut();

    // SAFETY: `read` and `write` are live, correctly typed out-parameters,
    // and `attributes` outlives the call. `nSize = 0` requests the system
    // default buffer size.
    let created = unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) };
    if created == 0 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: `CreatePipe` reported success, so both out-parameters hold
    // valid, open handles that this process exclusively owns and that are
    // neither NULL nor `INVALID_HANDLE_VALUE`. Transferring them to
    // `OwnedHandle` makes each one closed exactly once, by its destructor.
    unsafe {
        Ok((
            OwnedHandle::from_raw_handle(read),
            OwnedHandle::from_raw_handle(write),
        ))
    }
}

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
/// closed by us as soon as the console has been created: ConPTY keeps its own
/// duplicates, and dropping ours is what lets the server ends observe
/// end-of-file once the console host is gone.
#[derive(Debug)]
// The upcoming async front end is the consumer of these fields; until it
// lands, only this module's tests read them.
#[allow(dead_code)]
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
// The upcoming async front end is the caller; until it lands, only this
// module's tests call this.
#[allow(dead_code)]
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
    fn server_open_mode(self) -> u32 {
        let access = match self {
            Self::Inbound => PIPE_ACCESS_INBOUND,
            Self::Outbound => PIPE_ACCESS_OUTBOUND,
        };
        access | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE
    }

    /// `dwDesiredAccess` for the client's `CreateFileW`: the mirror image of
    /// the server's direction.
    fn client_desired_access(self) -> u32 {
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
    let mut attempts = 0;
    loop {
        attempts += 1;
        let name = unique_pipe_name();
        let server = match create_pipe_server(&name, direction) {
            Ok(server) => server,
            Err(err) if attempts < PIPE_NAME_ATTEMPTS && is_pipe_name_collision(&err) => continue,
            Err(err) => return Err(err),
        };
        // Failures from here on drop `server` (and then `client`), closing
        // them; a half-built pair never leaks.
        let client = open_pipe_client(&name, direction)?;
        confirm_client_connected(&server)?;
        return Ok((server, client));
    }
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
        nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 0, // FALSE
    }
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
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
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
        err.raw_os_error().map(|code| code as u32),
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
    let mut overlapped: OVERLAPPED = unsafe { mem::zeroed() };
    overlapped.hEvent = event.as_raw_handle();
    overlapped
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
    if connected != 0 {
        return Ok(());
    }

    let err = io::Error::last_os_error();
    match err.raw_os_error().map(|code| code as u32) {
        Some(ERROR_PIPE_CONNECTED) => Ok(()),
        Some(ERROR_IO_PENDING) => await_pending_connect(server, overlapped, event),
        _ => Err(err),
    }
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
    if wait == WAIT_OBJECT_0 {
        let mut transferred = 0u32;
        // SAFETY: the event is signalled, so `overlapped` describes a
        // completed operation on `server`; `bWait = FALSE` as no wait is
        // needed.
        let ok = unsafe {
            GetOverlappedResult(server.as_raw_handle(), &*overlapped, &mut transferred, 0)
        };
        if ok != 0 {
            return Ok(());
        }
        return Err(io::Error::last_os_error());
    }

    // Capture the primary error before CancelIo can clobber the thread's
    // last-error value.
    let primary = match wait {
        WAIT_TIMEOUT => io::Error::new(
            io::ErrorKind::TimedOut,
            "the named pipe client did not finish connecting in time",
        ),
        WAIT_FAILED => io::Error::last_os_error(),
        other => io::Error::other(format!(
            "unexpected wait result {other:#x} while waiting for the pipe client to connect"
        )),
    };

    // SAFETY: `server` is live; this requests cancellation of the I/O this
    // thread issued on it. The result is deliberately ignored — the bounded
    // wait below decides whether the operation actually finished.
    unsafe { CancelIo(server.as_raw_handle()) };
    // SAFETY: `event` is a live event handle.
    let drained = unsafe { WaitForSingleObject(event.as_raw_handle(), CANCEL_DRAIN_TIMEOUT_MS) };
    if drained != WAIT_OBJECT_0 {
        // The operation is still in flight and refuses to die. Leak the
        // structure it will eventually write to, and the event it will
        // eventually signal, so that completion lands in memory we still own.
        Box::leak(overlapped);
        mem::forget(event);
    }

    Err(primary)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs::File;
    use std::io::{Read, Write};

    use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE_FLAG_INHERIT};
    use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};

    use crate::core::is_disconnect_error;

    /// Returns whether the handle carries `HANDLE_FLAG_INHERIT`.
    fn is_inheritable(handle: &OwnedHandle) -> bool {
        let mut flags: u32 = 0;
        // SAFETY: `handle` is a live handle borrowed for the call, and
        // `flags` is a valid out-parameter.
        let ok = unsafe { GetHandleInformation(handle.as_raw_handle(), &mut flags) };
        assert_ne!(
            ok,
            0,
            "GetHandleInformation failed: {}",
            io::Error::last_os_error()
        );
        flags & HANDLE_FLAG_INHERIT != 0
    }

    #[test]
    fn write_then_read_round_trips_on_both_pipes() {
        let pipes = create_sync_pipes().expect("creating pipes must succeed");

        let mut conout_writer = File::from(pipes.conout_write);
        let mut conout_reader = File::from(pipes.conout_read);
        conout_writer
            .write_all(b"conout payload")
            .expect("writing to conout must succeed");
        let mut buf = [0u8; 14];
        conout_reader
            .read_exact(&mut buf)
            .expect("reading from conout must succeed");
        assert_eq!(&buf, b"conout payload");

        let mut conin_writer = File::from(pipes.conin_write);
        let mut conin_reader = File::from(pipes.conin_read);
        conin_writer
            .write_all(b"conin payload")
            .expect("writing to conin must succeed");
        let mut buf = [0u8; 13];
        conin_reader
            .read_exact(&mut buf)
            .expect("reading from conin must succeed");
        assert_eq!(&buf, b"conin payload");
    }

    #[test]
    fn dropping_the_write_end_reports_eof() {
        let pipes = create_sync_pipes().expect("creating pipes must succeed");

        let mut writer = File::from(pipes.conout_write);
        let mut reader = File::from(pipes.conout_read);
        writer.write_all(b"tail").expect("write must succeed");
        drop(writer);

        let mut sink = Vec::new();
        reader
            .read_to_end(&mut sink)
            .expect("a broken pipe must read as EOF, not an error");
        assert_eq!(sink, b"tail");
    }

    #[test]
    fn all_four_ends_are_non_inheritable() {
        let pipes = create_sync_pipes().expect("creating pipes must succeed");

        assert!(!is_inheritable(&pipes.conout_read));
        assert!(!is_inheritable(&pipes.conout_write));
        assert!(!is_inheritable(&pipes.conin_read));
        assert!(!is_inheritable(&pipes.conin_write));
    }

    #[test]
    fn the_two_pipes_are_independent() {
        let pipes = create_sync_pipes().expect("creating pipes must succeed");

        let mut conin_writer = File::from(pipes.conin_write);
        conin_writer
            .write_all(b"input only")
            .expect("write must succeed");
        drop(conin_writer);

        // Closing conin's write end must not affect conout.
        let mut conout_writer = File::from(pipes.conout_write);
        let mut conout_reader = File::from(pipes.conout_read);
        conout_writer.write_all(b"ok").expect("write must succeed");
        let mut buf = [0u8; 2];
        conout_reader
            .read_exact(&mut buf)
            .expect("read must succeed");
        assert_eq!(&buf, b"ok");
    }

    // ---- overlapped named-pipe pairs ------------------------------------

    #[test]
    fn all_four_overlapped_pipe_ends_are_non_inheritable() {
        let pipes = create_overlapped_pipes().expect("creating the pipe pair must succeed");

        assert!(!is_inheritable(&pipes.conout_server));
        assert!(!is_inheritable(&pipes.conout_client));
        assert!(!is_inheritable(&pipes.conin_server));
        assert!(!is_inheritable(&pipes.conin_client));
    }

    #[test]
    fn conout_flows_from_the_sync_client_to_the_overlapped_server() {
        let pipes = create_overlapped_pipes().expect("creating the pipe pair must succeed");

        let event = create_manual_reset_event().expect("creating the event must succeed");
        let mut overlapped = overlapped_with(&event);
        let mut buf = [0u8; 32];

        // Issue the read while the pipe is still empty. On an overlapped
        // handle it must go pending; a synchronous handle would block inside
        // ReadFile instead of returning, so a pending return doubles as
        // proof that conout_server really is in overlapped mode.
        //
        // SAFETY: `buf`, `overlapped`, and `event` all outlive the
        // operation, which the blocking GetOverlappedResult below sees
        // through to completion. The byte-count out-parameter is NULL, as
        // recommended for overlapped I/O.
        let issued = unsafe {
            ReadFile(
                pipes.conout_server.as_raw_handle(),
                buf.as_mut_ptr(),
                buf.len() as u32,
                ptr::null_mut(),
                &mut overlapped,
            )
        };
        assert_eq!(
            issued, 0,
            "a read from an empty pipe must not complete synchronously"
        );
        let err = io::Error::last_os_error();
        assert_eq!(
            err.raw_os_error(),
            Some(ERROR_IO_PENDING as i32),
            "conout_server must be an overlapped-mode handle: {err}"
        );

        // The client end must be synchronous: plain blocking `File` I/O.
        let mut client = File::from(pipes.conout_client);
        client
            .write_all(b"conout payload")
            .expect("writing to the sync client end must succeed");

        let mut transferred = 0u32;
        // SAFETY: `overlapped` belongs to the read issued above; `bWait =
        // TRUE` blocks until that read completes.
        let ok = unsafe {
            GetOverlappedResult(
                pipes.conout_server.as_raw_handle(),
                &overlapped,
                &mut transferred,
                1,
            )
        };
        assert_ne!(
            ok,
            0,
            "GetOverlappedResult failed: {}",
            io::Error::last_os_error()
        );
        assert_eq!(&buf[..transferred as usize], b"conout payload");
    }

    #[test]
    fn conin_flows_from_the_overlapped_server_to_the_sync_client() {
        let pipes = create_overlapped_pipes().expect("creating the pipe pair must succeed");

        // Twice the pipe buffer: on a blocking-wait byte pipe the write can
        // only complete once a reader has drained some of it, so it must go
        // pending — which doubles as proof that conin_server really is in
        // overlapped mode.
        let payload: Vec<u8> = (0..2 * PIPE_BUFFER_SIZE as usize)
            .map(|i| (i % 251) as u8)
            .collect();

        let event = create_manual_reset_event().expect("creating the event must succeed");
        let mut overlapped = overlapped_with(&event);

        // SAFETY: `payload`, `overlapped`, and `event` all outlive the
        // operation, which the blocking GetOverlappedResult below sees
        // through to completion. The byte-count out-parameter is NULL, as
        // recommended for overlapped I/O.
        let issued = unsafe {
            WriteFile(
                pipes.conin_server.as_raw_handle(),
                payload.as_ptr(),
                payload.len() as u32,
                ptr::null_mut(),
                &mut overlapped,
            )
        };
        assert_eq!(
            issued, 0,
            "a write larger than the pipe buffer must not complete synchronously"
        );
        let err = io::Error::last_os_error();
        assert_eq!(
            err.raw_os_error(),
            Some(ERROR_IO_PENDING as i32),
            "conin_server must be an overlapped-mode handle: {err}"
        );

        // The client end must be synchronous: plain blocking `File` I/O.
        let mut client = File::from(pipes.conin_client);
        let mut received = vec![0u8; payload.len()];
        client
            .read_exact(&mut received)
            .expect("reading from the sync client end must succeed");
        assert_eq!(received, payload);

        let mut transferred = 0u32;
        // SAFETY: `overlapped` belongs to the write issued above; `bWait =
        // TRUE` blocks until that write completes.
        let ok = unsafe {
            GetOverlappedResult(
                pipes.conin_server.as_raw_handle(),
                &overlapped,
                &mut transferred,
                1,
            )
        };
        assert_ne!(
            ok,
            0,
            "GetOverlappedResult failed: {}",
            io::Error::last_os_error()
        );
        assert_eq!(transferred as usize, payload.len());
    }

    #[test]
    fn a_pending_conout_read_ends_as_a_disconnect_when_the_client_closes() {
        let pipes = create_overlapped_pipes().expect("creating the pipe pair must succeed");

        let event = create_manual_reset_event().expect("creating the event must succeed");
        let mut overlapped = overlapped_with(&event);
        let mut buf = [0u8; 8];

        // SAFETY: as in the round-trip test above: all buffers outlive the
        // operation, which the blocking GetOverlappedResult below sees
        // through to completion.
        let issued = unsafe {
            ReadFile(
                pipes.conout_server.as_raw_handle(),
                buf.as_mut_ptr(),
                buf.len() as u32,
                ptr::null_mut(),
                &mut overlapped,
            )
        };
        assert_eq!(issued, 0);
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(ERROR_IO_PENDING as i32)
        );

        // The pseudoconsole's end of conout goes away while our read is
        // parked — exactly what the async reader sees when the console host
        // exits.
        drop(pipes.conout_client);

        let mut transferred = 0u32;
        // SAFETY: `overlapped` belongs to the read issued above; `bWait =
        // TRUE` blocks until that read completes.
        let ok = unsafe {
            GetOverlappedResult(
                pipes.conout_server.as_raw_handle(),
                &overlapped,
                &mut transferred,
                1,
            )
        };
        assert_eq!(ok, 0, "the read must not succeed after the writer is gone");
        let err = io::Error::last_os_error();
        assert!(
            is_disconnect_error(&err),
            "the failure must map to the EOF contract's disconnect set: {err}"
        );
    }

    #[test]
    fn back_to_back_creations_do_not_collide() {
        // Keeping every pair alive at once means a reused name would fail
        // its `CreateNamedPipeW` (single instance, first-instance flag).
        let pairs: Vec<OverlappedPipes> = (0..8)
            .map(|i| {
                create_overlapped_pipes()
                    .unwrap_or_else(|err| panic!("creating pipe pair {i} failed: {err}"))
            })
            .collect();
        drop(pairs);
    }

    #[test]
    fn a_squatted_pipe_name_is_rejected_not_joined() {
        let name = unique_pipe_name();
        let _squatter = create_pipe_server(&name, ServerDirection::Inbound)
            .expect("the first instance must claim the name");

        let err = create_pipe_server(&name, ServerDirection::Inbound)
            .expect_err("FILE_FLAG_FIRST_PIPE_INSTANCE must reject an already-taken name");
        assert!(
            is_pipe_name_collision(&err),
            "the rejection must be recognized as a name collision: {err}"
        );
    }
}
