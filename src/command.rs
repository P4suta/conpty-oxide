// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Process command builder and Windows command-line / environment-block
//! construction.
//!
//! This module holds the data-carrying part of a [`std::process::Command`]
//! style builder. The actual `spawn` implementation is layered on top in the
//! blocking / async front ends; this module is only concerned with recording
//! the caller's intent and lowering it into the raw UTF-16 buffers that
//! `CreateProcessW` consumes:
//!
//! - [`Command::build_command_line`] produces the `lpCommandLine` buffer using
//!   the MSVC CRT compatible quoting algorithm from the Rust standard library
//!   (`library/std/src/sys/args/windows.rs`), which itself follows the rules
//!   documented by Microsoft ("Parsing C++ command-line arguments") and
//!   Raymond Chen's "What's up with the strange treatment of quotation marks
//!   and backslashes by `CommandLineToArgvW`".
//! - [`Command::build_environment_block`] produces the `lpEnvironment` block,
//!   sorted case-insensitively as required by the `CreateProcessW`
//!   documentation.
//!
//! This module deliberately depends only on `std` (not on `crate::error`);
//! every fallible function reports failures as [`std::io::Error`] with
//! [`std::io::ErrorKind::InvalidInput`].

use std::cmp::Ordering;
use std::collections::btree_map::{BTreeMap, Entry};
use std::ffi::{OsStr, OsString};
use std::io;
use std::iter;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::TRUE;
use windows_sys::Win32::Globalization::{
    CompareStringOrdinal, CSTR_EQUAL, CSTR_GREATER_THAN, CSTR_LESS_THAN,
};

const QUOTE: u16 = b'"' as u16;
const BACKSLASH: u16 = b'\\' as u16;
const SPACE: u16 = b' ' as u16;

/// A single command-line argument.
///
/// Regular arguments go through the MSVC CRT compatible quoting algorithm;
/// raw arguments are appended verbatim (see [`Command::raw_arg`]).
#[derive(Debug)]
enum Arg {
    Regular(OsString),
    Raw(OsString),
}

/// A recorded environment modification, replayed over the inherited (or
/// cleared) environment when the block is built.
#[derive(Debug)]
enum EnvOp {
    /// Set (or overwrite, case-insensitively) a variable.
    Set(OsString, OsString),
    /// Remove a variable (case-insensitively).
    Remove(OsString),
}

/// A Windows environment-variable key with the operating system's ordinal,
/// case-insensitive equality and ordering.
///
/// Windows case folding is deliberately not reproduced in Rust: its mapping
/// is OS-specific and may change between Windows versions. This mirrors the
/// standard library's Windows `EnvKey` implementation.
#[derive(Debug)]
struct EnvKey {
    wide: Vec<u16>,
    len: i32,
}

impl EnvKey {
    fn new(key: &OsStr) -> io::Result<Self> {
        let wide: Vec<u16> = key.encode_wide().collect();
        let len = match i32::try_from(wide.len()) {
            Ok(len) => len,
            Err(err) => return Err(io::Error::new(io::ErrorKind::InvalidInput, err)),
        };
        Ok(Self { wide, len })
    }
}

impl Ord for EnvKey {
    fn cmp(&self, other: &Self) -> Ordering {
        // SAFETY: both slices remain live for the call, their lengths were
        // checked above, and CompareStringOrdinal only reads those slices.
        match unsafe {
            CompareStringOrdinal(
                self.wide.as_ptr(),
                self.len,
                other.wide.as_ptr(),
                other.len,
                TRUE,
            )
        } {
            CSTR_LESS_THAN => Ordering::Less,
            CSTR_EQUAL => Ordering::Equal,
            CSTR_GREATER_THAN => Ordering::Greater,
            // CompareStringOrdinal cannot fail with the valid pointers and
            // checked lengths above. Retain a deterministic, non-panicking
            // order if an unsupported Windows implementation violates that
            // contract.
            _ => self.wide.cmp(&other.wide),
        }
    }
}

impl PartialOrd for EnvKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for EnvKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for EnvKey {}

/// A builder describing how to launch a child process under a pseudoconsole.
///
/// Mirrors the `std::process::Command` builder API. This type only records
/// configuration; spawning is implemented by the blocking / async layers in a
/// later phase.
#[derive(Debug)]
pub(super) struct Command {
    program: OsString,
    args: Vec<Arg>,
    env_clear: bool,
    env_ops: Vec<EnvOp>,
    cwd: Option<PathBuf>,
    #[cfg(all(test, any(feature = "blocking", feature = "tokio")))]
    kill_on_drop: bool,
}

