// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Enforces repository-wide Rust source rules Clippy cannot express.
//!
//! This subcommand parses every project-owned `.rs` file and rejects trait
//! objects, ignored tests, and lint-suppression attributes. The only trait
//! object exception is the return type required by
//! `std::error::Error::source`. Macro token streams are inspected separately
//! because they are intentionally opaque to the Rust syntax tree.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use proc_macro2::{Delimiter, Span, TokenStream, TokenTree};
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{
    Attribute, FnArg, GenericArgument, ImplItemFn, ItemImpl, Meta, Path as SynPath, PathArguments,
    ReturnType, Token, TraitBoundModifier, Type, TypeParamBound, TypeTraitObject,
};

use crate::util::repository_root;

/// Top-level directories that never contain project-owned Rust source.
const EXCLUDED_TOP_LEVEL: [&str; 3] = [".git", "target", "vendor"];

const DYNAMIC_DISPATCH_RULE: &str = "dynamic dispatch is forbidden in project-owned Rust";
const IGNORED_TEST_RULE: &str = "ignored tests are forbidden";
const LINT_SUPPRESSION_RULE: &str = "lint allow/expect attributes are forbidden";
const INVALID_SYNTAX_RULE: &str = "invalid Rust syntax prevents source-policy analysis";

pub fn run() -> Result<()> {
    let root = repository_root()?;
    let mut files = Vec::new();
    collect_rust_files(root, root, &mut files)?;
    files.sort();

    let mut violations = Vec::new();
    for path in &files {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let relative = relative_repository_path(root, path)?;
        scan_source(&relative, &content, &mut violations);
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

fn scan_source(relative: &str, content: &str, violations: &mut Vec<String>) {
    let file = match syn::parse_file(content) {
        Ok(file) => file,
        Err(error) => {
            push_violation(
                violations,
                relative,
                error.span(),
                INVALID_SYNTAX_RULE,
                &error.to_string(),
            );
            return;
        },
    };

    SourcePolicyVisitor {
        relative,
        violations,
        in_error_impl: false,
        allowed_trait_object: None,
    }
    .visit_file(&file);
}

struct SourcePolicyVisitor<'output> {
    relative: &'output str,
    violations: &'output mut Vec<String>,
    in_error_impl: bool,
    allowed_trait_object: Option<*const TypeTraitObject>,
}

impl SourcePolicyVisitor<'_> {
    fn report(&mut self, span: Span, rule: &str, value: &str) {
        push_violation(self.violations, self.relative, span, rule, value);
    }

    fn inspect_attribute_meta(&mut self, meta: &Meta) {
        if meta.path().is_ident("ignore") {
            self.report(meta.span(), IGNORED_TEST_RULE, "ignore");
        } else if meta.path().is_ident("allow") || meta.path().is_ident("expect") {
            let name = if meta.path().is_ident("allow") {
                "allow"
            } else {
                "expect"
            };
            self.report(meta.span(), LINT_SUPPRESSION_RULE, name);
        } else if meta.path().is_ident("cfg_attr") {
            self.inspect_cfg_attr(meta);
        }
    }

    fn inspect_cfg_attr(&mut self, meta: &Meta) {
        let Meta::List(list) = meta else {
            return;
        };
        let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
        match parser.parse2(list.tokens.clone()) {
            Ok(arguments) => {
                // The first argument is the cfg predicate; all remaining
                // arguments are attributes selected by that predicate.
                for nested in arguments.iter().skip(1) {
                    self.inspect_attribute_meta(nested);
                }
            },
            Err(error) => self.report(
                error.span(),
                INVALID_SYNTAX_RULE,
                "malformed cfg_attr attribute",
            ),
        }
    }

    fn inspect_macro_tokens(&mut self, tokens: TokenStream) {
        let trees: Vec<TokenTree> = tokens.into_iter().collect();
        let mut index = 0;
        while index < trees.len() {
            match &trees[index] {
                TokenTree::Ident(ident) if ident == "dyn" => {
                    self.report(ident.span(), DYNAMIC_DISPATCH_RULE, "dyn");
                },
                TokenTree::Punct(punct) if punct.as_char() == '#' => {
                    let mut group_index = index + 1;
                    if matches!(
                        trees.get(group_index),
                        Some(TokenTree::Punct(punct)) if punct.as_char() == '!'
                    ) {
                        group_index += 1;
                    }
                    if let Some(TokenTree::Group(group)) = trees.get(group_index) {
                        if group.delimiter() == Delimiter::Bracket {
                            if let Ok(meta) = syn::parse2::<Meta>(group.stream()) {
                                self.inspect_attribute_meta(&meta);
                            }
                        }
                    }
                },
                _ => {},
            }

            if let TokenTree::Group(group) = &trees[index] {
                self.inspect_macro_tokens(group.stream());
            }
            index += 1;
        }
    }
}

