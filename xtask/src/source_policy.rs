// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Enforces repository-wide Rust source rules Clippy cannot express.
//!
//! This subcommand scans every project-owned `.rs` file for three banned
//! constructs: dynamic dispatch (with the single `error::Error` trait-object
//! exemption required by `std::error::Error::source`), ignored tests, and
//! lint-suppression attributes. The scan is textual, so this file must never
//! spell a banned construct literally; fixtures in the tests are assembled
//! from fragments at runtime for the same reason.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use regex::Regex;

/// Top-level directories that never contain project-owned Rust source.
const EXCLUDED_TOP_LEVEL: [&str; 3] = [".git", "target", "vendor"];

pub fn run() -> Result<()> {
    let root = repository_root()?;
    let mut files = Vec::new();
    collect_rust_files(root, root, &mut files)?;
    files.sort();

    let matchers = Matchers::new()?;
    let mut violations = Vec::new();
    for path in &files {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let relative = relative_repository_path(root, path)?;
        matchers.scan(&relative, &content, &mut violations);
    }

    if !violations.is_empty() {
        violations.sort();
        for violation in &violations {
            eprintln!("{violation}");
        }
        bail!(
            "source policy failed with {} violation(s)",
            violations.len()
        );
    }

    println!("Source policy passed for {} Rust files.", files.len());
    Ok(())
}

fn repository_root() -> Result<&'static Path> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("the xtask manifest directory has no parent")
}

fn relative_repository_path(root: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(root)
        .with_context(|| format!("path is outside the repository root: {}", path.display()))?;
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

/// Collects project-owned `.rs` files, skipping excluded top-level directories
/// and any nested `target` directory (such as this tool's own build output).
fn collect_rust_files(root: &Path, directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let entries = fs::read_dir(directory)
        .with_context(|| format!("failed to list {}", directory.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to read {}", directory.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if file_type.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let excluded = if directory == root {
                EXCLUDED_TOP_LEVEL.contains(&name.as_ref())
            } else {
                name == "target"
            };
            if !excluded {
                collect_rust_files(root, &path, files)?;
            }
        } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    Ok(())
}

struct Matchers {
    dynamic_dispatch: Regex,
    allowed_trait_object: Regex,
    ignored_test: Regex,
    lint_suppression: Regex,
}

impl Matchers {
    fn new() -> Result<Self> {
        Ok(Self {
            dynamic_dispatch: Regex::new(r"\bdyn\b").context("dynamic-dispatch pattern")?,
            allowed_trait_object: Regex::new(r"^\s+error::Error\b")
                .context("allowed trait-object pattern")?,
            ignored_test: Regex::new(r"#\s*!?\s*\[\s*ignore(?:\s*(?:\([^\]]*\)|=[^\]]*))?\s*\]")
                .context("ignored-test pattern")?,
            lint_suppression: Regex::new(
                r"(?s)#\s*!?\s*\[\s*(?:(?:cfg_attr)\s*\([^\]]*?)?\b(?:allow|expect)\s*\([^)]*\)[^\]]*\]",
            )
            .context("lint-suppression pattern")?,
        })
    }

    fn scan(&self, relative: &str, content: &str, violations: &mut Vec<String>) {
        for found in self.dynamic_dispatch.find_iter(content) {
            if self.allowed_trait_object.is_match(&content[found.end()..]) {
                continue;
            }
            push_violation(
                violations,
                relative,
                content,
                found.start(),
                found.as_str(),
                "dynamic dispatch is forbidden in project-owned Rust",
            );
        }
        for found in self.ignored_test.find_iter(content) {
            push_violation(
                violations,
                relative,
                content,
                found.start(),
                found.as_str(),
                "ignored tests are forbidden",
            );
        }
        for found in self.lint_suppression.find_iter(content) {
            push_violation(
                violations,
                relative,
                content,
                found.start(),
                found.as_str(),
                "lint allow/expect attributes are forbidden",
            );
        }
    }
}

fn push_violation(
    violations: &mut Vec<String>,
    relative: &str,
    content: &str,
    index: usize,
    value: &str,
    rule: &str,
) {
    let line = 1 + content[..index].matches('\n').count();
    let display = value.split_whitespace().collect::<Vec<_>>().join(" ");
    violations.push(format!("{relative}:{line}: {rule}: {display}"));
}

#[cfg(test)]
mod tests {
    use super::Matchers;

    /// Assembles a banned construct from fragments so this file never contains
    /// it literally; the textual scan covers this file too.
    fn assemble(fragments: &[&str]) -> String {
        fragments.concat()
    }

    fn scan(content: &str) -> Vec<String> {
        let matchers = Matchers::new().unwrap();
        let mut violations = Vec::new();
        matchers.scan("fixture.rs", content, &mut violations);
        violations
    }

    #[test]
    fn dynamic_dispatch_is_reported_with_its_line() {
        let keyword = assemble(&["d", "yn"]);
        let content = format!("fn main() {{}}\nfn f(_value: &{keyword} std::fmt::Debug) {{}}\n");
        let violations = scan(&content);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].starts_with("fixture.rs:2: dynamic dispatch"));
    }

    #[test]
    fn the_error_source_trait_object_is_exempt() {
        let keyword = assemble(&["d", "yn"]);
        let content =
            format!("fn source(&self) -> Option<&({keyword} error::Error + 'static)> {{ None }}\n");
        assert!(scan(&content).is_empty());
    }

    #[test]
    fn ignored_tests_are_reported() {
        let attribute = assemble(&["#", "[", "ignore", "]"]);
        let content = format!("{attribute}\nfn later() {{}}\n");
        let violations = scan(&content);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("ignored tests are forbidden"));
    }

    #[test]
    fn ignored_tests_with_reasons_are_reported() {
        let attribute = assemble(&["#", "[", "ignore", " = \"slow\"", "]"]);
        let violations = scan(&attribute);
        assert_eq!(violations.len(), 1);
    }

    #[test]
    fn lint_suppressions_are_reported() {
        let attribute = assemble(&["#", "[", "allow", "(dead_code)", "]"]);
        let violations = scan(&attribute);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("lint allow/expect attributes are forbidden"));
    }

    #[test]
    fn conditional_lint_suppressions_are_reported() {
        let attribute = assemble(&["#", "[", "cfg_attr", "(test, ", "expect", "(unused))", "]"]);
        let violations = scan(&attribute);
        assert_eq!(violations.len(), 1);
    }

    #[test]
    fn ordinary_attributes_pass() {
        let content = "#[cfg(test)]\n#[derive(Debug, Clone)]\nstruct Plain;\n";
        assert!(scan(content).is_empty());
    }
}