impl Command {
    /// Creates a new builder for launching `program`.
    ///
    /// Like `std::process::Command::new`, the program is not resolved or
    /// validated here; errors (embedded NUL, embedded `"`) surface when the
    /// command line is built.
    pub(super) fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_os_string(),
            args: Vec::new(),
            env_clear: false,
            env_ops: Vec::new(),
            cwd: None,
            #[cfg(all(test, any(feature = "blocking", feature = "tokio")))]
            kill_on_drop: false,
        }
    }

    /// Appends one argument, to be quoted/escaped as needed when the command
    /// line is built.
    pub(super) fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(Arg::Regular(arg.as_ref().to_os_string()));
        self
    }

    /// Appends multiple arguments; equivalent to calling [`Command::arg`] for
    /// each element.
    pub(super) fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            self.arg(arg);
        }
        self
    }

    /// Appends literal text to the command line, bypassing all quoting and
    /// escaping.
    ///
    /// Same semantics as `std::os::windows::process::CommandExt::raw_arg`: the
    /// text is separated from the previous argument by a single space and
    /// otherwise copied verbatim. Intended for `cmd.exe /c` style invocations
    /// where the callee parses the raw command line itself.
    pub(super) fn raw_arg(&mut self, text: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(Arg::Raw(text.as_ref().to_os_string()));
        self
    }

    /// Sets (or overwrites, case-insensitively) an environment variable for
    /// the child process.
    pub(super) fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.env_ops.push(EnvOp::Set(
            key.as_ref().to_os_string(),
            value.as_ref().to_os_string(),
        ));
        self
    }

    /// Sets multiple environment variables; equivalent to calling
    /// [`Command::env`] for each pair.
    pub(super) fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        for (key, value) in vars {
            self.env(key, value);
        }
        self
    }

    /// Removes an environment variable (case-insensitively) from the child
    /// process environment.
    pub(super) fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.env_ops
            .push(EnvOp::Remove(key.as_ref().to_os_string()));
        self
    }

    /// Clears the entire environment for the child process, including any
    /// modifications recorded so far. Variables set after this call still
    /// apply.
    pub(super) fn env_clear(&mut self) -> &mut Self {
        self.env_clear = true;
        self.env_ops.clear();
        self
    }

    /// Sets the working directory for the child process.
    pub(super) fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.cwd = Some(dir.as_ref().to_path_buf());
        self
    }

    /// Requests that the spawned process (tree) be terminated when its handle
    /// is dropped. Only recorded here; enforced by the spawn layers.
    #[cfg(all(test, any(feature = "blocking", feature = "tokio")))]
    pub(super) fn kill_on_drop(&mut self, kill: bool) -> &mut Self {
        self.kill_on_drop = kill;
        self
    }

    /// Returns the program name passed to [`Command::new`].
    #[cfg(any(feature = "blocking", feature = "tokio"))]
    pub(super) fn get_program(&self) -> &OsStr {
        &self.program
    }

    /// Returns the configured working directory, if any.
    pub(super) fn get_current_dir(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    /// Returns whether kill-on-drop was requested.
    #[cfg(all(test, any(feature = "blocking", feature = "tokio")))]
    pub(super) const fn get_kill_on_drop(&self) -> bool {
        self.kill_on_drop
    }

    /// Builds the NUL-terminated UTF-16 command line for the `lpCommandLine`
    /// parameter of `CreateProcessW`.
    ///
    /// The program name is always wrapped in double quotes (so paths with
    /// spaces resolve correctly) and must not itself contain a double quote.
    /// Regular arguments follow the MSVC CRT compatible algorithm used by the
    /// Rust standard library:
    ///
    /// - An argument is quoted if it is empty or contains a space, tab,
    ///   double quote, or vertical bar (the last is stricter than strictly
    ///   necessary for CRT parsing but harmless and safer).
    /// - Inside a quoted argument, `n` backslashes followed by a `"` are
    ///   written as `2n + 1` backslashes followed by `"`, and `n` trailing
    ///   backslashes are written as `2n` backslashes before the closing `"`.
    ///
    /// Raw arguments are appended verbatim. Any embedded NUL (in the program
    /// or any argument) yields an [`io::ErrorKind::InvalidInput`] error.
    pub(super) fn build_command_line(&self) -> io::Result<Vec<u16>> {
        ensure_no_nuls(&self.program)?;
        if self.program.as_encoded_bytes().contains(&b'"') {
            return Err(invalid_input(
                "program name must not contain a double quote",
            ));
        }
        let mut cmd: Vec<u16> = Vec::new();
        cmd.push(QUOTE);
        cmd.extend(self.program.encode_wide());
        cmd.push(QUOTE);
        for arg in &self.args {
            cmd.push(SPACE);
            match arg {
                Arg::Regular(arg) => append_regular_arg(&mut cmd, arg)?,
                Arg::Raw(text) => {
                    ensure_no_nuls(text)?;
                    cmd.extend(text.encode_wide());
                },
            }
        }
        cmd.push(0);
        Ok(cmd)
    }

    /// Builds the environment block for the `lpEnvironment` parameter of
    /// `CreateProcessW` (with `CREATE_UNICODE_ENVIRONMENT`).
    ///
    /// Returns `Ok(None)` when no modification was recorded, in which case
    /// the caller should pass `NULL` so the child inherits the parent
    /// environment directly. Otherwise the parent environment (or an empty
    /// one after [`Command::env_clear`]) is captured, the recorded
    /// modifications are replayed over it case-insensitively, and the result
    /// is flattened into a `KEY=VALUE\0...\0\0` UTF-16 block.
    ///
    /// Entries are sorted with Windows' ordinal ignore-case comparison, as
    /// required by the `CreateProcessW` documentation and equivalent to the
    /// standard library's Windows `EnvKey` ordering.
    ///
    /// Errors with [`io::ErrorKind::InvalidInput`] if a recorded name is
    /// empty, contains `=`, or if a name or value contains NUL.
    pub(super) fn build_environment_block(&self) -> io::Result<Option<Vec<u16>>> {
        if !self.env_clear && self.env_ops.is_empty() {
            return Ok(None);
        }

        // The BTreeMap ordering directly yields the sort order required by
        // `CreateProcessW` while values retain the caller's original casing.
        let mut map: BTreeMap<EnvKey, (OsString, OsString)> = BTreeMap::new();
        if !self.env_clear {
            for (key, value) in std::env::vars_os() {
                map.insert(EnvKey::new(&key)?, (key, value));
            }
        }
        for op in &self.env_ops {
            match op {
                EnvOp::Set(key, value) => {
                    validate_env_key(key)?;
                    ensure_no_nuls(value)?;
                    match map.entry(EnvKey::new(key)?) {
                        // Overwriting keeps the casing of the existing name,
                        // matching `BTreeMap::insert` semantics in std's
                        // `CommandEnv`.
                        Entry::Occupied(mut entry) => entry.get_mut().1.clone_from(value),
                        Entry::Vacant(entry) => {
                            entry.insert((key.clone(), value.clone()));
                        },
                    }
                },
                EnvOp::Remove(key) => {
                    validate_env_key(key)?;
                    map.remove(&EnvKey::new(key)?);
                },
            }
        }

        let mut block: Vec<u16> = Vec::new();
        for (key, value) in map.values() {
            block.extend(key.encode_wide());
            block.push(u16::from(b'='));
            block.extend(value.encode_wide());
            block.push(0);
        }
        // An empty environment is represented as two NULs; a non-empty block
        // already ends with the last entry's NUL plus the final NUL below.
        if block.is_empty() {
            block.push(0);
        }
        block.push(0);
        Ok(Some(block))
    }
}