impl<'ast> Visit<'ast> for SourcePolicyVisitor<'_> {
    fn visit_attribute(&mut self, attribute: &'ast Attribute) {
        self.inspect_attribute_meta(&attribute.meta);
        if let Meta::List(list) = &attribute.meta {
            self.inspect_macro_tokens(list.tokens.clone());
        }
        visit::visit_attribute(self, attribute);
    }

    fn visit_item_impl(&mut self, item_impl: &'ast ItemImpl) {
        let previous = self.in_error_impl;
        self.in_error_impl = item_impl
            .trait_
            .as_ref()
            .is_some_and(|(_, path, _)| is_standard_error_path(path));
        visit::visit_item_impl(self, item_impl);
        self.in_error_impl = previous;
    }

    fn visit_impl_item_fn(&mut self, method: &'ast ImplItemFn) {
        let previous = self.allowed_trait_object;
        self.allowed_trait_object = if self.in_error_impl {
            source_return_trait_object(method).map(|trait_object| trait_object as *const _)
        } else {
            None
        };
        visit::visit_impl_item_fn(self, method);
        self.allowed_trait_object = previous;
    }

    fn visit_macro(&mut self, item_macro: &'ast syn::Macro) {
        self.inspect_macro_tokens(item_macro.tokens.clone());
        visit::visit_macro(self, item_macro);
    }

    fn visit_type_trait_object(&mut self, trait_object: &'ast TypeTraitObject) {
        let current = trait_object as *const _;
        if self.allowed_trait_object != Some(current) {
            let span = trait_object
                .dyn_token
                .as_ref()
                .map_or_else(|| trait_object.span(), |dyn_token| dyn_token.span);
            self.report(span, DYNAMIC_DISPATCH_RULE, "dyn");
        }
        visit::visit_type_trait_object(self, trait_object);
    }
}

fn source_return_trait_object(method: &ImplItemFn) -> Option<&TypeTraitObject> {
    let signature = &method.sig;
    if signature.ident != "source"
        || signature.constness.is_some()
        || signature.asyncness.is_some()
        || signature.unsafety.is_some()
        || signature.abi.is_some()
        || signature.variadic.is_some()
        || !signature.generics.params.is_empty()
        || signature.generics.where_clause.is_some()
        || signature.inputs.len() != 1
    {
        return None;
    }

    let Some(FnArg::Receiver(receiver)) = signature.inputs.first() else {
        return None;
    };
    let Some((_, lifetime)) = &receiver.reference else {
        return None;
    };
    if lifetime.is_some() || receiver.mutability.is_some() || receiver.colon_token.is_some() {
        return None;
    }

    let ReturnType::Type(_, return_type) = &signature.output else {
        return None;
    };
    let Type::Path(option) = strip_type_wrappers(return_type) else {
        return None;
    };
    if option.qself.is_some() || !is_option_path(&option.path) {
        return None;
    }
    let segment = option.path.segments.last()?;
    let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    if arguments.args.len() != 1 {
        return None;
    }
    let Some(GenericArgument::Type(Type::Reference(reference))) = arguments.args.first() else {
        return None;
    };
    if reference.lifetime.is_some() || reference.mutability.is_some() {
        return None;
    }
    let Type::TraitObject(trait_object) = strip_type_wrappers(&reference.elem) else {
        return None;
    };
    if is_standard_error_trait_object(trait_object) {
        Some(trait_object)
    } else {
        None
    }
}

