//! Synchronous anonymous pipes for the pseudoconsole's I/O streams.
//!
//! A pseudoconsole session needs two unidirectional byte streams, and each is
//! an anonymous pipe created by `CreatePipe`:
//!
//! - **conout** — the pseudoconsole writes rendered output (UTF-8 text
//!   interleaved with virtual terminal sequences) into its write end; we read
//!   the read end.
//! - **conin** — we write user input into the write end; the pseudoconsole
//!   reads the read end.
//!
//! Both pipes are created **non-inheritable**. ConPTY does not rely on handle
//! inheritance: `CreatePseudoConsole` duplicates the two handles it is given
//! into the console host itself, and the child process is launched with
//! `bInheritHandles = FALSE` and the `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`
//! attribute. Keeping the handles non-inheritable means an unrelated
//! `CreateProcess` call elsewhere in the process cannot leak them and hold the
//! pipe open, which would defeat EOF detection on conout.
//!
//! `CreatePipe` handles are always synchronous (they never take an
//! `OVERLAPPED`), which is exactly what `CreatePseudoConsole` requires of
//! `hInput` and `hOutput`. The flip side is the documented deadlock hazard:
//! whoever owns these pipes must service them from a thread that is not
//! blocked on anything else, or a full pipe buffer will wedge the session.

use std::io;
use std::mem;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::ptr;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Pipes::CreatePipe;

/// The four pipe ends of a pseudoconsole session.
///
/// Field names describe the stream, and the doc comment on each field says who
/// owns it after the pseudoconsole has been created. The two ends handed to
/// `CreatePseudoConsole` must be closed by us as soon as the child has been
/// spawned: ConPTY keeps its own duplicates, and dropping ours is what lets
/// the pipe reach end-of-file once the console host is gone.
// TODO: drop the allow once the pseudoconsole lifecycle core consumes these.
#[allow(dead_code)]
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
// TODO: drop the allow once the pseudoconsole lifecycle core calls this.
#[allow(dead_code)]
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
    // `bInheritHandle: FALSE` is also `CreatePipe`'s behaviour when
    // `lpPipeAttributes` is NULL, but spelling the attributes out documents
    // the intent at the call site instead of relying on a default.
    let attributes = SECURITY_ATTRIBUTES {
        nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 0, // FALSE
    };

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

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE_FLAG_INHERIT};

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
}