/// Converts an `OsStr` to a NUL-terminated UTF-16 buffer, rejecting embedded
/// NULs with [`io::ErrorKind::InvalidInput`]. Used for `lpCurrentDirectory`
/// and similar wide-string parameters.
pub(super) fn to_wide_nul(s: &OsStr) -> io::Result<Vec<u16>> {
    ensure_no_nuls(s)?;
    let mut wide: Vec<u16> = s.encode_wide().collect();
    wide.push(0);
    Ok(wide)
}

fn invalid_input(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

fn ensure_no_nuls(s: &OsStr) -> io::Result<()> {
    if s.encode_wide().any(|unit| unit == 0) {
        Err(invalid_input("nul character found in provided data"))
    } else {
        Ok(())
    }
}

fn validate_env_key(key: &OsStr) -> io::Result<()> {
    if key.is_empty() {
        return Err(invalid_input("environment variable name must not be empty"));
    }
    ensure_no_nuls(key)?;
    if key.as_encoded_bytes().contains(&b'=') {
        return Err(invalid_input(
            "environment variable name must not contain `=`",
        ));
    }
    Ok(())
}

/// Appends a regular argument using the MSVC CRT compatible quoting rules
/// from `library/std/src/sys/args/windows.rs` (`Quote::Auto`).
fn append_regular_arg(cmd: &mut Vec<u16>, arg: &OsStr) -> io::Result<()> {
    ensure_no_nuls(arg)?;
    // Space, tab, quote, and bar are all ASCII, so scanning the WTF-8 bytes
    // is equivalent to scanning UTF-16 units here.
    let bytes = arg.as_encoded_bytes();
    let quote = bytes.is_empty()
        || bytes
            .iter()
            .any(|&b| b == b' ' || b == b'\t' || b == b'"' || b == b'|');
    if quote {
        cmd.push(QUOTE);
    }
    let mut backslashes: usize = 0;
    for unit in arg.encode_wide() {
        if unit == BACKSLASH {
            backslashes += 1;
        } else {
            if unit == QUOTE {
                // n preceding backslashes become 2n + 1 in total before a
                // literal quote (the n originals are already pushed).
                cmd.extend(iter::repeat(BACKSLASH).take(backslashes + 1));
            }
            backslashes = 0;
        }
        cmd.push(unit);
    }
    if quote {
        // n trailing backslashes become 2n in total so the closing quote is
        // not escaped.
        cmd.extend(iter::repeat(BACKSLASH).take(backslashes));
        cmd.push(QUOTE);
    }
    Ok(())
}

#[cfg(test)]
#[path = "command_tests.rs"]
mod tests;