fn strip_type_wrappers(mut ty: &Type) -> &Type {
    loop {
        ty = match ty {
            Type::Group(group) => &group.elem,
            Type::Paren(paren) => &paren.elem,
            _ => return ty,
        };
    }
}

fn is_option_path(path: &SynPath) -> bool {
    option_path_matches(path, &["Option"])
        || option_path_matches(path, &["std", "option", "Option"])
        || option_path_matches(path, &["core", "option", "Option"])
}

fn option_path_matches(path: &SynPath, expected: &[&str]) -> bool {
    path.segments.len() == expected.len()
        && path
            .segments
            .iter()
            .zip(expected)
            .enumerate()
            .all(|(index, (segment, expected))| {
                segment.ident == *expected
                    && (index + 1 == path.segments.len()
                        || matches!(segment.arguments, PathArguments::None))
            })
}

fn is_standard_error_path(path: &SynPath) -> bool {
    path_matches(path, &["std", "error", "Error"])
        || path_matches(path, &["core", "error", "Error"])
}

fn path_matches(path: &SynPath, expected: &[&str]) -> bool {
    path.segments.len() == expected.len()
        && path
            .segments
            .iter()
            .zip(expected)
            .all(|(segment, expected)| {
                segment.ident == *expected && matches!(segment.arguments, PathArguments::None)
            })
}

fn is_standard_error_trait_object(trait_object: &TypeTraitObject) -> bool {
    if trait_object.bounds.len() != 2 {
        return false;
    }
    let mut error_bound = false;
    let mut static_bound = false;
    for bound in &trait_object.bounds {
        match bound {
            TypeParamBound::Trait(bound)
                if matches!(bound.modifier, TraitBoundModifier::None)
                    && bound.lifetimes.is_none()
                    && is_standard_error_path(&bound.path) =>
            {
                error_bound = true;
            },
            TypeParamBound::Lifetime(lifetime) if lifetime.ident == "static" => {
                static_bound = true;
            },
            _ => return false,
        }
    }
    error_bound && static_bound
}

fn push_violation(
    violations: &mut Vec<String>,
    relative: &str,
    span: Span,
    rule: &str,
    value: &str,
) {
    let line = span.start().line.max(1);
    let display = value.split_whitespace().collect::<Vec<_>>().join(" ");
    violations.push(format!("{relative}:{line}: {rule}: {display}"));
}

#[cfg(test)]
mod tests {
    use super::{scan_source, IGNORED_TEST_RULE, LINT_SUPPRESSION_RULE};

    fn scan(content: &str) -> Vec<String> {
        let mut violations = Vec::new();
        scan_source("fixture.rs", content, &mut violations);
        violations
    }

    #[test]
    fn boxed_trait_objects_are_reported_with_their_line() {
        let violations = scan("fn main() {}\ntype Erased = Box<dyn std::fmt::Debug>;\n");
        assert_eq!(violations.len(), 1);
        assert!(violations[0].starts_with("fixture.rs:2: dynamic dispatch"));
    }

    #[test]
    fn referenced_and_arc_trait_objects_are_reported() {
        let violations = scan(
            "use std::sync::Arc;\n\
             fn borrowed(_: &dyn std::fmt::Debug) {}\n\
             fn shared(_: Arc<dyn Send>) {}\n",
        );
        assert_eq!(violations.len(), 2);
    }

    #[test]
    fn trait_object_type_aliases_are_reported() {
        let violations = scan("type Handler = dyn Fn() + Send;\n");
        assert_eq!(violations.len(), 1);
    }

    #[test]
    fn trait_objects_in_macro_tokens_are_reported() {
        let violations = scan(
            "macro_rules! erased {\n\
                 () => { type Hidden = Box<dyn std::fmt::Debug>; };\n\
             }\n",
        );
        assert_eq!(violations.len(), 1);
        assert!(violations[0].starts_with("fixture.rs:2: dynamic dispatch"));
    }

