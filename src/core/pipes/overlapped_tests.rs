// SPDX-FileCopyrightText: 2025 conpty-oxide contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

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
            u32::try_from(buf.len()).expect("the test buffer length must fit in a DWORD"),
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
        Some(i32::try_from(ERROR_IO_PENDING).expect("the Win32 error code must fit in an i32")),
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
        .map(|i| u8::try_from(i % 251).expect("the reduced byte must fit in u8"))
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
            u32::try_from(payload.len()).expect("the test payload length must fit in a DWORD"),
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
        Some(i32::try_from(ERROR_IO_PENDING).expect("the Win32 error code must fit in an i32")),
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
            u32::try_from(buf.len()).expect("the test buffer length must fit in a DWORD"),
            ptr::null_mut(),
            &mut overlapped,
        )
    };
    assert_eq!(issued, 0);
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(i32::try_from(ERROR_IO_PENDING).expect("the Win32 error code must fit in an i32"))
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

#[test]
fn pipe_constants_and_retry_boundaries_are_explicit() {
    assert_eq!(PIPE_BUFFER_SIZE, 128 * 1024);
    assert_eq!(
        pipe_mode(),
        PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS
    );
    assert_eq!(next_attempt(0), 1);
    assert_eq!(next_attempt(3), 4);

    let collision = io::Error::from_raw_os_error(
        i32::try_from(ERROR_ACCESS_DENIED).expect("error code must fit"),
    );
    assert!(should_retry_name_collision(1, &collision));
    assert!(should_retry_name_collision(
        PIPE_NAME_ATTEMPTS - 1,
        &collision
    ));
    assert!(!should_retry_name_collision(PIPE_NAME_ATTEMPTS, &collision));

    let unrelated = io::Error::from_raw_os_error(2);
    assert!(!should_retry_name_collision(1, &unrelated));
}

#[test]
fn collision_retry_loop_takes_both_error_branches() {
    let collision = || {
        io::Error::from_raw_os_error(
            i32::try_from(ERROR_ACCESS_DENIED).expect("error code must fit"),
        )
    };

    let mut collision_calls = 0;
    let value = retry_name_collisions(|| {
        collision_calls += 1;
        if collision_calls == 1 {
            Err(collision())
        } else {
            Ok(7)
        }
    })
    .expect("one collision must be retried");
    assert_eq!(value, 7);
    assert_eq!(collision_calls, 2);

    let mut unrelated_calls = 0;
    let error = retry_name_collisions(|| {
        unrelated_calls += 1;
        if unrelated_calls == 1 {
            Err(io::Error::from_raw_os_error(2))
        } else {
            Ok(7)
        }
    })
    .expect_err("an unrelated failure must be returned without a retry");
    assert_eq!(error.raw_os_error(), Some(2));
    assert_eq!(unrelated_calls, 1);
}

#[test]
fn security_attributes_have_the_required_length_and_no_inheritance() {
    let attributes = non_inheritable_attributes();
    assert_eq!(
        attributes.nLength,
        u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).expect("structure size must fit")
    );
    assert!(attributes.lpSecurityDescriptor.is_null());
    assert_eq!(attributes.bInheritHandle, 0);
}

#[test]
fn overlapped_records_the_completion_event() {
    let event = create_manual_reset_event().expect("creating the event must succeed");
    let overlapped = overlapped_with(&event);
    assert_eq!(overlapped.hEvent, event.as_raw_handle());
}

#[test]
fn connect_result_classifier_covers_every_documented_outcome() {
    assert_eq!(
        classify_connect_result(1, None),
        ConnectDisposition::Complete
    );
    assert_eq!(
        classify_connect_result(0, Some(ERROR_PIPE_CONNECTED)),
        ConnectDisposition::Complete
    );
    assert_eq!(
        classify_connect_result(0, Some(ERROR_IO_PENDING)),
        ConnectDisposition::Pending
    );
    assert_eq!(
        classify_connect_result(0, Some(ERROR_ACCESS_DENIED)),
        ConnectDisposition::Error
    );
    assert_eq!(classify_connect_result(0, None), ConnectDisposition::Error);
}

#[test]
fn confirming_a_non_pipe_handle_fails() {
    let event = create_manual_reset_event().expect("creating the event must succeed");
    assert!(confirm_client_connected(&event).is_err());
}

#[test]
fn pending_wait_classifiers_cover_completion_and_failure() {
    assert_eq!(
        classify_pending_wait(WAIT_OBJECT_0),
        PendingWaitDisposition::Completed
    );
    assert_eq!(
        classify_pending_wait(WAIT_TIMEOUT),
        PendingWaitDisposition::TimedOut
    );
    assert_eq!(
        classify_pending_wait(WAIT_FAILED),
        PendingWaitDisposition::Failed
    );
    assert_eq!(
        classify_pending_wait(0x1234),
        PendingWaitDisposition::Unexpected(0x1234)
    );
    assert!(!overlapped_result_succeeded(0));
    assert!(overlapped_result_succeeded(1));
    assert!(!cancellation_is_still_pending(WAIT_OBJECT_0));
    assert!(cancellation_is_still_pending(WAIT_TIMEOUT));
}

#[test]
fn pending_connect_checks_the_overlapped_result() {
    use windows_sys::Win32::System::Threading::SetEvent;

    let server = create_manual_reset_event().expect("creating the stand-in handle must succeed");
    let event = create_manual_reset_event().expect("creating the completion event must succeed");
    // SAFETY: `event` is a live event handle owned by this test.
    assert_ne!(unsafe { SetEvent(event.as_raw_handle()) }, 0);
    let mut overlapped = overlapped_with(&event);
    // `OVERLAPPED::Internal` is an NTSTATUS. Model an operation whose event
    // was signalled spuriously while the I/O itself is still pending.
    overlapped.Internal = 0x0000_0103; // STATUS_PENDING
    let overlapped = Box::new(overlapped);

    let err = await_pending_connect(&server, overlapped, event)
        .expect_err("a non-file handle cannot complete overlapped pipe I/O");
    assert!(
        err.raw_os_error().is_some(),
        "GetOverlappedResult must supply the failure, got: {err}"
    );
}
