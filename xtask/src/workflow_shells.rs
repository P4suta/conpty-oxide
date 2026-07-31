// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Runs ShellCheck over every explicit Bash block in one GitHub Actions
//! workflow.
//!
//! Actionlint's Windows pipe to ShellCheck can deadlock once a workflow
//! contains enough large embedded scripts. Actionlint still validates the
//! complete YAML and expression surface; this subcommand feeds each literal
//! Bash block to ShellCheck separately. The expected count makes the
//! indentation parser fail closed when the workflow gains, loses, or changes
//! a Bash block.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, ensure, Context, Result};
use regex::Regex;

struct Arguments {
    path: String,
    default_shell: String,
    expected_count: usize,
}

#[derive(Debug)]
struct BashBlock {
    name: String,
    line: usize,
    content: Vec<String>,
}

pub fn run(arguments: &[String]) -> Result<()> {
    let arguments = parse_arguments(arguments)?;
    let text = fs::read_to_string(&arguments.path)
        .with_context(|| format!("failed to read {}", arguments.path))?;
    let lines: Vec<&str> = text.lines().collect();
    let blocks = collect_bash_blocks(&lines, &arguments.default_shell)?;

    ensure!(
        blocks.len() == arguments.expected_count,
        "Found {} explicit Bash blocks in '{}'; expected {}.",
        blocks.len(),
        arguments.path,
        arguments.expected_count
    );

    let mut failed = false;
    for (index, block) in blocks.iter().enumerate() {
        let diagnostics = run_shellcheck(block, index)?;
        if let Some(diagnostics) = diagnostics {
            failed = true;
            println!(
                "ShellCheck failed for '{}' at {}:{}:",
                block.name, arguments.path, block.line
            );
            print!("{diagnostics}");
        }
    }
    ensure!(
        !failed,
        "ShellCheck rejected one or more Bash blocks in '{}'.",
        arguments.path
    );

    println!(
        "ShellCheck passed for all {} explicit Bash blocks in '{}'.",
        blocks.len(),
        arguments.path
    );
    Ok(())
}

fn parse_arguments(arguments: &[String]) -> Result<Arguments> {
    let mut path = None;
    let mut default_shell = String::from("none");
    let mut expected_count = None;
    let mut remaining = arguments.iter();
    while let Some(flag) = remaining.next() {
        let mut value = |flag: &str| {
            remaining
                .next()
                .with_context(|| format!("{flag} requires a value"))
        };
        match flag.as_str() {
            "--path" => path = Some(value("--path")?.clone()),
            "--default-shell" => {
                let shell = value("--default-shell")?;
                ensure!(
                    shell == "bash" || shell == "none",
                    "--default-shell must be `bash` or `none`, not `{shell}`"
                );
                default_shell = shell.clone();
            },
            "--expected-count" => {
                let count: usize = value("--expected-count")?
                    .parse()
                    .context("--expected-count must be an integer")?;
                ensure!(
                    (1..=1000).contains(&count),
                    "--expected-count must be between 1 and 1000"
                );
                expected_count = Some(count);
            },
            other => bail!("unknown workflow-shells flag `{other}`"),
        }
    }
    Ok(Arguments {
        path: path.context("--path is required")?,
        default_shell,
        expected_count: expected_count.context("--expected-count is required")?,
    })
}

fn leading_whitespace(line: &str) -> usize {
    line.chars().take_while(|c| c.is_whitespace()).count()
}

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