    #[test]
    fn the_standard_error_source_trait_object_is_exempt() {
        let content = "struct Example;\n\
            impl std::error::Error for Example {\n\
                fn source(&self) -> Option<&(dyn std::error::Error + 'static)> { None }\n\
            }\n";
        let violations = scan(content);
        assert!(violations.is_empty(), "{violations:?}");
    }

    #[test]
    fn an_error_trait_object_elsewhere_is_not_exempt() {
        let content = "struct Holder<'a> {\n\
                source: &'a (dyn std::error::Error + 'static),\n\
            }\n";
        let violations = scan(content);
        assert_eq!(violations.len(), 1);
    }

    #[test]
    fn a_source_method_outside_the_standard_error_impl_is_not_exempt() {
        let content = "struct Example;\n\
            impl Example {\n\
                fn source(&self) -> Option<&(dyn std::error::Error + 'static)> { None }\n\
            }\n";
        let violations = scan(content);
        assert_eq!(violations.len(), 1);
    }

    #[test]
    fn malformed_source_signatures_are_not_exempt() {
        let content = "struct Example;\n\
            impl std::error::Error for Example {\n\
                fn source(&mut self) -> Option<&(dyn std::error::Error + 'static)> { None }\n\
            }\n";
        let violations = scan(content);
        assert_eq!(violations.len(), 1);
    }

    #[test]
    fn ignored_tests_and_reasons_are_reported() {
        let violations = scan("#[ignore]\nfn first() {}\n#[ignore = \"slow\"]\nfn second() {}\n");
        assert_eq!(violations.len(), 2);
        assert!(violations
            .iter()
            .all(|violation| violation.contains(IGNORED_TEST_RULE)));
    }

    #[test]
    fn outer_and_inner_lint_suppressions_are_reported() {
        let content = "#![allow(dead_code)]\n#[expect(unused_variables)]\nfn main() {}\n";
        let violations = scan(content);
        assert_eq!(violations.len(), 2);
        assert!(violations
            .iter()
            .all(|violation| violation.contains(LINT_SUPPRESSION_RULE)));
    }

    #[test]
    fn conditional_policy_attributes_are_reported() {
        let content = "#![cfg_attr(test, allow(dead_code))]\n\
            #[cfg_attr(windows, cfg_attr(test, expect(unused_variables)))]\n\
            #[cfg_attr(test, ignore = \"slow\")]\n\
            fn main() {}\n";
        let violations = scan(content);
        assert_eq!(violations.len(), 3);
    }

    #[test]
    fn policy_attributes_in_macro_tokens_are_reported() {
        let content = "macro_rules! suppressed {\n\
                () => { #[allow(dead_code)] fn hidden() {} };\n\
            }\n";
        let violations = scan(content);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains(LINT_SUPPRESSION_RULE));
    }

    #[test]
    fn macro_attribute_metavariables_pass() {
        let content = "macro_rules! forwarded {\n\
                ($(#[$attribute:meta])*) => {};\n\
            }\n";
        assert!(scan(content).is_empty());
    }

    #[test]
    fn comments_strings_and_doc_text_are_not_code() {
        let content = r#"
// Box<dyn std::fmt::Debug> and #[allow(dead_code)] are examples.
/// `dyn Trait`, `#[ignore]`, and `#[expect(dead_code)]` are documentation.
const EXAMPLE: &str = "Box<dyn Trait> #[allow(dead_code)]";
struct Plain;
"#;
        assert!(scan(content).is_empty());
    }

    #[test]
    fn ordinary_attributes_pass() {
        let content = "#![cfg(windows)]\n#[derive(Debug, Clone)]\nstruct Plain;\n";
        assert!(scan(content).is_empty());
    }

    #[test]
    fn invalid_rust_is_reported_with_its_path_and_position() {
        let violations = scan("fn valid() {}\nfn broken(\n");
        assert_eq!(violations.len(), 1);
        assert!(violations[0].starts_with("fixture.rs:2: invalid Rust syntax"));
    }
}
