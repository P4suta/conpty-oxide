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
//!   and backslashes by CommandLineToArgvW".
//! - [`Command::build_environment_block`] produces the `lpEnvironment` block,
//!   sorted case-insensitively as required by the `CreateProcessW`
//!   documentation.
//!
//! This module deliberately depends only on `std` (not on `crate::error`);
//! every fallible function reports failures as [`std::io::Error`] with
//! [`std::io::ErrorKind::InvalidInput`].

use std::collections::btree_map::{BTreeMap, Entry};
use std::ffi::{OsStr, OsString};
use std::io;
use std::iter;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const QUOTE: u16 = b'"' as u16;
const BACKSLASH: u16 = b'\\' as u16;
const SPACE: u16 = b' ' as u16;

/// A single command-line argument.
///
/// Regular arguments go through the MSVC CRT compatible quoting algorithm;
/// raw arguments are appended verbatim (see [`Command::raw_arg`]).
#[derive(Debug, Clone)]
enum Arg {
    Regular(OsString),
    Raw(OsString),
}

/// A recorded environment modification, replayed over the inherited (or
/// cleared) environment when the block is built.
#[derive(Debug, Clone)]
enum EnvOp {
    /// Set (or overwrite, case-insensitively) a variable.
    Set(OsString, OsString),
    /// Remove a variable (case-insensitively).
    Remove(OsString),
}

/// A builder describing how to launch a child process under a pseudoconsole.
///
/// Mirrors the `std::process::Command` builder API. This type only records
/// configuration; spawning is implemented by the blocking / async layers in a
/// later phase.
#[derive(Debug, Clone)]
pub(crate) struct Command {
    program: OsString,
    args: Vec<Arg>,
    env_clear: bool,
    env_ops: Vec<EnvOp>,
    cwd: Option<PathBuf>,
    creation_flags: u32,
    kill_on_drop: bool,
}

