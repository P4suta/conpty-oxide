// SPDX-FileCopyrightText: 2025 conpty-oxide contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

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
    cmd.arg("x").current_dir(r"C:\");
    assert!(cmd.build_environment_block().unwrap().is_none());
}

#[test]
fn working_directory_getter_returns_the_configured_path() {
    let mut cmd = Command::new("prog");
    assert_eq!(cmd.get_current_dir(), None);
    cmd.current_dir(r"C:\configured-directory");
    assert_eq!(
        cmd.get_current_dir(),
        Some(Path::new(r"C:\configured-directory"))
    );
}

#[cfg(any(feature = "blocking", feature = "tokio"))]
#[test]
fn kill_on_drop_getter_returns_the_configured_policy() {
    let mut cmd = Command::new("prog");
    assert!(!cmd.get_kill_on_drop());
    cmd.kill_on_drop(true);
    assert!(cmd.get_kill_on_drop());
    cmd.kill_on_drop(false);
    assert!(!cmd.get_kill_on_drop());
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
    // Strictly increasing by Windows ordinal ignore-case keys: sorted and
    // deduplicated with the same comparison CreateProcessW expects.
    let keys: Vec<EnvKey> = pairs
        .iter()
        .map(|(k, _)| EnvKey::new(OsStr::new(k)).unwrap())
        .collect();
    assert!(
        keys.windows(2).all(|w| w[0] < w[1]),
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
fn env_uses_windows_case_folding_for_non_ascii_keys() {
    // Rust Unicode uppercasing maps long-s to S and micro-sign to Greek mu,
    // but Windows CompareStringOrdinal deliberately treats both pairs as
    // distinct environment keys.
    let mut cmd = Command::new("prog");
    cmd.env_clear()
        .env("S", "latin-s")
        .env("ſ", "long-s")
        .env("Μ", "greek-mu")
        .env("µ", "micro-sign");
    let pairs = parse_env_block(&cmd.build_environment_block().unwrap().unwrap());

    assert_eq!(pairs.len(), 4, "Windows-distinct keys were overwritten");
    for expected in [
        ("S", "latin-s"),
        ("ſ", "long-s"),
        ("Μ", "greek-mu"),
        ("µ", "micro-sign"),
    ] {
        assert!(
            pairs
                .iter()
                .any(|(key, value)| key == expected.0 && value == expected.1),
            "missing Windows-distinct environment entry {expected:?}: {pairs:?}"
        );
    }
}

#[test]
fn env_key_equality_matches_windows_ordinal_comparison() {
    let key = |text: &str| EnvKey::new(OsStr::new(text)).unwrap();

    assert_eq!(key("PATH"), key("path"));
    assert_ne!(key("S"), key("ſ"));
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