fn collect_bash_blocks(lines: &[&str], default_shell: &str) -> Result<Vec<BashBlock>> {
    let run_header = Regex::new(r"^(?P<indent>\s*)run:\s*(?P<style>[|>])[-+]?\s*$")
        .context("run-header pattern")?;
    let step_name =
        Regex::new(r"^\s*-\s+name:\s*(?P<name>.+?)\s*$").context("step-name pattern")?;
    let step_shell = Regex::new(r#"^\s*shell:\s*["']?(?P<shell>[^"'\s]+)["']?\s*$"#)
        .context("step-shell pattern")?;

    let mut blocks = Vec::new();
    let mut line_index = 0;
    while line_index < lines.len() {
        let Some(header) = run_header.captures(lines[line_index]) else {
            line_index += 1;
            continue;
        };
        let run_indent = header["indent"].chars().count();
        let mut shell = default_shell.to_owned();
        let mut name = format!("line {}", line_index + 1);
        for previous in (0..line_index).rev() {
            if is_blank(lines[previous]) {
                continue;
            }
            let previous_indent = leading_whitespace(lines[previous]);
            if previous_indent < run_indent {
                if let Some(found) = step_name.captures(lines[previous]) {
                    name = found["name"].to_owned();
                }
                break;
            }
            if previous_indent == run_indent {
                if let Some(found) = step_shell.captures(lines[previous]) {
                    shell = found["shell"].to_owned();
                }
            }
        }

        if shell != "bash" {
            line_index += 1;
            continue;
        }
        ensure!(
            &header["style"] == "|",
            "Explicit Bash step '{name}' must use a literal run block, not a folded block."
        );

        let mut end_index = line_index + 1;
        let mut block_indent = None;
        while end_index < lines.len() {
            let candidate = lines[end_index];
            if !is_blank(candidate) {
                let candidate_indent = leading_whitespace(candidate);
                if candidate_indent <= run_indent {
                    break;
                }
                if block_indent.is_none() {
                    block_indent = Some(candidate_indent);
                }
            }
            end_index += 1;
        }
        let Some(block_indent) = block_indent else {
            bail!("Explicit Bash step '{name}' has an empty run block.");
        };

        let mut content = Vec::new();
        for (content_index, content_line) in lines
            .iter()
            .copied()
            .enumerate()
            .take(end_index)
            .skip(line_index + 1)
        {
            if is_blank(content_line) {
                content.push(String::new());
                continue;
            }
            ensure!(
                content_line.chars().count() >= block_indent,
                "Malformed indentation in Bash step '{name}' at line {}.",
                content_index + 1
            );
            content.push(content_line.chars().skip(block_indent).collect());
        }

        blocks.push(BashBlock {
            name,
            line: line_index + 1,
            content,
        });
        line_index = end_index;
    }
    Ok(blocks)
}

/// Runs ShellCheck on one block; returns its diagnostics when it rejects the
/// script and `None` when the script is clean.
fn run_shellcheck(block: &BashBlock, index: usize) -> Result<Option<String>> {
    let temporary = temporary_script_path(index);
    let script = block.content.join("\n") + "\n";
    fs::write(&temporary, &script)
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    let output = Command::new("shellcheck")
        .args(["--shell", "bash", "--format", "gcc"])
        .arg(&temporary)
        .output();
    let removal = fs::remove_file(&temporary);
    let output = output.context("failed to run shellcheck")?;
    removal.with_context(|| format!("failed to remove {}", temporary.display()))?;
    if output.status.success() {
        return Ok(None);
    }
    let mut diagnostics = String::from_utf8_lossy(&output.stdout).into_owned();
    diagnostics.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok(Some(diagnostics))
}

fn temporary_script_path(index: usize) -> PathBuf {
    std::env::temp_dir().join(format!(
        "conpty-oxide-workflow-shell-{}-{index}.sh",
        std::process::id()
    ))
}

#[cfg(test)]
mod tests {
    use super::collect_bash_blocks;

    const WORKFLOW: &str = "\
jobs:
  demo:
    steps:
      - name: First step
        shell: bash
        run: |
          echo one
          echo two
      - name: Skipped step
        shell: pwsh
        run: |
          Write-Output three
";

    #[test]
    fn explicit_bash_blocks_are_extracted_with_names_and_lines() {
        let lines: Vec<&str> = WORKFLOW.lines().collect();
        let blocks = collect_bash_blocks(&lines, "none").unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].name, "First step");
        assert_eq!(blocks[0].line, 6);
        assert_eq!(blocks[0].content, ["echo one", "echo two"]);
    }

    #[test]
    fn the_default_shell_applies_when_no_shell_is_declared() {
        let workflow = "steps:\n  - name: Bare\n    run: |\n      echo bare\n";
        let lines: Vec<&str> = workflow.lines().collect();
        assert_eq!(collect_bash_blocks(&lines, "none").unwrap().len(), 0);
        assert_eq!(collect_bash_blocks(&lines, "bash").unwrap().len(), 1);
    }

    #[test]
    fn folded_bash_blocks_are_rejected() {
        let workflow = "steps:\n  - name: Folded\n    shell: bash\n    run: >\n      echo folded\n";
        let lines: Vec<&str> = workflow.lines().collect();
        let error = collect_bash_blocks(&lines, "none").unwrap_err();
        assert!(error.to_string().contains("must use a literal run block"));
    }

    #[test]
    fn empty_bash_blocks_are_rejected() {
        let workflow = "steps:\n  - name: Empty\n    shell: bash\n    run: |\n";
        let lines: Vec<&str> = workflow.lines().collect();
        let error = collect_bash_blocks(&lines, "none").unwrap_err();
        assert!(error.to_string().contains("has an empty run block"));
    }
}