impl Command {
    /// Creates a new builder for launching `program`.
    ///
    /// Like `std::process::Command::new`, the program is not resolved or
    /// validated here; errors (embedded NUL, embedded `"`) surface when the
    /// command line is built.
    pub(crate) fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_os_string(),
            args: Vec::new(),
            env_clear: false,
            env_ops: Vec::new(),
            cwd: None,
            creation_flags: 0,
            kill_on_drop: false,
        }
    }

    /// Appends one argument, to be quoted/escaped as needed when the command
    /// line is built.
    pub(crate) fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(Arg::Regular(arg.as_ref().to_os_string()));
        self
    }

    /// Appends multiple arguments; equivalent to calling [`Command::arg`] for
    /// each element.
    pub(crate) fn args<I, S>(&mut self, args: I) -> &mut Self
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
    pub(crate) fn raw_arg(&mut self, text: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(Arg::Raw(text.as_ref().to_os_string()));
        self
    }

    /// Sets (or overwrites, case-insensitively) an environment variable for
    /// the child process.
    pub(crate) fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.env_ops.push(EnvOp::Set(
            key.as_ref().to_os_string(),
            value.as_ref().to_os_string(),
        ));
        self
    }

    /// Sets multiple environment variables; equivalent to calling
    /// [`Command::env`] for each pair.
    pub(crate) fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
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
    pub(crate) fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.env_ops
            .push(EnvOp::Remove(key.as_ref().to_os_string()));
        self
    }

    /// Clears the entire environment for the child process, including any
    /// modifications recorded so far. Variables set after this call still
    /// apply.
    pub(crate) fn env_clear(&mut self) -> &mut Self {
        self.env_clear = true;
        self.env_ops.clear();
        self
    }

    /// Sets the working directory for the child process.
    pub(crate) fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.cwd = Some(dir.as_ref().to_path_buf());
        self
    }

    /// Sets extra process creation flags, OR'ed into the `dwCreationFlags`
    /// that the crate always passes to `CreateProcessW` (such as
    /// `EXTENDED_STARTUPINFO_PRESENT`). Same semantics as
    /// `std::os::windows::process::CommandExt::creation_flags`.
    pub(crate) fn creation_flags(&mut self, flags: u32) -> &mut Self {
        self.creation_flags = flags;
        self
    }

    /// Requests that the spawned process (tree) be terminated when its handle
    /// is dropped. Only recorded here; enforced by the spawn layers.
    pub(crate) fn kill_on_drop(&mut self, kill: bool) -> &mut Self {
        self.kill_on_drop = kill;
        self
    }

    /// Returns the program name passed to [`Command::new`].
    pub(crate) fn get_program(&self) -> &OsStr {
        &self.program
    }

    /// Returns the configured working directory, if any.
    pub(crate) fn get_current_dir(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    /// Returns the extra creation flags configured via
    /// [`Command::creation_flags`].
    pub(crate) fn get_creation_flags(&self) -> u32 {
        self.creation_flags
    }

    /// Returns whether kill-on-drop was requested.
    pub(crate) fn get_kill_on_drop(&self) -> bool {
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
    pub(crate) fn build_command_line(&self) -> io::Result<Vec<u16>> {
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
                }
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
    /// Entries are sorted case-insensitively by comparing the uppercased
    /// UTF-16 units of their names, as required by the `CreateProcessW`
    /// documentation and equivalent to the standard library's ordinal
    /// ignore-case `EnvKey` ordering.
    ///
    /// Errors with [`io::ErrorKind::InvalidInput`] if a recorded name is
    /// empty, contains `=`, or if a name or value contains NUL.
    pub(crate) fn build_environment_block(&self) -> io::Result<Option<Vec<u16>>> {
        if !self.env_clear && self.env_ops.is_empty() {
            return Ok(None);
        }

        // Map from uppercased UTF-16 name to (original name, value); the
        // BTreeMap ordering directly yields the sort order required by
        // `CreateProcessW`.
        let mut map: BTreeMap<Vec<u16>, (OsString, OsString)> = BTreeMap::new();
        if !self.env_clear {
            for (key, value) in std::env::vars_os() {
                map.insert(upcased_wide(&key), (key, value));
            }
        }
        for op in &self.env_ops {
            match op {
                EnvOp::Set(key, value) => {
                    validate_env_key(key)?;
                    ensure_no_nuls(value)?;
                    match map.entry(upcased_wide(key)) {
                        // Overwriting keeps the casing of the existing name,
                        // matching `BTreeMap::insert` semantics in std's
                        // `CommandEnv`.
                        Entry::Occupied(mut entry) => entry.get_mut().1 = value.clone(),
                        Entry::Vacant(entry) => {
                            entry.insert((key.clone(), value.clone()));
                        }
                    }
                }
                EnvOp::Remove(key) => {
                    validate_env_key(key)?;
                    map.remove(&upcased_wide(key));
                }
            }
        }

        let mut block: Vec<u16> = Vec::new();
        for (key, value) in map.values() {
            block.extend(key.encode_wide());
            block.push(b'=' as u16);
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
pub(crate) fn to_wide_nul(s: &OsStr) -> io::Result<Vec<u16>> {
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

/// Uppercases one UTF-16 code unit for ordinal ignore-case comparison,
/// approximating `RtlUpcaseUnicodeChar`: single-unit uppercase mappings are
/// applied, everything else (surrogate halves, multi-char or non-BMP
/// mappings) is left unchanged. Exact for ASCII, which covers virtually all
/// real environment variable names.
fn upcase_unit(unit: u16) -> u16 {
    match char::from_u32(u32::from(unit)) {
        Some(c) => {
            let mut upper = c.to_uppercase();
            match (upper.next(), upper.next()) {
                (Some(up), None) if (up as u32) <= u32::from(u16::MAX) => up as u16,
                _ => unit,
            }
        }
        None => unit,
    }
}

fn upcased_wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().map(upcase_unit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the command line and returns it as a `String`, asserting the
    /// trailing NUL along the way.
    fn cmdline(cmd: &Command) -> String {
        let wide = cmd.build_command_line().expect("build_command_line failed");
        assert_eq!(wide.last(), Some(&0), "command line must be NUL-terminated");
        String::from_utf16(&wide[..wide.len() - 1]).expect("command line is not valid UTF-16")
    }

    /// Parses an environment block back into (name, value) pairs, asserting
    /// the double-NUL terminator.
    fn parse_env_block(block: &[u16]) -> Vec<(String, String)> {
        assert!(block.len() >= 2, "block too short: {block:?}");
        assert_eq!(
            &block[block.len() - 2..],
            &[0, 0],
            "block must end with two NULs"
        );
        let mut pairs = Vec::new();
        let mut rest = &block[..block.len() - 1];
        while !rest.is_empty() {
            let end = rest.iter().position(|&u| u == 0).expect("missing NUL");
            if end == 0 {
                // Leading NUL of the empty-environment representation.
                break;
            }
            let entry = String::from_utf16(&rest[..end]).expect("entry is not valid UTF-16");
            let eq = entry.find('=').expect("entry has no `=`");
            pairs.push((entry[..eq].to_string(), entry[eq + 1..].to_string()));
            rest = &rest[end + 1..];
        }
        pairs
    }

    #[test]
    fn cmdline_quoting_table() {
        // Expected values follow Rust std's MSVC CRT compatible algorithm
        // (library/std/src/sys/args/windows.rs) and the CRT parsing rules
        // documented by Microsoft and Raymond Chen, except that `|` also
        // forces quoting (a deliberate, parse-neutral hardening).
        struct Case {
            name: &'static str,
            arg: &'static str,
            expected: &'static str,
        }
        let cases = [
            Case {
                name: "empty argument is quoted",
                arg: "",
                expected: r#""""#,
            },
            Case {
                name: "plain argument is unquoted",
                arg: "hello",
                expected: "hello",
            },
            Case {
                name: "space forces quoting",
                arg: "hello world",
                expected: r#""hello world""#,
            },
            Case {
                name: "tab forces quoting",
                arg: "a\tb",
                expected: "\"a\tb\"",
            },
            Case {
                name: "inner quotes are backslash-escaped",
                arg: r#"he said "hi""#,
                expected: r#""he said \"hi\"""#,
            },
            Case {
                name: "one backslash before quote doubles to 2n+1",
                arg: r#"a\"b"#,
                expected: r#""a\\\"b""#,
            },
            Case {
                name: "two backslashes before quote double to 2n+1",
                arg: r#"a\\"b"#,
                expected: r#""a\\\\\"b""#,
            },
            Case {
                name: "trailing backslash doubles inside quotes",
                arg: r"C:\dir with space\",
                expected: r#""C:\dir with space\\""#,
            },
            Case {
                name: "trailing backslash unquoted passes through",
                arg: r"C:\dir\",
                expected: r"C:\dir\",
            },
            Case {
                name: "inner backslashes not before a quote are untouched",
                arg: r"C:\a b\c",
                expected: r#""C:\a b\c""#,
            },
            Case {
                name: "non-ascii with ascii space is quoted",
                arg: "こんにちは 世界",
                expected: "\"こんにちは 世界\"",
            },
            Case {
                name: "non-ascii without triggers is unquoted",
                arg: "日本語",
                expected: "日本語",
            },
            Case {
                name: "ideographic space does not force quoting",
                arg: "日\u{3000}本",
                expected: "日\u{3000}本",
            },
            Case {
                name: "vertical bar forces quoting",
                arg: "a|b",
                expected: r#""a|b""#,
            },
        ];
        for case in &cases {
            let mut cmd = Command::new("prog");
            cmd.arg(case.arg);
            assert_eq!(
                cmdline(&cmd),
                format!("\"prog\" {}", case.expected),
                "case: {}",
                case.name
            );
        }
    }

    #[test]
    fn cmdline_program_is_always_quoted() {
        let cmd = Command::new(r"C:\Program Files\app.exe");
        assert_eq!(cmdline(&cmd), r#""C:\Program Files\app.exe""#);
    }

    #[test]
    fn cmdline_raw_arg_passes_through_verbatim() {
        let mut cmd = Command::new(r"C:\Windows\System32\cmd.exe");
        cmd.raw_arg(r#"/c "echo he said \"hi\"" | more"#);
        assert_eq!(
            cmdline(&cmd),
            r#""C:\Windows\System32\cmd.exe" /c "echo he said \"hi\"" | more"#
        );
    }

    #[test]
    fn cmdline_mixes_regular_and_raw_args_in_order() {
        let mut cmd = Command::new("prog");
        cmd.args(["one", "two words"]).raw_arg("three|raw").arg("");
        assert_eq!(cmdline(&cmd), r#""prog" one "two words" three|raw """#);
    }

    #[test]
    fn cmdline_rejects_nul_everywhere() {
        let kind = |cmd: &Command| cmd.build_command_line().unwrap_err().kind();

        let cmd = Command::new("pro\0g");
        assert_eq!(kind(&cmd), io::ErrorKind::InvalidInput);

        let mut cmd = Command::new("prog");
        cmd.arg("a\0b");
        assert_eq!(kind(&cmd), io::ErrorKind::InvalidInput);

        let mut cmd = Command::new("prog");
        cmd.raw_arg("a\0b");
        assert_eq!(kind(&cmd), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn cmdline_rejects_quote_in_program() {
        let cmd = Command::new(r#"pro"g"#);
        let err = cmd.build_command_line().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn env_no_changes_returns_none() {
        let mut cmd = Command::new("prog");
        cmd.arg("x").current_dir(r"C:\").creation_flags(0x0800_0000);
        assert!(cmd.build_environment_block().unwrap().is_none());
    }

    #[test]
    fn env_inherits_parent_and_sorts_case_insensitively() {
        let mut cmd = Command::new("prog");
        cmd.env("CONPTY_OXIDE_TEST_INHERIT", "value1");
        let block = cmd.build_environment_block().unwrap().unwrap();
        let pairs = parse_env_block(&block);
        assert!(
            pairs
                .iter()
                .any(|(k, v)| k == "CONPTY_OXIDE_TEST_INHERIT" && v == "value1"),
            "override missing from block"
        );
        assert!(pairs.len() > 1, "parent environment was not inherited");
        // Strictly increasing by uppercased UTF-16 units: sorted and deduped.
        let upcased: Vec<Vec<u16>> = pairs
            .iter()
            .map(|(k, _)| upcased_wide(OsStr::new(k)))
            .collect();
        assert!(
            upcased.windows(2).all(|w| w[0] < w[1]),
            "block is not sorted case-insensitively: {pairs:?}"
        );
    }

    #[test]
    fn env_clear_alone_yields_empty_double_nul_block() {
        let mut cmd = Command::new("prog");
        cmd.env_clear();
        let block = cmd.build_environment_block().unwrap().unwrap();
        assert_eq!(block, vec![0, 0]);
        assert!(parse_env_block(&block).is_empty());
    }

    #[test]
    fn env_clear_then_set_sorts_case_insensitively() {
        let mut cmd = Command::new("prog");
        cmd.env_clear().envs([("b", "2"), ("A", "1"), ("C", "3")]);
        let block = cmd.build_environment_block().unwrap().unwrap();
        let pairs = parse_env_block(&block);
        // "b" sorts between "A" and "C" because comparison is done on
        // uppercased units; a case-sensitive sort would put it last.
        assert_eq!(
            pairs,
            vec![
                ("A".to_string(), "1".to_string()),
                ("b".to_string(), "2".to_string()),
                ("C".to_string(), "3".to_string()),
            ]
        );
    }

    #[test]
    fn env_override_is_case_insensitive_and_keeps_first_casing() {
        let mut cmd = Command::new("prog");
        cmd.env_clear().env("FOO", "1").env("foo", "2");
        let block = cmd.build_environment_block().unwrap().unwrap();
        assert_eq!(
            parse_env_block(&block),
            vec![("FOO".to_string(), "2".to_string())]
        );
    }

    #[test]
    fn env_remove_is_case_insensitive() {
        let mut cmd = Command::new("prog");
        cmd.env_clear()
            .env("FOO", "1")
            .env("BAR", "2")
            .env_remove("foo");
        let block = cmd.build_environment_block().unwrap().unwrap();
        assert_eq!(
            parse_env_block(&block),
            vec![("BAR".to_string(), "2".to_string())]
        );
    }

    #[test]
    fn env_set_after_remove_reinstates_variable() {
        let mut cmd = Command::new("prog");
        cmd.env_clear().env("A", "1").env_remove("a").env("a", "2");
        let block = cmd.build_environment_block().unwrap().unwrap();
        assert_eq!(
            parse_env_block(&block),
            vec![("a".to_string(), "2".to_string())]
        );
    }

    #[test]
    fn env_rejects_invalid_names_and_nuls() {
        let kind = |cmd: &Command| cmd.build_environment_block().unwrap_err().kind();

        let mut cmd = Command::new("prog");
        cmd.env("BAD=KEY", "v");
        assert_eq!(kind(&cmd), io::ErrorKind::InvalidInput);

        let mut cmd = Command::new("prog");
        cmd.env("", "v");
        assert_eq!(kind(&cmd), io::ErrorKind::InvalidInput);

        let mut cmd = Command::new("prog");
        cmd.env("A\0B", "v");
        assert_eq!(kind(&cmd), io::ErrorKind::InvalidInput);

        let mut cmd = Command::new("prog");
        cmd.env("KEY", "va\0lue");
        assert_eq!(kind(&cmd), io::ErrorKind::InvalidInput);

        let mut cmd = Command::new("prog");
        cmd.env_remove("BAD=KEY");
        assert_eq!(kind(&cmd), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn to_wide_nul_appends_terminator() {
        let wide = to_wide_nul(OsStr::new("C:\\dir")).unwrap();
        let expected: Vec<u16> = "C:\\dir".encode_utf16().chain(Some(0)).collect();
        assert_eq!(wide, expected);
    }

    #[test]
    fn to_wide_nul_rejects_embedded_nul() {
        let err = to_wide_nul(OsStr::new("a\0b")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
