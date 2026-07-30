// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;

use std::fs::File;
use std::io::{Read, Write};
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE_FLAG_INHERIT};

fn is_inheritable(handle: &OwnedHandle) -> bool {
    let mut flags = 0;
    // SAFETY: `handle` is live and `flags` is a valid out-parameter.
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

    let mut conout_writer = File::from(pipes.conout_write);
    let mut conout_reader = File::from(pipes.conout_read);
    conout_writer.write_all(b"ok").expect("write must succeed");
    let mut buf = [0u8; 2];
    conout_reader
        .read_exact(&mut buf)
        .expect("read must succeed");
    assert_eq!(buf, *b"ok");
}
